use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::{
    body::Bytes,
    extract::Path,
    extract::{ConnectInfo, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{delete, get, patch, post},
    Router,
};

use serde::Deserialize;
use tokio::sync::oneshot;

use super::config::{Config, ScalePercent};
use super::lock::LockExt;
use super::negotiate;
use super::pipeline;
use super::platform;
use super::session::{self, DeviceInfo, OtpLimiter, OtpOutcome, SharedLocalIps, SharedOtpLimiter};
use super::webrtc_session::{self, RTCIceServer, SessionLocality};

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
    worklet: bool,
    /// The client actually probed its decoder and built its worklet, rather than only checking
    /// that the constructors exist. Absent means "unknown", which is what every client that
    /// predates the probe sends.
    #[serde(default)]
    verified: bool,
    /// The client knows how to switch to a standard Opus track if the WebCodecs path dies
    /// mid-session, and wants one in the answer as a safety net.
    ///
    /// It has to ask rather than always be given one: a client that attaches every inbound
    /// track on `ontrack` would report itself as being on the fallback path and play a silent
    /// element alongside its worklet. The knowledge of whether a spare track is useful lives on
    /// the client, so the request does too.
    #[serde(default, rename = "wantsFallbackTrack")]
    wants_fallback_track: bool,
    /// The client can parse a `FLAG_REDUNDANT` message (previous frame's Opus payload
    /// appended). Absent in every client that predates the flag, and the host must not send
    /// what the client cannot read, so `false` is the only safe default.
    #[serde(default)]
    redundancy: bool,
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
const TOPOLOGY_NUDGE_AFTER: std::time::Duration = std::time::Duration::from_millis(600);
const LEAVE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1500);
static DISPLAY_CORRELATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub const MIN_REFRESH_RATE: u32 = 15;
pub const MAX_REFRESH_RATE: u32 = 500;

