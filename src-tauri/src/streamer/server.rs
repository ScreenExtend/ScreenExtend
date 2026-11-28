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
    /// The client probed its decoder and built its worklet, not just checked that the
    /// constructors exist. Absent means "unknown" (a client older than the probe).
    #[serde(default)]
    verified: bool,
    /// The client wants a spare Opus track in the answer, to switch to if the WebCodecs path
    /// dies mid-session. Opt-in: a client that attaches every inbound track on `ontrack` would
    /// report the fallback path and play a silent element beside its worklet.
    #[serde(default, rename = "wantsFallbackTrack")]
    wants_fallback_track: bool,
    /// The client can parse a `FLAG_REDUNDANT` message (previous frame's Opus payload
    /// appended). Older clients cannot read one, so `false` is the only safe default.
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
    /// The session this tab already holds, when the client is renegotiating in place rather than
    /// joining fresh, so a reconfigure keeps its virtual display instead of minting a new one.
    /// Honoured only while that session is live and belongs to the same device token.
    #[serde(default, rename = "joinId")]
    join_id: String,
    /// Minted once per page load by the client (32 lowercase hex). Absent on older pages and
    /// on the relay's page. See `session::pending_join_for_attempt`.
    #[serde(default, rename = "attemptId")]
    attempt_id: String,
    /// The page holds its no-picture timers while the PONG says the picture is pending, so it may
    /// be answered before the capture opens. Absent on older pages and on the relay's copy, which
    /// are answered after the capture opens.
    #[serde(default, rename = "pendingPicture")]
    pending_picture: bool,
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
pub const MAX_REFRESH_RATE: u32 = 550;

/// Compose cadence to ask the virtual display for, independent of the stream's frame rate.
/// Every created display registers this rate (`COMMON_REFRESH_RATES`); if the driver refuses
/// it the join carries on at whatever it did land on.
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

    /// Whether this join has to be turned away for want of a display slot. Only the
    /// one-at-a-time AirPlay tier can answer yes (see [`session::concurrent_display_cap`]).
    ///
    /// `holder` is the asking device's token, when known: a device that already holds the one
    /// display is reconnecting to it and is never told the host is full. `None` is the page
    /// load, which happens before any token is presented.
    pub fn display_slot_unavailable(&self, holder: Option<&str>) -> bool {
        let Some(client) = self.config.virtual_display.as_ref() else {
            return false;
        };
        let Some(max) = session::shared_display_cap(client) else {
            return false;
        };
        let Some(sessions) = self.config.sessions.as_ref() else {
            return false;
        };
        if let Some(token) = holder.filter(|t| !t.is_empty()) {
            let holds = session::joins_for_token(sessions, token)
                .iter()
                .any(|join| session::get_live_display(sessions, join).is_some());
            if holds {
                return false;
            }
        }
        session::live_display_count(sessions) >= max
    }

    pub fn display_capacity(&self) -> (Option<usize>, usize) {
        let max = self
            .config
            .virtual_display
            .as_ref()
            .and_then(session::shared_display_cap);
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

    /// Classify a peer before the handshake, so the session's MTU, gather wait, pacing and
    /// playout-delay hint are right from the first packet. `detect_session_locality` checks the
    /// guess afterwards against the nominated ICE pair, which cannot exist yet at join time.
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
            // private but off our subnets: not a relay case, but a router hop away, so it gets
            // no LAN-only shortcuts
            return SessionLocality::Unknown;
        }
        SessionLocality::CrossNetwork
    }

    /// The locality of a join, decided from where it arrived rather than from a parse.
    ///
    /// A relayed join has no peer address: [`JoinOrigin::relayed`] puts the relay's opaque client
    /// id (`c_Lu6CRIn28D7GxgD8qMpTow`) in `peer_ip`, which never parses, so
    /// [`AppState::locality_for`] alone would say `Unknown` and pick the LAN transport constants:
    ///
    /// ```text
    /// session transport: locality=Unknown mtu=1188 pacing=Light playout_delay_max=0ms
    /// session locality was pre-classified Unknown but the nominated candidate pair says
    /// CrossNetwork; transport constants for this session were chosen for the former
    /// ```
    ///
    /// The relay exists only to carry clients that cannot reach this host directly, so a relayed
    /// join is cross-network by construction.
    pub fn locality_for_origin(&self, origin: JoinOrigin<'_>) -> SessionLocality {
        if origin.relay_client.is_some() {
            return SessionLocality::CrossNetwork;
        }
        self.locality_for(origin.peer_ip)
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

    /// ICE servers by locality. Same-network drops STUN only: a same-subnet pair has no use for
    /// a reflexive candidate. TURN stays whenever configured, so a misclassified peer can connect.
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
        // `locality` decides which candidate the client may stop gathering on, and it cannot be
        // inferred from the server list: a same-subnet peer is still handed TURN as a backup
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
    /// When the answer existed, so the handler can say how long it took to hand it back.
    pub ready_at: Option<std::time::Instant>,
}

impl ProcessedResponse {
    fn err(status: StatusCode, body: impl Into<String>) -> Self {
        Self {
            status: status.as_u16(),
            content_type: "text/plain",
            body: body.into(),
            device_token: None,
            resource: None,
            ready_at: None,
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
                format!("HTTP server error on {http_addr}: port in use or blocked by firewall?")
            })
        },
        async {
            https.await.with_context(|| {
                format!("HTTPS server error on {https_addr}: port in use or blocked by firewall?")
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
        // the WHEP resource (RFC 9725 section 4.6); a client that ignores `Location` is unaffected
        .route("/whep/{id}", patch(whep_patch))
        .route("/whep/{id}", delete(whep_delete))
        // GET too: the oldest pages can only say goodbye with an image request
        .route("/leave", post(leave).get(leave))
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

// COOP + COEP on the document, in a secure context, make `crossOriginIsolated` true so the
// client can use the SharedArrayBuffer ring; over plain HTTP they are inert and it falls back to
// the postMessage ring. A dedicated worker's script is its own document: Chrome refuses to start
// it unless that response carries COEP:require-corp too, hence `worker_asset`.
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

/// The client page with `__SAME_DEVICE_FLAG__` already substituted, one cached variant each, so
/// a page load is not a `String::replace` over the largest asset in the tree.
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
    headers: HeaderMap,
) -> Response {
    // a page load carries no device token; the join cookie is what keeps a reload of the one
    // AirPlay display from being told the host is full
    let holder = state
        .config
        .sessions
        .as_ref()
        .zip(cookie_join(&headers))
        .and_then(|(s, join)| session::device_token_for(s, &join));
    if state.display_slot_unavailable(holder.as_deref()) {
        return at_capacity_page();
    }
    let html = same_device_page(state.is_same_device(peer.ip()));
    (isolation_headers(), Html(html)).into_response()
}

/// A join id as it is allowed to appear on the wire: exactly what [`session::mint_join_id`]
/// produces. Anything else is discarded rather than used as a map key.
fn valid_join_id(raw: &str) -> bool {
    raw.len() == 32
        && raw
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn cookie_join(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|pair| pair.trim().strip_prefix(JOIN_COOKIE_PREFIX))
        .find(|v| valid_join_id(v))
        .map(str::to_string)
}

const JOIN_COOKIE: &str = "se_join";
const JOIN_COOKIE_PREFIX: &str = "se_join=";

#[derive(Deserialize, Default)]
struct JoinQuery {
    #[serde(default)]
    join: String,
}

/// Which live session a request belongs to.
///
/// `?join=<id>` wins over the `se_join` cookie: a cookie is per-origin, so two tabs share it and
/// only the parameter tells them apart. The cookie covers a page cached before the id existed.
///
/// No address fallback, deliberately: an address is recycled between devices and moves under a
/// live session, so resolving by it lets one device drive another's display.
fn request_join(query: &JoinQuery, headers: &HeaderMap) -> Option<String> {
    if valid_join_id(&query.join) {
        return Some(query.join.clone());
    }
    cookie_join(headers)
}

/// The session a request names, with its address label refreshed. That is the only thing an
/// address is used for: a peer that roams keeps its session and the dashboard shows where it is.
fn resolve_join(
    state: &AppState,
    query: &JoinQuery,
    headers: &HeaderMap,
    peer: IpAddr,
) -> Option<String> {
    let join = request_join(query, headers)?;
    if let Some(sessions) = state.config.sessions.as_ref() {
        session::note_session_ip(sessions, &join, &peer.to_string());
    }
    Some(join)
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

// Each asset's ETag is a hash of its own bytes, folded at compile time. Not the crate version:
// the client page is edited by hand with no build step, so an edit without a version bump would
// 304 against a stale cached copy. `no-cache` makes the client revalidate (a 304, not ~120 KB a
// reload). `index` is left uncached: it carries a per-request substitution.
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

// A script loaded with `new Worker` (or `addModule`) on the isolated page. Without its own COEP
// header Chrome fails the worker with a message-less `error` and audio drops to the Opus track.
// The 304 carries it too: a revalidated worker script is checked again.
fn worker_asset(etag: &str, headers: &HeaderMap, body: &'static str) -> Response {
    let mut response = asset("text/javascript", etag, headers, body);
    let h = response.headers_mut();
    h.insert(
        header::HeaderName::from_static("cross-origin-embedder-policy"),
        header::HeaderValue::from_static("require-corp"),
    );
    h.insert(
        header::HeaderName::from_static("cross-origin-resource-policy"),
        header::HeaderValue::from_static("same-origin"),
    );
    response
}

// FNV-1a, `const` so every call site folds to a literal and no request pays for the hash
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
    worker_asset(&asset_etag(TAG), &headers, BODY)
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
    worker_asset(&asset_etag(TAG), &headers, BODY)
}

async fn audio_worker_js(headers: HeaderMap) -> Response {
    const BODY: &str = include_str!("static/audio-worker.js");
    const TAG: u64 = asset_hash(BODY.as_bytes());
    worker_asset(&asset_etag(TAG), &headers, BODY)
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
    // no STUN for a same-subnet peer: both ends would block on a public round trip before the
    // join starts (client up to 3 s gathering, host up to 1.5 s), and an offline or
    // hosted-network host pays both timeouts in full
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
    axum::extract::Query(query): axum::extract::Query<JoinQuery>,
    headers: HeaderMap,
) -> Response {
    let body = match resolve_join(&state, &query, &headers, peer.ip()) {
        Some(join) => process_reconfig(&state, &join),
        // no session named: an unknown join reads zeros, which the client treats as "no change"
        None => process_reconfig(&state, ""),
    };
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

async fn leave(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::extract::Query(query): axum::extract::Query<JoinQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(join) = resolve_join(&state, &query, &headers, peer.ip()) {
        process_leave(&state, &join);
    }
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
    /// Optional: a body without it says nothing about the browser, and the host must not read
    /// that as "cannot switch speakers" (the device sheet's only use of `false`).
    #[serde(default)]
    supported: Option<bool>,
    #[serde(default)]
    outputs: Vec<AudioOutputEntry>,
    /// The sink the client is playing on right now, which is not always the one the host
    /// asked for: the user can change it in the browser's own output control. Optional, so an
    /// older client still parses.
    #[serde(default)]
    current: Option<String>,
    /// Whether the labels in `outputs` are the operating system's or the client's own
    /// numbering. Optional, for the same reason `current` is.
    #[serde(default)]
    labelled: Option<bool>,
    /// Each output-discovery API's result at the join. Values are taken as they come and
    /// stringified, so a page that sends something unexpected still parses.
    #[serde(default)]
    discovery: Option<serde_json::Map<String, serde_json::Value>>,
}

const MAX_DISCOVERY_ENTRIES: usize = 24;
const MAX_DISCOVERY_KEY: usize = 64;
const MAX_DISCOVERY_VALUE: usize = 32;

fn bounded_discovery(
    d: serde_json::Map<String, serde_json::Value>,
) -> std::collections::BTreeMap<String, String> {
    d.into_iter()
        .take(MAX_DISCOVERY_ENTRIES)
        .map(|(k, v)| {
            let v = match v {
                serde_json::Value::String(s) => s,
                other => other.to_string(),
            };
            (
                truncate_chars(&k, MAX_DISCOVERY_KEY),
                truncate_chars(&v, MAX_DISCOVERY_VALUE),
            )
        })
        .collect()
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

async fn audio_outputs(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    axum::extract::Query(query): axum::extract::Query<JoinQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(join) = resolve_join(&state, &query, &headers, peer.ip()) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    process_audio_outputs(&state, &join, &body);
    StatusCode::NO_CONTENT.into_response()
}

/// One `/audio-outputs` body for one join, from either transport. The relay names the join in
/// the query it tunnels; a join the host does not hold is ignored rather than created.
pub fn process_audio_outputs(state: &AppState, join: &str, body: &[u8]) {
    let req: AudioOutputsRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(_) => return,
    };
    let outputs: Vec<session::AudioOutput> = req
        .outputs
        .into_iter()
        .take(MAX_AUDIO_OUTPUTS)
        .map(|o| session::AudioOutput {
            id: truncate_chars(&o.id, MAX_AUDIO_OUTPUT_STR),
            label: truncate_chars(&o.label, MAX_AUDIO_OUTPUT_STR),
        })
        .collect();

    let current = req
        .current
        .map(|c| truncate_chars(&c, MAX_AUDIO_OUTPUT_STR));
    let discovery = req.discovery.map(bounded_discovery);

    let Some(s) = state.config.sessions.as_ref() else {
        return;
    };
    if !s.lock_recover().contains_key(join) {
        return;
    }
    let token = session::device_token_for(s, join).unwrap_or_default();
    // the only host-side record of which device reported `supported`, which is the device
    // sheet's "can't switch speakers"
    tprintln!(
        "[audio-outputs] join {join}: supported={} outputs={} ({}) labelled={} current={} discovery={}",
        req.supported
            .map_or("unsaid".to_string(), |b| b.to_string()),
        outputs.len(),
        outputs
            .iter()
            .map(|o| if o.id.starts_with("se:") {
                o.id.as_str()
            } else if o.id.is_empty() {
                "default"
            } else {
                "device"
            })
            .collect::<Vec<_>>()
            .join(","),
        req.labelled.map_or("unsaid".to_string(), |b| b.to_string()),
        current.as_deref().map_or("unsaid", |c| if c.is_empty() {
            "default"
        } else {
            "device"
        }),
        discovery.as_ref().map_or("unsaid".to_string(), |d| d
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect::<Vec<_>>()
            .join(",")),
    );
    session::set_audio_outputs(
        s,
        join,
        req.supported,
        outputs,
        current,
        req.labelled,
        discovery,
    );
    if let Some(reporter) = state.config.device_reporter.as_ref() {
        let report = state.audio_outputs_report(join);
        reporter.report_audio_outputs(join.to_string(), token, report);
    }
}

impl AppState {
    fn audio_outputs_report(&self, join: &str) -> session::AudioOutputsReport {
        match (
            self.config.sessions.as_ref(),
            self.config.audio_sinks.as_ref(),
        ) {
            (Some(s), Some(sinks)) => session::audio_outputs_report(s, sinks, join),
            _ => Default::default(),
        }
    }

    fn device_audio_sink(&self, join: &str) -> String {
        match (
            self.config.sessions.as_ref(),
            self.config.audio_sinks.as_ref(),
        ) {
            (Some(s), Some(sinks)) => session::device_token_for(s, join)
                .and_then(|token| session::device_audio_sink(sinks, &token))
                .unwrap_or_default(),
            _ => String::new(),
        }
    }
}

pub fn process_reconfig(state: &AppState, join: &str) -> String {
    let (epoch, kick) = state
        .config
        .sessions
        .as_ref()
        .map(|s| {
            (
                session::reconfig_epoch(s, join),
                session::kick_epoch(s, join),
            )
        })
        .unwrap_or((0, 0));
    let audio_sink = state.device_audio_sink(join);
    serde_json::json!({ "epoch": epoch, "kick": kick, "audioSink": audio_sink }).to_string()
}

/// `/reconfig` for a tunnelled client that named no join.
///
/// The relay's deployed client page polls `/reconfig` with no `?join=`, and the empty string
/// matches no session: `{"epoch":0,"kick":0}` for ever, so no kick or settings change would
/// reach a device that joined over the internet. `relay_client` maps the relay's client id onto
/// the join without touching an address. A client that does send a join id is answered on it.
pub fn process_relay_reconfig(state: &AppState, join: &str, client_id: &str) -> String {
    if !join.is_empty() {
        return process_reconfig(state, join);
    }
    let resolved = state
        .config
        .sessions
        .as_ref()
        .and_then(|s| session::live_join_for_relay_client(s, client_id));
    match resolved.as_deref() {
        Some(j) => process_reconfig(state, j),
        None => process_reconfig(state, join),
    }
}

pub fn process_leave(state: &AppState, join: &str) {
    if let Some(s) = state.config.sessions.as_ref() {
        tprintln!("leave beacon for join {join}; tearing down session");
        session::signal_leave(s, join);
    }
}

/// The relay announces a departure by its own `client_id` and nothing else, so every join that
/// arrived under it ends.
pub fn process_relay_client_gone(state: &AppState, client_id: &str) {
    let Some(s) = state.config.sessions.as_ref() else {
        return;
    };
    for join in session::joins_for_relay_client(s, client_id) {
        tprintln!("[cloud] relay client {client_id} gone; tearing down join {join}");
        session::signal_leave(s, &join);
    }
}

fn build_net_json(config: &Config) -> String {
    // `trickleIce` tells the client it may POST the offer before its gathering has finished and
    // PATCH the candidates afterwards. Without it, on either side, the client gathers then posts.
    serde_json::json!({ "httpsPort": config.https_port, "trickleIce": true }).to_string()
}

async fn whep(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req_headers: HeaderMap,
    body: Bytes,
) -> Response {
    let received = std::time::Instant::now();
    let client_ip = peer.ip().to_string();
    let presented = presented_device_token(&req_headers, &body);
    if state.display_slot_unavailable(Some(presented.as_str())) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "This Mac creates its extended display over AirPlay and can only drive one at a time. The device already connected keeps it: disconnect that device to free the slot, or use macOS 10.15 or later, where every device gets its own display.",
        )
            .into_response();
    }
    let ice = state.ice_for(state.locality_for(&client_ip));
    let out = process_whep_detached(&state, JoinOrigin::direct(&client_ip), &body, ice).await;
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
    // RFC 9725 section 4.1: the answer that carries a resource's `Location` is a `201`. Only
    // here: the cloud tunnel has no URL space and sets no `Location`. The client reads `resp.ok`.
    if let Some(resource) = out.resource.as_deref() {
        if let Ok(value) = header::HeaderValue::from_str(&format!("/whep/{resource}")) {
            headers.insert(header::LOCATION, value);
            if status == StatusCode::OK {
                status = StatusCode::CREATED;
            }
        }
        // the resource id is the session's name, so it also goes out as a header (no URL to
        // parse) and as a cookie, for a cached page that predates `?join=`
        if let Ok(value) = header::HeaderValue::from_str(resource) {
            headers.insert("x-join-id", value);
        }
        if let Ok(value) = header::HeaderValue::from_str(&format!(
            "{JOIN_COOKIE}={resource}; Path=/; SameSite=Lax"
        )) {
            headers.insert(header::SET_COOKIE, value);
        }
    }
    if let Some(ready) = out.ready_at {
        // once per join: a response that reaches the browser long after this line was held up
        // after the handler let go of it
        tprintln!(
            "join response returned at wall_ms={} ({} ms after the answer was ready, {} ms after \
             the request)",
            wall_ms(),
            ready.elapsed().as_millis(),
            received.elapsed().as_millis()
        );
    }
    (status, headers, out.body).into_response()
}

/// Milliseconds since the Unix epoch, comparable with a browser's network log.
fn wall_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

/// The device token a request presents: the `X-Device-Token` header (`PATCH`/`DELETE`) or the
/// join body (`POST /whep`). Both are read here so the at-capacity check can run before the
/// body is parsed properly.
fn presented_device_token(headers: &HeaderMap, body: &[u8]) -> String {
    if let Some(v) = headers.get("x-device-token").and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if !v.is_empty() {
            return v.to_string();
        }
    }
    #[derive(Deserialize)]
    struct JustToken {
        #[serde(default, rename = "deviceToken")]
        device_token: String,
    }
    serde_json::from_slice::<JustToken>(body)
        .map(|t| t.device_token.trim().to_string())
        .unwrap_or_default()
}

