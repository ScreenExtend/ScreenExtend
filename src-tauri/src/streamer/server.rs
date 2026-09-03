use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Router,
};

use serde::Deserialize;
use tokio::sync::oneshot;

use super::config::{Config, ScalePercent};
use super::pipeline;
use super::platform;
use super::session::{self, DeviceInfo, OtpLimiter, OtpOutcome, SharedLocalIps, SharedOtpLimiter};
use super::webrtc_session::{self, RTCIceServer};

const MAX_DEVICE_NAME_CHARS: usize = 64;
const MAX_SESSION_ID_LEN: usize = 64;
const MAX_OTP_LEN: usize = 32;
const MAX_OS_LEN: usize = 64;
const MAX_SDP_LEN: usize = 64 * 1024;

#[derive(Deserialize, Default, Clone, Copy, Debug)]
struct AudioCapabilities {
    #[serde(default, rename = "webcodecsOpus")]
    webcodecs_opus: bool,
    #[serde(default)]
    sab: bool,
    #[serde(default)]
    worklet: bool,
}

#[derive(Deserialize)]
struct JoinRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    otp: String,
    #[serde(default, rename = "deviceToken")]
    device_token: String,
    #[serde(default, rename = "audioCapabilities")]
    audio_capabilities: AudioCapabilities,
    #[serde(
        default,
        rename = "deviceName",
        deserialize_with = "deserialize_device_name"
    )]
    device_name: String,
    #[serde(default)]
    os: String,
    #[serde(default, rename = "refreshRate")]
    refresh_rate: u32,
    width: u32,
    height: u32,
    #[serde(default = "default_dpr")]
    dpr: f64,
    sdp: String,
}

fn default_dpr() -> f64 {
    1.0
}

fn sanitize_device_name(raw: &str) -> String {
    raw.trim()
        .chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(*c,
                    '\u{200B}'..='\u{200F}'
                    | '\u{202A}'..='\u{202E}'
                    | '\u{2066}'..='\u{2069}'
                    | '\u{FEFF}')
        })
        .take(MAX_DEVICE_NAME_CHARS)
        .collect()
}

fn deserialize_device_name<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(d)?;
    Ok(sanitize_device_name(&raw))
}

const DISPLAY_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LEAVE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);
static DISPLAY_CORRELATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub const MIN_REFRESH_RATE: u32 = 15;
pub const MAX_REFRESH_RATE: u32 = 500;
pub const MIN_DISPLAY_SCALE: u32 = 25;
pub const MAX_DISPLAY_SCALE: u32 = 200;
pub const MAX_EFFECTIVE_SCALE: u32 = 500;

#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    ice_servers: Arc<Vec<RTCIceServer>>,
    net_json: Arc<String>,
    otp_limiter: SharedOtpLimiter,
    local_ips: SharedLocalIps,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            ice_servers: Arc::new(build_ice_servers(&config)),
            net_json: Arc::new(build_net_json(&config)),
            otp_limiter: config
                .otp_limiter
                .clone()
                .unwrap_or_else(|| Arc::new(OtpLimiter::new())),
            local_ips: config
                .local_ips
                .clone()
                .unwrap_or_else(session::new_shared_local_ips),
            config: Arc::new(config),
        }
    }

    pub fn display_slot_unavailable(&self, client_ip: &str) -> bool {
        let Some(client) = self.config.virtual_display.as_ref() else {
            return false;
        };
        let Some(max) = client.max_concurrent_displays() else {
            return false;
        };
        let Some(sessions) = self.config.sessions.as_ref() else {
            return false;
        };
        if session::get_live_display(sessions, client_ip).is_some() {
            return false;
        }
        session::live_display_count(sessions) >= max
    }

    pub fn display_capacity(&self) -> (Option<usize>, usize) {
        let max = self
            .config
            .virtual_display
            .as_ref()
            .and_then(|c| c.max_concurrent_displays());
        let in_use = self
            .config
            .sessions
            .as_ref()
            .map(session::live_display_count)
            .unwrap_or(0);
        (max, in_use)
    }

    pub fn is_same_device(&self, peer: IpAddr) -> bool {
        let ip = normalize_peer_ip(peer);
        ip.is_loopback() || self.local_ips.lock().unwrap().contains(&ip)
    }

    pub fn fallback_ice_servers(&self) -> Vec<RTCIceServer> {
        self.ice_servers.as_ref().clone()
    }

    pub fn ice_with_turn(&self, mut base: Vec<RTCIceServer>) -> Vec<RTCIceServer> {
        if let Some(turn) = user_turn_ice_server(&self.config) {
            base.push(turn);
        }
        if let Some(turn) = ephemeral_turn_ice_server(&self.config) {
            base.push(turn);
        }
        base
    }

    pub fn ice_json_live(&self) -> String {
        let mut servers: Vec<serde_json::Value> = Vec::new();
        if !self.config.stun_urls.is_empty() {
            servers.push(serde_json::json!({ "urls": self.config.stun_urls }));
        }
        if let (Some(url), Some(user), Some(cred)) = (
            &self.config.turn_url,
            &self.config.turn_username,
            &self.config.turn_credential,
        ) {
            servers.push(serde_json::json!({
                "urls": [url], "username": user, "credential": cred
            }));
        }
        for turn in [
            user_turn_ice_server(&self.config),
            ephemeral_turn_ice_server(&self.config),
        ]
        .into_iter()
        .flatten()
        {
            servers.push(serde_json::json!({
                "urls": turn.urls, "username": turn.username, "credential": turn.credential
            }));
        }
        serde_json::json!({ "iceServers": servers }).to_string()
    }
}

