use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};

use serde::Deserialize;
use tokio::sync::oneshot;

use super::config::{Config, ScalePercent};
use super::pipeline;
use super::session::{self, DeviceInfo, DeviceOverride, OtpLimiter, OtpOutcome, SharedOtpLimiter};
use super::webrtc_session::{self, RTCIceServer};

#[derive(Deserialize)]
struct JoinRequest {
    #[serde(rename = "sessionId")]
    session_id: String,
    otp: String,
    #[serde(default, rename = "deviceName")]
    device_name: String,
    #[serde(default)]
    os: String,
    #[serde(default, rename = "refreshRate")]
    refresh_rate: u32,
    width: u32,
    height: u32,
    sdp: String,
}

const DISPLAY_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const LEAVE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);
static DISPLAY_CORRELATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub const MIN_REFRESH_RATE: u32 = 15;
pub const MAX_REFRESH_RATE: u32 = 500;
pub const MIN_DISPLAY_SCALE: u32 = 25;
pub const MAX_DISPLAY_SCALE: u32 = 200;

#[derive(Clone)]
pub struct AppState {
    config: Arc<Config>,
    ice_servers: Arc<Vec<RTCIceServer>>,
    net_json: Arc<String>,
    otp_limiter: SharedOtpLimiter,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        Self {
            ice_servers: Arc::new(build_ice_servers(&config)),
            net_json: Arc::new(build_net_json(&config)),
            otp_limiter: Arc::new(OtpLimiter::new()),
            config: Arc::new(config),
        }
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
        ..Default::default()
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
            ..Default::default()
        }),
        Err(e) => {
            teprintln!("[turn] failed to mint ephemeral credentials: {e}");
            None
        }
    }
}

pub struct ProcessedResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: String,
}