/// Trickled ICE from the client, RFC 9725 section 4.6. Without it the client cannot POST until
/// its own gathering has finished or timed out.
///
/// The resource id is a 128-bit capability, but its `Location` header travels in cleartext on
/// the HTTP listener most LAN joins use. So the device token that opened the session is checked
/// too, and the fragment count is bounded: adding remote candidates is a media-redirection
/// primitive. The second factor is the token and not the peer's address, which moves under a
/// live session and is recycled between devices.
async fn whep_patch(
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(text) = std::str::from_utf8(&body) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if text.len() > MAX_SDP_LEN {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    }
    let _ = peer;
    let device_token = presented_device_token(&headers, &[]);
    match webrtc_session::add_trickled_candidates(&id, &device_token, text).await {
        webrtc_session::TrickleLookup::Found => StatusCode::NO_CONTENT.into_response(),
        // the same answer for "no such id" and "not yours", so a probe cannot tell them apart.
        // Must stay `404`: the client stops trickling on exactly that (`startTrickle`)
        webrtc_session::TrickleLookup::Unknown => StatusCode::NOT_FOUND.into_response(),
        webrtc_session::TrickleLookup::Exhausted => StatusCode::TOO_MANY_REQUESTS.into_response(),
    }
}

/// Tear the resource down, RFC 9725 section 4.4, so the resource the `Location` header hands
/// out is a real one (the client itself uses `/leave`). The id is checked against the same
/// registry and device token as `whep_patch`.
async fn whep_delete(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let _ = peer;
    let device_token = presented_device_token(&headers, &[]);
    match webrtc_session::trickle_owner(&id, &device_token) {
        Some(join) => {
            process_leave(&state, &join);
            StatusCode::NO_CONTENT.into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Where a join arrived from. Everything in here is a label or a rate-limit bucket; none of it
/// is an identity.
#[derive(Clone, Copy)]
pub struct JoinOrigin<'a> {
    /// The peer's address, for the log, the locality guess and the OTP rate-limit bucket.
    pub peer_ip: &'a str,
    /// The relay's id for this client, when the join came through the cloud tunnel.
    pub relay_client: Option<&'a str>,
}

impl<'a> JoinOrigin<'a> {
    pub fn direct(peer_ip: &'a str) -> Self {
        Self {
            peer_ip,
            relay_client: None,
        }
    }

    pub fn relayed(client_id: &'a str) -> Self {
        Self {
            peer_ip: client_id,
            relay_client: Some(client_id),
        }
    }
}

/// [`process_whep`], run so that it finishes even when the request that asked for it goes away.
///
/// A page that closes or aborts its `POST` makes hyper drop the request's future wherever it is
/// waiting: inside a display create, or after capture started and before anything records how to
/// stop it. A join nobody ever connects to is closed after [`FIRST_BYTE_WINDOW`].
pub async fn process_whep_detached(
    state: &AppState,
    origin: JoinOrigin<'_>,
    body: &[u8],
    ice_servers: Vec<RTCIceServer>,
) -> ProcessedResponse {
    let state = state.clone();
    let peer_ip = origin.peer_ip.to_string();
    let relay_client = origin.relay_client.map(str::to_string);
    let body = body.to_vec();
    detach_join(async move {
        let origin = JoinOrigin {
            peer_ip: &peer_ip,
            relay_client: relay_client.as_deref(),
        };
        process_whep(&state, origin, &body, ice_servers).await
    })
    .await
}

/// Run a join on its own task. Dropping the returned future detaches the task, never cancels it.
async fn detach_join<F>(join: F) -> ProcessedResponse
where
    F: std::future::Future<Output = ProcessedResponse> + Send + 'static,
{
    match tokio::spawn(join).await {
        Ok(out) => {
            if let Some(ready) = out.ready_at {
                // a long hop back to the handler means the worker it queued behind was busy
                let hop = ready.elapsed();
                if hop >= std::time::Duration::from_millis(20) {
                    teprintln!(
                        "join: the handler resumed {} ms after the join finished",
                        hop.as_millis()
                    );
                }
            }
            out
        }
        Err(e) => {
            teprintln!("join task failed: {e}");
            ProcessedResponse::err(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("join task failed: {e}"),
            )
        }
    }
}

/// How long an offer from a page load waits for the same page load's earlier attempt to record
/// its display. The page gives the retry 10 s and treats any error as final, so the retry waits
/// almost all of that; a refusal is only sent when the page would have given up anyway.
const ATTEMPT_DISPLAY_WAIT: std::time::Duration = std::time::Duration::from_secs(9);

/// The refusal a retry gets when the first attempt from its page load is still building its
/// display at [`ATTEMPT_DISPLAY_WAIT`]; `None` once that attempt has its display, has ended, or
/// never existed. Asked on both the pre-approved and the OTP path, before either decides a token.
async fn attempt_wait_refusal(
    state: &AppState,
    req: &JoinRequest,
    presented_token: &str,
) -> Option<ProcessedResponse> {
    if !req.join_id.trim().is_empty() {
        return None;
    }
    let s = state.config.sessions.as_ref()?;
    let attempt = req.attempt_id.trim();
    if await_attempt_display(s, attempt, presented_token, ATTEMPT_DISPLAY_WAIT).await {
        return None;
    }
    Some(ProcessedResponse::err(
        StatusCode::SERVICE_UNAVAILABLE,
        "this page's first offer is still setting up its display",
    ))
}

/// Wait for the display of this page load's first attempt, so a retry continues the attempt in
/// flight instead of starting a second one beside it. `true` at once when no such attempt
/// exists, and as soon as it records its display or ends; `false` when it is still building at
/// the bound, and the retry is then refused.
async fn await_attempt_display(
    sessions: &session::SharedSessions,
    attempt: &str,
    presented_token: &str,
    bound: std::time::Duration,
) -> bool {
    let attempt = attempt_shape(attempt);
    let deadline = tokio::time::Instant::now() + bound;
    let mut waited = false;
    while session::attempt_in_flight(sessions, attempt, presented_token) {
        if !waited {
            tprintln!(
                "offer from attempt {attempt}: its first attempt is still building; waiting for it"
            );
            waited = true;
        }
        if tokio::time::Instant::now() >= deadline {
            teprintln!(
                "offer from attempt {attempt}: the first attempt is still building after {} s; \
                 refusing this one rather than building a second display",
                bound.as_secs()
            );
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    true
}

pub async fn process_whep(
    state: &AppState,
    origin: JoinOrigin<'_>,
    body: &[u8],
    ice_servers: Vec<RTCIceServer>,
) -> ProcessedResponse {
    let device_key = origin.peer_ip;
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
        .is_some_and(|banned| session::is_device_banned(banned, &presented_token))
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
        // a returning device is pre-approved, so its lost answer comes back this way
        if let Some(refused) = attempt_wait_refusal(state, &req, &presented_token).await {
            return refused;
        }
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

        // A device the host holds settings for keeps its token: overrides are keyed by it, and a
        // fresh one would leave the rebuild that a settings change triggers with no settings.
        // A page that lost its answer offers again with no join id and, on a first join, no
        // token: if the join that answer opened is still waiting, this is that device.
        if let Some(refused) = attempt_wait_refusal(state, &req, &presented_token).await {
            return refused;
        }
        let retried = if req.join_id.trim().is_empty() {
            state.config.sessions.as_ref().and_then(|s| {
                session::pending_join_for_attempt(s, req.attempt_id.trim(), &presented_token)
            })
        } else {
            None
        };
        match state.config.device_overrides.as_ref() {
            _ if retried.is_some() => {
                let (join, token) = retried.unwrap_or_default();
                tprintln!(
                    "join retried attempt {} for join {join}, whose answer never arrived; \
                     keeping its token",
                    req.attempt_id.trim()
                );
                token
            }
            Some(overrides) if session::is_known_device(overrides, &presented_token) => {
                tprintln!(
                    "join accepted: {device_key} re-presented a device token this host has \
                     settings for; keeping it"
                );
                presented_token
            }
            _ => {
                let token = session::mint_device_token();
                tprintln!("issued a new device token to {device_key} after successful OTP");
                token
            }
        }
    };

    match start_session(state, &req, origin, &device_token, ice_servers).await {
        Ok((answer, resource)) => {
            tprintln!(
                "join accepted: WHEP answer generated ({} bytes) at wall_ms={}",
                answer.len(),
                wall_ms()
            );
            ProcessedResponse {
                status: StatusCode::OK.as_u16(),
                content_type: "application/sdp",
                body: answer,
                device_token: Some(device_token),
                resource: Some(resource),
                ready_at: Some(std::time::Instant::now()),
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

/// Whether this offer continues a session that is already open, or starts a new one.
///
/// A client renegotiating in place (a display setting, a rotation, a video-path step down)
/// presents the join id it holds. Honouring it keeps the virtual display, the dashboard row and
/// the windows arranged on that display.
///
/// The id is honoured only for a session that is still open *and* belongs to the device token
/// this join authenticated as, so a leaked or stale id buys nothing. Anything else is a new join.
fn continuing_join(
    sessions: &session::SharedSessions,
    claimed: &str,
    device_token: &str,
) -> Option<String> {
    let claimed = claimed.trim();
    if claimed.is_empty() || device_token.is_empty() {
        return None;
    }
    match session::device_token_for(sessions, claimed) {
        Some(t) if t == device_token => Some(claimed.to_string()),
        Some(_) => {
            teprintln!(
                "join presented session {claimed}, which belongs to another device; treating it \
                 as a new join"
            );
            None
        }
        None => None,
    }
}

/// The client's `attemptId` when it has the one shape the page mints (32 lowercase hex),
/// else empty: a malformed value is simply not an attempt id.
fn attempt_shape(raw: &str) -> &str {
    let raw = raw.trim();
    if valid_join_id(raw) {
        raw
    } else {
        ""
    }
}

/// A fresh offer (no join id) from the page load whose earlier answer never arrived.
///
/// `process_whep` has already validated the OTP and adopted that join's token, so the token
/// check here is what ties the two together; `session::pending_join_for_attempt` refuses any
/// join that has connected or that already has its own different device.
fn attempt_continuation(
    sessions: &session::SharedSessions,
    claimed: &str,
    attempt: &str,
    device_token: &str,
) -> Option<String> {
    if !claimed.trim().is_empty() || device_token.is_empty() {
        return None;
    }
    let attempt = attempt_shape(attempt);
    let (join, token) = session::pending_join_for_attempt(sessions, attempt, device_token)?;
    (token == device_token).then(|| {
        tprintln!("offer from attempt {attempt} continues join {join}, whose answer was lost");
        join
    })
}

/// A relayed offer that names no join, from a relay client that already holds a live one.
///
/// The relay serves its own, older copy of the client page, which answers an epoch bump by
/// offering again with no `joinId`. As a fresh join that would be a second display and a second
/// dashboard row for every settings change. The relay's `client_id` is per connection, so an
/// offer under it while one of its joins is still streaming is that same page renegotiating.
/// UNVERIFIED (not run): no relayed session can be driven on this machine.
fn relayed_continuation(
    sessions: &session::SharedSessions,
    claimed: &str,
    origin: JoinOrigin<'_>,
    device_token: &str,
) -> Option<String> {
    if !claimed.trim().is_empty() || device_token.is_empty() {
        return None;
    }
    let client_id = origin.relay_client?;
    let join = session::live_join_for_relay_client(sessions, client_id)?;
    session::get_live_display(sessions, &join)?;
    (session::device_token_for(sessions, &join).as_deref() == Some(device_token)).then(|| {
        tprintln!(
            "relayed offer from client {client_id} names no join; continuing its live join {join}"
        );
        join
    })
}

async fn start_session(
    state: &AppState,
    req: &JoinRequest,
    origin: JoinOrigin<'_>,
    device_token: &str,
    ice_servers: Vec<RTCIceServer>,
) -> Result<(String, String)> {
    let client_ip = origin.peer_ip;
    // The WHEP resource id is the session's identity: two tabs of one browser are two sessions.
    // A renegotiating tab presents the id it holds and keeps its session and display; the
    // sequence number below tells the outgoing session's teardown it was superseded, not ended.
    let continuing = state.config.sessions.as_ref().and_then(|s| {
        continuing_join(s, &req.join_id, device_token)
            .or_else(|| attempt_continuation(s, &req.join_id, &req.attempt_id, device_token))
            .or_else(|| relayed_continuation(s, &req.join_id, origin, device_token))
    });
    let join_id = match &continuing {
        Some(join) => {
            tprintln!("join {join} is renegotiating in place; keeping its session and display");
            join.clone()
        }
        None => session::mint_join_id(),
    };
    if let Some(s) = state.config.sessions.as_ref() {
        session::open_session_via(s, &join_id, device_token, client_ip, origin.relay_client);
        session::set_session_attempt(s, &join_id, attempt_shape(&req.attempt_id));
        session::set_session_labels(
            s,
            &join_id,
            req.device_name.trim(),
            &format!("{}x{}", req.width, req.height),
        );
    }
    let locality = state.locality_for_origin(origin);
    let client = state
        .config
        .virtual_display
        .as_ref()
        .context("virtual-display driver unavailable (not running under Tauri)")?;

    let session_seq = state
        .config
        .sessions
        .as_ref()
        .map(|s| session::next_session_seq(s, &join_id))
        .unwrap_or(0);

    // the outgoing session's capture still runs against the display this one reuses, and its
    // teardown task stands down on finding the entry replaced, so only this stops it
    if continuing.is_some() {
        if let Some(s) = state.config.sessions.as_ref() {
            if let Some(stop) = session::take_active_capture(s, &join_id) {
                tprintln!("stopping the previous capture for join {join_id} before renegotiating");
                let _ = tokio::task::spawn_blocking(stop).await;
            }
        }
    }

    // the rate the device reports is the rate it gets, within the host's own bounds
    let detected_refresh = if req.refresh_rate == 0 {
        60
    } else {
        req.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
    };
    if req.refresh_rate != 0 && req.refresh_rate != detected_refresh {
        tprintln!(
            "client reports a {} Hz screen; streaming at {detected_refresh} Hz (host bounds)",
            req.refresh_rate
        );
    }

    let mut cfg = state.config.as_ref().clone();
    cfg.max_fps = detected_refresh;
    // Rolling intra refresh stays off on live joins: in a flat region the wave's intra-coded
    // stripe quantises chroma differently from the inter-predicted blocks around it (green flicker
    // over plain grey on the client). Loss recovery is PLI/FIR -> IDR through `KeyframeGate`;
    // `SE_INTRA_REFRESH=1` opts a host in for a measurement.
    cfg.intra_refresh = matches!(
        crate::envcfg::var("SE_INTRA_REFRESH").as_deref(),
        Ok("1") | Ok("on") | Ok("true")
    );
    let device_settings = state
        .config
        .device_overrides
        .as_ref()
        .and_then(|o| o.lock_recover().get(device_token).copied());
    // shared with the injector rather than copied into it, so `set_device_override` and
    // `set_device_pointer_scope` reach a session that is already running
    let live_input = crate::streamer::input::LiveInput::shared(
        device_settings.map(|o| o.control_enabled).unwrap_or(true),
        device_settings.map(|o| o.pointer_scope).unwrap_or_default(),
    );
    if let Some(s) = state.config.sessions.as_ref() {
        session::set_live_input(s, &join_id, std::sync::Arc::clone(&live_input));
    }
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
                link_loss: None, // wired in by `activate_whep`, which owns the BWE
                locality,
                hub: std::sync::Arc::clone(hub),
                overrides: std::sync::Arc::clone(overrides),
                device_token: device_token.to_string(),
            })
        }
        _ => None,
    };
    apply_join_quality(
        &mut cfg,
        device_settings.as_ref(),
        state.config.device_overrides.is_some(),
        detected_refresh,
        session::is_join_seeded(device_token),
    );

    // read before the encoder is built: the profile is an encoder-session parameter. Baseline
    // (CAVLC, no 8x8 transform) gives away 5-15 % of the bitrate on a client that can take High
    let offered_video = negotiate::read_offer(&req.sdp);

    let native_dpr = if req.dpr.is_finite() { req.dpr } else { 1.0 };
    // `native_dpr` on means the slider is ignored and the device's own ratio is followed on
    // every join; off pins whatever the sheet shows.
    let dpr = platform::snap_display_dpr(
        device_settings
            .filter(|o| !o.native_dpr)
            .map(|o| o.dpr)
            .unwrap_or(native_dpr),
    );
    let backing = |css: u32| ((css as f64 * dpr).round() as u32).clamp(2, 16384) & !1;
    let width = backing(req.width);
    let height = backing(req.height);
    let stream_fps = if cfg.max_fps == 0 {
        60
    } else {
        cfg.max_fps.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
    };
    // An indirect display has no hardware vblank, so its refresh rate is DWM's compose cadence:
    // a captured frame is on average 8.3 ms stale at 60 Hz and 4.2 ms at 120 Hz. The ~4 ms costs
    // the compositor, the capture and the copy a doubling, so it is taken only with
    // `autoFrameRate` on (off is the default). The pipeline caps delivery at `max_fps` either way.
    let refresh = if cfg.auto_frame_rate {
        stream_fps.max(DISPLAY_COMPOSE_HZ)
    } else {
        stream_fps
    }
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

    // only a tab renegotiating on its own id has a display to reuse: the mode changes under a
    // display that never goes away
    let existing: Option<session::LiveDisplay> = match (&continuing, state.config.sessions.as_ref())
    {
        (Some(join), Some(s)) => session::get_live_display(s, join),
        _ => None,
    };
    let existed_before = existing.is_some();
    let client_portrait = height > width;
    // The saved orientation is honoured while the device holds the same session, or the
    // per-device setting would be inert. A device that has physically rotated since its last
    // offer is believed: that is a fact about the hardware, not a setting.
    let prev_client_portrait = existing.as_ref().map(|p| p.height > p.width);
    let device_rotated = prev_client_portrait == Some(!client_portrait);
    // `auto_orientation` on: the device is believed on every offer, not only on a rotation
    let follow_device = device_settings.map(|o| o.auto_orientation).unwrap_or(false);
    let honor_device = existing.is_none() || device_rotated || follow_device;
    let portrait = if honor_device {
        client_portrait
    } else {
        device_settings
            .map(|o| o.orientation_portrait)
            .unwrap_or(client_portrait)
    };
    if honor_device {
        if let Some(overrides) = state.config.device_overrides.as_ref() {
            if let Some(o) = overrides.lock_recover().get_mut(device_token) {
                o.orientation_portrait = client_portrait;
            }
        }
    }
    let swap_axes = portrait != client_portrait;

    // Seed the row for a device the host holds no settings for yet, or its first save has
    // nothing to compare against and is classified as a change to everything (a rename would
    // rebuild the session). The values must be what the sheet shows (`Device::defaults`) and
    // what this join really runs: `apply_join_quality` applied the same `join_defaults()` above.
    if let Some(overrides) = state.config.device_overrides.as_ref() {
        let mut map = overrides.lock_recover();
        match map.entry(device_token.to_string()) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(session::DeviceOverride {
                    orientation_portrait: client_portrait,
                    refresh_rate: detected_refresh,
                    dpr,
                    ..session::DeviceOverride::join_defaults()
                });
                session::mark_join_seeded(device_token);
            }
            // still only a detection: keep it current, so the sheet shows the rate this join is
            // really running at and an untouched save still compares equal
            std::collections::hash_map::Entry::Occupied(mut o)
                if session::is_join_seeded(device_token) =>
            {
                o.get_mut().refresh_rate = cfg.max_fps;
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }

    let base_scale = device_settings
        .map(|o| o.scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE))
        .unwrap_or(100);
    let scale =
        ((dpr * base_scale as f64).round() as u32).clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);

    let desired = session::LiveDisplay {
        display_id: 0,
        device_name: String::new(),
        device_path: String::new(),
        width,
        height,
        refresh,
        scale,
        portrait,
    };

    // Two saves in quick succession are two `POST /whep` for one join, both commanding the same
    // display. Each attempt checks its own sequence number where it is about to spend something,
    // and the older one stands down.
    let still_current = {
        let sessions = state.config.sessions.clone();
        let join = join_id.clone();
        move || {
            sessions
                .as_ref()
                .map(|s| session::is_current_session(s, &join, session_seq))
                .unwrap_or(true)
        }
    };

    // a capture that opens behind the answer can fail before this attempt has registered the
    // leave signal its teardown listens on; that failure waits here until it has
    let capture_failure: CaptureFailure = Default::default();
    let t_display = std::time::Instant::now();
    let display_and_pipeline = async {
        // The DPI write runs beside `start_on_monitor`, not before it: it changes the extended
        // desktop's scaling, not the captured geometry, and `set_display_scale_owned` wants five
        // stable read-backs 100 ms apart (p50 423 ms with the settle).
        // Holds the target, and what to record if the write cannot be made at all.
        let mut deferred_scale: Option<(session::DisplayTarget, u32)> = None;
        let mut mode_applied = true;
        // What the DPI and the refresh rate actually land on, not always what was asked for: a
        // display takes only a band of DPI steps, and an IDD monitor's mode list is fixed at
        // create time. The session has to record the truth or the next edit compares against a
        // value no display ever held.
        let applied_scale;
        let mut applied_refresh = refresh;
        let (display_id, device_name, device_path) = match existing.clone() {
            Some(prev) => {
                let display_changed = prev.display_params() != desired.display_params();
                let target = prev.target();
                if display_changed && !still_current() {
                    bail!(
                        "join {join_id} was superseded by a newer offer before its mode change; \
                         standing down"
                    );
                }
                // only the DPI differs (never written by an overtaken attempt, or it landed on
                // another step): a mode set would be a whole reconfiguration for nothing
                let scale_only = display_changed && only_scale_differs(&prev, &desired);
                if scale_only {
                    deferred_scale = Some((target.clone(), prev.scale));
                    applied_scale = scale;
                    tprintln!(
                        "virtual display id={} needs only its DPI ({scale}%)",
                        prev.display_id
                    );
                } else if display_changed {
                    let name = prev.device_name.clone();
                    let write = target.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        pipeline::set_display_mode_owned(&write, width, height, refresh, swap_axes)
                    })
                    .await;
                    match res {
                        Ok(Ok(hz)) => applied_refresh = hz,
                        Ok(Err(e)) => {
                            mode_applied = false;
                            teprintln!("could not apply display mode to {name}: {e}");
                        }
                        Err(e) => {
                            mode_applied = false;
                            teprintln!("set-mode task for {name} panicked: {e}");
                        }
                    }
                    // only a mode that landed gets a settle and a DPI write: after a refusal the
                    // settle polls a display that has not moved (4 s of the client's reconnect
                    // budget) and the DPI would be derived from a resolution it is not at
                    if mode_applied {
                        wait_for_display_settle(
                            &target,
                            Some(monitor_dims(width, height, swap_axes)),
                        )
                        .await;
                        deferred_scale = Some((target.clone(), prev.scale));
                        applied_scale = scale;
                        tprintln!(
                            "virtual display id={} settings changed in place via Windows APIs ({width}x{height}@{applied_refresh}, scale={applied_scale}%)",
                            prev.display_id
                        );
                        if applied_refresh != refresh {
                            teprintln!(
                                "virtual display id={} would not run at {refresh} Hz (its mode \
                                 list was fixed when it was created); it is at {applied_refresh} Hz",
                                prev.display_id
                            );
                        }
                    } else {
                        applied_scale = prev.scale;
                        applied_refresh = prev.refresh;
                        teprintln!(
                            "virtual display id={} refused {width}x{height}@{refresh}; keeping the \
                             mode and the DPI it already has",
                            prev.display_id
                        );
                    }
                } else {
                    applied_scale = prev.scale;
                    applied_refresh = prev.refresh;
                    tprintln!(
                        "virtual display id={} untouched (encoder-only edit)",
                        prev.display_id
                    );
                }
                (
                    prev.display_id,
                    prev.device_name.clone(),
                    prev.device_path.clone(),
                )
            }
            None => {
                let extra_modes = dpr_mode_ladder(req.width, req.height);
                let (display_id, device_name, device_path, topology_reset) =
                    create_display_correlated(
                        client.clone(),
                        display_name,
                        (width, height, refresh),
                        extra_modes,
                        DISPLAY_LOCKED_BOUND,
                        pipeline::claim_display_identity,
                    )
                    .await?;
                #[cfg(windows)]
                crate::windows_utils::streamer::capture::note_display_disturbed(&device_path);

                // a backend that negotiates its geometry knows what it landed on, and forcing the
                // mode again fails whenever the size is off the ladder the OS published. `None`
                // means the backend does not know (every commanded backend)
                let mut already_at_size = client.created_geometry(display_id)
                    == Some(monitor_dims(width, height, swap_axes));
                // a display created at exactly this mode, unrotated, needs no forced mode set:
                // the write measured p50 172 ms and up to 3.7 s, and every mode commit costs the
                // driver host memory it never gives back
                if !already_at_size && !swap_axes {
                    let name = device_name.clone();
                    let current = tokio::task::spawn_blocking(move || current_mode_of(&name))
                        .await
                        .ok()
                        .flatten();
                    if current == Some((width, height, refresh)) {
                        tprintln!(
                            "virtual display {device_name} is already at {width}x{height}@{refresh}; \
                             no mode set needed"
                        );
                        already_at_size = true;
                    }
                }
                if already_at_size {
                    tprintln!(
                        "virtual display {device_name} was created at {width}x{height}; skipping the forced mode set"
                    );
                }

                let fresh = session::DisplayTarget {
                    device_path: device_path.clone(),
                    device_name: device_name.clone(),
                };

                if !already_at_size {
                    let write = fresh.clone();
                    let res = tokio::task::spawn_blocking(move || {
                        pipeline::set_display_mode_owned(&write, width, height, refresh, swap_axes)
                    })
                    .await;
                    match res {
                        Ok(Ok(hz)) => {
                            applied_refresh = hz;
                            tprintln!(
                            "virtual display {device_name} set to {width}x{height}@{hz} (portrait={portrait})"
                        );
                        }
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

                if !already_at_size && mode_applied {
                    wait_for_display_settle(&fresh, Some(monitor_dims(width, height, swap_axes)))
                        .await;
                }
                // 0: nothing was ever written to this display, and if the write fails that is
                // what the record says
                deferred_scale = Some((fresh.clone(), 0));
                applied_scale = scale;
                if topology_reset {
                    if let Some(sessions) = state.config.sessions.as_ref() {
                        let others = session::live_displays_except(sessions, &join_id);
                        if !others.is_empty() {
                            tokio::spawn(restore_displays(others));
                        }
                    }
                }
                (display_id, device_name, device_path)
            }
        };
        let target = session::DisplayTarget {
            device_path: device_path.clone(),
            device_name: device_name.clone(),
        };

        if let Some(s) = state.config.sessions.as_ref() {
            session::set_host_ip(s, &join_id, state.config.lan_ip.clone());
            // Until the DPI write lands, the record keeps the scale the display had before it
            // (none, for a display just made): a retry that adopts it first must find the write
            // still to do, never a scale that was not written.
            let recorded_scale =
                scale_to_record(deferred_scale.is_some(), existing.as_ref(), applied_scale);
            let outcome = if mode_applied {
                ModeOutcome::Applied {
                    scale: recorded_scale,
                    refresh: applied_refresh,
                }
            } else {
                let actual = read_back_dimensions(&target, swap_axes)
                    .await
                    .or_else(|| existing.as_ref().map(|p| (p.width, p.height)))
                    .unwrap_or((width, height));
                teprintln!(
                    "virtual display {device_name} stayed at {}x{} instead of {width}x{height}; \
                 will retry on the next reconfigure",
                    actual.0,
                    actual.1
                );
                ModeOutcome::Refused {
                    width: actual.0,
                    height: actual.1,
                    scale: recorded_scale,
                    refresh: applied_refresh,
                }
            };
            session::set_live_display(
                s,
                &join_id,
                landed_display(
                    &desired,
                    display_id,
                    device_name.clone(),
                    device_path.clone(),
                    outcome,
                ),
            );
        }

        // a display made for this join lands wherever Windows puts it: put it back where the user
        // last placed this device, before the capture opens on it
        #[cfg(windows)]
        if existing.is_none() {
            let token = device_token.to_string();
            let path = device_path.clone();
            let moved = tokio::task::spawn_blocking(move || {
                crate::display_arrange::restore_placement(&token, &path)
            })
            .await
            .unwrap_or(false);
            if moved {
                crate::windows_utils::streamer::capture::note_display_disturbed(&device_path);
            }
        }

        let display_ms = t_display.elapsed().as_secs_f64() * 1000.0;

        if !still_current() {
            bail!(
                "join {join_id} was superseded by a newer offer before its capture started; \
                 standing down"
            );
        }

        // `start_on_monitor` is the heaviest blocking call on the join path (D3D11 device, scaler,
        // encoder session, capture start), so it goes to the blocking pool: inline it parks a
        // tokio worker that also owns other clients' RTP writers and `/reconfig`.
        let t_pipeline = std::time::Instant::now();
        // Windows: the DPI write is the last display write of the join, and a monitor handle
        // resolved before it can be stale by the time the capture opens. With this on it lands
        // first and the capture resolves its monitor afterwards.
        #[cfg(windows)]
        let writes_first = crate::windows_utils::streamer::tune::on("capture_after_display_writes");
        #[cfg(not(windows))]
        let writes_first = false;
        let mut landed_first = None;
        if writes_first && deferred_scale.is_some() && !still_current() {
            bail!("join {join_id} was superseded by a newer offer before its scale write; standing down");
        }
        if writes_first {
            if let Some((t, fallback)) = deferred_scale.take() {
                #[cfg(windows)]
                let before = read_effective_scale(&t).await;
                let written = apply_display_scale(&t, scale).await;
                // nothing was written if the scale was already in effect, so nothing to wait for
                #[cfg(windows)]
                if let Some(landed) = written.filter(|_| before != Some(scale)) {
                    wait_for_scale_to_land(&t, landed, before).await;
                }
                landed_first = Some(written.unwrap_or(fallback));
            }
        }
        // Windows, `answer_before_capture`: the capture opens behind the answer, on a thread of its
        // own, once the monitor hands out a capture item; the shell can take seconds to let it.
        #[cfg(windows)]
        let early = answers_early(
            req.pending_picture,
            crate::windows_utils::streamer::tune::on("answer_before_capture"),
        );
        #[cfg(not(windows))]
        let early = false;
        let start = {
            let cfg = cfg.clone();
            let display = target.clone();
            let current = still_current.clone();
            let (native_w, native_h) = monitor_dims(width, height, swap_axes);
            let native = (native_w, native_h, applied_refresh);
            let give_up = end_session_on_capture_failure(
                state.config.sessions.clone(),
                join_id.clone(),
                session_seq,
                Arc::clone(&capture_failure),
            );
            let queued = std::time::Instant::now();
            tokio::task::spawn_blocking(move || {
                let hop = queued.elapsed();
                if hop >= std::time::Duration::from_millis(50) {
                    tprintln!(
                        "capture start for {} waited {} ms for a blocking-pool thread",
                        display.device_name,
                        hop.as_millis()
                    );
                }
                if early {
                    start_capture_deferred(&cfg, &display, native, current, give_up)
                } else {
                    start_capture(&cfg, &display, &current)
                }
            })
        };
        let (started, landed_scale) = match deferred_scale.take() {
            Some((t, fallback)) => {
                let (s, landed) = tokio::join!(start, apply_display_scale(&t, scale));
                (s, Some(landed.unwrap_or(fallback)))
            }
            None => (start.await, landed_first),
        };
        if let (Some(landed), Some(s)) = (landed_scale, state.config.sessions.as_ref()) {
            if let Some(mut d) = session::get_live_display(s, &join_id) {
                // 0 stays 0: no scale has been written, and the next edit must still write one
                d.scale = if landed == 0 {
                    0
                } else {
                    landed.clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE)
                };
                session::set_live_display(s, &join_id, d);
            }
        }
        let session = match started.context("start-capture task")? {
            Ok(s) => s,
            Err(e) => return Err(e.context("starting capture for virtual display")),
        };
        let pipeline_ms = t_pipeline.elapsed().as_secs_f64() * 1000.0;

        // published under the join id so `status --json` and the verbose summary can find this
        // session's counters without going through the session map
        crate::streamer::telemetry::publish(&join_id, Arc::clone(&session.pipeline.telemetry));

        {
            let name = device_name.clone();
            let rect = tokio::task::spawn_blocking(move || {
                let r = pipeline::monitor_rect(&name);
                if let Some((left, top, width, height)) = r {
                    tprintln!("remote-input display {name}: {width}x{height} at ({left},{top})");
                }
            });
            // only a log line, so the join does not wait for it (p50 128 ms when it did)
            drop(rect);
        }

        anyhow::Ok((display_id, device_name, session, display_ms, pipeline_ms))
    };

    // Nothing in the handshake needs the pipeline until the RTP writer is spawned, so the two
    // halves run concurrently: 13-20 ms off a ~1600 ms LAN join, and on a relayed path the
    // 1500 ms host gather overlaps the 1500-2100 ms display half.
    let t_handshake = std::time::Instant::now();
    let (display_res, whep_res) = tokio::join!(
        display_and_pipeline,
        webrtc_session::prepare_whep(
            req.sdp.clone(),
            &join_id,
            device_token,
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
            if let Ok((_, _, session, _, _)) = display_res {
                discard_capture(&join_id, session).await;
            }
            abandon_failed_attempt(
                state,
                &join_id,
                session_seq,
                still_current(),
                existed_before,
            )
            .await;
            return Err(e.context("WHEP handshake"));
        }
    };
    let (display_id, device_name, session, display_ms, pipeline_ms) = match display_res {
        Ok(v) => v,
        Err(e) => {
            pending.abandon().await;
            abandon_failed_attempt(
                state,
                &join_id,
                session_seq,
                still_current(),
                existed_before,
            )
            .await;
            return Err(e);
        }
    };
    // A newer offer for this join arrived while this one was building, and the page has already
    // thrown this one's peer connection away. Finishing it would leave a capture and an encoder
    // nobody watches, and `set_active_capture` below would overwrite the newer attempt's stopper.
    if !still_current() {
        discard_capture(&join_id, session).await;
        pending.abandon().await;
        bail!("join {join_id} was superseded by a newer offer before its answer; standing down");
    }

    let (closed_tx, closed_rx) = oneshot::channel();
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let resource_id = pending.resource_id().to_string();
    let (answer, whep) = match webrtc_session::activate_whep(
        pending,
        &session.pipeline,
        Some(closed_tx),
        webrtc_session::InputParams {
            device: Some(device_name.clone()),
            #[cfg(windows)]
            device_path: state
                .config
                .sessions
                .as_ref()
                .and_then(|s| session::get_live_display(s, &join_id))
                .map(|d| d.device_path)
                .filter(|p| !p.is_empty()),
            client_os: req.os.trim().to_string(),
            live: std::sync::Arc::clone(&live_input),
            control_rx: Some(control_rx),
        },
        audio_params,
    )
    .await
    {
        Ok(pair) => pair,
        Err(e) => {
            discard_capture(&join_id, session).await;
            abandon_failed_attempt(
                state,
                &join_id,
                session_seq,
                still_current(),
                existed_before,
            )
            .await;
            return Err(e.context("WHEP activation"));
        }
    };
    // The same check once more: `activate_whep` awaits, and a newer offer can land inside it.
    // The control channel and the leave signal are registered in the same step as the check, so
    // an attempt overtaken between the two cannot put its own over the newer attempt's.
    let leave = match state.config.sessions.as_ref() {
        Some(s) => match session::register_attempt(s, &join_id, session_seq, control_tx.clone()) {
            Some(leave) => Some(leave),
            None => {
                discard_capture(&join_id, session).await;
                whep.close().await;
                bail!(
                    "join {join_id} was superseded by a newer offer while it activated; \
                     standing down"
                );
            }
        },
        None => None,
    };
    // registered: a capture failure that came first ends the session now
    arm_capture_failure(
        &capture_failure,
        state.config.sessions.clone(),
        &join_id,
        session_seq,
    );

    #[cfg(windows)]
    let first_frame = {
        let backend = session.pipeline.telemetry.first_frame_backend.get();
        if session
            .pipeline
            .telemetry
            .capture_waiting
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            " first_frame=pending (answered before the capture opened)".to_string()
        } else {
            format!(" first_frame={backend}")
        }
    };
    #[cfg(not(windows))]
    let first_frame = "";
    tprintln!(
        "join stages ({locality:?}): display={display_ms:.0}ms pipeline={pipeline_ms:.0}ms handshake={handshake_ms:.0}ms (overlapped){first_frame}"
    );

    if let Some(reporter) = state.config.device_reporter.as_ref() {
        let info = DeviceInfo {
            join_id: join_id.clone(),
            ip: client_ip.to_string(),
            token: device_token.to_string(),
            name: req.device_name.trim().to_string(),
            os: req.os.trim().to_string(),
            screen_size: format!("{}x{}", req.width, req.height),
            refresh_rate: detected_refresh,
            portrait,
            dpr: req.dpr,
        };
        // A renegotiation is the row moving, not a device arriving: reported as a join it raises
        // "a device joined" on every refresh-rate or DPR change. A join the dashboard has not
        // heard of is still reported as one, whichever attempt got it here.
        let first_report = state
            .config
            .sessions
            .as_ref()
            .is_none_or(|s| session::take_join_report(s, &join_id));
        if first_report {
            reporter.report_join(info);
        } else {
            reporter.report_modify(info);
        }
    }

    // The first byte that reaches the client marks the join as connected, after which no
    // `attemptId` may take it over. The writer only counts bytes the bound track accepted, so
    // this is "the client has a picture", not "the answer was generated". A join that delivers
    // nothing inside the window is closed the way a failed connection would be.
    let abandoned_rx = state.config.sessions.as_ref().map(|s| {
        spawn_first_byte_watch(
            Arc::clone(s),
            join_id.clone(),
            session_seq,
            Arc::clone(&session.pipeline.telemetry),
            FIRST_BYTE_WINDOW,
        )
    });
    #[cfg(windows)]
    {
        use crate::windows_utils::streamer::tune;
        let on = RebuildOn {
            closed: tune::on("capture_replaced_rebuild"),
            stopped: tune::candidate("capture_rebuild_watchdog"),
            dark_after: tune::on("capture_late_first_frame").then_some(CAPTURE_DARK_WAIT),
        };
        let any = on.closed || on.stopped || on.dark_after.is_some();
        if let (true, Some(s)) = (any, state.config.sessions.as_ref()) {
            tokio::spawn(watch_capture(
                Arc::clone(s),
                join_id.clone(),
                session_seq,
                Arc::clone(&session.pipeline.telemetry),
                on,
                CAPTURE_WATCH_TICK,
                session::get_live_display(s, &join_id).map_or_else(
                    || session::DisplayTarget {
                        device_path: String::new(),
                        device_name: device_name.clone(),
                    },
                    |d| d.target(),
                ),
            ));
        }
        // the governor is the only thing that moves what this watch reads
        if let (true, Some(s), Some(reporter)) = (
            tune::on("link_governor"),
            state.config.sessions.as_ref(),
            state.config.device_reporter.as_ref(),
        ) {
            tokio::spawn(crate::windows_utils::streamer::slow_link::watch_slow_link(
                Arc::clone(s),
                session_seq,
                Arc::clone(&session.pipeline.telemetry),
                Arc::clone(reporter),
                DeviceInfo {
                    join_id: join_id.clone(),
                    ip: client_ip.to_string(),
                    token: device_token.to_string(),
                    name: req.device_name.trim().to_string(),
                    ..DeviceInfo::default()
                },
                state.config.lan_ip.clone(),
                locality,
            ));
        }
    }
    let own_telemetry = Arc::clone(&session.pipeline.telemetry);
    let session_holder = match state.config.sessions.as_ref() {
        Some(s) => {
            session::set_active_capture(s, &join_id, session_seq, Box::new(move || session.stop()));
            None
        }
        None => Some(session),
    };

    let client = client.clone();
    let reporter = state.config.device_reporter.clone();
    let sessions = state.config.sessions.clone();
    let disconnect_grace = state.config.disconnect_grace.clone();
    let report_join = join_id.clone();
    let report_token = device_token.to_string();
    tokio::spawn(async move {
        let end = wait_session_end(closed_rx, leave.clone(), abandoned_rx).await;
        if let SessionEnd::Abandoned(why) = end {
            tprintln!("join {report_join} closed without a client ({why:?}); no grace");
        }

        if let Some(s) = sessions.as_ref() {
            session::clear_control_sender(s, &report_join, &control_tx);
        }
        // Only this session's own handle: after a renegotiation the join id names the
        // successor's telemetry, and this teardown runs once the successor has connected.
        crate::streamer::telemetry::unregister_if(&report_join, &own_telemetry);

        // gives back the peer connection, its ICE agent and UDP sockets, and the injector thread
        whep.close().await;
        tprintln!(
            "join {report_join} closed; live transport sessions: {}",
            webrtc_session::live_sessions()
        );

        let stop = sessions
            .as_ref()
            .and_then(|s| session::take_active_capture_if(s, &report_join, session_seq));
        if let Some(stop) = stop {
            let _ = tokio::task::spawn_blocking(stop).await;
        } else if let Some(session) = session_holder {
            let _ = tokio::task::spawn_blocking(move || session.stop()).await;
        }

        let grace = std::time::Duration::from_secs(
            disconnect_grace
                .as_ref()
                .map(|g| g.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(session::DEFAULT_DISCONNECT_GRACE_SECS),
        );
        let label = format!("display id={display_id} ({device_name})");
        let left = match end_of_attempt(
            sessions.as_ref(),
            &report_join,
            session_seq,
            end,
            grace,
            &label,
        )
        .await
        {
            Disposal::Keep => return,
            Disposal::Remove { left } => left,
        };
        tprintln!(
            "session for display id={display_id} ({device_name}) ended ({}); removing display",
            if left {
                "page closed"
            } else {
                "disconnected, no rejoin"
            }
        );
        if let Some(s) = sessions.as_ref() {
            forget_display(session::take_live_display(s, &report_join));
            // keyed by join, the map would otherwise gain an entry for every tab ever opened
            session::remove_session(s, &report_join);
        }
        remove_display_async(&client, display_id).await;
        if let Some(reporter) = reporter {
            reporter.report_remove(report_join, report_token);
        }
    });

    Ok((answer, resource_id))
}