pub fn user_turn_ice_server(config: &Config) -> Option<RTCIceServer> {
    let shared = config.user_turn.as_ref()?;
    let cfg = shared.lock().unwrap();
    if cfg.urls.is_empty() {
        return None;
    }
    Some(RTCIceServer {
        urls: cfg.urls.clone(),
        username: cfg.username.clone(),
        credential: cfg.credential.clone(),
    })
}

pub fn ephemeral_turn_ice_server(config: &Config) -> Option<RTCIceServer> {
    let secret = config.turn_secret.as_deref()?;
    if config.turn_urls.is_empty() {
        return None;
    }
    let ttl = std::time::Duration::from_secs(config.turn_ttl_secs.max(60));
    match turn::auth::generate_long_term_credentials(secret, ttl) {
        Ok((username, credential)) => Some(RTCIceServer {
            urls: config.turn_urls.clone(),
            username,
            credential,
        }),
        Err(e) => {
            teprintln!("[turn] failed to mint ephemeral credentials: {e}");
            None
        }
    }
}

fn normalize_peer_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

pub fn collect_local_ips<I, S>(adapter_ips: I) -> Vec<IpAddr>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut ips: Vec<IpAddr> = vec![
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(Ipv6Addr::LOCALHOST),
    ];
    for ip in adapter_ips {
        if let Ok(parsed) = ip.as_ref().parse::<IpAddr>() {
            ips.push(normalize_peer_ip(parsed));
        }
    }
    ips.sort();
    ips.dedup();
    ips
}

pub struct ProcessedResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
    pub device_token: Option<String>,
}

impl ProcessedResponse {
    fn err(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status: status.as_u16(),
            content_type: "text/plain",
            body: body.into(),
            device_token: None,
        }
    }
}

pub async fn run(config: Config, handle: Option<axum_server::Handle>) -> Result<()> {
    let handle = handle.unwrap_or_default();

    let state = AppState::new(config.clone());

    let app = router(state);

    let http_addr = SocketAddr::from((config.bind_ip, config.port));
    let https_addr = SocketAddr::from((config.bind_ip, config.https_port));

    let extra_sans: Vec<String> = config.lan_ip.iter().cloned().collect();
    let material = super::tls::load_or_generate(
        config.tls_cert.as_deref(),
        config.tls_key.as_deref(),
        &extra_sans,
    )?;
    let self_signed = material.self_signed;
    let tls_config = super::tls::rustls_config(&material).await?;

    log_urls(
        config.lan_ip.as_deref(),
        config.port,
        config.https_port,
        self_signed,
    );

    use axum_server::accept::NoDelayAcceptor;
    let http = axum_server::bind(http_addr)
        .acceptor(NoDelayAcceptor)
        .handle(handle.clone())
        .serve(
            app.clone()
                .into_make_service_with_connect_info::<SocketAddr>(),
        );
    let https = axum_server::bind_rustls(https_addr, tls_config)
        .map(|rustls| rustls.acceptor(NoDelayAcceptor))
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<SocketAddr>());

    tokio::try_join!(
        async {
            http.await.with_context(|| {
                format!("HTTP server error on {http_addr} — port in use or blocked by firewall?")
            })
        },
        async {
            https.await.with_context(|| {
                format!("HTTPS server error on {https_addr} — port in use or blocked by firewall?")
            })
        },
    )?;
    Ok(())
}

fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/", get(index))
        .route("/whep", post(whep))
        .route("/leave", post(leave))
        .route("/transform-worker.js", get(transform_worker))
        .route("/input.js", get(input_js))
        .route("/audio.js", get(audio_js))
        .route("/audio-worklet.js", get(audio_worklet_js))
        .route("/nosleep.js", get(nosleep_js))
        .route("/logo.svg", get(logo))
        .route("/styles.css", get(styles))
        .route("/ice-config", get(ice_config))
        .route("/net-config", get(net_config))
        .route("/reconfig", get(reconfig))
        .route("/audio-outputs", post(audio_outputs))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