/// Compose cadence to ask the virtual display for, independent of the stream's frame rate.
/// Every created display registers this rate alongside whatever was requested
/// (`COMMON_REFRESH_RATES`), so selecting it costs nothing; if the driver refuses it the join
/// carries on at whatever it did land on.
pub const DISPLAY_COMPOSE_HZ: u32 = 120;
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
        ip.is_loopback() || self.local_ips.lock_recover().contains(&ip)
    }

    /// Classify a peer *before* the handshake, so the session's MTU, gather wait, pacing and
    /// playout-delay hint are right for the first packet rather than 500 ms after connection.
    ///
    /// `detect_session_locality` still runs afterwards and warns when this guess was wrong; it
    /// reads the nominated ICE candidate pair, which cannot exist yet at join time.
    pub fn locality_for(&self, client_ip: &str) -> SessionLocality {
        let Ok(peer) = client_ip.parse::<IpAddr>() else {
            return SessionLocality::Unknown;
        };
        let peer = normalize_peer_ip(peer);
        if peer.is_loopback() {
            return SessionLocality::SameNetwork;
        }
        let locals = self.local_ips.lock_recover().clone();
        if locals.iter().any(|l| same_subnet(l, &peer)) {
            return SessionLocality::SameNetwork;
        }
        if is_private(&peer) {
            // A private address that does not share a subnet with any of ours is still on the
            // local side of any NAT, so it is not a relay case — but it is a router hop away and
            // does not get the LAN-only shortcuts.
            return SessionLocality::Unknown;
        }
        SessionLocality::CrossNetwork
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

    /// The ICE list for a session already known to be on this subnet, or for one that is not.
    ///
    /// TURN is always offered when configured: it is what a relayed path needs, and a
    /// misclassified peer must still be able to connect. Only STUN is dropped, because a
    /// reflexive candidate is exactly the thing a same-subnet pair has no use for.
    pub fn ice_for(&self, locality: SessionLocality) -> Vec<RTCIceServer> {
        let base = if locality == SessionLocality::SameNetwork {
            Vec::new()
        } else {
            self.ice_servers.as_ref().clone()
        };
        self.ice_with_turn(base)
    }

    pub fn ice_json_for(&self, locality: SessionLocality) -> String {
        if locality == SessionLocality::SameNetwork {
            let mut servers: Vec<serde_json::Value> = Vec::new();
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
            return serde_json::json!({
                "iceServers": servers,
                "locality": "same-network",
            })
            .to_string();
        }
        // `locality` answers the one question the client cannot answer for itself: whether this
        // session will actually be relayed. It has to gather before it POSTs — WHEP has no PATCH
        // to trickle candidates in later — and *which* candidate it may stop gathering on depends
        // entirely on that. Inferring it from the shape of the server list ("a TURN entry is
        // present, so a relay is needed") is wrong in exactly the case above, where a same-subnet
        // peer is handed TURN as a backup and will be nominated on a host candidate that arrived
        // in the first millisecond.
        let mut v: serde_json::Value = serde_json::from_str(&self.ice_json_live())
            .unwrap_or_else(|_| serde_json::json!({ "iceServers": [] }));
        if let Some(obj) = v.as_object_mut() {
            let name = match locality {
                SessionLocality::SameNetwork => "same-network",
                SessionLocality::CrossNetwork => "cross-network",
                SessionLocality::Unknown => "unknown",
            };
            obj.insert(
                "locality".into(),
                serde_json::Value::String(name.to_owned()),
            );
        }
        v.to_string()
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
    let cfg = shared.lock_recover();
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

fn is_private(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// Same /24 for IPv4, same /64 for IPv6. Coarse on purpose: it is a hint for choosing transport
/// constants, and the ICE-pair detector corrects it once the session is up.
fn same_subnet(a: &IpAddr, b: &IpAddr) -> bool {
    match (a, b) {
        (IpAddr::V4(x), IpAddr::V4(y)) => x.octets()[..3] == y.octets()[..3],
        (IpAddr::V6(x), IpAddr::V6(y)) => x.segments()[..4] == y.segments()[..4],
        _ => false,
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
    /// The WHEP resource this join created, as RFC 9725 requires: the client PATCHes ICE
    /// candidates to it instead of holding the offer back until its own gathering is finished.
    /// `None` on every error path, and on the cloud relay, which has no URL space of its own.
    pub resource: Option<String>,
}

impl ProcessedResponse {
    fn err(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status: status.as_u16(),
            content_type: "text/plain",
            body: body.into(),
            device_token: None,
            resource: None,
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
        // The WHEP resource, RFC 9725 §4.6. A client that ignores the `Location` header behaves
        // exactly as it did before, which is what makes this safe to ship ahead of the client.
        .route("/whep/{id}", patch(whep_patch))
        .route("/whep/{id}", delete(whep_delete))
        .route("/leave", post(leave))
        .route("/transform-worker.js", get(transform_worker))
        .route("/input.js", get(input_js))
        .route("/audio.js", get(audio_js))
        .route("/audio-worklet.js", get(audio_worklet_js))
        .route("/audio-worker.js", get(audio_worker_js))
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

// Cross-origin isolation (`docs/recovered/PRD.md` §6.4): with these on the document *and* a
// secure context (HTTPS or localhost), `crossOriginIsolated` becomes true and the client can use
// the SharedArrayBuffer ring buffer. Over plain HTTP they're inert (isolation stays off; client
// falls back to the postMessage ring). All our sub-resources are same-origin, so
// COEP:require-corp does not block them — verify the existing video worker still loads if this
// ever regresses.
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

/// The client page with `__SAME_DEVICE_FLAG__` already substituted, one variant each.
///
/// `String::replace` on the largest asset in the tree, on every page load, is a full scan plus a
/// fresh allocation for a substitution that has exactly two possible answers. Every other static
/// route already serves a `&'static str` with no copy; this one now does too. The cost was never
/// large — the page is fetched once per join — but the join path is what this work measures, and a
/// client stuck in a reload loop paid it repeatedly.
fn same_device_page(same_device: bool) -> &'static str {
    static SAME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    static REMOTE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let cell = if same_device { &SAME } else { &REMOTE };
    cell.get_or_init(|| {
        include_str!("static/index.html").replace(
            "__SAME_DEVICE_FLAG__",
            if same_device { "true" } else { "false" },
        )
    })
}

async fn index(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    if state.display_slot_unavailable(&peer.ip().to_string()) {
        return at_capacity_page();
    }
    let html = same_device_page(state.is_same_device(peer.ip()));
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

// The static assets are `include_str!`'d, so each one's ETag is a hash of its own bytes,
// folded at compile time. The version string was a proxy for the content and a wrong one:
// the client page has no build step and is edited by hand, so any edit that did not also bump
// the crate version served a 304 against the stale copy every browser had already cached —
// a trap aimed squarely at the workflow that touches these files. `no-cache` keeps the client
// revalidating rather than pinning a stale asset across an update, and the revalidation costs
// a 304 instead of the ~120 KB the page used to refetch on every reload. `index` is
// deliberately left uncached: it carries a per-request substitution.
fn asset(
    content_type: &'static str,
    etag: &str,
    headers: &HeaderMap,
    body: &'static str,
) -> Response {
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, "no-cache")],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::ETAG, etag),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

// FNV-1a, `const` so every call site folds to a literal and no request pays for the hash.
const fn asset_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

fn asset_etag(hash: u64) -> String {
    format!("\"{:016x}\"", hash)
}

async fn transform_worker(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/transform-worker.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/javascript", &asset_etag(TAG), &headers, BODY)
}

async fn input_js(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/input.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/javascript", &asset_etag(TAG), &headers, BODY)
}

async fn audio_js(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/audio.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/javascript", &asset_etag(TAG), &headers, BODY)
}

async fn audio_worklet_js(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/audio-worklet.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/javascript", &asset_etag(TAG), &headers, BODY)
}

async fn audio_worker_js(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/audio-worker.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/javascript", &asset_etag(TAG), &headers, BODY)
}

async fn nosleep_js(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/nosleep.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/javascript", &asset_etag(TAG), &headers, BODY)
}

async fn logo(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/logo.svg");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("image/svg+xml", &asset_etag(TAG), &headers, BODY)
}

async fn styles(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/styles.css");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    asset("text/css", &asset_etag(TAG), &headers, BODY)
}

async fn ice_config(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    // A same-subnet peer needs no reflexive candidate, and handing it a STUN server makes both
    // ends block on a round trip to the public internet before the join can even start: the
    // client waits up to 3 s for its own gathering, the host up to 1.5 s for the answer's. An
    // offline or hosted-network host pays both timeouts in full, for a candidate nobody uses.
    let locality = state.locality_for(&peer.ip().to_string());
    (
        [(header::CONTENT_TYPE, "application/json")],
        state.ice_json_for(locality),
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
    // `trickleIce` is how the client learns it may POST the offer before its own gathering has
    // finished and PATCH the candidates afterwards. Probed, never assumed: a client that does not
    // read it, or a host that does not send it, keeps the gather-then-post shape exactly.
    serde_json::json!({ "httpsPort": config.https_port, "trickleIce": true }).to_string()
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
            "This Mac creates its extended display over AirPlay and can only drive one at a time. The device already connected keeps it — disconnect that device to free the slot, or use macOS 10.15 or later, where every device gets its own display.",
        )
            .into_response();
    }
    let ice = state.ice_for(state.locality_for(&client_ip));
    let out = process_whep(&state, &client_ip, &body, ice).await;
    let mut status = StatusCode::from_u16(out.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
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
    // RFC 9725 §4.1: a WHEP resource is created, so the answer that carries its `Location` is a
    // `201`, not a `200`. Only here — the cloud tunnel has no URL space and sets no `Location`,
    // and a `201` without one would be a lie. Our own client reads `resp.ok`, which covers both.
    if let Some(resource) = out.resource.as_deref() {
        if let Ok(value) = header::HeaderValue::from_str(&format!("/whep/{resource}")) {
            headers.insert(header::LOCATION, value);
            if status == StatusCode::OK {
                status = StatusCode::CREATED;
            }
        }
    }
    (status, headers, out.body).into_response()
}

/// Trickled ICE from the client, RFC 9725 §4.6.
///
/// The point is what it removes from the join: without it the client cannot POST until its own
/// gathering has finished or timed out, so the offer it sends is the only one the host will ever
/// see. With TURN configured that wait runs until a relay candidate arrives; on a network where no
/// candidate arrives at all it burns the full deadline.
///
/// The resource id is a 128-bit capability, but it is handed out in a `Location` header that
/// travels in cleartext on the HTTP listener — the listener most LAN joins actually use. So the
/// id alone is not the authority: the peer that opened the session is checked too, and the
/// fragment count is bounded, because adding remote candidates is a media-redirection primitive.
async fn whep_patch(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Ok(text) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if text.len() > MAX_SDP_LEN {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let device_key = peer.ip().to_string();
    match webrtc_session::add_trickled_candidates(&id, &device_key, text).await {
        webrtc_session::TrickleLookup::Found => StatusCode::NO_CONTENT.into_response(),
        // Not Found, not Gone: the same answer for "no such id" and "not yours", so a probe
        // cannot tell them apart. The client stops trickling on exactly `404`
        // (`static/index.html`, `startTrickle`), which is what makes this the right status to
        // keep — a session it can no longer reach is one there is nothing to retry into.
        webrtc_session::TrickleLookup::Unknown => StatusCode::NOT_FOUND.into_response(),
        webrtc_session::TrickleLookup::Exhausted => StatusCode::TOO_MANY_REQUESTS.into_response(),
    }
}

/// Tear the resource down, RFC 9725 §4.4. The client already has `/leave`; this exists so the
/// resource the `Location` header hands out is a real one.
///
/// The id is resolved through the same registry `whep_patch` uses rather than trusting the
/// caller's address: the session is keyed by device, so without the lookup `DELETE /whep/<any
/// string at all>` would tear down whatever session the caller happened to own.
async fn whep_delete(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
) -> Response {
    let device_key = peer.ip().to_string();
    match webrtc_session::trickle_owner(&id, &device_key) {
        Some(key) => {
            process_leave(&state, &key);
            StatusCode::NO_CONTENT.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
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
        Ok((answer, resource)) => {
            tprintln!(
                "join accepted: WHEP answer generated ({} bytes)",
                answer.len()
            );
            ProcessedResponse {
                status: StatusCode::OK.as_u16(),
                content_type: "application/sdp",
                body: answer,
                device_token: Some(device_token),
                resource: Some(resource),
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
) -> Result<(String, String)> {
    let locality = state.locality_for(client_ip);
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
    // Rolling intra refresh. It has been plumbed through the NVENC, Quick Sync and VideoToolbox
    // backends since they were written and no shipping path ever set it, so every recoverable
    // loss cost a full IDR — the single largest frame the encoder emits, sent unpaced, on a link
    // that had just demonstrated it could not carry the previous burst. With a refresh wave the
    // decoder rebuilds over a few frames instead, and a client that missed the session's opening
    // keyframe recovers on its own rather than sitting black behind an infinite GOP.
    cfg.intra_refresh = true;
    let override_for_ip = state
        .config
        .device_overrides
        .as_ref()
        .and_then(|o| o.lock_recover().get(client_ip).copied());
    let control_enabled = override_for_ip.map(|o| o.control_enabled).unwrap_or(true);
    let audio_params = match (
        state.config.audio_hub.as_ref(),
        state.config.device_overrides.as_ref(),
    ) {
        (Some(hub), Some(overrides)) => {
            let caps = req.audio_capabilities;
            let fast = caps.webcodecs_opus && caps.worklet;
            tprintln!(
                "audio capabilities: webcodecsOpus={} worklet={} verified={} wantsFallbackTrack={} redundancy={} -> {}",
                caps.webcodecs_opus,
                caps.worklet,
                caps.verified,
                caps.wants_fallback_track,
                caps.redundancy,
                if fast { "DataChannel" } else { "Opus track" }
            );
            Some(webrtc_session::AudioParams {
                fast,
                fallback_track: caps.wants_fallback_track,
                redundancy: caps.redundancy,
                link_loss: None, // wired in by `start_session`, which owns the BWE
                locality,
                hub: std::sync::Arc::clone(hub),
                overrides: std::sync::Arc::clone(overrides),
                device_key: client_ip.to_string(),
            })
        }
        _ => None,
    };
    if let Some(o) = override_for_ip {
        cfg.scale = ScalePercent::new(o.video_scale);
        cfg.quality_qp = Some(o.video_quality);
        cfg.max_fps = o.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE);
    }

    // What the client's offer says it can decode, read before the encoder is built because the
    // profile is an encoder-session parameter and cannot be changed afterwards. Baseline was the
    // hardcoded answer to every client until now — CAVLC and no 8x8 transform, 5-15 % of the
    // bitrate given away on every session that could have taken High.
    let offered_video = negotiate::read_offer(&req.sdp);

    let native_dpr = if req.dpr.is_finite() { req.dpr } else { 1.0 };
    let dpr = platform::snap_display_dpr(override_for_ip.map(|o| o.dpr).unwrap_or(native_dpr));
    let backing = |css: u32| ((css as f64 * dpr).round() as u32).clamp(2, 16384) & !1;
    let width = backing(req.width);
    let height = backing(req.height);
    let stream_fps = if cfg.max_fps == 0 {
        60
    } else {
        cfg.max_fps.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
    };
    // The virtual display's refresh rate and the stream's frame rate were one number. They are
    // not the same thing: an indirect display has no hardware vblank, so the mode's refresh rate
    // *is* DWM's compose cadence and therefore the ceiling on how fresh any captured frame can
    // be. At 60 Hz a frame is on average 8.3 ms stale before capture even sees it; at 120 Hz,
    // 4.2 ms. Every display is already created with 120 Hz registered alongside its requested
    // rate, and it was simply never selected. `cfg.max_fps` stays the stream cap.
    let refresh = stream_fps
        .max(DISPLAY_COMPOSE_HZ)
        .clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE);

    let video = negotiate::choose(&offered_video, width, height, stream_fps);
    cfg.h264_profile = video.profile;
    tprintln!(
        "H.264 negotiated: profile={:?} level=0x{:02x} (offered={:?}, level-asymmetry={}, hevc={})",
        video.profile,
        video.level_idc,
        offered_video.h264_profiles,
        offered_video.level_asymmetry_allowed,
        offered_video.hevc
    );

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
            if let Some(o) = overrides.lock_recover().get_mut(client_ip) {
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

    // The join used to be a strictly serial chain: create and attach the display, apply the mode,
    // settle, scale, start the pipeline, and only then negotiate. Nothing in the handshake needs
    // the pipeline until the RTP writer is spawned — the peer connection, the tracks,
    // `set_remote_description`, `create_answer` and ICE gathering all depend only on the offer, the
    // negotiated codec and the ICE server list — so the two run as concurrent futures and the
    // handshake term comes out of time-to-first-frame entirely. On a LAN that is 13-20 ms of a
    // ~1600 ms join; on a relayed path, where the host gather runs to its 1500 ms cap against a
    // 1500-2100 ms display term, it is most of the gather.
    let t_display = std::time::Instant::now();
    let display_and_pipeline = async {
        let mut mode_applied = true;
        let (display_id, device_name) = match existing.clone() {
            Some(prev) => {
                let display_changed = prev.display_params() != desired.display_params();
                if display_changed {
                    let name = prev.device_name.clone();
                    let name2 = name.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        pipeline::set_display_mode(&name2, width, height, refresh, swap_axes)
                    })
                    .await;
                    match res {
                        Ok(Ok(())) => {}
                        Ok(Err(e)) => {
                            mode_applied = false;
                            teprintln!("could not apply display mode to {name}: {e}");
                        }
                        Err(e) => {
                            mode_applied = false;
                            teprintln!("set-mode task for {name} panicked: {e}");
                        }
                    }
                    wait_for_display_settle(&name, Some(monitor_dims(width, height, swap_axes)))
                        .await;
                    apply_display_scale(&name, scale).await;
                    tprintln!(
                    "virtual display id={} settings changed in place via Windows APIs ({width}x{height}@{refresh}, scale={scale}%)",
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
                let extra_modes = dpr_mode_ladder(req.width, req.height);
                let (display_id, device_name, topology_reset) = {
                    // Held only for as long as the before/after device-name diff needs exclusivity.
                    // A second device joining seconds after the first used to wait out the first's
                    // whole create-and-attach window — up to `DISPLAY_ATTACH_TIMEOUT` — before its own
                    // could start, so two simultaneous joins cost t1 + t2 instead of max(t1, t2).
                    let t_lock = std::time::Instant::now();
                    let guard = DISPLAY_CORRELATION_LOCK.lock().await;
                    let lock_wait_ms = t_lock.elapsed().as_secs_f64() * 1000.0;
                    if lock_wait_ms > 1.0 {
                        tprintln!(
                        "display correlation lock: waited {lock_wait_ms:.0}ms behind another join"
                    );
                    }

                    let before = tokio::task::spawn_blocking(pipeline::monitor_device_names)
                        .await
                        .unwrap_or_default();

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
                    tprintln!(
                        "virtual display created (id={display_id}, {width}x{height}@{refresh})"
                    );

                    // A backend that can name its own display needs no correlation at all: the
                    // snapshot above is never read and the lock can go immediately, so the next join's
                    // create starts while this one is still waiting for Windows to attach the monitor.
                    let named = {
                        let client = client.clone();
                        tokio::task::spawn_blocking(move || client.display_device_name(display_id))
                            .await
                            .ok()
                            .flatten()
                    };
                    if named.is_some() {
                        drop(guard);
                    }

                    let (attached, topology_reset) = match named {
                        Some(name) => {
                            tprintln!(
                            "virtual display id={display_id} named directly by the backend (no correlation needed)"
                        );
                            (Some(name), false)
                        }
                        None => wait_for_new_monitor(&before).await,
                    };
                    match attached {
                        Some(name) => {
                            tprintln!("virtual display id={display_id} attached as {name}");
                            (display_id, name, topology_reset)
                        }
                        None => {
                            remove_display_async(client, display_id).await;
                            bail!("virtual display {display_id} did not attach within timeout");
                        }
                    }
                };

                // A backend that negotiates its geometry rather than being commanded already knows
                // what it landed on, and forcing the mode again is a whole extra reconfiguration
                // that is guaranteed to fail whenever the requested size is off the ladder the OS
                // published. `None` means "the backend does not know", which is every commanded
                // backend, and keeps the old behaviour.
                let already_at_size = client.created_geometry(display_id)
                    == Some(monitor_dims(width, height, swap_axes));
                if already_at_size {
                    tprintln!(
                        "virtual display {device_name} was created at {width}x{height}; skipping the forced mode set"
                    );
                }

                if !already_at_size {
                    let name = device_name.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        pipeline::set_display_mode(&name, width, height, refresh, swap_axes)
                    })
                    .await;
                    match res {
                    Ok(Ok(())) => tprintln!(
                        "virtual display {device_name} set to {width}x{height}@{refresh} (portrait={portrait})"
                    ),
                    Ok(Err(e)) => {
                        mode_applied = false;
                        teprintln!("could not force {device_name} to {width}x{height}: {e}");
                    }
                    Err(e) => {
                        mode_applied = false;
                        teprintln!("set-mode task for {device_name} panicked: {e}");
                    }
                }
                }

                if !already_at_size {
                    wait_for_display_settle(
                        &device_name,
                        Some(monitor_dims(width, height, swap_axes)),
                    )
                    .await;
                }
                apply_display_scale(&device_name, scale).await;
                if topology_reset {
                    if let Some(sessions) = state.config.sessions.as_ref() {
                        let others = session::live_displays_except(sessions, client_ip);
                        if !others.is_empty() {
                            tokio::spawn(restore_displays(others));
                        }
                    }
                }
                (display_id, device_name)
            }
        };

        if let Some(s) = state.config.sessions.as_ref() {
            session::set_host_ip(s, client_ip, state.config.lan_ip.clone());
            let landed = if mode_applied {
                session::LiveDisplay {
                    display_id,
                    device_name: device_name.clone(),
                    ..desired
                }
            } else {
                let actual = read_back_dimensions(&device_name, swap_axes)
                    .await
                    .or_else(|| existing.as_ref().map(|p| (p.width, p.height)))
                    .unwrap_or((width, height));
                teprintln!(
                    "virtual display {device_name} stayed at {}x{} instead of {width}x{height}; \
                 will retry on the next reconfigure",
                    actual.0,
                    actual.1
                );
                session::LiveDisplay {
                    display_id,
                    device_name: device_name.clone(),
                    width: actual.0,
                    height: actual.1,
                    ..desired
                }
            };
            session::set_live_display(s, client_ip, landed);
        }

        let display_ms = t_display.elapsed().as_secs_f64() * 1000.0;

        // `start_on_monitor` is the single heaviest blocking call on the join path: a DWM check,
        // display enumeration, a D3D11 device, the scaler, an NVENC/oneVPL/x264 session and the
        // capture start, with a DXGI fallback stacked behind it on failure. Running it inline parked
        // a tokio worker that also owns other clients' RTP writers, `/reconfig` and bitrate polls.
        let t_pipeline = std::time::Instant::now();
        let session = {
            let cfg = cfg.clone();
            let name = device_name.clone();
            match tokio::task::spawn_blocking(move || pipeline::start_on_monitor(&cfg, &name))
                .await
                .context("start-capture task")?
            {
                Ok(s) => s,
                Err(e) => return Err(e.context("starting capture for virtual display")),
            }
        };
        let pipeline_ms = t_pipeline.elapsed().as_secs_f64() * 1000.0;

        // Publish this session's counters under the client IP so `status --json` and the verbose
        // summary can find them without going through the session map, which is not reachable from
        // the process that runs `status`.
        crate::streamer::telemetry::publish(client_ip, Arc::clone(&session.pipeline.telemetry));

        {
            let name = device_name.clone();
            if let Ok(Some((left, top, width, height))) =
                tokio::task::spawn_blocking(move || pipeline::monitor_rect(&name)).await
            {
                tprintln!("remote-input display {device_name}: {width}x{height} at ({left},{top})");
            }
        }

        anyhow::Ok((display_id, device_name, session, display_ms, pipeline_ms))
    };

    let t_handshake = std::time::Instant::now();
    let (display_res, whep_res) = tokio::join!(
        display_and_pipeline,
        webrtc_session::prepare_whep(
            req.sdp.clone(),
            client_ip,
            ice_servers,
            locality,
            video,
            audio_params.as_ref(),
            state.config.bind_ip,
        )
    );
    let handshake_ms = t_handshake.elapsed().as_secs_f64() * 1000.0;

    // Both halves have to be unwound in whichever order they failed: the display half owns a real
    // virtual display and a capture session, and the handshake half owns a peer connection, its
    // ICE sockets and the mux read worker.
    let pending = match whep_res {
        Ok(p) => p,
        Err(e) => {
            if let Ok((display_id, _, session, _, _)) = display_res {
                session.stop();
                if !existed_before {
                    if let Some(s) = state.config.sessions.as_ref() {
                        let _ = session::take_live_display(s, client_ip);
                    }
                    remove_display_async(client, display_id).await;
                }
            }
            return Err(e.context("WHEP handshake"));
        }
    };
    let (display_id, device_name, session, display_ms, pipeline_ms) = match display_res {
        Ok(v) => v,
        Err(e) => {
            pending.abandon().await;
            return Err(e);
        }
    };

    let (closed_tx, closed_rx) = oneshot::channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let resource_id = pending.resource_id().to_string();
    let (answer, whep) = match webrtc_session::activate_whep(
        pending,
        &session.pipeline,
        Some(closed_tx),
        webrtc_session::InputParams {
            device: Some(device_name.clone()),
            client_os: req.os.trim().to_string(),
            control_enabled,
            control_rx: Some(control_rx),
        },
        audio_params,
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            session.stop();
            if !existed_before {
                if let Some(s) = state.config.sessions.as_ref() {
                    let _ = session::take_live_display(s, client_ip);
                }
                remove_display_async(client, display_id).await;
            }
            return Err(e.context("WHEP activation"));
        }
    };

    tprintln!(
        "join stages ({locality:?}): display={display_ms:.0}ms pipeline={pipeline_ms:.0}ms handshake={handshake_ms:.0}ms (overlapped)"
    );

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

    if let Some(s) = state.config.sessions.as_ref() {
        session::set_control_sender(s, client_ip, control_tx.clone());
    }

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

        if let Some(s) = sessions.as_ref() {
            session::clear_control_sender(s, &report_ip, &control_tx);
        }
        crate::streamer::telemetry::unregister(&report_ip);

        // Give back the peer connection, its ICE agent, that agent's UDP sockets and the
        // injector thread. Nothing did this before, and because the connection held an `Arc` to
        // itself through its own state-change handler, nothing could have.
        whep.close().await;
        tprintln!(
            "session for {report_ip} closed; live transport sessions: {}",
            webrtc_session::live_sessions()
        );

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

    Ok((answer, resource_id))
}

async fn remove_display_async(client: &session::SharedVirtualDisplay, id: u32) {
    let client = client.clone();
    let _ = tokio::task::spawn_blocking(move || client.remove_display(id)).await;
}

fn dpr_mode_ladder(css_w: u32, css_h: u32) -> Vec<(u32, u32)> {
    let even = |v: f64| (v.round() as u32).clamp(2, 16384) & !1;
    let mut modes: Vec<(u32, u32)> = platform::display_dpr_ladder()
        .into_iter()
        .map(|r| (even(css_w as f64 * r), even(css_h as f64 * r)))
        .collect();
    modes.sort_unstable();
    modes.dedup();
    modes
}

/// Requested geometry expressed the way the OS reports it back: a portrait-rotated display
/// enumerates with its axes swapped.
fn monitor_dims(width: u32, height: u32, swap_axes: bool) -> (u32, u32) {
    if swap_axes {
        (height, width)
    } else {
        (width, height)
    }
}

async fn read_back_dimensions(device_name: &str, swap_axes: bool) -> Option<(u32, u32)> {
    let name = device_name.to_string();
    let (w, h) = tokio::task::spawn_blocking(move || pipeline::monitor_dimensions(&name))
        .await
        .ok()
        .flatten()?;
    Some(if swap_axes { (h, w) } else { (w, h) })
}

async fn restore_displays(displays: Vec<session::LiveDisplay>) {
    for display in displays {
        let name = display.device_name.clone();
        if name.is_empty() {
            continue;
        }
        tprintln!("re-applying {name} after the extend topology was re-applied");
        let (w, h, hz, swap) = (
            display.width,
            display.height,
            display.refresh,
            display.swap_axes(),
        );
        let name2 = name.clone();
        let res =
            tokio::task::spawn_blocking(move || pipeline::set_display_mode(&name2, w, h, hz, swap))
                .await;
        if let Ok(Err(e)) = res {
            teprintln!("could not restore {name} to {w}x{h}: {e}");
        }
        wait_for_display_settle(&name, Some(monitor_dims(w, h, swap))).await;
        apply_display_scale(&name, display.scale).await;
    }
}

/// Apply a display-scale change to a live display without making the client rejoin.
///
/// Every override the desktop UI can write used to bump the reconfig epoch, and the client answers
/// an epoch bump by tearing the whole session down and starting again — a new peer connection, a
/// new encoder, a new capture session, a new injector thread, and the full join cost. For most
/// overrides that is unavoidable today because they are encoder-session parameters. **The display
/// scale is not one of them**: it changes the extended desktop's DPI, not the captured geometry, so
/// the stream the client is already decoding is bit-for-bit the stream it wants. Returns whether it
/// was applied, so the caller can fall back to the epoch bump.
pub async fn apply_scale_live(
    sessions: &session::SharedSessions,
    ip: &str,
    effective: u32,
) -> bool {
    let Some(display) = session::get_live_display(sessions, ip) else {
        return false;
    };
    apply_display_scale(&display.device_name, effective).await;
    let mut updated = display.clone();
    updated.scale = effective.clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);
    session::set_live_display(sessions, ip, updated);
    tprintln!("display scale for {ip} applied live at {effective}% (no rejoin)");
    true
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

/// Wait for a display's reported geometry to stop moving after a mode change.
///
/// `expected` is what we asked for, in monitor coordinates. Passing it turns the common case —
/// the mode landed immediately — from "read, sleep 75 ms, read again, compare" into a single
/// read, which is 75 ms off every join that touches a display.
async fn wait_for_display_settle(device_name: &str, expected: Option<(u32, u32)>) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut last: Option<(u32, u32)> = None;
    loop {
        let name = device_name.to_string();
        let dims = tokio::task::spawn_blocking(move || pipeline::monitor_dimensions(&name))
            .await
            .ok()
            .flatten();
        if dims.is_some() && (dims == last || dims == expected) {
            return;
        }
        last = dims;
        if tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
    }
}

async fn wait_for_new_monitor(before: &[String]) -> (Option<String>, bool) {
    let start = tokio::time::Instant::now();
    let deadline = start + DISPLAY_ATTACH_TIMEOUT;
    let mut topology_reset = false;
    loop {
        // `EnumDisplayDevices` is a blocking Win32 call and this loop can run 50 times over the
        // 5 s attach timeout, all of it on a tokio worker that also drives other clients' RTP.
        let now = tokio::task::spawn_blocking(pipeline::monitor_device_names)
            .await
            .unwrap_or_default();
        if let Some(name) = now.iter().find(|n| !before.contains(n)) {
            return (Some(name.clone()), topology_reset);
        }
        if tokio::time::Instant::now() >= deadline {
            return (None, topology_reset);
        }
        if tokio::time::Instant::now() - start >= TOPOLOGY_NUDGE_AFTER {
            let applied = tokio::task::spawn_blocking(pipeline::set_display_topology_extend)
                .await
                .unwrap_or(false);
            topology_reset |= applied;
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

/// An address as it has to appear in a URL. RFC 3986 requires an IPv6 literal to be bracketed, and
/// an unbracketed one is not a URL a browser will open.
pub fn url_host(ip: &str) -> String {
    match ip.parse::<IpAddr>() {
        Ok(IpAddr::V6(_)) => format!("[{ip}]"),
        _ => ip.to_string(),
    }
}

fn log_urls(lan_ip: Option<&str>, http_port: u16, https_port: u16, self_signed: bool) {
    tprintln!("server listening — HTTP :{http_port}, HTTPS :{https_port}");
    match lan_ip {
        Some(ip) => {
            let host = url_host(ip);
            tprintln!("  LAN (open this first):  http://{host}:{http_port}/");
            tprintln!("  LAN (secure / WebCodecs): https://{host}:{https_port}/");
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

    fn state_with_local_ips(ips: &[&str]) -> AppState {
        let local_ips = session::new_shared_local_ips();
        *local_ips.lock_recover() = ips.iter().map(|s| s.parse().unwrap()).collect();
        AppState::new(Config {
            local_ips: Some(local_ips),
            stun_urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Config::default()
        })
    }

    #[test]
    fn an_ipv6_literal_is_bracketed_in_a_url() {
        assert_eq!(url_host("192.168.1.20"), "192.168.1.20");
        assert_eq!(url_host("2001:db8::1"), "[2001:db8::1]");
        assert_eq!(url_host("localhost"), "localhost");
    }

    #[test]
    fn a_peer_on_our_subnet_is_classified_before_the_handshake() {
        let s = state_with_local_ips(&["192.168.1.10", "10.0.0.5"]);
        assert_eq!(
            s.locality_for("192.168.1.44"),
            SessionLocality::SameNetwork,
            "same /24 as an adapter"
        );
        assert_eq!(s.locality_for("10.0.0.200"), SessionLocality::SameNetwork);
        assert_eq!(s.locality_for("127.0.0.1"), SessionLocality::SameNetwork);
    }

    #[test]
    fn a_private_peer_a_router_hop_away_is_not_claimed_as_lan() {
        // Still behind the same NAT, so not a relay case — but it does not get the
        // same-subnet shortcuts either, because the extra hop is where jitter comes from.
        let s = state_with_local_ips(&["192.168.1.10"]);
        assert_eq!(s.locality_for("192.168.9.7"), SessionLocality::Unknown);
    }

    #[test]
    fn a_public_peer_is_cross_network() {
        let s = state_with_local_ips(&["192.168.1.10"]);
        assert_eq!(s.locality_for("203.0.113.9"), SessionLocality::CrossNetwork);
        assert_eq!(
            s.locality_for("not-an-ip"),
            SessionLocality::Unknown,
            "an unparseable key must not be optimistically treated as local"
        );
    }

    #[test]
    fn a_lan_peer_is_offered_no_stun_server() {
        let s = state_with_local_ips(&["192.168.1.10"]);
        assert!(
            s.ice_for(SessionLocality::SameNetwork).is_empty(),
            "a same-subnet pair has no use for a reflexive candidate"
        );
        assert!(!s.ice_for(SessionLocality::CrossNetwork).is_empty());
        let json = s.ice_json_for(SessionLocality::SameNetwork);
        assert!(!json.contains("stun:"), "served to the client: {json}");
        assert!(s
            .ice_json_for(SessionLocality::CrossNetwork)
            .contains("stun:"));
    }

    #[test]
    fn portrait_geometry_is_read_back_with_its_axes_swapped() {
        assert_eq!(monitor_dims(1080, 1920, false), (1080, 1920));
        assert_eq!(monitor_dims(1080, 1920, true), (1920, 1080));
    }

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

    #[test]
    fn every_reachable_pixel_ratio_has_a_registered_mode() {
        let (css_w, css_h) = (1280u32, 720u32);
        let ladder = dpr_mode_ladder(css_w, css_h);
        let even = |v: f64| (v.round() as u32).clamp(2, 16384) & !1;
        for raw in [1.0, 1.4, 1.5, 2.0, 2.625, 3.0, 3.5, 4.0, 9.0] {
            let dpr = platform::snap_display_dpr(raw);
            let mode = (even(css_w as f64 * dpr), even(css_h as f64 * dpr));
            assert!(
                ladder.contains(&mode),
                "{raw} snapped to {dpr}, whose {mode:?} is not in {ladder:?}"
            );
        }
    }

    #[test]
    fn a_snapped_ratio_can_be_expressed_as_a_display_scale() {
        let steps = platform::display_scale_steps();
        if steps.is_empty() {
            return;
        }
        for raw in [1.0, 1.4, 1.5, 2.0, 2.625, 3.0, 4.0] {
            let dpr = platform::snap_display_dpr(raw);
            assert!(dpr <= raw + 1e-9, "{raw} snapped up to {dpr}");
            let scale = (dpr * 100.0).round() as u32;
            assert!(
                steps.contains(&scale),
                "{raw} snapped to {dpr}, needing an unavailable {scale}% scale"
            );
        }
    }

    #[test]
    fn swap_axes_is_recoverable_from_a_recorded_display() {
        let upright = session::LiveDisplay {
            display_id: 1,
            device_name: "1".into(),
            width: 1080,
            height: 1920,
            refresh: 60,
            scale: 100,
            portrait: true,
        };
        assert!(!upright.swap_axes());
        let rotated = session::LiveDisplay {
            portrait: false,
            ..upright.clone()
        };
        assert!(rotated.swap_axes());
    }
}

#[cfg(test)]
mod created_geometry_tests {
    use super::*;

    #[derive(Debug)]
    struct Negotiating(Option<(u32, u32)>);

    impl session::VirtualDisplayController for Negotiating {
        fn create_display(&self, _: String, _: u32, _: u32, _: u32) -> Result<u32, String> {
            Ok(0)
        }
        fn remove_display(&self, _: u32) {}
        fn remove_all_displays(&self) {}
        fn created_geometry(&self, _: u32) -> Option<(u32, u32)> {
            self.0
        }
    }

    #[derive(Debug)]
    struct Commanded;

    impl session::VirtualDisplayController for Commanded {
        fn create_display(&self, _: String, _: u32, _: u32, _: u32) -> Result<u32, String> {
            Ok(0)
        }
        fn remove_display(&self, _: u32) {}
        fn remove_all_displays(&self) {}
    }

    /// The condition `start_session` uses to decide whether the forced mode set and the settle
    /// poll can be skipped.
    fn would_skip(
        c: &dyn session::VirtualDisplayController,
        w: u32,
        h: u32,
        swap_axes: bool,
    ) -> bool {
        c.created_geometry(0) == Some(monitor_dims(w, h, swap_axes))
    }

    #[test]
    fn a_backend_that_landed_on_the_requested_size_skips_the_forced_mode_set() {
        assert!(would_skip(
            &Negotiating(Some((1920, 1080))),
            1920,
            1080,
            false
        ));
    }

    #[test]
    fn a_backend_that_landed_somewhere_else_still_forces_the_mode() {
        assert!(!would_skip(
            &Negotiating(Some((1920, 1080))),
            2560,
            1440,
            false
        ));
    }

    #[test]
    fn a_portrait_request_is_compared_in_monitor_coordinates() {
        // The client asked for 1080x1920; the display is driven landscape and rotated, so the
        // geometry to compare against is the transposed one.
        assert!(would_skip(
            &Negotiating(Some((1920, 1080))),
            1080,
            1920,
            true
        ));
        assert!(!would_skip(
            &Negotiating(Some((1080, 1920))),
            1080,
            1920,
            true
        ));
    }

    #[test]
    fn a_commanded_backend_keeps_the_old_behaviour() {
        // Windows registers a mode list and the OS picks from it, so the backend genuinely does
        // not know — the default `None` must never let the mode set be skipped.
        assert!(!would_skip(&Commanded, 1920, 1080, false));
    }
}