/// How long a join waits for its create-and-attach once it holds `DISPLAY_CORRELATION_LOCK`: the
/// driver's own 5 s reply timeout for the create, `DISPLAY_ATTACH_TIMEOUT` for the attach, and the
/// snapshots and the claim (single display queries) with room to spare. The work itself is never
/// cut short; see [`create_display_correlated`].
const DISPLAY_LOCKED_BOUND: std::time::Duration = std::time::Duration::from_secs(20);

/// What the end of an attempt does with its display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposal {
    /// A newer attempt owns the join, and the display with it.
    Keep,
    /// Nobody holds the join any more. `left` when no grace was waited (the page left, or no
    /// client ever reached the join).
    Remove { left: bool },
}

/// The end of an attempt whose connection is gone: wait the disconnect grace unless `end` says
/// nobody is coming back, then keep the display when a newer attempt owns the join, or remove it.
async fn end_of_attempt(
    sessions: Option<&session::SharedSessions>,
    join: &str,
    seq: u64,
    end: SessionEnd,
    grace: std::time::Duration,
    label: &str,
) -> Disposal {
    // a display no client ever saw has nobody to wait a grace for
    let left = end.skips_grace();
    // Already taken over (a renegotiation in place replaced this transport): the display is the
    // newer attempt's, and there is no rejoin to wait for.
    if sessions.is_some_and(|s| !session::is_current_session(s, join, seq)) {
        tprintln!("session for {label} superseded; keeping display");
        return Disposal::Keep;
    }
    if !left {
        tprintln!(
            "session for {label} PC closed; waiting {grace:?} for a rejoin before removing the \
             display"
        );
        tokio::time::sleep(grace).await;
    }
    let superseded = sessions.is_some_and(|s| !session::is_current_session(s, join, seq));
    if superseded {
        tprintln!("session for {label} superseded; keeping display");
        return Disposal::Keep;
    }
    Disposal::Remove { left }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEnd {
    /// The connection failed or closed; `left` when the page said it was leaving.
    Closed { left: bool },
    /// The page's `/leave`.
    Left,
    /// No client ever connected to this attempt: its window closed, or a newer attempt took the
    /// join over first.
    Abandoned(FirstByte),
}

impl SessionEnd {
    /// Only a connection that dropped without the page saying goodbye waits for a rejoin.
    fn skips_grace(self) -> bool {
        !matches!(self, SessionEnd::Closed { left: false })
    }
}

async fn wait_session_end(
    closed_rx: oneshot::Receiver<()>,
    leave: Option<Arc<session::LeaveSignal>>,
    abandoned_rx: Option<oneshot::Receiver<FirstByte>>,
) -> SessionEnd {
    let abandoned = async {
        match abandoned_rx {
            // a sender dropped without a send means the client connected: never resolve
            Some(rx) => match rx.await {
                Ok(why) => why,
                Err(_) => std::future::pending().await,
            },
            None => std::future::pending().await,
        }
    };
    match leave {
        Some(sig) => {
            tokio::select! {
                _ = closed_rx => {
                    tokio::time::sleep(LEAVE_SETTLE).await;
                    SessionEnd::Closed { left: sig.left.load(std::sync::atomic::Ordering::SeqCst) }
                }
                _ = sig.notify.notified() => SessionEnd::Left,
                why = abandoned => SessionEnd::Abandoned(why),
            }
        }
        None => {
            tokio::select! {
                _ = closed_rx => SessionEnd::Closed { left: false },
                why = abandoned => SessionEnd::Abandoned(why),
            }
        }
    }
}

/// Watch an attempt for its first sign of a client, and report it as abandoned when none comes:
/// no connection inside `window`, or a newer attempt (the page's retry, a settings change) took
/// the join over before this one ever connected. Either way its transport, injector thread and
/// audio subscription belong to nobody and are closed at once. Silent when it connected.
fn spawn_first_byte_watch(
    sessions: session::SharedSessions,
    join: String,
    seq: u64,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
    window: std::time::Duration,
) -> oneshot::Receiver<FirstByte> {
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let seen = watch_first_byte(&sessions, &join, seq, &telemetry, window).await;
        if matches!(seen, FirstByte::NeverConnected | FirstByte::Superseded) {
            let _ = tx.send(seen);
        }
    });
    rx
}