// Cross-origin isolation (§6.4): with these on the document *and* a secure context (HTTPS or
// localhost), `crossOriginIsolated` becomes true and the client can use the SharedArrayBuffer
// ring buffer. Over plain HTTP they're inert (isolation stays off; client falls back to the
// postMessage ring). All our sub-resources are same-origin, so COEP:require-corp does not block
// them — verify the existing video worker still loads if this ever regresses.
fn isolation_headers() -> [(header::HeaderName, header::HeaderValue); 2] {
    [
        (
            header::HeaderName::from_static("cross-origin-opener-policy"),
            header::HeaderValue::from_static("same-origin"),
        ),
        (
            header::HeaderName::from_static("cross-origin-embedder-policy"),
            header::HeaderValue::from_static("require-corp"),
        ),
    ]
}

async fn index(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if state.display_slot_unavailable(&peer.ip().to_string()) {
        return at_capacity_page();
    }
    let flag = if state.is_same_device(peer.ip()) {
        "true"
    } else {
        "false"
    };
    let html = include_str!("static/index.html").replace("__SAME_DEVICE_FLAG__", flag);
    (isolation_headers(), Html(html)).into_response()
}

fn at_capacity_page() -> Response {
    const PAGE: &str = include_str!("static/at-capacity.html");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        PAGE,
    )
        .into_response()
}

async fn transform_worker() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript")],
        include_str!("static/transform-worker.js"),
    )
        .into_response()
}

async fn input_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript")],
        include_str!("static/input.js"),
    )
        .into_response()
}

async fn audio_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript")],
        include_str!("static/audio.js"),
    )
        .into_response()
}

async fn audio_worklet_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript")],
        include_str!("static/audio-worklet.js"),
    )
        .into_response()
}

async fn nosleep_js() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript")],
        include_str!("static/nosleep.js"),
    )
        .into_response()
}

async fn logo() -> Response {
    (
        [(header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("static/logo.svg"),
    )
        .into_response()
}

async fn styles() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css")],
        include_str!("static/styles.css"),
    )
        .into_response()
}

async fn ice_config(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        state.ice_json_live(),
    )
        .into_response()
}

async fn net_config(State(state): State<AppState>) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        state.net_json.as_ref().clone(),
    )
        .into_response()
}