pub async fn run(config: Config, handle: Option<axum_server::Handle>) -> Result<()> {
    let handle = handle.unwrap_or_else(axum_server::Handle::new);

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

    log_urls(config.lan_ip.as_deref(), config.port, config.https_port, self_signed);

    use axum_server::accept::NoDelayAcceptor;
    let http = axum_server::bind(http_addr)
        .acceptor(NoDelayAcceptor)
        .handle(handle.clone())
        .serve(app.clone().into_make_service_with_connect_info::<SocketAddr>());
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
        .route("/logo.svg", get(logo))
        .route("/styles.css", get(styles))
        .route("/ice-config", get(ice_config))
        .route("/net-config", get(net_config))
        .route("/reconfig", get(reconfig))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn index() -> Html<&'static str> {
    Html(include_str!("static/index.html"))
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
    (
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

async fn leave(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    process_leave(&state, &peer.ip().to_string());
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
    serde_json::json!({ "epoch": epoch, "kick": kick }).to_string()
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
    let ice = state.ice_with_turn(state.fallback_ice_servers());
    let out = process_whep(&state, &peer.ip().to_string(), &body, ice).await;
    (
        StatusCode::from_u16(out.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        [(header::CONTENT_TYPE, out.content_type)],
        out.body,
    )
        .into_response()
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
            return ProcessedResponse {
                status: StatusCode::BAD_REQUEST.as_u16(),
                content_type: "text/plain",
                body: format!("invalid join request: {e}"),
            };
        }
    };

    tprintln!(
        "join request: device={:?}, session={}, screen={}x{}, sdp_bytes={}",
        req.device_name,
        req.session_id,
        req.width,
        req.height,
        req.sdp.len()
    );

    // Refuse outright if this device is still serving an OTP lockout.
    if let Some(retry_after) = state.otp_limiter.locked_for(device_key) {
        let secs = retry_after.as_secs() + 1;
        tprintln!("join rejected: {device_key} locked out, {secs}s remaining on OTP timeout");
        return ProcessedResponse {
            status: StatusCode::TOO_MANY_REQUESTS.as_u16(),
            content_type: "text/plain",
            body: format!("too many invalid OTP attempts; try again in {secs}s"),
        };
    }

    match state.config.session_auth.as_ref() {
        Some(auth) if auth.validate(&req.session_id, &req.otp) => {
            state.otp_limiter.record_success(device_key);
        }
        _ => match state.otp_limiter.record_failure(device_key) {
            OtpOutcome::LockedOut { retry_after } => {
                let secs = retry_after.as_secs() + 1;
                tprintln!(
                    "join rejected: invalid OTP from {device_key}; \
                     max attempts reached, locked out for {secs}s"
                );
                return ProcessedResponse {
                    status: StatusCode::TOO_MANY_REQUESTS.as_u16(),
                    content_type: "text/plain",
                    body: format!("too many invalid OTP attempts; try again in {secs}s"),
                };
            }
            OtpOutcome::Rejected { remaining } => {
                tprintln!(
                    "join rejected: invalid session id or OTP from {device_key} \
                     ({remaining} attempt(s) left)"
                );
                return ProcessedResponse {
                    status: StatusCode::UNAUTHORIZED.as_u16(),
                    content_type: "text/plain",
                    body: format!(
                        "invalid session id or OTP ({remaining} attempt(s) left)"
                    ),
                };
            }
        },
    }

    match start_session(state, &req, device_key, ice_servers).await {
        Ok(answer) => {
            tprintln!("join accepted: WHEP answer generated ({} bytes)", answer.len());
            ProcessedResponse {
                status: StatusCode::OK.as_u16(),
                content_type: "application/sdp",
                body: answer,
            }
        }
        Err(e) => {
            teprintln!("join failed: {e:?}");
            ProcessedResponse {
                status: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                content_type: "text/plain",
                body: format!("join failed: {e}"),
            }
        }
    }
}

async fn start_session(
    state: &AppState,
    req: &JoinRequest,
    client_ip: &str,
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
    if let Some(o) = override_for_ip {
        cfg.scale = ScalePercent::new(o.video_scale);
        cfg.qp = Some(o.video_quality);
        cfg.max_fps = o.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE);
    }

    let width = req.width.clamp(2, 16384) & !1;
    let height = req.height.clamp(2, 16384) & !1;
    let refresh = if cfg.max_fps == 0 { 60 } else { cfg.max_fps.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE) };

    let display_name = if req.device_name.trim().is_empty() {
        "ScreenExtend".to_string()
    } else {
        format!("ScreenExtend - {}", req.device_name.trim())
    };

    let portrait = false;
    let scale = override_for_ip
        .map(|o| o.scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE))
        .unwrap_or(100);

    let desired = session::LiveDisplay {
        display_id: 0,
        device_name: String::new(),
        width,
        height,
        refresh,
        scale,
        portrait,
    };

    let existing = state
        .config
        .sessions
        .as_ref()
        .and_then(|s| session::get_live_display(s, client_ip));
    let existed_before = existing.is_some();

    let (display_id, device_name) = match existing {
        Some(prev) => {
            let display_changed = prev.display_params() != desired.display_params();
            if display_changed {
                let name = prev.device_name.clone();
                let name2 = name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    pipeline::set_display_mode(&name2, width, height, refresh, portrait)
                })
                .await;
                if let Ok(Err(e)) = res {
                    teprintln!("could not apply display mode to {name}: {e}");
                }
                apply_display_scale(&name, override_for_ip).await;
                wait_for_display_settle(&name).await;
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
            let (display_id, device_name) = {
                let _guard = DISPLAY_CORRELATION_LOCK.lock().await;

                let before = pipeline::monitor_device_names();

                let display_id = {
                    let client = client.clone();
                    tokio::task::spawn_blocking(move || {
                        client.create_display(display_name, width, height, refresh)
                    })
                    .await
                    .context("create-display task")?
                    .map_err(|e| anyhow::anyhow!("creating virtual display: {e}"))?
                };
                tprintln!("virtual display created (id={display_id}, {width}x{height}@{refresh})");

                match wait_for_new_monitor(&before).await {
                    Some(name) => {
                        tprintln!("virtual display id={display_id} attached as {name}");
                        (display_id, name)
                    }
                    None => {
                        remove_display_async(client, display_id).await;
                        bail!("virtual display {display_id} did not attach within timeout");
                    }
                }
            };

            {
                let name = device_name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    pipeline::set_display_mode(&name, width, height, refresh, portrait)
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

            apply_display_scale(&device_name, override_for_ip).await;
            (display_id, device_name)
        }
    };

    if let Some(s) = state.config.sessions.as_ref() {
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
        tprintln!(
            "remote-input display {device_name}: {width}x{height} at ({left},{top})"
        );
    }

    let (closed_tx, closed_rx) = oneshot::channel();
    let answer = match webrtc_session::handle_whep_offer(
        req.sdp.clone(),
        &session.pipeline,
        ice_servers,
        Some(closed_tx),
        Some(device_name.clone()),
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
            name: req.device_name.trim().to_string(),
            os: req.os.trim().to_string(),
            screen_size: format!("{}x{}", req.width, req.height),
            refresh_rate: detected_refresh,
        });
    }

    let leave = state
        .config
        .sessions
        .as_ref()
        .map(|s| session::arm_leave(s, client_ip));

    let session_holder = match state.config.sessions.as_ref() {
        Some(s) => {
            session::set_active_capture(s, client_ip, session_seq, Box::new(move || session.stop()));
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
            tprintln!("session for display id={display_id} ({device_name}) superseded; keeping display");
            return;
        }
        tprintln!(
            "session for display id={display_id} ({device_name}) ended ({}); removing display",
            if left { "page closed" } else { "disconnected, no rejoin" }
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

async fn apply_display_scale(device_name: &str, over: Option<DeviceOverride>) {
    let Some(o) = over else { return };
    let name = device_name.to_string();
    let scale = o.scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE);
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

    match (&config.turn_url, &config.turn_username, &config.turn_credential) {
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