/// The before-create snapshot, or a value that makes the claim refuse when the task itself died.
///
/// An empty list would make every attached monitor look new, so a panicked snapshot has to fail
/// the claim the same way a snapshot that could not read the displays does.
fn snapshot_or_refusal(taken: Result<Vec<String>, tokio::task::JoinError>) -> Vec<String> {
    taken.unwrap_or_else(|e| {
        teprintln!("[display] the snapshot before a create failed ({e})");
        snapshot_failed()
    })
}

#[cfg(windows)]
fn snapshot_failed() -> Vec<String> {
    vec![crate::windows_utils::virtual_display::SNAPSHOT_FAILED.to_string()]
}

// no other backend reads the before-set, so an empty list is safe
#[cfg(not(windows))]
fn snapshot_failed() -> Vec<String> {
    Vec::new()
}

#[cfg(windows)]
const CAPTURE_WATCH_TICK: std::time::Duration = std::time::Duration::from_millis(500);
/// At most this many rebuilds of one join's capture for as long as the join lives, and never two
/// closer than [`CAPTURE_REBUILD_FLOOR`]. A per-minute budget would let a capture that dies
/// straight after every rebuild renegotiate forever; a per-session one cannot.
#[cfg(windows)]
const CAPTURE_REBUILD_LIMIT: usize = 3;
#[cfg(windows)]
const CAPTURE_REBUILD_FLOOR: std::time::Duration = std::time::Duration::from_secs(2);
/// A capture answered without a first frame is opened again after this long without one, then
/// twice as long after each rebuild, within the same budget. `capture_late_first_frame`.
#[cfg(windows)]
const CAPTURE_DARK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

#[cfg(windows)]
type RebuildLog = std::sync::Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>;

#[cfg(windows)]
fn capture_rebuild_log() -> &'static RebuildLog {
    static REBUILDS: std::sync::OnceLock<RebuildLog> = std::sync::OnceLock::new();
    REBUILDS.get_or_init(Default::default)
}

#[cfg(windows)]
fn capture_rebuilds_made(join: &str) -> usize {
    capture_rebuild_log()
        .lock_recover()
        .get(join)
        .map_or(0, Vec::len)
}

/// Whether one more rebuild of `join`'s capture fits its budget, recording it if it does. Joins
/// that no longer exist are forgotten here, so the record lives exactly as long as the join.
#[cfg(windows)]
fn capture_rebuild_allowed(
    sessions: &session::SharedSessions,
    join: &str,
    now: std::time::Instant,
) -> bool {
    let live: std::collections::HashSet<String> = sessions.lock_recover().keys().cloned().collect();
    let mut map = capture_rebuild_log().lock_recover();
    map.retain(|j, _| live.contains(j));
    let times = map.entry(join.to_string()).or_default();
    let too_soon = times
        .last()
        .is_some_and(|t| now.saturating_duration_since(*t) < CAPTURE_REBUILD_FLOOR);
    if too_soon || times.len() >= CAPTURE_REBUILD_LIMIT {
        return false;
    }
    times.push(now);
    true
}

/// What makes a session's capture worth building again.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RebuildOn {
    /// The captured monitor went away (the capture item's closed event).
    /// `capture_replaced_rebuild`, ships on.
    closed: bool,
    /// The capture ended for any other reason (the ring gave up, a fatal capture error).
    /// `capture_rebuild_watchdog`, a candidate.
    stopped: bool,
    /// The capture was answered without a first frame and is still dark this long after (doubled
    /// per rebuild already made), while the desktop is not withheld. `capture_late_first_frame`,
    /// ships on; `None` is off.
    dark_after: Option<std::time::Duration>,
}

/// Why a watcher asked for a rebuild.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureEnd {
    Closed,
    Stopped,
    Dark,
}

/// Watch one attempt's capture and, on the first sign it can no longer show the display (see
/// [`RebuildOn`]), renegotiate the join in place, as a settings change would, so a new capture
/// opens on the display as it is now. One request per watcher; it stands down as soon as a
/// newer attempt owns the join, and the bump itself is made only while this attempt still does.
#[cfg(windows)]
async fn watch_capture(
    sessions: session::SharedSessions,
    join: String,
    seq: u64,
    telemetry: crate::streamer::telemetry::SharedTelemetry,
    on: RebuildOn,
    tick: std::time::Duration,
    display: session::DisplayTarget,
) -> Option<CaptureEnd> {
    use std::sync::atomic::Ordering::Relaxed;
    let d = &telemetry.degradation;
    // Only what happens to this attempt's capture counts: an opening the capture start already
    // gave up on and replaced (the first-frame reopen) left its own events behind.
    let (lost0, stopped0) = (
        d.capture_lost.load(Relaxed),
        d.capture_stopped.load(Relaxed),
    );
    let dark_after = on
        .dark_after
        .map(|base| base.saturating_mul(1u32 << capture_rebuilds_made(&join).min(4)));
    let mut dark_since = dark_after
        .filter(|_| telemetry.capture_frames_arrived.load(Relaxed) == 0)
        .map(|_| tokio::time::Instant::now());
    loop {
        tokio::time::sleep(tick).await;
        if !session::is_current_session(&sessions, &join, seq) {
            return None;
        }
        let end = if on.closed && d.capture_lost.load(Relaxed) > lost0 {
            CaptureEnd::Closed
        } else if on.stopped && d.capture_stopped.load(Relaxed) > stopped0 {
            CaptureEnd::Stopped
        } else if let (Some(since), Some(wait)) = (dark_since, dark_after) {
            if telemetry.capture_frames_arrived.load(Relaxed) > 0 {
                dark_since = None;
                continue;
            }
            // Still waiting for its monitor (answered before the capture opened): not dark yet,
            // and that wait has a budget of its own. The clock starts once it opens.
            if telemetry.capture_waiting.load(Relaxed) {
                dark_since = Some(tokio::time::Instant::now());
                continue;
            }
            if since.elapsed() < wait {
                continue;
            }
            // Locked, or on the secure desktop: no capture can deliver, so the wait starts over
            // instead of rebuilding. A dark DXGI capture of this session holds the output, so a
            // second duplication cannot tell; it is rebuilt (from WGC).
            let ours_dxgi = telemetry.capture_backend.get() == "dxgi";
            let target = display.clone();
            let withheld = !ours_dxgi
                && tokio::task::spawn_blocking(move || {
                    crate::windows_utils::streamer::pipeline::desktop_withheld_for(&target)
                })
                .await
                .unwrap_or(false);
            if withheld {
                dark_since = Some(tokio::time::Instant::now());
                continue;
            }
            CaptureEnd::Dark
        } else {
            continue;
        };
        if !capture_rebuild_allowed(&sessions, &join, std::time::Instant::now()) {
            teprintln!(
                "join {join}: its capture needs opening again ({end:?}) but it has had \
                 {CAPTURE_REBUILD_LIMIT} rebuilds; the picture stays until the client reconnects"
            );
            return None;
        }
        if !session::bump_reconfig_epoch_if_current(&sessions, &join, seq) {
            return None;
        }
        if end == CaptureEnd::Dark {
            d.capture_dark_rebuilds.fetch_add(1, Relaxed);
        }
        tprintln!("join {join}: capture {end:?}; renegotiating in place to open it again");
        return Some(end);
    }
}

/// How long a join may go without delivering a byte to its client before it is closed.
///
/// Twice webrtc-rs's own ICE failure window (5 s disconnected plus 25 s failed), so it never
/// races a connection ICE could still complete, and six times the page's 10 s wait for an
/// answer, so a page still retrying has long since taken the join over when it fires. A join
/// whose offer carried no candidates and whose page went away never reaches ICE failure at
/// all; this is what closes it.
const FIRST_BYTE_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
const FIRST_BYTE_TICK: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstByte {
    /// A byte reached the client; the join is marked connected.
    Connected,
    /// A newer attempt owns the join now, or it is gone.
    Superseded,
    /// Nothing inside the window, but the join had connected before (a renegotiation of a
    /// watched session). Left to the transport.
    Quiet,
    /// Nothing inside the window and nothing ever: no page holds this join.
    NeverConnected,
}

async fn watch_first_byte(
    sessions: &session::SharedSessions,
    join: &str,
    seq: u64,
    telemetry: &crate::streamer::telemetry::SessionTelemetry,
    window: std::time::Duration,
) -> FirstByte {
    use std::sync::atomic::Ordering::Relaxed;
    let ticks = window.as_millis() / FIRST_BYTE_TICK.as_millis();
    for _ in 0..ticks {
        tokio::time::sleep(FIRST_BYTE_TICK).await;
        if !session::is_current_session(sessions, join, seq) {
            return FirstByte::Superseded;
        }
        // Judged by the connection: a client whose transport is up is there, whether or not a
        // picture has reached it yet (a capture that has not delivered is the capture's problem).
        if telemetry.transport_connected.load(Relaxed) || telemetry.bytes_sent.load(Relaxed) > 0 {
            session::mark_session_connected(sessions, join);
            return FirstByte::Connected;
        }
    }
    let ever = sessions.lock_recover().get(join).map(|s| s.ever_connected);
    match ever {
        Some(false) if session::is_current_session(sessions, join, seq) => {
            FirstByte::NeverConnected
        }
        Some(true) => FirstByte::Quiet,
        _ => FirstByte::Superseded,
    }
}

/// How long a join waits for the window manager to apply a scale the display configuration has
/// already taken. Measured: the system's announcement of it came 0.1 to 0.75 s after a new
/// monitor stopped refusing a capture item, 2.9 to 6.6 s after the attach.
#[cfg(windows)]
const SCALE_LAND_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// Hold the capture start until the scale this join wrote is the one the window manager applies
/// to the monitor, not only the one the display configuration reports. Bounded; a scale that has
/// not landed by then is logged and the capture's own readiness poll takes over.
#[cfg(windows)]
async fn wait_for_scale_to_land(
    target: &session::DisplayTarget,
    percent: u32,
    before: Option<u32>,
) {
    if !crate::windows_utils::streamer::tune::on("capture_after_scale_lands") {
        return;
    }
    let t = target.clone();
    let waited = tokio::task::spawn_blocking(move || {
        crate::windows_utils::streamer::capture::wait_for_effective_scale(
            &t,
            percent,
            SCALE_LAND_BOUND,
        )
    })
    .await;
    // one line per join: whether this wait is a no-op or a cost shows only on a live system
    match waited {
        Ok(seen) => match seen.landed_after {
            Some(took) => tprintln!(
                "virtual display {}: scale {percent}% written; in effect {before:?} before the \
                 write, {:?} at the first read after it, {percent}% after {} ms",
                target.device_name,
                seen.first,
                took.as_millis()
            ),
            None => teprintln!(
                "virtual display {}: scale {percent}% written; in effect {before:?} before the \
                 write, {:?} at the first read after it, still {:?} after {} s; starting the \
                 capture anyway",
                target.device_name,
                seen.first,
                seen.last,
                SCALE_LAND_BOUND.as_secs()
            ),
        },
        Err(e) => teprintln!(
            "scale-landing wait for {} panicked: {e}",
            target.device_name
        ),
    }
}

/// The scale the window manager applies to `target`'s monitor now, for the log.
#[cfg(windows)]
async fn read_effective_scale(target: &session::DisplayTarget) -> Option<u32> {
    let t = target.clone();
    tokio::task::spawn_blocking(move || {
        crate::windows_utils::streamer::capture::effective_scale_percent(&t)
    })
    .await
    .ok()
    .flatten()
}

/// Start capturing a display this join holds.
///
/// Windows finds the monitor by its device path, re-resolved immediately before the capture
/// opens and on every stale-handle retry, so a `\\.\DISPLAYn` slot that moved to another
/// monitor cannot hand this client somebody else's pixels.
///
/// `still_current` is asked at every turn of the Windows start's waits and retries, so an attempt
/// a newer offer has overtaken stops there instead of opening a second capture and encoder on the
/// same display.
#[cfg(windows)]
fn start_capture(
    cfg: &Config,
    display: &session::DisplayTarget,
    still_current: &dyn Fn() -> bool,
) -> Result<pipeline::SessionCapture> {
    pipeline::start_on_display_while(cfg, display, still_current)
}

#[cfg(not(windows))]
fn start_capture(
    cfg: &Config,
    display: &session::DisplayTarget,
    _still_current: &dyn Fn() -> bool,
) -> Result<pipeline::SessionCapture> {
    pipeline::start_on_monitor(cfg, &display.device_name)
}