async fn reconfig(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let body = process_reconfig(&state, &peer.ip().to_string());
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn leave(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    process_leave(&state, &peer.ip().to_string());
    StatusCode::NO_CONTENT.into_response()
}

const MAX_AUDIO_OUTPUTS: usize = 32;
const MAX_AUDIO_OUTPUT_STR: usize = 256;

#[derive(Deserialize)]
struct AudioOutputEntry {
    #[serde(default)]
    id: String,
    #[serde(default)]
    label: String,
}

#[derive(Deserialize)]
struct AudioOutputsRequest {
    #[serde(default)]
    supported: bool,
    #[serde(default)]
    outputs: Vec<AudioOutputEntry>,
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

async fn audio_outputs(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    let req: AudioOutputsRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return StatusCode::NO_CONTENT.into_response(),
    };
    let ip = peer.ip().to_string();
    let outputs: Vec<session::AudioOutput> = req
        .outputs
        .into_iter()
        .take(MAX_AUDIO_OUTPUTS)
        .map(|o| session::AudioOutput {
            id: truncate_chars(&o.id, MAX_AUDIO_OUTPUT_STR),
            label: truncate_chars(&o.label, MAX_AUDIO_OUTPUT_STR),
        })
        .collect();

    if let Some(s) = state.config.sessions.as_ref() {
        session::set_audio_outputs(s, &ip, req.supported, outputs);
        if let Some(reporter) = state.config.device_reporter.as_ref() {
            let report = session::audio_outputs_report(s, &ip);
            reporter.report_audio_outputs(ip.clone(), report);
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

pub fn process_reconfig(state: &AppState, device_key: &str) -> String {
    let (epoch, kick) = state
        .config
        .sessions
        .as_ref()
        .map(|s| {
            (
                session::reconfig_epoch(s, device_key),
                session::kick_epoch(s, device_key),
            )
        })
        .unwrap_or((0, 0));
    let audio_sink = state
        .config
        .sessions
        .as_ref()
        .and_then(|s| session::selected_audio_output(s, device_key))
        .unwrap_or_default();
    serde_json::json!({ "epoch": epoch, "kick": kick, "audioSink": audio_sink }).to_string()
}

pub fn process_leave(state: &AppState, device_key: &str) {
    if let Some(s) = state.config.sessions.as_ref() {
        tprintln!("leave beacon from {device_key}; tearing down session");
        session::signal_leave(s, device_key);
    }
}

fn build_net_json(config: &Config) -> String {
    serde_json::json!({ "httpsPort": config.https_port }).to_string()
}

async fn whep(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_ip = peer.ip().to_string();
    if state.display_slot_unavailable(&client_ip) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "This Mac can only drive one extended display at a time. Disconnect the device that is using it, or upgrade to macOS 10.15 or later.",
        )
            .into_response();
    }
    let ice = state.ice_with_turn(state.fallback_ice_servers());
    let out = process_whep(&state, &client_ip, &body, ice).await;
    let status = StatusCode::from_u16(out.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(out.content_type),
    );
    if let Some(token) = out.device_token.as_deref() {
        if let Ok(value) = header::HeaderValue::from_str(token) {
            headers.insert("x-device-token", value);
        }
    }
    (status, headers, out.body).into_response()
}

pub async fn process_whep(
    state: &AppState,
    device_key: &str,
    body: &[u8],
    ice_servers: Vec<RTCIceServer>,
) -> ProcessedResponse {
    let req: JoinRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => {
            return ProcessedResponse::err(
                StatusCode::BAD_REQUEST,
                format!("invalid join request: {e}"),
            );
        }
    };

    if req.session_id.len() > MAX_SESSION_ID_LEN
        || req.otp.len() > MAX_OTP_LEN
        || req.os.len() > MAX_OS_LEN
        || req.sdp.len() > MAX_SDP_LEN
    {
        return ProcessedResponse::err(StatusCode::BAD_REQUEST, "join request field too large");
    }

    tprintln!(
        "join request: device={:?}, session={}, screen={}x{}, sdp_bytes={}",
        req.device_name,
        req.session_id,
        req.width,
        req.height,
        req.sdp.len()
    );

    let presented_token = req.device_token.trim().to_string();

    if state
        .config
        .banned_devices
        .as_ref()
        .is_some_and(|banned| session::is_device_banned(banned, &presented_token, device_key))
    {
        tprintln!("join rejected: {device_key} is banned by the host");
        return ProcessedResponse::err(
            StatusCode::FORBIDDEN,
            "this device has been banned by the host",
        );
    }

    let pre_approved = state
        .config
        .approved_devices
        .as_ref()
        .is_some_and(|approved| session::is_device_approved(approved, &presented_token));

    let device_token = if pre_approved {
        state.otp_limiter.record_success(device_key);
        tprintln!("join accepted without OTP: {device_key} presented a known device token");
        presented_token
    } else {
        if let Some(retry_after) = state.otp_limiter.global_paused() {
            let secs = retry_after.as_secs() + 1;
            return ProcessedResponse::err(
                StatusCode::TOO_MANY_REQUESTS,
                format!("join attempts temporarily paused; try again in {secs}s"),
            );
        }

        if let Some(retry_after) = state.otp_limiter.locked_for(device_key) {
            let secs = retry_after.as_secs() + 1;
            tprintln!("join rejected: {device_key} locked out, {secs}s remaining on OTP timeout");
            return ProcessedResponse::err(
                StatusCode::TOO_MANY_REQUESTS,
                format!("too many invalid OTP attempts; try again in {secs}s"),
            );
        }

        match state.config.session_auth.as_ref() {
            Some(auth) if auth.validate(&req.session_id, &req.otp) => {
                state.otp_limiter.record_success(device_key);
            }
            _ => {
                if let Some(pause) = state.otp_limiter.note_global_failure() {
                    let secs = pause.as_secs();
                    teprintln!(
                        "[security] brute-force guard tripped: {}+ failed OTP attempts across \
                         devices within {}s; pausing new joins for {secs}s",
                        session::MAX_GLOBAL_OTP_ATTEMPTS,
                        session::GLOBAL_OTP_WINDOW.as_secs()
                    );
                    if let Some(reporter) = state.config.device_reporter.as_ref() {
                        reporter.report_join_attempts_paused(secs);
                    }
                    state.otp_limiter.record_failure(device_key);
                    return ProcessedResponse::err(
                        StatusCode::TOO_MANY_REQUESTS,
                        format!("join attempts temporarily paused; try again in {secs}s"),
                    );
                }
                match state.otp_limiter.record_failure(device_key) {
                    OtpOutcome::LockedOut { retry_after } => {
                        let secs = retry_after.as_secs() + 1;
                        tprintln!(
                            "join rejected: invalid OTP from {device_key}; \
                         max attempts reached, locked out for {secs}s"
                        );
                        return ProcessedResponse::err(
                            StatusCode::TOO_MANY_REQUESTS,
                            format!("too many invalid OTP attempts; try again in {secs}s"),
                        );
                    }
                    OtpOutcome::Rejected { remaining } => {
                        tprintln!(
                            "join rejected: invalid session id or OTP from {device_key} \
                         ({remaining} attempt(s) left)"
                        );
                        return ProcessedResponse::err(
                            StatusCode::UNAUTHORIZED,
                            format!("invalid session id or OTP ({remaining} attempt(s) left)"),
                        );
                    }
                }
            }
        }

        let token = session::mint_device_token();
        tprintln!("issued a new device token to {device_key} after successful OTP");
        token
    };

    match start_session(state, &req, device_key, &device_token, ice_servers).await {
        Ok(answer) => {
            tprintln!(
                "join accepted: WHEP answer generated ({} bytes)",
                answer.len()
            );
            ProcessedResponse {
                status: StatusCode::OK.as_u16(),
                content_type: "application/sdp",
                body: answer,
                device_token: Some(device_token),
            }
        }
        Err(e) => {
            teprintln!("join failed: {e:?}");
            ProcessedResponse::err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("join failed: {e}"),
            )
        }
    }
}