/// [`start_capture`] answered before the capture opens (Windows, `answer_before_capture`).
#[cfg(windows)]
fn start_capture_deferred(
    cfg: &Config,
    display: &session::DisplayTarget,
    requested: (u32, u32, u32),
    still_current: impl Fn() -> bool + Send + Sync + 'static,
    give_up: Box<dyn FnOnce(String) + Send>,
) -> Result<pipeline::SessionCapture> {
    // frame duration and bitrate cap follow the mode the display is actually at, which a refused
    // mode set or a refresh rate it would not take can make differ from the one asked for;
    // read by device path, since a cached `\\.\DISPLAYn` can name another monitor by now
    let native = crate::windows_utils::virtual_display::resolve_owned_display(display)
        .ok()
        .and_then(|r| crate::windows_utils::virtual_display::current_display_mode(&r.gdi_name))
        .filter(|&(w, h, hz)| w > 0 && h > 0 && hz > 0)
        .unwrap_or(requested);
    pipeline::start_on_display_deferred(cfg, display, native, Arc::new(still_current), give_up)
}

#[cfg(not(windows))]
fn start_capture_deferred(
    cfg: &Config,
    display: &session::DisplayTarget,
    _native: (u32, u32, u32),
    still_current: impl Fn() -> bool + Send + Sync + 'static,
    _give_up: Box<dyn FnOnce(String) + Send>,
) -> Result<pipeline::SessionCapture> {
    start_capture(cfg, display, &still_current)
}

/// Whether a join is answered before its capture opens: the page must say it holds its
/// no-picture timers while the picture is pending, and the switch must be on.
#[cfg_attr(not(windows), allow(dead_code))]
fn answers_early(page_understands_pending: bool, switch_on: bool) -> bool {
    page_understands_pending && switch_on
}

/// A capture failure that arrived before its attempt registered its leave signal.
#[derive(Default)]
struct CaptureFailureState {
    armed: bool,
    pending: Option<String>,
}

type CaptureFailure = Arc<std::sync::Mutex<CaptureFailureState>>;

/// How a capture that opens behind the answer ends its session when it cannot open: as a page
/// leaving would (the one teardown task runs, no grace, display removed), and only while this
/// attempt still owns the join (a newer one has its own capture). Before the attempt registers
/// its leave signal (`armed`) the failure is kept for [`arm_capture_failure`]: a signal sent then
/// would reach nobody, or the previous attempt's teardown.
fn end_session_on_capture_failure(
    sessions: Option<session::SharedSessions>,
    join: String,
    seq: u64,
    failure: CaptureFailure,
) -> Box<dyn FnOnce(String) + Send> {
    Box::new(move |why| {
        let now = {
            let mut f = failure.lock_recover();
            if !f.armed {
                f.pending = Some(why.clone());
            }
            f.armed
        };
        if now {
            end_session_for_capture(sessions.as_ref(), &join, seq, &why);
        }
    })
}

/// The attempt has registered its leave signal: from here a capture failure ends the session at
/// once, and one that came before does so now.
fn arm_capture_failure(
    failure: &CaptureFailure,
    sessions: Option<session::SharedSessions>,
    join: &str,
    seq: u64,
) {
    let pending = {
        let mut f = failure.lock_recover();
        f.armed = true;
        f.pending.take()
    };
    if let Some(why) = pending {
        end_session_for_capture(sessions.as_ref(), join, seq, &why);
    }
}

fn end_session_for_capture(
    sessions: Option<&session::SharedSessions>,
    join: &str,
    seq: u64,
    why: &str,
) {
    let Some(s) = sessions else {
        return;
    };
    if session::is_current_session(s, join, seq) {
        teprintln!("join {join}: {why}; ending the session");
        session::signal_leave(s, join);
    }
}

/// Stop the capture an attempt built but will not use, and withdraw the telemetry it published.
///
/// `SessionCapture::stop` joins the capture thread: milliseconds normally, seconds when the
/// capture is wedged (WGC on a stale monitor handle). Inline it parks a tokio worker, and a task
/// in that worker's LIFO slot cannot be stolen by the idle ones, so it goes to the blocking
/// pool. Awaited, so the display is only removed after the capture on it has stopped.
async fn discard_capture(join_id: &str, session: pipeline::SessionCapture) {
    crate::streamer::telemetry::unregister_if(join_id, &session.pipeline.telemetry);
    let _ = tokio::task::spawn_blocking(move || session.stop()).await;
}

/// Forget a session whose join attempt failed, unless a newer attempt has taken it over: two
/// saves in quick succession are two attempts on one join, and the loser's failure must not
/// delete the entry the winner is about to record its display in.
///
/// It must never forget a session that still has a display attached. The display id lives only
/// in the session, so removing the entry orphans the display for the life of the host, and the
/// page's retry on the same join id gets a second display and a second dashboard row.
///
/// - A fresh join that failed removes the display it created (if it got that far) with the
///   session.
/// - A renegotiation that failed keeps both, because the page answers a failed rebuild by
///   offering again on the same join id. It is reaped the way a closed session would be: after
///   the disconnect grace, unless a newer attempt has taken the join over by then.
async fn abandon_failed_attempt(
    state: &AppState,
    join_id: &str,
    seq: u64,
    mine: bool,
    continuing: bool,
) {
    let Some(s) = state.config.sessions.as_ref() else {
        return;
    };
    if !mine {
        tprintln!("join {join_id} failed but a newer offer owns it now; leaving the session alone");
        return;
    }
    let Some(client) = state.config.virtual_display.as_ref() else {
        session::remove_session(s, join_id);
        return;
    };
    if !continuing {
        let taken = session::take_live_display(s, join_id);
        session::remove_session(s, join_id);
        if let Some(d) = taken {
            let id = d.display_id;
            forget_display(Some(d));
            remove_display_async(client, id).await;
        }
        return;
    }
    let grace = std::time::Duration::from_secs(
        state
            .config
            .disconnect_grace
            .as_ref()
            .map(|g| g.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(session::DEFAULT_DISCONNECT_GRACE_SECS),
    );
    tprintln!(
        "join {join_id} failed to renegotiate; keeping its display {grace:?} for the page to \
         offer again"
    );
    let sessions = s.clone();
    let client = client.clone();
    let reporter = state.config.device_reporter.clone();
    let join = join_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(LEAVE_SETTLE + grace).await;
        if !session::is_current_session(&sessions, &join, seq) {
            return;
        }
        let token = session::device_token_for(&sessions, &join).unwrap_or_default();
        let taken = session::take_live_display(&sessions, &join);
        session::remove_session(&sessions, &join);
        if let Some(d) = taken {
            let id = d.display_id;
            tprintln!(
                "join {join} never recovered from a failed renegotiation; removing display id={id}"
            );
            forget_display(Some(d));
            remove_display_async(&client, id).await;
        }
        if let Some(reporter) = reporter {
            reporter.report_remove(join, token);
        }
    });
}

/// What a mode change actually did, as opposed to what it was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeOutcome {
    /// The display took the mode. `scale` is the DPI it settled at and `refresh` the rate it is
    /// running at, neither of which is always the one requested: a display reports the band of
    /// DPI steps it will take, and an IDD monitor's mode list is fixed when it is created.
    Applied { scale: u32, refresh: u32 },
    /// The display refused it and is still where it was. `width`/`height` are what it reports
    /// now, `scale` the DPI it still holds and `refresh` the rate it kept: no DPI and no rate
    /// are written when the mode is refused.
    Refused {
        width: u32,
        height: u32,
        scale: u32,
        refresh: u32,
    },
}

/// What the session records after a mode change: what the display reached, never what was asked
/// for. A refused mode leaves the display and its DPI where they were, and recording the request
/// would make the next edit compare equal, so the change could never be retried.
fn landed_display(
    desired: &session::LiveDisplay,
    display_id: u32,
    device_name: String,
    device_path: String,
    outcome: ModeOutcome,
) -> session::LiveDisplay {
    let base = session::LiveDisplay {
        display_id,
        device_name,
        device_path,
        ..desired.clone()
    };
    match outcome {
        ModeOutcome::Applied { scale, refresh } => session::LiveDisplay {
            scale,
            refresh,
            ..base
        },
        ModeOutcome::Refused {
            width,
            height,
            scale,
            refresh,
        } => session::LiveDisplay {
            width,
            height,
            scale,
            refresh,
            ..base
        },
    }
}

/// Stop treating a display as one of ours, so no later write can reach it. Every mode and DPI
/// write is checked against the ownership set, and the GDI name a removed display held belongs
/// to whatever attaches next.
fn forget_display(taken: Option<session::LiveDisplay>) {
    if let Some(d) = taken {
        if !d.device_path.is_empty() {
            pipeline::forget_display_identity(d.display_id, &d.device_path);
        }
    }
}

fn only_scale_differs(prev: &session::LiveDisplay, desired: &session::LiveDisplay) -> bool {
    prev.scale != desired.scale
        && (prev.width, prev.height, prev.refresh, prev.portrait)
            == (
                desired.width,
                desired.height,
                desired.refresh,
                desired.portrait,
            )
}

/// The scale to record for a display before its DPI write has landed: what it had before (0 for a
/// display just made, which has had none written), so whoever adopts it next still finds the
/// write to do. With no write pending, what was applied.
fn scale_to_record(
    write_pending: bool,
    existing: Option<&session::LiveDisplay>,
    applied: u32,
) -> u32 {
    if write_pending {
        existing.map_or(0, |p| p.scale)
    } else {
        applied
    }
}

/// What a correlated create made: the driver's id, the display's device name and path, and
/// whether attaching it reset the topology.
type Correlated = (u32, String, String, bool);

type CorrelationGuard = tokio::sync::MutexGuard<'static, ()>;

/// Create a display and find it, holding the correlation lock for exactly as long as that takes.
///
/// The work is its own task and owns the lock. The join waits at most `bound` from when the lock
/// was taken, so time queued behind other joins' creates does not count. Giving up does not cut
/// the work short: the lock is never released with a create in flight, so a display that attaches
/// late cannot be taken for the next join's own, and it is removed when it does attach.
async fn create_display_correlated(
    client: session::SharedVirtualDisplay,
    display_name: String,
    mode: (u32, u32, u32),
    extra_modes: Vec<(u32, u32)>,
    bound: std::time::Duration,
    identify: fn(&str, &[String]) -> Option<String>,
) -> Result<Correlated> {
    let t_lock = std::time::Instant::now();
    let guard = DISPLAY_CORRELATION_LOCK.lock().await;
    let lock_wait_ms = t_lock.elapsed().as_secs_f64() * 1000.0;
    if lock_wait_ms > 1.0 {
        tprintln!("display correlation lock: waited {lock_wait_ms:.0}ms behind another join");
    }
    let (tx, mut rx) = oneshot::channel::<Result<Correlated>>();
    let worker = client.clone();
    tokio::spawn(async move {
        let (made, guard) =
            create_and_identify(&worker, display_name, mode, extra_modes, identify, guard).await;
        if let Err(Ok(late)) = tx.send(made) {
            teprintln!(
                "virtual display id={} finished after its join gave up; removing it",
                late.0
            );
            give_back_late_display(&worker, late, guard).await;
        }
    });
    match tokio::time::timeout(bound, &mut rx).await {
        Ok(Ok(made)) => made,
        Ok(Err(_)) => bail!("the create-display task ended without an answer"),
        Err(_) => {
            // Closed first, so a result can no longer land where nobody reads it: either it is
            // here now, or the task finds the channel closed and removes the display itself.
            rx.close();
            if let Ok(Ok(late)) = rx.try_recv() {
                tokio::spawn(async move { give_back_late_display(&client, late, None).await });
            }
            bail!(
                "creating and attaching the display took longer than {} s; the join gives up, \
                 and the display is removed when its create finishes",
                bound.as_secs()
            )
        }
    }
}

/// Remove a display its join no longer wants, under the correlation lock (the one the create
/// still holds, or a fresh hold).
async fn give_back_late_display(
    client: &session::SharedVirtualDisplay,
    (id, _, device_path, _): Correlated,
    guard: Option<CorrelationGuard>,
) {
    match guard {
        Some(_held) => remove_display_now(client, id).await,
        None => remove_display_async(client, id).await,
    }
    if !device_path.is_empty() {
        pipeline::forget_display_identity(id, &device_path);
    }
}

/// The create and its correlation, under `guard`. Gives the guard back unless the backend named
/// its display, in which case no correlation was needed and the lock went early.
async fn create_and_identify(
    client: &session::SharedVirtualDisplay,
    display_name: String,
    (width, height, refresh): (u32, u32, u32),
    extra_modes: Vec<(u32, u32)>,
    identify: fn(&str, &[String]) -> Option<String>,
    guard: CorrelationGuard,
) -> (Result<Correlated>, Option<CorrelationGuard>) {
    let mut guard = Some(guard);
    let made = async {
            let before = tokio::task::spawn_blocking(pipeline::monitor_device_names)
                .await
                .unwrap_or_default();
            // identities attached before the create: a correlation that hands back a display
            // that was already there must be refused, or every later ownership check would
            // agree with the mistake
            let paths_before = snapshot_or_refusal(
                tokio::task::spawn_blocking(pipeline::attached_display_paths).await,
            );

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

            // a backend that can name its own display needs no correlation: the lock goes now,
            // so the next join's create starts while this one waits for the monitor to attach
            let named = {
                let client = client.clone();
                tokio::task::spawn_blocking(move || client.display_device_name(display_id))
                    .await
                    .ok()
                    .flatten()
            };
            if named.is_some() {
                guard.take();
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
                    // Read back while the correlation lock still keeps another join from
                    // creating a display between the two reads. Every later write resolves
                    // the display's name from this identity; without one it would be
                    // commanded by name, and a name is exactly what moves.
                    let ident = {
                        let n = name.clone();
                        tokio::task::spawn_blocking(move || {
                            identify(&n, &paths_before)
                        })
                        .await
                        .ok()
                        .flatten()
                    };
                    let Some(device_path) = ident else {
                        remove_display_now(client, display_id).await;
                        bail!(
                            "virtual display {display_id} attached as {name}, which was \
                             already there before the create; refusing to drive it"
                        );
                    };
                    tprintln!(
                        "virtual display id={display_id} attached as {name} ({device_path})"
                    );
                    anyhow::Ok((display_id, name, device_path, topology_reset))
                }
                None => {
                    // still inside the correlation lock, so this is the unlocked form
                    remove_display_now(client, display_id).await;
                    bail!("virtual display {display_id} did not attach within timeout");
                }
            }

    }
    .await;
    (made, guard)
}

/// Remove a display, holding the correlation lock while it happens.
///
/// A create identifies its new display by diffing the OS's device-name list before and after.
/// Windows reuses a name the moment one is freed, so a removal that overlaps that window can
/// hand the same name to the new display: the diff finds nothing new and the join fails after
/// `DISPLAY_ATTACH_TIMEOUT`, with a display created and orphaned. A client that rejoins
/// releases one display while asking for another, so the two overlap by construction.
async fn remove_display_async(client: &session::SharedVirtualDisplay, id: u32) {
    let _guard = DISPLAY_CORRELATION_LOCK.lock().await;
    remove_display_now(client, id).await;
}

/// The same, for a caller that is already inside the correlation lock.
async fn remove_display_now(client: &session::SharedVirtualDisplay, id: u32) {
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

/// The mode a display reports right now, as `(width, height, refresh)`.
#[cfg(target_os = "windows")]
fn current_mode_of(gdi_name: &str) -> Option<(u32, u32, u32)> {
    crate::windows_utils::virtual_display::current_display_mode(gdi_name)
}

#[cfg(not(target_os = "windows"))]
fn current_mode_of(_gdi_name: &str) -> Option<(u32, u32, u32)> {
    None
}

async fn read_back_dimensions(
    target: &session::DisplayTarget,
    swap_axes: bool,
) -> Option<(u32, u32)> {
    let t = target.clone();
    let (w, h) = tokio::task::spawn_blocking(move || pipeline::monitor_dimensions_owned(&t))
        .await
        .ok()
        .flatten()?;
    Some(if swap_axes { (h, w) } else { (w, h) })
}

async fn restore_displays(displays: Vec<session::LiveDisplay>) {
    for display in displays {
        let target = display.target();
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
        // a mode it still has is not written again
        let current = {
            let n = name.clone();
            tokio::task::spawn_blocking(move || current_mode_of(&n))
                .await
                .ok()
                .flatten()
        };
        if swap || current != Some((w, h, hz)) {
            let write = target.clone();
            let res = tokio::task::spawn_blocking(move || {
                pipeline::set_display_mode_owned(&write, w, h, hz, swap)
            })
            .await;
            if let Ok(Err(e)) = res {
                teprintln!("could not restore {name} to {w}x{h}: {e}");
                continue;
            }
            wait_for_display_settle(&target, Some(monitor_dims(w, h, swap))).await;
        }
        // 0: a join still waiting for its own scale write. That write is coming; writing the
        // lowest step here would race it.
        if display.scale != 0 {
            apply_display_scale(&target, display.scale).await;
        }
    }
}

/// Apply a display-scale change to a live display without making the client renegotiate. The
/// scale changes the extended desktop's DPI, not the captured geometry, so the stream the client
/// is already decoding is the stream it wants. Returns whether it was applied, so the caller can
/// fall back to the epoch bump.
pub async fn apply_scale_live(
    sessions: &session::SharedSessions,
    join: &str,
    effective: u32,
) -> bool {
    let Some(display) = session::get_live_display(sessions, join) else {
        return false;
    };
    let Some(applied) = apply_display_scale(&display.target(), effective).await else {
        return false;
    };
    let mut updated = display.clone();
    updated.scale = applied.clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);
    session::set_live_display(sessions, join, updated);
    tprintln!("display scale for join {join} applied live at {applied}% (no rejoin)");
    true
}

/// The same, for every display one device is driving. A per-device scale change lands on all of
/// them.
pub async fn apply_scale_live_for_device(
    sessions: &session::SharedSessions,
    token: &str,
    effective: u32,
) -> usize {
    let mut applied = 0usize;
    for join in session::joins_for_token(sessions, token) {
        if apply_scale_live(sessions, &join, effective).await {
            applied += 1;
        }
    }
    applied
}

/// Apply a display's DPI, and report the scale it is actually at afterwards.
///
/// `None` means nothing was written (the target could not be identified as one of ours), which
/// the caller must not read as applied. A `Some` that is not the requested percentage is a
/// display that will not go that far, and the session records what it reached so the next edit
/// compares against the truth.
async fn apply_display_scale(target: &session::DisplayTarget, percent: u32) -> Option<u32> {
    let t = target.clone();
    let name = target.device_name.clone();
    let scale = percent.clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);
    let res = tokio::task::spawn_blocking(move || {
        #[cfg(windows)]
        let in_effect = || crate::windows_utils::streamer::capture::effective_scale_percent(&t);
        #[cfg(not(windows))]
        let in_effect = || None;
        let (written, wrote) = scale_unless_in_effect(scale, in_effect, || {
            pipeline::set_display_scale_owned(&t, scale)
        });
        if !wrote {
            tprintln!("virtual display {name}: {scale}% already in effect; no scale write");
        }
        match written {
            Ok(applied) => Some(applied),
            Err(e) => {
                teprintln!("could not set scale for {name}: {e}");
                None
            }
        }
    })
    .await;
    match res {
        Ok(applied) => applied,
        Err(e) => {
            teprintln!(
                "apply-display-scale task for {} panicked: {e}",
                target.device_name
            );
            None
        }
    }
}

/// Write `wanted` only when the monitor is not already at it. A display write that changes
/// nothing still has Windows process the monitor again, and a monitor refuses capture items for
/// seconds after that. Read back and found equal is as known as written. The second value is
/// whether a write was made.
fn scale_unless_in_effect(
    wanted: u32,
    in_effect: impl FnOnce() -> Option<u32>,
    write: impl FnOnce() -> Result<u32>,
) -> (Result<u32>, bool) {
    if in_effect() == Some(wanted) {
        return (Ok(wanted), false);
    }
    (write(), true)
}

/// Wait for a display's reported geometry to stop moving after a mode change.
///
/// `expected` is the requested geometry, in monitor coordinates. With it, a mode that landed
/// immediately is confirmed by a single read instead of two reads 75 ms apart.
async fn wait_for_display_settle(target: &session::DisplayTarget, expected: Option<(u32, u32)>) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut last: Option<(u32, u32)> = None;
    loop {
        let t = target.clone();
        let dims = tokio::task::spawn_blocking(move || pipeline::monitor_dimensions_owned(&t))
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
        // `EnumDisplayDevices` blocks and this loop can run 50 times over the 5 s attach timeout,
        // so it stays off the tokio workers, which also drive other clients' RTP
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
                "TURN relay configured ({url}): it MUST be local/regional to preserve latency"
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
    tprintln!("server listening: HTTP :{http_port}, HTTPS :{https_port}");
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

/// Apply a device row's stream-quality fields to the join's `cfg`: video scale, the quality slider
/// (`video_quality` as constant QP, or as a QP ceiling under adaptive bitrate), the frame-rate cap
/// and whether it is a ceiling or a promise.
fn apply_override_quality(cfg: &mut Config, o: &session::DeviceOverride, max_fps: u32) {
    cfg.scale = ScalePercent::new(o.video_scale);
    // The slider is constant QP unless adaptive quality is on: as a QP floor under CBR (~12 Mbps
    // at 1080p60) text goes soft, with a chroma fringe on white glyphs. Constant QP has no bitrate
    // to adapt: `set_bitrate` reports `IgnoredConstantQp` once and the loss controller stands down.
    if o.adaptive_quality {
        cfg.qp = None;
        cfg.quality_qp = Some(o.video_quality);
    } else {
        cfg.qp = Some(o.video_quality);
        cfg.quality_qp = None;
    }
    cfg.max_fps = max_fps;
    // Whether `max_fps` is a ceiling or a promise. Off (the default) makes it a promise: the
    // pipeline holds the cadence and repeats an unchanged frame rather than letting the rate sag.
    // Copied here once, so no frame path has to reach back into the override map.
    cfg.auto_frame_rate = o.auto_frame_rate;
}

/// The join's quality configuration. A saved row applies as it is. A device the host holds no
/// row for (every device's first join) runs `DeviceOverride::join_defaults()`, the row the join
/// seeds and the settings sheet shows, so a first and a second join of an untouched device run
/// the same thing. Unchanged when there is no override map at all (the probe harness, whose
/// `live_config` passes none) or the base config carries an explicit `qp`.
fn apply_join_quality(
    cfg: &mut Config,
    device_settings: Option<&session::DeviceOverride>,
    has_overrides: bool,
    detected_refresh: u32,
    join_seeded: bool,
) {
    match device_settings {
        Some(o) => {
            // a rate the join path recorded is what the device reported then, not a choice anyone
            // made; the device's report now wins over it (see `session::mark_join_seeded`)
            let max_fps = if join_seeded {
                detected_refresh
            } else {
                o.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
            };
            apply_override_quality(cfg, o, max_fps);
        }
        None if has_overrides && cfg.qp.is_none() => {
            apply_override_quality(
                cfg,
                &session::DeviceOverride::join_defaults(),
                detected_refresh,
            );
        }
        None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_first_join_runs_what_the_row_it_seeds_says() {
        let quality = |c: &Config| (c.qp, c.quality_qp, c.scale, c.max_fps, c.auto_frame_rate);
        // first join: no row yet
        let mut first = Config::default();
        apply_join_quality(&mut first, None, true, 120, false);
        // second join of the untouched device: the row the first join seeded (join_defaults with
        // the detected rate), marked seeded
        let seeded = session::DeviceOverride {
            refresh_rate: 120,
            ..session::DeviceOverride::join_defaults()
        };
        let mut second = Config::default();
        apply_join_quality(&mut second, Some(&seeded), true, 120, true);
        assert_eq!(quality(&first), quality(&second));
        assert_eq!(first.qp, Some(15), "constant QP 15, not the adaptive tier");
        assert_eq!(first.quality_qp, None);
        assert_eq!(first.max_fps, 120);
        // no override map (the probe harness): untouched, its --scale included
        let mut probe = Config {
            scale: crate::streamer::config::ScalePercent::new(50),
            ..Config::default()
        };
        apply_join_quality(&mut probe, None, false, 120, false);
        assert_eq!(probe.qp, None);
        assert_eq!(probe.scale, crate::streamer::config::ScalePercent::new(50));
        // an explicit base qp (probe --qp): untouched
        let mut probe_qp = Config {
            qp: Some(22),
            ..Config::default()
        };
        apply_join_quality(&mut probe_qp, None, true, 120, false);
        assert_eq!(probe_qp.qp, Some(22));
        // a saved adaptive row still runs adaptive with the slider as the QP ceiling
        let adaptive = session::DeviceOverride {
            adaptive_quality: true,
            video_quality: 20,
            ..session::DeviceOverride::join_defaults()
        };
        let mut a = Config::default();
        apply_join_quality(&mut a, Some(&adaptive), true, 60, false);
        assert_eq!((a.qp, a.quality_qp), (None, Some(20)));
    }

    fn state_with_local_ips(ips: &[&str]) -> AppState {
        let local_ips = session::new_shared_local_ips();
        *local_ips.lock_recover() = ips.iter().map(|s| s.parse().unwrap()).collect();
        AppState::new(Config {
            local_ips: Some(local_ips),
            stun_urls: vec!["stun:stun.l.google.com:19302".to_string()],
            ..Config::default()
        })
    }

    /// The scripts the isolated page loads as workers must carry their own COEP header, on the
    /// 200 and on a revalidation 304 alike, or Chrome refuses to start the worker.
    #[test]
    fn worker_scripts_carry_coep_on_200_and_304() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        type Handler =
            fn(HeaderMap) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response>>>;
        let handlers: [(&str, Handler); 3] = [
            ("audio-worker.js", |h| Box::pin(audio_worker_js(h))),
            ("transform-worker.js", |h| Box::pin(transform_worker(h))),
            ("audio-worklet.js", |h| Box::pin(audio_worklet_js(h))),
        ];
        for (name, handler) in handlers {
            let plain = rt.block_on(handler(HeaderMap::new()));
            assert_eq!(plain.status(), StatusCode::OK, "{name}");
            assert_eq!(
                plain.headers()["cross-origin-embedder-policy"],
                "require-corp",
                "{name} 200"
            );
            let etag = plain.headers()[header::ETAG].clone();

            let mut conditional = HeaderMap::new();
            conditional.insert(header::IF_NONE_MATCH, etag);
            let revalidated = rt.block_on(handler(conditional));
            assert_eq!(revalidated.status(), StatusCode::NOT_MODIFIED, "{name}");
            assert_eq!(
                revalidated.headers()["cross-origin-embedder-policy"],
                "require-corp",
                "{name} 304"
            );
        }
    }

    /// One display-affecting edit, end to end: the host bumps the epoch, the client offers again
    /// on the id it already holds, and the host recognises that offer as the same session. The
    /// join id and the display id must not change, and the mode change must be one mode change
    /// rather than a removal and a create.
    #[test]
    fn a_display_change_keeps_the_join_and_re_drives_the_same_display() {
        let sessions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            session::DeviceSessionState,
        >::new()));
        let token = session::mint_device_token();
        let join = session::mint_join_id();
        session::open_session(&sessions, &join, &token, "192.168.1.44");
        let before = session::LiveDisplay {
            display_id: 11,
            device_name: "DISPLAY7".to_string(),
            device_path: r"\?\DISPLAY#TEST7".to_string(),
            width: 1920,
            height: 1080,
            refresh: 60,
            scale: 100,
            portrait: false,
        };
        session::set_live_display(&sessions, &join, before.clone());

        // a refresh-rate edit changes the stream, so every tab of this device is told to
        // renegotiate
        assert_eq!(
            session::bump_reconfig_epoch_for_device(&sessions, &token),
            1
        );

        // the client answers by offering again and presenting the id it holds
        let continued = continuing_join(&sessions, &join, &token);
        assert_eq!(
            continued.as_deref(),
            Some(join.as_str()),
            "a renegotiation must land on the session it came from, not on a new one"
        );

        // so `start_session` finds a display to reuse rather than creating one
        let existing = session::get_live_display(&sessions, &join).expect("the display survives");
        assert_eq!(existing.display_id, before.display_id);

        // one mode change: only the refresh rate moves, and `display_params()` (what decides
        // whether the display is touched at all) reports exactly that difference
        let desired = session::LiveDisplay {
            refresh: 120,
            ..existing.clone()
        };
        assert_ne!(existing.display_params(), desired.display_params());
        assert_eq!(
            session::LiveDisplay {
                display_id: existing.display_id,
                device_name: existing.device_name.clone(),
                ..desired.clone()
            }
            .display_params(),
            desired.display_params(),
            "the display keeps its identity across the mode change"
        );
        session::set_live_display(&sessions, &join, desired);

        // nothing ended: same join, same display id, no kick
        assert_eq!(
            session::joins_for_token(&sessions, &token),
            vec![join.clone()]
        );
        assert_eq!(
            session::get_live_display(&sessions, &join)
                .map(|d| d.display_id)
                .unwrap(),
            11
        );
        assert_eq!(session::kick_epoch(&sessions, &join), 0);
    }

    /// A display that refuses the mode it is given leaves the session where it was, in full.
    ///
    /// The IDD driver only accepts resolutions that were pre-registered when the display was
    /// created, so a DPR off that ladder is a `DISP_CHANGE_BADMODE` and nothing moves. If the
    /// requested geometry and scale were recorded anyway, the next save would compare equal and
    /// the client would keep rendering at a size the display never had.
    #[test]
    fn a_refused_mode_records_what_the_display_is_at_and_not_what_was_asked_for() {
        let prev = session::LiveDisplay {
            display_id: 11,
            device_name: r"\\.\DISPLAY9".to_string(),
            device_path: r"\\?\DISPLAY#CHY0000#1&virtual".to_string(),
            width: 1920,
            height: 1080,
            refresh: 60,
            scale: 100,
            portrait: false,
        };
        // a DPR change: twice the geometry, twice the scale
        let desired = session::LiveDisplay {
            width: 3840,
            height: 2160,
            scale: 200,
            ..prev.clone()
        };

        let applied = landed_display(
            &desired,
            prev.display_id,
            prev.device_name.clone(),
            prev.device_path.clone(),
            ModeOutcome::Applied {
                scale: 200,
                refresh: 60,
            },
        );
        assert_eq!(applied.display_params(), (3840, 2160, 60, 200, false));
        assert_eq!(applied.device_path, prev.device_path);

        // the display refused it: it is still 1920x1080 and still at 100 %, and both halves of
        // that have to survive into the session or the retry never happens
        let refused = landed_display(
            &desired,
            prev.display_id,
            prev.device_name.clone(),
            prev.device_path.clone(),
            ModeOutcome::Refused {
                width: 1920,
                height: 1080,
                scale: 100,
                refresh: 60,
            },
        );
        assert_eq!(refused.display_params(), prev.display_params());
        assert_ne!(
            refused.display_params(),
            desired.display_params(),
            "a refused change must still read as a change, so the next reconfigure retries it"
        );

        // a display that took the mode but would not scale past 175 % records 175 %, so the
        // next save at 200 % is still a change and is tried again
        let clamped = landed_display(
            &desired,
            prev.display_id,
            prev.device_name.clone(),
            prev.device_path.clone(),
            ModeOutcome::Applied {
                scale: 175,
                refresh: 60,
            },
        );
        assert_eq!(clamped.display_params(), (3840, 2160, 60, 175, false));
        assert_ne!(clamped.display_params(), desired.display_params());
    }

    /// The join id is a capability, and it is honoured as one: only for a session that is still
    /// open and only for the device that owns it.
    #[test]
    fn a_presented_join_is_honoured_only_for_the_device_that_owns_it() {
        let sessions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            session::DeviceSessionState,
        >::new()));
        let mine = session::mint_device_token();
        let theirs = session::mint_device_token();
        let join = session::mint_join_id();
        session::open_session(&sessions, &join, &mine, "192.168.1.44");

        assert_eq!(
            continuing_join(&sessions, &join, &mine).as_deref(),
            Some(join.as_str())
        );
        assert_eq!(
            continuing_join(&sessions, &join, &theirs),
            None,
            "another device presenting this id must not inherit the session or its display"
        );
        assert_eq!(
            continuing_join(&sessions, &session::mint_join_id(), &mine),
            None,
            "an id naming no live session is a fresh join, not a failure"
        );
        assert_eq!(
            continuing_join(&sessions, "", &mine),
            None,
            "a first join presents nothing"
        );
        assert_eq!(
            continuing_join(&sessions, &join, ""),
            None,
            "a join that authenticated as no device cannot claim one"
        );

        // once the session is gone the id is worth nothing, which is what makes a stale id in a
        // reloaded tab harmless
        session::remove_session(&sessions, &join);
        assert_eq!(continuing_join(&sessions, &join, &mine), None);
    }

    /// An audio-sink change is visible in `/reconfig` without moving either epoch, which is why
    /// it costs no renegotiation.
    #[test]
    fn an_audio_output_change_reaches_the_client_without_moving_an_epoch() {
        let sessions = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::<
            String,
            session::DeviceSessionState,
        >::new()));
        let sinks = session::new_shared_audio_sinks();
        let token = session::mint_device_token();
        let join = session::mint_join_id();
        session::open_session(&sessions, &join, &token, "192.168.1.44");
        let state = AppState::new(Config {
            sessions: Some(sessions.clone()),
            audio_sinks: Some(sinks.clone()),
            ..Config::default()
        });

        let first: serde_json::Value =
            serde_json::from_str(&process_reconfig(&state, &join)).expect("json");
        assert_eq!(first["epoch"], 0);
        assert_eq!(first["audioSink"], "");

        session::set_device_audio_sink(&sinks, &token, Some("sink-7".to_string()));
        let after: serde_json::Value =
            serde_json::from_str(&process_reconfig(&state, &join)).expect("json");
        assert_eq!(after["audioSink"], "sink-7");
        assert_eq!(
            after["epoch"], 0,
            "choosing an output device must not renegotiate anything"
        );
        assert_eq!(after["kick"], 0);
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
        // still behind the same NAT, so not a relay case, but it does not get the same-subnet
        // shortcuts either: the extra hop is where jitter comes from
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
    fn a_relayed_join_is_cross_network_however_its_client_id_is_spelled() {
        // The relay forwards an opaque client id, never an address, and classifying a join by
        // parsing it would put a cross-network client on the same-subnet transport constants.
        // The id's spelling is the relay's business (`c-1`, `c_Lu6CRIn28D7GxgD8qMpTow`), so
        // nothing here may depend on it.
        let s = state_with_local_ips(&["192.168.1.10"]);
        for id in ["c_Lu6CRIn28D7GxgD8qMpTow", "c-1", "", "192.168.1.44"] {
            assert_eq!(
                s.locality_for_origin(JoinOrigin::relayed(id)),
                SessionLocality::CrossNetwork,
                "a join through the cloud tunnel is cross-network by construction, not by \
                 whatever `{id}` happens to parse as"
            );
        }
    }

    #[test]
    fn a_direct_join_still_classifies_by_address() {
        let s = state_with_local_ips(&["192.168.1.10"]);
        assert_eq!(
            s.locality_for_origin(JoinOrigin::direct("192.168.1.44")),
            SessionLocality::SameNetwork
        );
        assert_eq!(
            s.locality_for_origin(JoinOrigin::direct("203.0.113.9")),
            SessionLocality::CrossNetwork
        );
        assert_eq!(
            s.locality_for_origin(JoinOrigin::direct("192.168.9.7")),
            SessionLocality::Unknown
        );
    }

    #[test]
    fn a_relayed_join_is_offered_the_full_ice_list() {
        // `cloud.rs` builds the relayed list as `ice_with_turn(fallback_ice_servers())` and the
        // direct handler as `ice_for(locality)`. The two must agree, or a tunnelled client and a
        // direct one would gather against different servers.
        let s = state_with_local_ips(&["192.168.1.10"]);
        let relayed = s.ice_with_turn(s.fallback_ice_servers());
        let direct = s.ice_for(SessionLocality::CrossNetwork);
        assert_eq!(relayed.len(), direct.len());
        assert!(
            !relayed.is_empty(),
            "a relayed client with no STUN cannot produce a reflexive candidate, and without a \
             TURN server that is the only kind of candidate that can reach it"
        );
        assert!(relayed
            .iter()
            .zip(direct.iter())
            .all(|(a, b)| a.urls == b.urls));
    }

    #[test]
    fn a_relayed_reconfig_that_names_no_join_still_finds_its_session() {
        // the relay's deployed client page polls `/reconfig` with no `?join=`, and "" matches
        // nothing (epoch 0 / kick 0 for ever), so a kick raised in the desktop UI would never
        // reach a device that joined over the internet
        let sessions: session::SharedSessions =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let state = AppState::new(Config {
            sessions: Some(Arc::clone(&sessions)),
            ..Config::default()
        });
        let join = session::mint_join_id();
        session::open_session_via(&sessions, &join, "tok", "c-7", Some("c-7"));
        session::bump_kick_epoch(&sessions, &join);

        let blind: serde_json::Value =
            serde_json::from_str(&process_reconfig(&state, "")).expect("json");
        assert_eq!(
            blind["kick"], 0,
            "the empty join is what the old page asks with, and it matches no session"
        );

        let answered: serde_json::Value =
            serde_json::from_str(&process_relay_reconfig(&state, "", "c-7")).expect("json");
        assert_eq!(
            answered["kick"], 1,
            "the relay client id maps onto the join without needing an address"
        );

        // a client that does know its join id is still answered on it, not on the fallback
        let named: serde_json::Value =
            serde_json::from_str(&process_relay_reconfig(&state, &join, "c-7")).expect("json");
        assert_eq!(named["kick"], 1);

        // an unknown relay client is not silently mapped onto somebody else's session
        let stranger: serde_json::Value =
            serde_json::from_str(&process_relay_reconfig(&state, "", "c-99")).expect("json");
        assert_eq!(stranger["kick"], 0);
    }

    /// The relay's old page renegotiates with no join id. Its live join must be the one both
    /// `/reconfig` and the next offer land on, never the stale one still in its grace.
    #[test]
    fn a_relayed_page_without_join_ids_lands_on_its_live_join() {
        let sessions: session::SharedSessions = Default::default();
        let display = session::LiveDisplay {
            display_id: 3,
            device_name: String::new(),
            device_path: String::new(),
            width: 1920,
            height: 1080,
            refresh: 60,
            scale: 100,
            portrait: false,
        };
        let stale = session::mint_join_id();
        let live = session::mint_join_id();
        session::open_session_via(&sessions, &stale, "tok", "c-7", Some("c-7"));
        session::open_session_via(&sessions, &live, "tok", "c-7", Some("c-7"));
        session::set_live_display(&sessions, &stale, display.clone());
        session::set_live_display(&sessions, &live, display);
        session::set_active_capture(&sessions, &live, 1, Box::new(|| {}));
        for _ in 0..8 {
            assert_eq!(
                session::live_join_for_relay_client(&sessions, "c-7").as_deref(),
                Some(live.as_str())
            );
        }
        let origin = JoinOrigin::relayed("c-7");
        assert_eq!(
            relayed_continuation(&sessions, "", origin, "tok").as_deref(),
            Some(live.as_str())
        );
        // another device under the same relay client, a named join, or a direct join: fresh
        assert!(relayed_continuation(&sessions, "", origin, "other").is_none());
        assert!(relayed_continuation(&sessions, &live, origin, "tok").is_none());
        assert!(relayed_continuation(&sessions, "", JoinOrigin::direct("c-7"), "tok").is_none());
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

    #[derive(Debug, Default)]
    struct RecordingDisplays(std::sync::Mutex<Vec<u32>>);

    impl session::VirtualDisplayController for RecordingDisplays {
        fn create_display(&self, _: String, _: u32, _: u32, _: u32) -> Result<u32, String> {
            Err("not used in this test".into())
        }
        fn remove_display(&self, id: u32) {
            self.0.lock().unwrap().push(id);
        }
        fn remove_all_displays(&self) {}
    }

    /// One join holding display 7, at its first attempt, with no disconnect grace.
    fn failed_attempt_fixture() -> (AppState, std::sync::Arc<RecordingDisplays>, String, u64) {
        let sessions: session::SharedSessions = Default::default();
        let token = session::mint_device_token();
        let join = session::mint_join_id();
        session::open_session(&sessions, &join, &token, "192.168.1.44");
        session::set_live_display(
            &sessions,
            &join,
            session::LiveDisplay {
                display_id: 7,
                device_name: String::new(),
                device_path: String::new(),
                width: 1920,
                height: 1080,
                refresh: 60,
                scale: 100,
                portrait: false,
            },
        );
        let seq = session::next_session_seq(&sessions, &join);
        let displays = std::sync::Arc::new(RecordingDisplays::default());
        let controller: session::SharedVirtualDisplay = displays.clone();
        let state = AppState::new(Config {
            virtual_display: Some(controller),
            sessions: Some(sessions),
            disconnect_grace: Some(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0))),
            ..Default::default()
        });
        (state, displays, join, seq)
    }

    /// A renegotiation that fails must not forget its session: the display id lives only there,
    /// so the display would be orphaned and the page's retry on the same join id would be given
    /// a second display and a second dashboard row.
    #[tokio::test]
    async fn a_failed_renegotiation_keeps_its_display_for_the_retry_and_then_reaps_it() {
        let (state, displays, join, seq) = failed_attempt_fixture();
        let sessions = state.config.sessions.clone().unwrap();
        let token = session::device_token_for(&sessions, &join).unwrap();
        abandon_failed_attempt(&state, &join, seq, true, true).await;
        assert_eq!(
            continuing_join(&sessions, &join, &token).as_deref(),
            Some(join.as_str()),
            "the page's retry must land on the same join"
        );
        assert_eq!(
            session::get_live_display(&sessions, &join).map(|d| d.display_id),
            Some(7),
            "and find the display still there to reuse"
        );
        assert!(displays.0.lock().unwrap().is_empty());
        // not early: still there just short of the settle
        tokio::time::sleep(LEAVE_SETTLE - std::time::Duration::from_millis(200)).await;
        assert!(
            displays.0.lock().unwrap().is_empty(),
            "reaped before the grace"
        );
        // nobody offered again: after the grace it goes, display and session together, within
        // a deadline generous enough for a loaded machine
        let reaped = eventually(std::time::Duration::from_secs(15), || {
            *displays.0.lock().unwrap() == vec![7]
                && session::device_token_for(&sessions, &join).is_none()
        })
        .await;
        assert!(reaped, "removed: {:?}", *displays.0.lock().unwrap());
    }

    #[tokio::test]
    async fn a_retry_after_a_failed_renegotiation_owns_the_display() {
        let (state, displays, join, seq) = failed_attempt_fixture();
        let sessions = state.config.sessions.clone().unwrap();
        abandon_failed_attempt(&state, &join, seq, true, true).await;
        // the retry arrives and takes the join over
        session::next_session_seq(&sessions, &join);
        tokio::time::sleep(LEAVE_SETTLE + std::time::Duration::from_millis(400)).await;
        assert!(displays.0.lock().unwrap().is_empty());
        assert_eq!(
            session::get_live_display(&sessions, &join).map(|d| d.display_id),
            Some(7)
        );
    }

    /// Poll `done` every 20 ms until it holds or `limit` passes. For assertions about something a
    /// spawned task does: a fixed sleep before them fails on a loaded machine.
    async fn eventually(limit: std::time::Duration, done: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if done() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    /// The lost answer: the page's retry from the same page load lands on the join its first
    /// offer opened, under that join's token, instead of opening a second session and display.
    #[test]
    fn a_retry_from_the_same_page_load_continues_the_join_whose_answer_was_lost() {
        let (state, _displays, join, _seq) = failed_attempt_fixture();
        let sessions = state.config.sessions.clone().unwrap();
        let token = session::device_token_for(&sessions, &join).unwrap();
        let attempt = session::mint_join_id();
        session::set_session_attempt(&sessions, &join, &attempt);

        // a first join's retry presents no token: the lost answer was what carried it
        assert_eq!(
            session::pending_join_for_attempt(&sessions, &attempt, ""),
            Some((join.clone(), token.clone()))
        );
        assert_eq!(
            attempt_continuation(&sessions, "", &attempt, &token).as_deref(),
            Some(join.as_str())
        );
    }

    #[test]
    fn an_attempt_id_never_reaches_a_connected_or_foreign_session() {
        let (state, _displays, join, _seq) = failed_attempt_fixture();
        let sessions = state.config.sessions.clone().unwrap();
        let token = session::device_token_for(&sessions, &join).unwrap();
        let attempt = session::mint_join_id();
        session::set_session_attempt(&sessions, &join, &attempt);

        // another device presenting the same attempt id
        let other = session::mint_device_token();
        assert!(session::pending_join_for_attempt(&sessions, &attempt, &other).is_none());
        assert!(attempt_continuation(&sessions, "", &attempt, &other).is_none());
        // a different page load, or a malformed id
        let stranger = session::mint_join_id();
        assert!(session::pending_join_for_attempt(&sessions, &stranger, "").is_none());
        assert!(session::pending_join_for_attempt(&sessions, "", "").is_none());
        assert!(
            session::pending_join_for_attempt(&sessions, &attempt.to_uppercase(), "").is_none()
        );
        assert_eq!(attempt_shape("not-hex"), "");
        // an offer that names a join is a renegotiation, never an attempt continuation
        assert!(attempt_continuation(&sessions, &join, &attempt, &token).is_none());
        // once a picture has reached the client the attempt id buys nothing
        session::mark_session_connected(&sessions, &join);
        assert!(session::pending_join_for_attempt(&sessions, &attempt, "").is_none());
        assert!(attempt_continuation(&sessions, "", &attempt, &token).is_none());
    }

    #[test]
    fn an_attempt_without_a_display_yet_is_not_taken_over() {
        // still being built: continuing it would make a second display beside the one its own
        // attempt is about to record
        let sessions: session::SharedSessions = Default::default();
        let token = session::mint_device_token();
        let join = session::mint_join_id();
        let attempt = session::mint_join_id();
        session::open_session(&sessions, &join, &token, "192.168.1.44");
        session::set_session_attempt(&sessions, &join, &attempt);
        assert!(session::pending_join_for_attempt(&sessions, &attempt, "").is_none());
    }

    #[tokio::test]
    async fn a_failed_fresh_join_takes_its_display_with_it() {
        let (state, displays, join, seq) = failed_attempt_fixture();
        let sessions = state.config.sessions.clone().unwrap();
        abandon_failed_attempt(&state, &join, seq, true, false).await;
        assert_eq!(*displays.0.lock().unwrap(), vec![7]);
        assert!(session::device_token_for(&sessions, &join).is_none());
    }

    #[tokio::test]
    async fn a_join_that_panics_is_answered_rather_than_dropped() {
        let out = detach_join(async { panic!("join blew up") }).await;
        assert_eq!(out.status, StatusCode::INTERNAL_SERVER_ERROR.as_u16());
    }

    fn first_byte_fixture() -> (session::SharedSessions, String, u64) {
        let (state, _displays, join, seq) = failed_attempt_fixture();
        (state.config.sessions.clone().unwrap(), join, seq)
    }

    const SHORT_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

    fn telemetry() -> crate::streamer::telemetry::SharedTelemetry {
        crate::streamer::telemetry::SessionTelemetry::new_shared()
    }

    #[tokio::test]
    async fn a_join_nobody_ever_reads_is_reported_unwatched() {
        let (sessions, join, seq) = first_byte_fixture();
        let seen = watch_first_byte(&sessions, &join, seq, &telemetry(), SHORT_WINDOW).await;
        assert_eq!(seen, FirstByte::NeverConnected);
    }

    #[tokio::test]
    async fn a_join_whose_client_got_a_byte_is_connected_and_kept() {
        let (sessions, join, seq) = first_byte_fixture();
        let attempt = session::mint_join_id();
        session::set_session_attempt(&sessions, &join, &attempt);
        let t = telemetry();
        t.bytes_sent
            .fetch_add(1200, std::sync::atomic::Ordering::Relaxed);
        let seen = watch_first_byte(&sessions, &join, seq, &t, SHORT_WINDOW).await;
        assert_eq!(seen, FirstByte::Connected);
        assert!(session::pending_join_for_attempt(&sessions, &attempt, "").is_none());
    }

    /// Judged by the connection: a client whose transport is up and whose capture has delivered
    /// nothing yet is still a client.
    #[tokio::test]
    async fn a_connected_client_without_a_picture_is_never_closed_as_absent() {
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        t.transport_connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let seen = watch_first_byte(&sessions, &join, seq, &t, SHORT_WINDOW).await;
        assert_eq!(seen, FirstByte::Connected);
    }

    #[tokio::test]
    async fn a_quiet_renegotiation_of_a_watched_join_is_not_closed() {
        let (sessions, join, seq) = first_byte_fixture();
        session::mark_session_connected(&sessions, &join);
        let seen = watch_first_byte(&sessions, &join, seq, &telemetry(), SHORT_WINDOW).await;
        assert_eq!(seen, FirstByte::Quiet);
    }

    /// What the teardown task is given and how it ends, wired exactly as `start_session` wires
    /// them (`spawn_first_byte_watch` into `wait_session_end`).
    async fn ending(
        sessions: &session::SharedSessions,
        join: &str,
        seq: u64,
        t: crate::streamer::telemetry::SharedTelemetry,
        meanwhile: impl FnOnce(),
    ) -> SessionEnd {
        let (_closed_tx, closed_rx) = oneshot::channel();
        let leave = session::arm_leave(sessions, join);
        let abandoned = spawn_first_byte_watch(
            sessions.clone(),
            join.to_string(),
            seq,
            t,
            std::time::Duration::from_secs(2),
        );
        meanwhile();
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            wait_session_end(closed_rx, Some(leave), Some(abandoned)),
        )
        .await
        .expect("the teardown never woke")
    }

    /// A lost answer: the page's retry takes the join over before the first attempt's client ever
    /// connected. The first attempt's transport has nobody, and its offer may have carried no
    /// candidates, so ICE would never fail it; the teardown must end it at once, without a grace.
    #[tokio::test]
    async fn an_attempt_overtaken_before_it_connected_is_closed_at_once() {
        let (sessions, join, seq) = first_byte_fixture();
        let started = std::time::Instant::now();
        let end = ending(&sessions, &join, seq, telemetry(), || {
            session::next_session_seq(&sessions, &join);
        })
        .await;
        assert_eq!(end, SessionEnd::Abandoned(FirstByte::Superseded));
        // what the teardown then does with it, under a grace far longer than the test
        let grace = std::time::Duration::from_secs(600);
        let disposal = end_of_attempt(Some(&sessions), &join, seq, end, grace, "test").await;
        assert_eq!(disposal, Disposal::Keep, "the display goes with the join");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// No client ever came: the display nobody saw comes down with no disconnect grace.
    #[tokio::test]
    async fn a_join_no_client_ever_reached_ends_without_a_grace() {
        let (sessions, join, seq) = first_byte_fixture();
        let end = ending(&sessions, &join, seq, telemetry(), || {}).await;
        assert_eq!(end, SessionEnd::Abandoned(FirstByte::NeverConnected));
        let started = std::time::Instant::now();
        let grace = std::time::Duration::from_secs(600);
        let disposal = end_of_attempt(Some(&sessions), &join, seq, end, grace, "test").await;
        assert_eq!(disposal, Disposal::Remove { left: true });
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "no grace"
        );

        // a connection that closed waits the grace first
        let grace = std::time::Duration::from_millis(150);
        let closed = SessionEnd::Closed { left: false };
        let started = std::time::Instant::now();
        let disposal = end_of_attempt(Some(&sessions), &join, seq, closed, grace, "test").await;
        assert_eq!(disposal, Disposal::Remove { left: false });
        assert!(started.elapsed() >= grace);
    }

    /// The transport a renegotiation in place replaced closes after the newer attempt owns the
    /// join: no grace is waited and nothing is removed, at once.
    #[tokio::test]
    async fn a_replaced_transport_closing_waits_no_grace() {
        let (sessions, join, seq) = first_byte_fixture();
        session::next_session_seq(&sessions, &join);
        let started = std::time::Instant::now();
        let grace = std::time::Duration::from_secs(600);
        let closed = SessionEnd::Closed { left: false };
        let disposal = end_of_attempt(Some(&sessions), &join, seq, closed, grace, "test").await;
        assert_eq!(disposal, Disposal::Keep);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "no grace waited"
        );
    }

    /// A connected client is never ended by the watcher, only by its connection or its page.
    #[tokio::test]
    async fn a_connected_attempt_is_ended_only_by_its_connection() {
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        t.transport_connected
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let (_closed_tx, closed_rx) = oneshot::channel();
        let abandoned = spawn_first_byte_watch(
            sessions.clone(),
            join.clone(),
            seq,
            t,
            std::time::Duration::from_millis(300),
        );
        let waited = tokio::time::timeout(
            std::time::Duration::from_millis(800),
            wait_session_end(closed_rx, None, Some(abandoned)),
        )
        .await;
        assert!(waited.is_err(), "still running: nothing has ended it");
    }

    /// A retry from the same page load while the first attempt has not recorded its display yet
    /// waits for it, so it continues that attempt instead of building a second display.
    #[tokio::test]
    async fn a_retry_waits_for_the_attempt_still_building_its_display() {
        let sessions: session::SharedSessions = Default::default();
        let token = session::mint_device_token();
        let join = session::mint_join_id();
        let attempt = session::mint_join_id();
        session::open_session(&sessions, &join, &token, "10.0.0.5");
        session::set_session_attempt(&sessions, &join, &attempt);
        assert!(session::pending_join_for_attempt(&sessions, &attempt, "").is_none());

        let building = sessions.clone();
        let building_join = join.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            session::set_live_display(
                &building,
                &building_join,
                session::LiveDisplay {
                    display_id: 3,
                    device_name: String::new(),
                    device_path: String::new(),
                    width: 1280,
                    height: 720,
                    refresh: 60,
                    scale: 100,
                    portrait: false,
                },
            );
        });
        let wait = std::time::Duration::from_secs(5);
        assert!(await_attempt_display(&sessions, &attempt, "", wait).await);
        assert_eq!(
            session::pending_join_for_attempt(&sessions, &attempt, "").map(|(j, _)| j),
            Some(join),
            "the retry now finds the attempt in flight and continues it"
        );
        // nothing in flight: no wait at all
        let other = session::mint_join_id();
        assert!(await_attempt_display(&sessions, &other, "", wait).await);

        // an attempt still building at the bound refuses the retry instead of a second display
        let stuck = session::mint_join_id();
        let stuck_attempt = session::mint_join_id();
        session::open_session(&sessions, &stuck, &token, "10.0.0.5");
        session::set_session_attempt(&sessions, &stuck, &stuck_attempt);
        let short = std::time::Duration::from_millis(200);
        assert!(!await_attempt_display(&sessions, &stuck_attempt, "", short).await);
    }

    #[cfg(windows)]
    fn watched(
        sessions: &session::SharedSessions,
        join: &str,
        seq: u64,
        telemetry: crate::streamer::telemetry::SharedTelemetry,
        on: RebuildOn,
    ) -> tokio::task::JoinHandle<Option<CaptureEnd>> {
        tokio::spawn(watch_capture(
            sessions.clone(),
            join.to_string(),
            seq,
            telemetry,
            on,
            std::time::Duration::from_millis(10),
            session::DisplayTarget {
                device_path: String::new(),
                device_name: r"\\.\DISPLAY-THAT-DOES-NOT-EXIST".to_string(),
            },
        ))
    }

    #[cfg(windows)]
    const CLOSED_ONLY: RebuildOn = RebuildOn {
        closed: true,
        stopped: false,
        dark_after: None,
    };

    #[cfg(windows)]
    #[tokio::test]
    async fn a_closed_capture_item_renegotiates_its_join_once() {
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        let epoch = session::reconfig_epoch(&sessions, &join);
        let watch = watched(&sessions, &join, seq, t.clone(), CLOSED_ONLY);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        t.degradation
            .capture_lost
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(watch.await.unwrap(), Some(CaptureEnd::Closed));
        assert_eq!(session::reconfig_epoch(&sessions, &join), epoch + 1);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_stopped_capture_renegotiates_only_with_the_candidate() {
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        let both = RebuildOn {
            closed: true,
            stopped: true,
            dark_after: None,
        };
        let shipped = watched(&sessions, &join, seq, t.clone(), CLOSED_ONLY);
        let candidate = watched(&sessions, &join, seq, t.clone(), both);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        t.degradation
            .capture_stopped
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(candidate.await.unwrap(), Some(CaptureEnd::Stopped));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !shipped.is_finished(),
            "the shipped trigger ignores a stopped capture"
        );
        shipped.abort();
    }

    /// A join taken over, or torn down, while the watcher was looking is never bumped, and a
    /// removed session is never brought back by the bump.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_watcher_never_bumps_a_join_it_no_longer_owns() {
        use std::sync::atomic::Ordering::Relaxed;
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        // the watcher is running when the join is taken over, and the capture is lost after
        let watch = watched(&sessions, &join, seq, t.clone(), CLOSED_ONLY);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        session::next_session_seq(&sessions, &join);
        let epoch = session::reconfig_epoch(&sessions, &join);
        t.degradation.capture_lost.fetch_add(1, Relaxed);
        assert_eq!(watch.await.unwrap(), None);
        assert_eq!(
            session::reconfig_epoch(&sessions, &join),
            epoch,
            "the newer attempt's join was not renegotiated"
        );

        // torn down while watched: nothing is bumped and no entry comes back
        let t = telemetry();
        let watch = watched(&sessions, &join, seq + 1, t.clone(), CLOSED_ONLY);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        session::remove_session(&sessions, &join);
        t.degradation.capture_lost.fetch_add(1, Relaxed);
        assert_eq!(watch.await.unwrap(), None);
        assert!(sessions.lock().unwrap().is_empty());
        assert!(!session::bump_reconfig_epoch_if_current(
            &sessions,
            &join,
            seq + 1
        ));
        assert!(
            session::device_token_for(&sessions, &join).is_none()
                && sessions.lock().unwrap().is_empty(),
            "no entry was created for a removed join"
        );
    }

    /// A capture answered without a first frame is opened again once it has stayed dark for the
    /// wait, and counted; one that delivers in the meantime is left alone.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_capture_that_stays_dark_is_opened_again_and_counted() {
        use std::sync::atomic::Ordering::Relaxed;
        let dark = RebuildOn {
            closed: false,
            stopped: false,
            dark_after: Some(std::time::Duration::from_millis(80)),
        };
        let (sessions, join, seq) = first_byte_fixture();
        let epoch = session::reconfig_epoch(&sessions, &join);
        let t = telemetry();
        let watch = watched(&sessions, &join, seq, t.clone(), dark);
        assert_eq!(watch.await.unwrap(), Some(CaptureEnd::Dark));
        assert_eq!(session::reconfig_epoch(&sessions, &join), epoch + 1);
        assert_eq!(t.degradation.capture_dark_rebuilds.load(Relaxed), 1);

        // a wait long enough that a delayed frame on a loaded machine still lands inside it
        let slow_dark = RebuildOn {
            dark_after: Some(std::time::Duration::from_secs(3)),
            ..dark
        };
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        let watch = watched(&sessions, &join, seq, t.clone(), slow_dark);
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        t.capture_frames_arrived.fetch_add(1, Relaxed);
        // past the whole dark wait: a capture that painted is never reopened
        tokio::time::sleep(std::time::Duration::from_millis(3500)).await;
        assert!(
            !watch.is_finished(),
            "a capture that painted is not reopened"
        );
        watch.abort();

        // a capture that had already delivered when it was answered is never watched for this
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        t.capture_frames_arrived.fetch_add(1, Relaxed);
        let watch = watched(&sessions, &join, seq, t.clone(), dark);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!watch.is_finished());
        watch.abort();
    }

    /// A capture that never becomes ready ends its session the way a page leaving would, so the
    /// one teardown runs; only while that attempt still owns the join, and only once.
    #[test]
    fn a_capture_that_never_opens_ends_its_session_once() {
        use std::sync::atomic::Ordering::SeqCst;
        let sessions: session::SharedSessions = Default::default();
        let join = session::mint_join_id();
        session::open_session(&sessions, &join, "tok", "10.0.0.5");
        let seq = session::next_session_seq(&sessions, &join);
        let failure: CaptureFailure = Default::default();
        let give_up = end_session_on_capture_failure(
            Some(sessions.clone()),
            join.clone(),
            seq,
            Arc::clone(&failure),
        );

        // it fails at once, before the attempt has registered its leave signal: kept, and nothing
        // (in particular no previous attempt's signal) is told anything
        let previous = session::arm_leave(&sessions, &join);
        give_up("no capture item".into());
        assert!(
            !previous.left.load(SeqCst),
            "the previous attempt's signal is left alone"
        );

        // the attempt registers: the failure it kept ends the session now
        let leave = session::arm_leave(&sessions, &join);
        arm_capture_failure(&failure, Some(sessions.clone()), &join, seq);
        assert!(leave.left.load(SeqCst), "the teardown is told to run");

        // after registration a failure ends the session at once
        let failure: CaptureFailure = Default::default();
        let leave = session::arm_leave(&sessions, &join);
        arm_capture_failure(&failure, Some(sessions.clone()), &join, seq);
        end_session_on_capture_failure(Some(sessions.clone()), join.clone(), seq, failure)(
            "no capture item".into(),
        );
        assert!(leave.left.load(SeqCst));

        // overtaken first: the newer attempt has its own capture, and its session is left alone
        let newer = session::next_session_seq(&sessions, &join);
        let leave = session::arm_leave(&sessions, &join);
        let failure: CaptureFailure = Default::default();
        arm_capture_failure(&failure, Some(sessions.clone()), &join, seq);
        end_session_on_capture_failure(Some(sessions.clone()), join.clone(), seq, failure)(
            "no capture item".into(),
        );
        assert!(!leave.left.load(SeqCst));
        assert!(session::is_current_session(&sessions, &join, newer));
    }

    #[test]
    fn only_a_page_that_holds_its_timers_is_answered_early() {
        assert!(answers_early(true, true));
        assert!(
            !answers_early(false, true),
            "an old page or the relay's copy"
        );
        assert!(
            !answers_early(true, false),
            "the switch off is today's order"
        );
        assert!(!answers_early(false, false));
    }

    /// While the capture still waits for its monitor it is not dark; the dark clock starts when
    /// it opens.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_capture_still_waiting_for_its_monitor_is_not_rebuilt_as_dark() {
        use std::sync::atomic::Ordering::Relaxed;
        let dark = RebuildOn {
            closed: false,
            stopped: false,
            dark_after: Some(std::time::Duration::from_millis(80)),
        };
        let (sessions, join, seq) = first_byte_fixture();
        let t = telemetry();
        t.capture_waiting.store(true, Relaxed);
        let watch = watched(&sessions, &join, seq, t.clone(), dark);
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        assert!(!watch.is_finished(), "not dark while it waits");
        t.capture_waiting.store(false, Relaxed);
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), watch)
            .await
            .expect("dark once open")
            .unwrap();
        assert_eq!(end, Some(CaptureEnd::Dark));
    }

    /// A scale already in effect is not written again, and counts as landed; one that differs, or
    /// cannot be read, is written.
    #[test]
    fn a_scale_already_in_effect_is_not_written() {
        let writes = std::cell::Cell::new(0);
        let write = || {
            writes.set(writes.get() + 1);
            anyhow::Ok(150)
        };
        let (got, wrote) = scale_unless_in_effect(100, || Some(100), write);
        assert_eq!((got.unwrap(), wrote, writes.get()), (100, false, 0));
        let (got, wrote) = scale_unless_in_effect(150, || Some(100), write);
        assert_eq!((got.unwrap(), wrote, writes.get()), (150, true, 1));
        let (_, wrote) = scale_unless_in_effect(150, || None, write);
        assert!(wrote, "an unreadable monitor is written");
        assert_eq!(writes.get(), 2);
    }

    /// A display adopted before its first attempt wrote the DPI is recorded as unwritten, and the
    /// adopter's renegotiation then writes the DPI alone, with no mode set.
    #[test]
    fn a_scale_never_written_is_never_recorded() {
        let made = session::LiveDisplay {
            display_id: 5,
            device_name: String::new(),
            device_path: String::new(),
            width: 1280,
            height: 720,
            refresh: 60,
            scale: 150,
            portrait: false,
        };
        assert_eq!(
            scale_to_record(true, None, 150),
            0,
            "a new display, write pending"
        );
        assert_eq!(scale_to_record(false, None, 150), 150);
        let before = session::LiveDisplay {
            scale: 125,
            ..made.clone()
        };
        assert_eq!(scale_to_record(true, Some(&before), 150), 125);

        let unwritten = session::LiveDisplay {
            scale: 0,
            ..made.clone()
        };
        assert!(only_scale_differs(&unwritten, &made));
        assert!(!only_scale_differs(&made, &made));
        let resized = session::LiveDisplay {
            width: 1920,
            scale: 0,
            ..made.clone()
        };
        assert!(
            !only_scale_differs(&resized, &made),
            "a mode change is not DPI only"
        );
    }

    /// A display backend whose create is slow and whose display is always found, for the create
    /// that outlives its join.
    #[cfg(windows)]
    #[derive(Debug, Default)]
    struct LateDisplays {
        removed: std::sync::Mutex<Vec<u32>>,
        create_ms: u64,
    }

    #[cfg(windows)]
    impl session::VirtualDisplayController for LateDisplays {
        fn create_display(&self, _: String, _: u32, _: u32, _: u32) -> Result<u32, String> {
            std::thread::sleep(std::time::Duration::from_millis(self.create_ms));
            Ok(77)
        }
        fn remove_display(&self, id: u32) {
            self.removed.lock().unwrap().push(id);
        }
        fn remove_all_displays(&self) {}
        fn display_device_name(&self, _: u32) -> Option<String> {
            Some(r"\\.\DISPLAY-LATE".to_string())
        }
    }

    #[cfg(windows)]
    fn found_it(_: &str, _: &[String]) -> Option<String> {
        Some("test-path-late".to_string())
    }

    /// The join gives up while the create is still running: the lock stays held until the create
    /// ends, and what it made is then removed, once.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_create_that_outlives_its_join_keeps_the_lock_and_is_removed() {
        let displays = std::sync::Arc::new(LateDisplays {
            create_ms: 600,
            ..Default::default()
        });
        let client: session::SharedVirtualDisplay = displays.clone();
        let gave_up = create_display_correlated(
            client,
            "late".into(),
            (1280, 720, 60),
            Vec::new(),
            std::time::Duration::from_millis(100),
            found_it,
        )
        .await;
        assert!(gave_up.is_err(), "the join stops waiting at its bound");
        assert!(
            DISPLAY_CORRELATION_LOCK.try_lock().is_err(),
            "not given back while the create is in flight"
        );
        assert!(displays.removed.lock().unwrap().is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while displays.removed.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(*displays.removed.lock().unwrap(), vec![77], "removed, once");
    }

    /// Inside the bound the create is the join's, and nothing is removed.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_create_inside_the_bound_belongs_to_its_join() {
        let displays = std::sync::Arc::new(LateDisplays {
            create_ms: 20,
            ..Default::default()
        });
        let client: session::SharedVirtualDisplay = displays.clone();
        let made = create_display_correlated(
            client,
            "prompt".into(),
            (1280, 720, 60),
            Vec::new(),
            std::time::Duration::from_secs(10),
            found_it,
        )
        .await
        .expect("created");
        assert_eq!(made.0, 77);
        assert_eq!(made.2, "test-path-late");
        assert!(displays.removed.lock().unwrap().is_empty());
    }

    /// A retry that arrives through the real join path while the first attempt from its page load
    /// is still building waits for that display and continues it: no second display is created.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retry_through_the_join_path_continues_the_attempt_in_flight() {
        retry_through_the_join_path(false).await;
    }

    /// The same for a returning device, which is pre-approved and skips the OTP: the path every
    /// device takes after its first join.
    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_pre_approved_retry_continues_the_attempt_in_flight() {
        retry_through_the_join_path(true).await;
    }

    #[cfg(windows)]
    async fn retry_through_the_join_path(pre_approved: bool) {
        let token = session::mint_device_token();
        let (state, displays, _) = slow_join_state_with(pre_approved.then_some(token.as_str()));
        let sessions = state.config.sessions.clone().unwrap();
        let join = session::mint_join_id();
        let attempt = session::mint_join_id();
        session::open_session(&sessions, &join, &token, "10.0.0.9");
        session::set_session_attempt(&sessions, &join, &attempt);
        let building = sessions.clone();
        let building_join = join.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            session::set_live_display(
                &building,
                &building_join,
                session::LiveDisplay {
                    display_id: 3,
                    device_name: String::new(),
                    device_path: String::new(),
                    width: 1280,
                    height: 720,
                    refresh: 60,
                    scale: 100,
                    portrait: false,
                },
            );
        });
        let body = serde_json::json!({
            "sessionId": "test-session",
            "otp": if pre_approved { "" } else { "123456" },
            "deviceToken": if pre_approved { token.as_str() } else { "" },
            "width": 1280,
            "height": 720,
            "attemptId": attempt,
            "sdp": "v=0\r\nnot an offer\r\n",
        })
        .to_string();
        let request = whep(
            State(state),
            ConnectInfo("10.0.0.9:50000".parse().unwrap()),
            HeaderMap::new(),
            Bytes::from(body),
        );
        // long enough for a fresh join to have created its display, not for this one to finish
        let _ = tokio::time::timeout(std::time::Duration::from_millis(1500), request).await;
        assert!(
            displays.created.lock().unwrap().is_none(),
            "the retry built no display of its own"
        );
        assert_eq!(
            session::get_live_display(&sessions, &join).map(|d| d.display_id),
            Some(3)
        );
    }

    #[cfg(windows)]
    #[test]
    fn capture_rebuilds_are_bounded_per_join_for_its_life() {
        let (sessions, join, _seq) = first_byte_fixture();
        let mut at = std::time::Instant::now();
        for _ in 0..CAPTURE_REBUILD_LIMIT {
            assert!(capture_rebuild_allowed(&sessions, &join, at));
            let soon = at + std::time::Duration::from_millis(100);
            assert!(
                !capture_rebuild_allowed(&sessions, &join, soon),
                "never two inside the floor"
            );
            at += CAPTURE_REBUILD_FLOOR;
        }
        let much_later = at + std::time::Duration::from_secs(3600);
        assert!(
            !capture_rebuild_allowed(&sessions, &join, much_later),
            "the budget is the session's, not the minute's"
        );
        // a join that has gone is forgotten; a new one starts with a full budget
        session::remove_session(&sessions, &join);
        let next = session::mint_join_id();
        session::open_session(&sessions, &next, "", "10.0.0.6");
        assert!(capture_rebuild_allowed(&sessions, &next, much_later));
    }

    /// A display backend whose create is slow and synchronous, as the real one is.
    #[cfg(windows)]
    #[derive(Debug, Default)]
    struct SlowDisplays {
        created: std::sync::Mutex<Option<(std::time::Instant, std::time::Instant)>>,
        removed: std::sync::Mutex<Vec<u32>>,
    }

    #[cfg(windows)]
    impl session::VirtualDisplayController for SlowDisplays {
        fn create_display(&self, _: String, _: u32, _: u32, _: u32) -> Result<u32, String> {
            let started = std::time::Instant::now();
            std::thread::sleep(std::time::Duration::from_millis(400));
            *self.created.lock().unwrap() = Some((started, std::time::Instant::now()));
            Ok(41)
        }
        fn remove_display(&self, id: u32) {
            self.removed.lock().unwrap().push(id);
        }
        fn remove_all_displays(&self) {}
        fn display_device_name(&self, _: u32) -> Option<String> {
            // no monitor of that name exists, so the join refuses to claim it and unwinds
            Some(r"\\.\DISPLAY-THAT-DOES-NOT-EXIST".to_string())
        }
    }

    #[cfg(windows)]
    fn slow_join_state() -> (AppState, std::sync::Arc<SlowDisplays>, String) {
        slow_join_state_with(None)
    }

    #[cfg(windows)]
    fn slow_join_state_with(
        approved: Option<&str>,
    ) -> (AppState, std::sync::Arc<SlowDisplays>, String) {
        let displays = std::sync::Arc::new(SlowDisplays::default());
        let controller: session::SharedVirtualDisplay = displays.clone();
        let approved_devices = approved.map(|token| {
            let set = session::new_shared_approved_devices();
            set.lock().unwrap().insert(token.to_string());
            set
        });
        let state = AppState::new(Config {
            approved_devices,
            virtual_display: Some(controller),
            sessions: Some(Default::default()),
            session_auth: Some(session::SessionAuth {
                session_id: std::sync::Arc::new(std::sync::Mutex::new("test-session".into())),
                otp: std::sync::Arc::new(std::sync::Mutex::new("123456".into())),
            }),
            ..Default::default()
        });
        let body = serde_json::json!({
            "sessionId": "test-session",
            "otp": "123456",
            "width": 1280,
            "height": 720,
            "sdp": "v=0\r\nnot an offer\r\n",
        })
        .to_string();
        (state, displays, body)
    }

    /// The page goes away while its join is creating the display. The request handler is dropped
    /// mid-join; the join still runs to its end and gives back the display it made.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_join_whose_request_is_dropped_still_cleans_up_after_itself() {
        let (state, displays, body) = slow_join_state();
        let request = whep(
            State(state),
            ConnectInfo("10.0.0.9:50000".parse().unwrap()),
            HeaderMap::new(),
            Bytes::from(body),
        );
        let gone = tokio::time::timeout(std::time::Duration::from_millis(150), request).await;
        assert!(gone.is_err(), "the page left before the answer");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while displays.removed.lock().unwrap().is_empty() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            *displays.removed.lock().unwrap(),
            vec![41],
            "the display created for a request that went away was removed"
        );
    }

    /// The join's slow synchronous steps run off the runtime's workers: on a runtime with exactly
    /// one, an unrelated task keeps running while the display is being created.
    #[cfg(windows)]
    #[test]
    fn a_join_never_holds_the_only_worker_while_a_display_is_created() {
        let (state, displays, body) = slow_join_state();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let ticks = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        rt.block_on(async {
            let seen = std::sync::Arc::clone(&ticks);
            let ticker = tokio::spawn(async move {
                loop {
                    seen.lock().unwrap().push(std::time::Instant::now());
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            });
            let out = process_whep_detached(
                &state,
                JoinOrigin::direct("10.0.0.9"),
                body.as_bytes(),
                vec![],
            )
            .await;
            ticker.abort();
            assert_ne!(out.status, 200, "the stub display never attaches");
        });
        let (started, ended) = displays
            .created
            .lock()
            .unwrap()
            .expect("the join created a display");
        let during = ticks
            .lock()
            .unwrap()
            .iter()
            .filter(|t| **t > started && **t < ended)
            .count();
        assert!(
            during >= 5,
            "only {during} ticks ran while the display was created: the create held the worker"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn a_snapshot_task_that_died_refuses_the_claim() {
        let died =
            tokio::task::spawn_blocking(|| -> Vec<String> { panic!("snapshot blew up") }).await;
        let before = snapshot_or_refusal(died);
        assert_eq!(
            before,
            vec![crate::windows_utils::virtual_display::SNAPSHOT_FAILED.to_string()],
            "an empty before-set would make every monitor look new"
        );
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

    /// `holders` are device tokens; each is given one join holding one display.
    fn state_with(cap: Option<usize>, holders: &[&str]) -> AppState {
        let sessions: session::SharedSessions = Default::default();
        for (i, token) in holders.iter().enumerate() {
            let join = format!("join-{i}");
            session::open_session(&sessions, &join, token, "192.168.1.5");
            session::set_live_display(
                &sessions,
                &join,
                session::LiveDisplay {
                    display_id: 1,
                    device_name: "1".into(),
                    device_path: "p1".into(),
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
        let state = state_with(None, &["token-a", "token-b"]);
        assert!(!state.display_slot_unavailable(Some("token-c")));
        assert!(!state.display_slot_unavailable(None));
        assert_eq!(state.display_capacity(), (None, 2));
    }

    #[test]
    fn a_single_display_backend_turns_away_a_second_device() {
        let state = state_with(Some(1), &["token-a"]);
        assert!(state.display_slot_unavailable(Some("token-b")));
        assert_eq!(state.display_capacity(), (Some(1), 1));
    }

    #[test]
    fn the_device_already_holding_the_display_is_never_blocked() {
        // keyed by device token, so the same device reconnecting (a new tab, a new address) is
        // recognised as the one that already holds the slot
        let state = state_with(Some(1), &["token-a"]);
        assert!(!state.display_slot_unavailable(Some("token-a")));
    }

    #[test]
    fn the_slot_frees_up_when_the_holder_leaves() {
        let state = state_with(Some(1), &[]);
        assert!(!state.display_slot_unavailable(Some("token-b")));
        assert_eq!(state.display_capacity(), (Some(1), 0));
    }

    /// One display at a time is the macOS AirPlay tier's limit and nothing else's. A backend
    /// that reports any other finite cap is a bug, and the product rule is that the user is
    /// never turned away: the cap is dropped, the join goes ahead, and whatever really goes
    /// wrong reports itself. `at-capacity.html` must be unreachable from here.
    #[test]
    fn only_a_one_display_backend_can_turn_a_device_away() {
        for cap in [Some(2), Some(3), Some(8)] {
            let state = state_with(cap, &["token-a", "token-b", "token-c"]);
            assert!(
                !state.display_slot_unavailable(Some("token-d")),
                "cap {cap:?}: an uncapped backend must never serve at-capacity.html"
            );
            assert_eq!(
                state.display_capacity().0,
                None,
                "cap {cap:?}: must be reported as uncapped, so the dashboard never reads full"
            );
        }
    }

    #[test]
    fn the_holders_adapter_is_identifiable_for_keeping_its_server_up() {
        let sessions: session::SharedSessions = Default::default();
        session::set_host_ip(&sessions, "join-1", Some("192.168.1.5".into()));
        session::set_live_display(
            &sessions,
            "join-1",
            session::LiveDisplay {
                display_id: 1,
                device_name: "1".into(),
                device_path: "p1".into(),
                width: 1920,
                height: 1080,
                refresh: 60,
                scale: 100,
                portrait: false,
            },
        );
        session::set_host_ip(&sessions, "join-2", Some("192.168.1.9".into()));

        assert_eq!(
            session::display_holder_host_ip(&sessions),
            Some("192.168.1.5".into())
        );
    }

    #[test]
    fn no_adapter_is_singled_out_when_nobody_holds_a_display() {
        let sessions: session::SharedSessions = Default::default();
        session::set_host_ip(&sessions, "join-1", Some("192.168.1.5".into()));
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
            crate::test_support::skip_because(
                "a_snapped_ratio_can_be_expressed_as_a_display_scale",
                "platform::display_scale_steps() is empty on this OS",
            );
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
            device_path: "p1".into(),
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
        // the client asked for 1080x1920; the display is driven landscape and rotated, so the
        // geometry to compare against is the transposed one
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
        // Windows registers a mode list and the OS picks from it, so the backend does not know:
        // the default `None` must never let the mode set be skipped
        assert!(!would_skip(&Commanded, 1920, 1080, false));
    }
}