async fn start_session(
    state: &AppState,
    req: &JoinRequest,
    client_ip: &str,
    device_token: &str,
    ice_servers: Vec<RTCIceServer>,
) -> Result<String> {
    let client = state
        .config
        .virtual_display
        .as_ref()
        .context("virtual-display driver unavailable (not running under Tauri)")?;

    let session_seq = state
        .config
        .sessions
        .as_ref()
        .map(|s| session::next_session_seq(s, client_ip))
        .unwrap_or(0);

    if let Some(s) = state.config.sessions.as_ref() {
        if let Some(stop) = session::take_active_capture(s, client_ip) {
            tprintln!("stopping previous capture for {client_ip} before starting a new session");
            let _ = tokio::task::spawn_blocking(stop).await;
        }
    }

    let detected_refresh = if req.refresh_rate == 0 {
        60
    } else {
        req.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
    };

    let mut cfg = state.config.as_ref().clone();
    cfg.max_fps = detected_refresh;
    let override_for_ip = state
        .config
        .device_overrides
        .as_ref()
        .and_then(|o| o.lock().unwrap().get(client_ip).copied());
    let control_enabled = override_for_ip.map(|o| o.control_enabled).unwrap_or(true);
    let audio_params = match (
        state.config.audio_hub.as_ref(),
        state.config.device_overrides.as_ref(),
    ) {
        (Some(hub), Some(overrides)) => Some(webrtc_session::AudioParams {
            fast: req.audio_capabilities.webcodecs_opus && req.audio_capabilities.worklet,
            hub: std::sync::Arc::clone(hub),
            overrides: std::sync::Arc::clone(overrides),
            device_key: client_ip.to_string(),
        }),
        _ => None,
    };
    if let Some(o) = override_for_ip {
        cfg.scale = ScalePercent::new(o.video_scale);
        cfg.qp = Some(o.video_quality);
        cfg.max_fps = o.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE);
    }

    let native_dpr = if req.dpr.is_finite() { req.dpr } else { 1.0 };
    let dpr = override_for_ip
        .map(|o| o.dpr)
        .unwrap_or(native_dpr)
        .clamp(1.0, platform::max_display_dpr());
    let backing = |css: u32| ((css as f64 * dpr).round() as u32).clamp(2, 16384) & !1;
    let width = backing(req.width);
    let height = backing(req.height);
    let refresh = if cfg.max_fps == 0 {
        60
    } else {
        cfg.max_fps.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
    };

    let display_name = if req.device_name.trim().is_empty() {
        "ScreenExtend".to_string()
    } else {
        format!("ScreenExtend - {}", req.device_name.trim())
    };

    let existing = state
        .config
        .sessions
        .as_ref()
        .and_then(|s| session::get_live_display(s, client_ip));
    let existed_before = existing.is_some();
    let client_portrait = height > width;
    let prev_client_portrait = existing.as_ref().map(|p| p.height > p.width);
    let device_rotated = prev_client_portrait == Some(!client_portrait);
    let honor_device = existing.is_none() || device_rotated;
    let portrait = if honor_device {
        client_portrait
    } else {
        override_for_ip
            .map(|o| o.orientation_portrait)
            .unwrap_or(client_portrait)
    };
    if honor_device {
        if let Some(overrides) = state.config.device_overrides.as_ref() {
            if let Some(o) = overrides.lock().unwrap().get_mut(client_ip) {
                o.orientation_portrait = client_portrait;
            }
        }
    }
    let swap_axes = portrait != client_portrait;
    let base_scale = override_for_ip
        .map(|o| o.scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE))
        .unwrap_or(100);
    let scale =
        ((dpr * base_scale as f64).round() as u32).clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);

    let desired = session::LiveDisplay {
        display_id: 0,
        device_name: String::new(),
        width,
        height,
        refresh,
        scale,
        portrait,
    };

    let (display_id, device_name) = match existing {
        Some(prev) => {
            let display_changed = prev.display_params() != desired.display_params();
            if display_changed {
                let name = prev.device_name.clone();
                let name2 = name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    pipeline::set_display_mode(&name2, width, height, refresh, swap_axes)
                })
                .await;
                if let Ok(Err(e)) = res {
                    teprintln!("could not apply display mode to {name}: {e}");
                }
                wait_for_display_settle(&name).await;
                apply_display_scale(&name, scale).await;
                tprintln!(
                    "virtual display id={} settings changed in place via Windows APIs ({width}x{height}@{refresh})",
                    prev.display_id
                );
            } else {
                tprintln!(
                    "virtual display id={} untouched (encoder-only edit)",
                    prev.display_id
                );
            }
            (prev.display_id, prev.device_name.clone())
        }
        None => {
            let extra_modes = dpr_mode_ladder(req.width, req.height, native_dpr);
            let (display_id, device_name) = {
                let _guard = DISPLAY_CORRELATION_LOCK.lock().await;

                let before = pipeline::monitor_device_names();

                let display_id = {
                    let client = client.clone();
                    tokio::task::spawn_blocking(move || {
                        client.create_display_with_modes(
                            display_name,
                            width,
                            height,
                            refresh,
                            &extra_modes,
                        )
                    })
                    .await
                    .context("create-display task")?
                    .map_err(|e| anyhow::anyhow!("creating virtual display: {e}"))?
                };
                tprintln!("virtual display created (id={display_id}, {width}x{height}@{refresh})");

                let named = {
                    let client = client.clone();
                    tokio::task::spawn_blocking(move || client.display_device_name(display_id))
                        .await
                        .ok()
                        .flatten()
                };

                match named {
                    Some(name) => {
                        tprintln!("virtual display id={display_id} attached as {name}");
                        (display_id, name)
                    }
                    None => match wait_for_new_monitor(&before).await {
                        Some(name) => {
                            tprintln!("virtual display id={display_id} attached as {name}");
                            (display_id, name)
                        }
                        None => {
                            remove_display_async(client, display_id).await;
                            bail!("virtual display {display_id} did not attach within timeout");
                        }
                    },
                }
            };

            {
                let name = device_name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    pipeline::set_display_mode(&name, width, height, refresh, swap_axes)
                })
                .await;
                match res {
                    Ok(Ok(())) => tprintln!(
                        "virtual display {device_name} set to {width}x{height}@{refresh} (portrait={portrait})"
                    ),
                    Ok(Err(e)) => teprintln!("could not force {device_name} to {width}x{height}: {e}"),
                    Err(e) => teprintln!("set-mode task for {device_name} panicked: {e}"),
                }
            }

            wait_for_display_settle(&device_name).await;
            apply_display_scale(&device_name, scale).await;
            (display_id, device_name)
        }
    };

    if let Some(s) = state.config.sessions.as_ref() {
        session::set_host_ip(s, client_ip, state.config.lan_ip.clone());
        session::set_live_display(
            s,
            client_ip,
            session::LiveDisplay {
                display_id,
                device_name: device_name.clone(),
                ..desired
            },
        );
    }

    let session = match pipeline::start_on_monitor(&cfg, &device_name) {
        Ok(s) => s,
        Err(e) => return Err(e.context("starting capture for virtual display")),
    };

    if let Some((left, top, width, height)) = pipeline::monitor_rect(&device_name) {
        tprintln!("remote-input display {device_name}: {width}x{height} at ({left},{top})");
    }

    let (closed_tx, closed_rx) = oneshot::channel();
    let answer = match webrtc_session::handle_whep_offer(
        req.sdp.clone(),
        &session.pipeline,
        ice_servers,
        Some(closed_tx),
        Some(device_name.clone()),
        control_enabled,
        audio_params,
    )
    .await
    {
        Ok(answer) => answer,
        Err(e) => {
            session.stop();
            if !existed_before {
                if let Some(s) = state.config.sessions.as_ref() {
                    let _ = session::take_live_display(s, client_ip);
                }
                remove_display_async(client, display_id).await;
            }
            return Err(e.context("WHEP handshake"));
        }
    };

    if let Some(reporter) = state.config.device_reporter.as_ref() {
        reporter.report_join(DeviceInfo {
            ip: client_ip.to_string(),
            token: device_token.to_string(),
            name: req.device_name.trim().to_string(),
            os: req.os.trim().to_string(),
            screen_size: format!("{}x{}", req.width, req.height),
            refresh_rate: detected_refresh,
            portrait,
            dpr: req.dpr,
        });
    }

    let leave = state
        .config
        .sessions
        .as_ref()
        .map(|s| session::arm_leave(s, client_ip));

    let session_holder = match state.config.sessions.as_ref() {
        Some(s) => {
            session::set_active_capture(
                s,
                client_ip,
                session_seq,
                Box::new(move || session.stop()),
            );
            None
        }
        None => Some(session),
    };

    let client = client.clone();
    let reporter = state.config.device_reporter.clone();
    let sessions = state.config.sessions.clone();
    let disconnect_grace = state.config.disconnect_grace.clone();
    let report_ip = client_ip.to_string();
    tokio::spawn(async move {
        let left = match &leave {
            Some(sig) => {
                tokio::select! {
                    _ = closed_rx => {
                        tokio::time::sleep(LEAVE_SETTLE).await;
                        sig.left.load(std::sync::atomic::Ordering::SeqCst)
                    }
                    _ = sig.notify.notified() => true,
                }
            }
            None => {
                let _ = closed_rx.await;
                false
            }
        };

        let stop = sessions
            .as_ref()
            .and_then(|s| session::take_active_capture_if(s, &report_ip, session_seq));
        if let Some(stop) = stop {
            let _ = tokio::task::spawn_blocking(stop).await;
        } else if let Some(session) = session_holder {
            session.stop();
        }

        if !left {
            let grace = std::time::Duration::from_secs(
                disconnect_grace
                    .as_ref()
                    .map(|g| g.load(std::sync::atomic::Ordering::Relaxed))
                    .unwrap_or(session::DEFAULT_DISCONNECT_GRACE_SECS),
            );
            tprintln!(
                "session for display id={display_id} ({device_name}) PC closed; \
                 waiting {grace:?} for a rejoin before removing the display"
            );
            tokio::time::sleep(grace).await;
        }

        let superseded = sessions
            .as_ref()
            .map(|s| !session::is_current_session(s, &report_ip, session_seq))
            .unwrap_or(false);
        if superseded {
            tprintln!(
                "session for display id={display_id} ({device_name}) superseded; keeping display"
            );
            return;
        }
        tprintln!(
            "session for display id={display_id} ({device_name}) ended ({}); removing display",
            if left {
                "page closed"
            } else {
                "disconnected, no rejoin"
            }
        );
        if let Some(s) = sessions.as_ref() {
            let _ = session::take_live_display(s, &report_ip);
        }
        remove_display_async(&client, display_id).await;
        if let Some(reporter) = reporter {
            reporter.report_remove(report_ip);
        }
    });

    Ok(answer)
}

async fn remove_display_async(client: &session::SharedVirtualDisplay, id: u32) {
    let client = client.clone();
    let _ = tokio::task::spawn_blocking(move || client.remove_display(id)).await;
}

fn dpr_mode_ladder(css_w: u32, css_h: u32, native_dpr: f64) -> Vec<(u32, u32)> {
    let cap = platform::max_display_dpr();
    let even = |v: f64| (v.round() as u32).clamp(2, 16384) & !1;
    let mut ratios: Vec<f64> = Vec::new();
    let mut r = 1.0_f64;
    while r <= cap + 1e-9 {
        ratios.push(r);
        r += 0.5;
    }
    if native_dpr.is_finite() {
        ratios.push(native_dpr.clamp(1.0, cap));
    }
    let mut modes: Vec<(u32, u32)> = ratios
        .iter()
        .map(|&r| (even(css_w as f64 * r), even(css_h as f64 * r)))
        .collect();
    modes.sort_unstable();
    modes.dedup();
    modes
}

async fn apply_display_scale(device_name: &str, percent: u32) {
    let name = device_name.to_string();
    let scale = percent.clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);
    let res = tokio::task::spawn_blocking(move || {
        if let Err(e) = pipeline::set_display_scale(&name, scale) {
            teprintln!("could not set scale for {name}: {e}");
        }
    })
    .await;
    if let Err(e) = res {
        teprintln!("apply-display-scale task for {device_name} panicked: {e}");
    }
}

async fn wait_for_display_settle(device_name: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut last: Option<(u32, u32)> = None;
    loop {
        let name = device_name.to_string();
        let dims = tokio::task::spawn_blocking(move || pipeline::monitor_dimensions(&name))
            .await
            .ok()
            .flatten();
        if dims.is_some() && dims == last {
            return;
        }
        last = dims;
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    }
}

async fn wait_for_new_monitor(before: &[String]) -> Option<String> {
    let deadline = tokio::time::Instant::now() + DISPLAY_ATTACH_TIMEOUT;
    loop {
        let _ = tokio::task::spawn_blocking(pipeline::set_display_topology_extend).await;

        let now = pipeline::monitor_device_names();
        if let Some(name) = now.iter().find(|n| !before.contains(n)) {
            return Some(name.clone());
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn build_ice_servers(config: &Config) -> Vec<RTCIceServer> {
    let mut servers = Vec::new();

    if !config.stun_urls.is_empty() {
        servers.push(RTCIceServer {
            urls: config.stun_urls.clone(),
            ..Default::default()
        });
    }

    match (
        &config.turn_url,
        &config.turn_username,
        &config.turn_credential,
    ) {
        (Some(url), Some(user), Some(cred)) => {
            servers.push(RTCIceServer {
                urls: vec![url.clone()],
                username: user.clone(),
                credential: cred.clone(),
            });
            teprintln!(
                "TURN relay configured ({url}) — MUST be local/regional to preserve latency"
            );
        }
        (Some(_), _, _) => {
            teprintln!("TURN_URL set but credentials missing, TURN disabled");
        }
        _ => {}
    }

    if servers.is_empty() {
        tprintln!("ICE servers: none configured -> host candidates only (same-network)");
    } else {
        for s in &servers {
            tprintln!(
                "ICE server configured (urls={:?}, has_creds={})",
                s.urls,
                !s.username.is_empty()
            );
        }
    }

    servers
}

fn log_urls(lan_ip: Option<&str>, http_port: u16, https_port: u16, self_signed: bool) {
    tprintln!("server listening — HTTP :{http_port}, HTTPS :{https_port}");
    match lan_ip {
        Some(ip) => {
            tprintln!("  LAN (open this first):  http://{ip}:{http_port}/");
            tprintln!("  LAN (secure / WebCodecs): https://{ip}:{https_port}/");
        }
        None => tprintln!("  LAN IP not set; use this machine's IP manually (or pass --lan-ip)"),
    }
    tprintln!(
        "  local:  http://localhost:{http_port}/   health: http://localhost:{http_port}/health"
    );
    if self_signed {
        tprintln!(
            "HTTPS uses a self-signed dev cert: browser shows a one-time warning, accept to proceed; \
             supply --tls-cert/--tls-key for a trusted cert"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct CappedDisplays(Option<usize>);

    impl session::VirtualDisplayController for CappedDisplays {
        fn create_display(&self, _: String, _: u32, _: u32, _: u32) -> Result<u32, String> {
            Err("not used in this test".into())
        }
        fn remove_display(&self, _: u32) {}
        fn remove_all_displays(&self) {}
        fn max_concurrent_displays(&self) -> Option<usize> {
            self.0
        }
    }

    fn state_with(cap: Option<usize>, holders: &[&str]) -> AppState {
        let sessions: session::SharedSessions = Default::default();
        for ip in holders {
            session::set_live_display(
                &sessions,
                ip,
                session::LiveDisplay {
                    display_id: 1,
                    device_name: "1".into(),
                    width: 1920,
                    height: 1080,
                    refresh: 60,
                    scale: 100,
                    portrait: false,
                },
            );
        }
        AppState::new(Config {
            virtual_display: Some(std::sync::Arc::new(CappedDisplays(cap))),
            sessions: Some(sessions),
            ..Default::default()
        })
    }

    #[test]
    fn an_uncapped_backend_never_turns_anyone_away() {
        let state = state_with(None, &["10.0.0.1", "10.0.0.2"]);
        assert!(!state.display_slot_unavailable("10.0.0.9"));
        assert_eq!(state.display_capacity(), (None, 2));
    }

    #[test]
    fn a_single_display_backend_turns_away_a_second_device() {
        let state = state_with(Some(1), &["10.0.0.1"]);
        assert!(state.display_slot_unavailable("10.0.0.2"));
        assert_eq!(state.display_capacity(), (Some(1), 1));
    }

    #[test]
    fn the_device_already_holding_the_display_is_never_blocked() {
        let state = state_with(Some(1), &["10.0.0.1"]);
        assert!(!state.display_slot_unavailable("10.0.0.1"));
    }

    #[test]
    fn the_slot_frees_up_when_the_holder_leaves() {
        let state = state_with(Some(1), &[]);
        assert!(!state.display_slot_unavailable("10.0.0.2"));
        assert_eq!(state.display_capacity(), (Some(1), 0));
    }

    #[test]
    fn the_holders_adapter_is_identifiable_for_keeping_its_server_up() {
        let sessions: session::SharedSessions = Default::default();
        session::set_host_ip(&sessions, "10.0.0.1", Some("192.168.1.5".into()));
        session::set_live_display(
            &sessions,
            "10.0.0.1",
            session::LiveDisplay {
                display_id: 1,
                device_name: "1".into(),
                width: 1920,
                height: 1080,
                refresh: 60,
                scale: 100,
                portrait: false,
            },
        );
        session::set_host_ip(&sessions, "10.0.0.2", Some("192.168.1.9".into()));

        assert_eq!(
            session::display_holder_host_ip(&sessions),
            Some("192.168.1.5".into())
        );
    }

    #[test]
    fn no_adapter_is_singled_out_when_nobody_holds_a_display() {
        let sessions: session::SharedSessions = Default::default();
        session::set_host_ip(&sessions, "10.0.0.1", Some("192.168.1.5".into()));
        assert_eq!(session::display_holder_host_ip(&sessions), None);
    }

    #[test]
    fn device_name_is_sanitized() {
        assert_eq!(sanitize_device_name("Nina's iPad"), "Nina's iPad");
        assert_eq!(sanitize_device_name("  spaced  "), "spaced");
        assert_eq!(
            sanitize_device_name("evil\r\nINFO: fake log"),
            "evilINFO: fake log"
        );
        assert_eq!(sanitize_device_name("null\0byte"), "nullbyte");
        assert_eq!(sanitize_device_name("a\u{202E}b\u{200B}c"), "abc");
        let long = "x".repeat(200);
        assert_eq!(
            sanitize_device_name(&long).chars().count(),
            MAX_DEVICE_NAME_CHARS
        );
    }
}
