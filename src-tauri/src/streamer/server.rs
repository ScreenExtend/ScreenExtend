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
    /// The session this tab already holds, when the client is renegotiating in place rather than
    /// joining fresh.
    ///
    /// A reconfigure — a display-affecting settings change, a rotation, a demotion to `<video>` —
    /// tears the peer connection down and offers again. Without this it minted a new session
    /// every time: a new join id, a *new virtual display*, a `DeviceRemove` and a `DeviceJoin` on
    /// the dashboard, and every window the user had on the old display shuffled back to the
    /// primary monitor. The id is a 128-bit capability the host handed this tab at `POST /whep`
    /// and is what `PATCH`/`DELETE /whep/{id}`, `/reconfig` and `/leave` are already addressed
    /// by, so presenting it is the same authority; it is honoured only when the session is still
    /// live and belongs to the same device token.
    #[serde(default, rename = "joinId")]
    join_id: String,
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

/// The fastest rate a client is given **by default**, whatever it says its own screen runs at.
///
/// The client measures its refresh rate and the host used to take that number at face value: a
/// browser on a 360 Hz laptop panel asked for 360, and the host duly built a 360 Hz virtual
/// display and configured the encoder for 360 fps. Three things go wrong at once, and they were
/// all visible on the Windows dev box. The compositor now has a second high-rate display to
/// compose on the same integrated GPU, and the *real* desktop's animation cadence collapsed from
/// 357 fps to 60. The encoder is asked for a frame budget it cannot meet at that geometry, so its
/// rate control spends the whole session at the wrong operating point. And the number is
/// persisted into the device row, so it survives every later join.
///
/// A second screen does not need to be faster than this. `MAX_REFRESH_RATE` is still what the
/// per-device setting may be raised to — that is a choice the user makes, once, in the UI, for a
/// device where it is worth it. This is only the ceiling on what a client may claim for itself.
pub const DEFAULT_MAX_AUTO_REFRESH: u32 = 60;

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

    /// Whether this join has to be turned away for want of a display slot.
    ///
    /// Only the one-at-a-time AirPlay tier can answer yes — see
    /// [`session::concurrent_display_cap`]. On every other backend this is `false` whatever the
    /// session map looks like, so a join that is going to fail fails with its own error rather
    /// than being reported as a full host.
    ///
    /// `holder` is the device token of whoever is asking, when it is known. A device that
    /// already holds the one display is never told the host is full — it is reconnecting to
    /// what it already has. `None` is the page load, which happens before any token is
    /// presented.
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

    /// The locality of a join, decided from where it arrived rather than from a parse.
    ///
    /// A join tunnelled through the cloud relay has no peer address at all: the relay forwards
    /// its own opaque client id (`c_Lu6CRIn28D7GxgD8qMpTow`), and [`JoinOrigin::relayed`] puts
    /// that in `peer_ip` for the log and the rate-limit bucket. It is not an address and never
    /// parses as one, so [`AppState::locality_for`] returned `Unknown` on its first line and
    /// every locality-driven decision took its `_` arm — which is the LAN arm. A relayed session
    /// was handed the same-subnet MTU, no playout delay and `PacingMode::Light`, on the one path
    /// in this app that is cross-network by definition. The running host said so itself, once per
    /// relayed join:
    ///
    /// ```text
    /// session transport: locality=Unknown mtu=1188 pacing=Light playout_delay_max=0ms
    /// session locality was pre-classified Unknown but the nominated candidate pair says
    /// CrossNetwork; transport constants for this session were chosen for the former
    /// ```
    ///
    /// The relay exists only to carry clients that cannot reach this host directly, so a relayed
    /// join is cross-network by construction. That is a fact about the route, not a guess about
    /// an address, and it is read here instead of being inferred.
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
    headers: HeaderMap,
) -> Response {
    // The page load carries no device token, but a device that is already connected does carry
    // its join cookie — enough for a reload of the one AirPlay display not to be told the host
    // is full.
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
/// `?join=<id>` is the mechanism. The `se_join` cookie is a bridge for a page that was loaded
/// before this host learned to hand the id out, and because a cookie is per-origin rather than
/// per-tab the explicit parameter always wins — two tabs share the jar and only the parameter
/// tells them apart.
///
/// There is deliberately no address fallback. An address is recycled between devices and moves
/// under a live session, so resolving a request by it is exactly how one device ends up driving
/// another's display.
fn request_join(query: &JoinQuery, headers: &HeaderMap) -> Option<String> {
    if valid_join_id(&query.join) {
        return Some(query.join.clone());
    }
    cookie_join(headers)
}

/// The session a request names, with its address label refreshed.
///
/// The refresh is the only thing an address is used for: a peer that roams keeps its session and
/// the dashboard stops showing where it used to be.
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
    axum::extract::Query(query): axum::extract::Query<JoinQuery>,
    headers: HeaderMap,
) -> Response {
    let body = match resolve_join(&state, &query, &headers, peer.ip()) {
        Some(join) => process_reconfig(&state, &join),
        // No session named, so nothing to report on. Stable zeros are what an unknown join has
        // always read, and the client treats them as "no change".
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
    #[serde(default)]
    supported: bool,
    #[serde(default)]
    outputs: Vec<AudioOutputEntry>,
    /// The sink the client is playing on right now, which is not always the one the host
    /// asked for: the user can change it in the browser's own output control. Optional, so a
    /// client built before the field still parses.
    #[serde(default)]
    current: Option<String>,
    /// Whether the labels in `outputs` are the operating system's or the client's own
    /// numbering. Optional, for the same reason `current` is.
    #[serde(default)]
    labelled: Option<bool>,
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
    let req: AudioOutputsRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(_) => return StatusCode::NO_CONTENT.into_response(),
    };
    let Some(join) = resolve_join(&state, &query, &headers, peer.ip()) else {
        return StatusCode::NO_CONTENT.into_response();
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

    if let Some(s) = state.config.sessions.as_ref() {
        session::set_audio_outputs(s, &join, req.supported, outputs, current, req.labelled);
        if let Some(reporter) = state.config.device_reporter.as_ref() {
            let token = session::device_token_for(s, &join).unwrap_or_default();
            let report = state.audio_outputs_report(&join);
            reporter.report_audio_outputs(join, token, report);
        }
    }
    StatusCode::NO_CONTENT.into_response()
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
/// The join id reaches a direct client in the answer's `Location`, and a relayed one only in the
/// `x-join-id` header this host added for it. The relay's deployed client page predates both, so
/// it polls `/reconfig` with no `?join=` and the epoch lookup was made against the empty string —
/// which matches no session and answers `{"epoch":0,"kick":0}` for the life of every relayed
/// session. A kick never arrived, and neither did a display-scale, orientation, refresh-rate or
/// audio-output change: the desktop UI's controls were inert for exactly the devices that joined
/// over the internet.
///
/// The relay does know which of its clients this is, and `relay_client` maps that onto the join
/// without touching an address — the same fallback `/leave` already takes. A client that does
/// send a join id is answered on it, so a page that learns the id loses nothing.
pub fn process_relay_reconfig(state: &AppState, join: &str, client_id: &str) -> String {
    if !join.is_empty() {
        return process_reconfig(state, join);
    }
    let resolved = state.config.sessions.as_ref().and_then(|s| {
        session::joins_for_relay_client(s, client_id)
            .into_iter()
            .next()
    });
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
    // `trickleIce` is how the client learns it may POST the offer before its own gathering has
    // finished and PATCH the candidates afterwards. Probed, never assumed: a client that does not
    // read it, or a host that does not send it, keeps the gather-then-post shape exactly.
    serde_json::json!({ "httpsPort": config.https_port, "trickleIce": true }).to_string()
}

async fn whep(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req_headers: HeaderMap,
    body: Bytes,
) -> Response {
    let client_ip = peer.ip().to_string();
    let presented = presented_device_token(&req_headers, &body);
    if state.display_slot_unavailable(Some(presented.as_str())) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "This Mac creates its extended display over AirPlay and can only drive one at a time. The device already connected keeps it — disconnect that device to free the slot, or use macOS 10.15 or later, where every device gets its own display.",
        )
            .into_response();
    }
    let ice = state.ice_for(state.locality_for(&client_ip));
    let out = process_whep(&state, JoinOrigin::direct(&client_ip), &body, ice).await;
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
        // The resource id *is* the session's name, so it is handed over under its own header
        // too: a client that only wanted to know which session it is need not parse a URL. The
        // cookie is the bridge for a page cached from before the id existed — per-origin, so
        // the explicit `?join=` parameter overrides it wherever two tabs are open.
        if let Ok(value) = header::HeaderValue::from_str(resource) {
            headers.insert("x-join-id", value);
        }
        if let Ok(value) = header::HeaderValue::from_str(&format!(
            "{JOIN_COOKIE}={resource}; Path=/; SameSite=Lax"
        )) {
            headers.insert(header::SET_COOKIE, value);
        }
    }
    (status, headers, out.body).into_response()
}

/// The device token a request presents, from the `X-Device-Token` header or the join body.
///
/// The header is what `PATCH`/`DELETE` carry; the join body is what `POST /whep` carries,
/// because the client has to send its whole join request anyway. Both are read here so the
/// at-capacity check can run before the body is parsed properly.
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

/// Trickled ICE from the client, RFC 9725 §4.6.
///
/// The point is what it removes from the join: without it the client cannot POST until its own
/// gathering has finished or timed out, so the offer it sends is the only one the host will ever
/// see. With TURN configured that wait runs until a relay candidate arrives; on a network where no
/// candidate arrives at all it burns the full deadline.
///
/// The resource id is a 128-bit capability, but it is handed out in a `Location` header that
/// travels in cleartext on the HTTP listener — the listener most LAN joins actually use. So the
/// id alone is not the authority: the **device token** that opened the session is checked too,
/// and the fragment count is bounded, because adding remote candidates is a media-redirection
/// primitive.
///
/// The second factor used to be the peer's address. It is the token now for the same reason the
/// rest of this change exists: an address moves under a live session (a roam, a lease renewal, a
/// NAT rebind), so checking it broke trickle for a peer that had done nothing wrong, and it is
/// recycled between devices, so it did not identify one anyway.
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
/// The id is resolved through the same registry `whep_patch` uses, and against the same device
/// token, so `DELETE /whep/<any string at all>` reaches nothing.
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

        // A device the host already holds settings for keeps its name. Minting here renamed a
        // device on every OTP join, and the per-device override map is keyed by that name — so
        // the rebuild a settings change triggers came back under a token with no settings, and
        // the change the user had just made was the one thing the reconnect did not carry.
        match state.config.device_overrides.as_ref() {
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

/// Whether this offer continues a session that is already open, or starts a new one.
///
/// The client presents the join id it holds when it is renegotiating in place — a display
/// setting moved, the device rotated, the video path stepped down a tier. Honouring it is what
/// keeps the virtual display, the dashboard row and the windows the user has arranged on that
/// display; minting a fresh id instead removed one display and built another, which is what the
/// owner saw as "the client disconnects and everything goes weird".
///
/// The id is a 128-bit capability the host itself handed that tab, and it is already what
/// `PATCH`/`DELETE /whep/{id}`, `/reconfig` and `/leave` are addressed by — but it is honoured
/// only for a session that is still open *and* belongs to the device token this join
/// authenticated as, so a leaked or stale id buys nothing. Anything else is a new join, which is
/// always safe: the worst case is the cost this change exists to avoid.
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

async fn start_session(
    state: &AppState,
    req: &JoinRequest,
    origin: JoinOrigin<'_>,
    device_token: &str,
    ice_servers: Vec<RTCIceServer>,
) -> Result<(String, String)> {
    let client_ip = origin.peer_ip;
    // One join, one name. The WHEP resource id *is* the session's identity, so `PATCH`,
    // `DELETE`, `/reconfig`, `/leave` and `/audio-outputs` all address the same thing, and two
    // tabs of one browser — same device, same token, same address — are two of them.
    //
    // A tab that is renegotiating presents the id it already holds, and then this is the *same*
    // session: the display it created stays up, the dashboard row stays put, and the sequence
    // number below is what tells the outgoing session's teardown task that it has been
    // superseded rather than ended. Only a live session owned by this same device qualifies —
    // an unknown id, or one belonging to another device, is a fresh join.
    let continuing = state
        .config
        .sessions
        .as_ref()
        .and_then(|s| continuing_join(s, &req.join_id, device_token));
    let join_id = match &continuing {
        Some(join) => {
            tprintln!("join {join} is renegotiating in place; keeping its session and display");
            join.clone()
        }
        None => session::mint_join_id(),
    };
    if let Some(s) = state.config.sessions.as_ref() {
        session::open_session_via(s, &join_id, device_token, client_ip, origin.relay_client);
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

    // The outgoing session's capture is still running against the display this one is about to
    // reuse. Its own teardown task will find the entry replaced and stand down, so stopping it
    // here is the only thing that does.
    if continuing.is_some() {
        if let Some(s) = state.config.sessions.as_ref() {
            if let Some(stop) = session::take_active_capture(s, &join_id) {
                tprintln!("stopping the previous capture for join {join_id} before renegotiating");
                let _ = tokio::task::spawn_blocking(stop).await;
            }
        }
    }

    let detected_refresh = if req.refresh_rate == 0 {
        60
    } else {
        req.refresh_rate
            .clamp(MIN_REFRESH_RATE, DEFAULT_MAX_AUTO_REFRESH)
    };
    if req.refresh_rate > detected_refresh {
        tprintln!(
            "client reports a {} Hz screen; streaming at {detected_refresh} Hz              (raise it for this device in Devices if you want more)",
            req.refresh_rate
        );
    }

    let mut cfg = state.config.as_ref().clone();
    cfg.max_fps = detected_refresh;
    // Rolling intra refresh is **off** on every live join, as it was before the overhaul.
    //
    // d996635 turned it on for every session so a recoverable loss would heal over a refresh
    // wave instead of costing an IDR, and measured that it did not change the keyframe rate. It
    // never looked at the picture. A refresh wave is a stripe of intra-coded macroblocks moving
    // through every frame (NVENC codes it continuously, `cnt = period - 1`), and in a flat
    // region — the grey right half of an editor — those intra blocks quantise chroma differently
    // from the inter-predicted blocks around them, so the stripe reads as a band of another tint
    // scanning a surface whose colour never changed. On the client that is green flicker over
    // plain grey. The probe harness's own `--intra-refresh` help text has said so since it was
    // written: "on = periodic refresh waves can be visible as bands repainting the image".
    // Loss recovery is back to PLI/FIR -> IDR through `KeyframeGate`, which is what shipped.
    // `SE_INTRA_REFRESH=1` opts a host in for a measurement.
    cfg.intra_refresh = matches!(
        std::env::var("SE_INTRA_REFRESH").as_deref(),
        Ok("1") | Ok("on") | Ok("true")
    );
    let device_settings = state
        .config
        .device_overrides
        .as_ref()
        .and_then(|o| o.lock_recover().get(device_token).copied());
    // Shared with the injector rather than copied into it, so `set_device_override` and
    // `set_device_pointer_scope` reach a session that is already running.
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
                link_loss: None, // wired in by `start_session`, which owns the BWE
                locality,
                hub: std::sync::Arc::clone(hub),
                overrides: std::sync::Arc::clone(overrides),
                device_token: device_token.to_string(),
            })
        }
        _ => None,
    };
    if let Some(o) = device_settings {
        cfg.scale = ScalePercent::new(o.video_scale);
        cfg.quality_qp = Some(o.video_quality);
        cfg.max_fps = o.refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE);
        // Whether `max_fps` is a ceiling or a promise. Off — the default — makes it a promise:
        // the pipeline holds the cadence and repeats an unchanged frame rather than letting the
        // rate sag. Copied here, once, so nothing downstream has to reach back into the
        // override map on a frame path.
        cfg.auto_frame_rate = o.auto_frame_rate;
    }

    // What the client's offer says it can decode, read before the encoder is built because the
    // profile is an encoder-session parameter and cannot be changed afterwards. Baseline was the
    // hardcoded answer to every client until now — CAVLC and no 8x8 transform, 5-15 % of the
    // bitrate given away on every session that could have taken High.
    let offered_video = negotiate::read_offer(&req.sdp);

    let native_dpr = if req.dpr.is_finite() { req.dpr } else { 1.0 };
    let dpr = platform::snap_display_dpr(device_settings.map(|o| o.dpr).unwrap_or(native_dpr));
    let backing = |css: u32| ((css as f64 * dpr).round() as u32).clamp(2, 16384) & !1;
    let width = backing(req.width);
    let height = backing(req.height);
    let stream_fps = if cfg.max_fps == 0 {
        60
    } else {
        cfg.max_fps.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE)
    };
    // The virtual display's refresh rate and the stream's frame rate are not the same number: an
    // indirect display has no hardware vblank, so the mode's refresh rate *is* DWM's compose
    // cadence and therefore the ceiling on how fresh any captured frame can be. At 60 Hz a frame
    // is on average 8.3 ms stale before capture even sees it; at 120 Hz, 4.2 ms.
    //
    // That is worth ~4 ms and it costs the compositor, the capture and the copy a doubling, so it
    // is only taken when the device asked for an adaptive rate. With `autoFrameRate` off — the
    // default — the display is created at exactly the rate the device asked for, and the pipeline
    // holds that cadence. The pipeline caps delivery at `max_fps` either way, so the extra
    // compose steps are only ever paid for freshness, never sent.
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

    // A fresh join has no display to reuse — that is what made a second tab replace the first
    // when sessions were keyed by address. A tab renegotiating on its own id does, and reusing it
    // is the whole point: the mode changes under a display that never goes away.
    let existing: Option<session::LiveDisplay> = match (&continuing, state.config.sessions.as_ref())
    {
        (Some(join), Some(s)) => session::get_live_display(s, join),
        _ => None,
    };
    let existed_before = existing.is_some();
    let client_portrait = height > width;
    // The saved orientation is honoured for as long as the device is holding the same session.
    // Overwriting it on every join, which is what happened while every join was new, made the
    // per-device orientation setting inert: it was saved, replayed and then immediately replaced
    // by whichever way round the phone happened to be. A device that has physically rotated
    // since its last offer is believed, because that is a statement about the hardware and not a
    // setting.
    let prev_client_portrait = existing.as_ref().map(|p| p.height > p.width);
    let device_rotated = prev_client_portrait == Some(!client_portrait);
    let honor_device = existing.is_none() || device_rotated;
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

    // Record what this join is actually running at, for a device the host holds no settings for
    // yet. Without it the override map has no row until the first save, so that save had nothing
    // to compare against and was classified as a change to everything — which is how renaming a
    // freshly joined device rebuilt its session. The values are exactly `Device::defaults`, which
    // is what the sheet is showing the user, so an untouched save now compares equal.
    if let Some(overrides) = state.config.device_overrides.as_ref() {
        overrides
            .lock_recover()
            .entry(device_token.to_string())
            .or_insert(session::DeviceOverride {
                orientation_portrait: client_portrait,
                refresh_rate: detected_refresh,
                dpr,
                ..session::DeviceOverride::join_defaults()
            });
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

    // The join used to be a strictly serial chain: create and attach the display, apply the mode,
    // settle, scale, start the pipeline, and only then negotiate. Nothing in the handshake needs
    // the pipeline until the RTP writer is spawned — the peer connection, the tracks,
    // `set_remote_description`, `create_answer` and ICE gathering all depend only on the offer, the
    // negotiated codec and the ICE server list — so the two run as concurrent futures and the
    // handshake term comes out of time-to-first-frame entirely. On a LAN that is 13-20 ms of a
    // ~1600 ms join; on a relayed path, where the host gather runs to its 1500 ms cap against a
    // 1500-2100 ms display term, it is most of the gather.
    // Whether this attempt is still the one the session is waiting on.
    //
    // Two saves in quick succession are two `POST /whep` for the same join, and the second
    // arrives while the first is still setting a mode and writing a DPI. Both then command the
    // same display, in an order nobody chose, and the loser's `start_on_monitor` builds an
    // encoder and a capture session for a mode the winner has already replaced. Each attempt
    // therefore checks its own sequence number at the points where it is about to spend
    // something, and the older one stands down — the same rule the teardown task already uses,
    // moved to where the work is.
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

    let t_display = std::time::Instant::now();
    let display_and_pipeline = async {
        let mut mode_applied = true;
        // What the display's DPI is actually at when this is done. Not always `scale`: a display
        // reports the band of steps it will take, so a DPR above that band lands on the closest
        // step, and the session has to record the truth or the next edit compares against a value
        // no display ever held. The same goes for the refresh rate: an IDD monitor's mode list is
        // fixed at create time, so a rate that was never registered is refused and the display
        // keeps the one it had.
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
                if display_changed {
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
                    // Only what actually landed gets a settle and a DPI write. Running both after
                    // a refused mode change spent four seconds of the client's reconnect budget
                    // polling a display that had not moved, and then wrote a DPI derived from a
                    // resolution the display was not at — with the session already reporting the
                    // old geometry back, because `mode_applied` is false. The session stays at the
                    // mode it had and says so.
                    if mode_applied {
                        wait_for_display_settle(
                            &target,
                            Some(monitor_dims(width, height, swap_axes)),
                        )
                        .await;
                        applied_scale = apply_display_scale(&target, scale)
                            .await
                            .unwrap_or(prev.scale);
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
                let (display_id, device_name, device_path, topology_reset) = {
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
                    // The identities attached before the create, so the one that turns up
                    // afterwards can be checked against them. A correlation that hands back a
                    // display which was already there is a mis-correlation, and recording it as
                    // ours would make every later ownership check agree with the mistake.
                    let paths_before =
                        tokio::task::spawn_blocking(pipeline::attached_display_paths)
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
                            // The identity, read back while the correlation lock still keeps
                            // another join from creating a display between the two reads. Every
                            // later write to this display resolves its name from this, and a
                            // display whose identity cannot be established is not one we are
                            // willing to command: it would be commanded by name, and a name is
                            // exactly what moves.
                            let ident = {
                                let n = name.clone();
                                tokio::task::spawn_blocking(move || {
                                    pipeline::claim_display_identity(&n, &paths_before)
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
                            (display_id, name, device_path, topology_reset)
                        }
                        None => {
                            // Still inside the correlation lock, so this is the unlocked form.
                            remove_display_now(client, display_id).await;
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
                applied_scale = apply_display_scale(&fresh, scale).await.unwrap_or(scale);
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
            let outcome = if mode_applied {
                ModeOutcome::Applied {
                    scale: applied_scale,
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
                    scale: applied_scale,
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

        let display_ms = t_display.elapsed().as_secs_f64() * 1000.0;

        if !still_current() {
            bail!(
                "join {join_id} was superseded by a newer offer before its capture started; \
                 standing down"
            );
        }

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
        crate::streamer::telemetry::publish(&join_id, Arc::clone(&session.pipeline.telemetry));

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
            if let Ok((display_id, _, session, _, _)) = display_res {
                session.stop();
                if !existed_before {
                    if let Some(s) = state.config.sessions.as_ref() {
                        forget_display(session::take_live_display(s, &join_id));
                    }
                    remove_display_async(client, display_id).await;
                }
            }
            abandon_failed_attempt(state, &join_id, still_current());
            return Err(e.context("WHEP handshake"));
        }
    };
    let (display_id, device_name, session, display_ms, pipeline_ms) = match display_res {
        Ok(v) => v,
        Err(e) => {
            pending.abandon().await;
            abandon_failed_attempt(state, &join_id, still_current());
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
            live: std::sync::Arc::clone(&live_input),
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
                    forget_display(session::take_live_display(s, &join_id));
                }
                remove_display_async(client, display_id).await;
            }
            abandon_failed_attempt(state, &join_id, still_current());
            return Err(e.context("WHEP activation"));
        }
    };

    tprintln!(
        "join stages ({locality:?}): display={display_ms:.0}ms pipeline={pipeline_ms:.0}ms handshake={handshake_ms:.0}ms (overlapped)"
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
        // A renegotiation is the row moving, not a device arriving. Reported as a join it was
        // indistinguishable from one: the dashboard replaced the row — correctly — and then
        // raised "a device joined" over the desktop, every time a refresh rate or a DPR changed.
        if continuing.is_some() {
            reporter.report_modify(info);
        } else {
            reporter.report_join(info);
        }
    }

    let leave = state
        .config
        .sessions
        .as_ref()
        .map(|s| session::arm_leave(s, &join_id));

    if let Some(s) = state.config.sessions.as_ref() {
        session::set_control_sender(s, &join_id, control_tx.clone());
    }

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
            session::clear_control_sender(s, &report_join, &control_tx);
        }
        crate::streamer::telemetry::unregister(&report_join);

        // Give back the peer connection, its ICE agent, that agent's UDP sockets and the
        // injector thread. Nothing did this before, and because the connection held an `Arc` to
        // itself through its own state-change handler, nothing could have.
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
            .map(|s| !session::is_current_session(s, &report_join, session_seq))
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
            forget_display(session::take_live_display(s, &report_join));
            // Keyed by join, the map gains an entry for every tab ever opened unless the last
            // teardown forgets it. Keyed by address it never had to.
            session::remove_session(s, &report_join);
        }
        remove_display_async(&client, display_id).await;
        if let Some(reporter) = reporter {
            reporter.report_remove(report_join, report_token);
        }
    });

    Ok((answer, resource_id))
}

/// Forget a session whose join attempt failed — unless a newer attempt has taken it over.
///
/// A failed attempt used to remove the session outright, which is right when it was the only one
/// and catastrophic when it was not: a second save arriving while the first renegotiation is
/// still running makes two attempts on one join, and the loser's failure would then delete the
/// entry the winner is about to record its display in. The session survives, the display it owns
/// survives with it, and nothing is left holding a display with no session to reap it.
fn abandon_failed_attempt(state: &AppState, join_id: &str, mine: bool) {
    let Some(s) = state.config.sessions.as_ref() else {
        return;
    };
    if !mine {
        tprintln!("join {join_id} failed but a newer offer owns it now; leaving the session alone");
        return;
    }
    session::remove_session(s, join_id);
}

/// What a mode change actually did, as opposed to what it was asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModeOutcome {
    /// The display took the mode. `scale` is the DPI it settled at and `refresh` the rate it is
    /// running at, neither of which is always the one requested: a display reports the band of
    /// DPI steps it will take, and an IDD monitor's mode list is fixed when it is created.
    Applied { scale: u32, refresh: u32 },
    /// The display refused it and is still where it was. `width`/`height` are what it reports
    /// now, `scale` the DPI it still holds and `refresh` the rate it kept — no DPI and no rate
    /// are written when the mode is refused.
    Refused {
        width: u32,
        height: u32,
        scale: u32,
        refresh: u32,
    },
}

/// What the session records after a mode change.
///
/// A refused mode leaves the display exactly where it was, and leaves its DPI alone with it. So
/// recording the *requested* geometry and the *requested* scale is not a small inaccuracy: it is
/// what makes the next edit compare equal, decide there is nothing to do, and leave the client
/// rendering at a mode and a ratio the display has never held — with no way back, because every
/// subsequent save agrees that the display is already there.
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

/// Stop treating a display as one of ours, so no later write can reach it.
///
/// The ownership set is what every mode and DPI write is checked against, and a display that has
/// been taken down is not one this host may command — its device path is gone and the GDI name it
/// held belongs to whatever attaches next.
fn forget_display(taken: Option<session::LiveDisplay>) {
    if let Some(d) = taken {
        if !d.device_path.is_empty() {
            pipeline::forget_display_identity(&d.device_path);
        }
    }
}

/// Remove a display, holding the correlation lock while it happens.
///
/// A create identifies its new display by diffing the OS's device-name list before and after.
/// Windows reuses a name the moment one is freed, so a removal that overlaps that window can
/// hand the *same* name back to the new display and the diff then finds nothing new — the join
/// waits out `DISPLAY_ATTACH_TIMEOUT` and fails, with a display created and orphaned. It could
/// not happen while a session was replaced by address, because the replacement reused the
/// display rather than creating a second one; now that every join is its own session, a client
/// that rejoins releases one display while asking for another, and the two overlap by
/// construction.
async fn remove_display_async(client: &session::SharedVirtualDisplay, id: u32) {
    let _guard = DISPLAY_CORRELATION_LOCK.lock().await;
    remove_display_now(client, id).await;
}

/// The same, for the one caller that is already inside the correlation lock.
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
        apply_display_scale(&target, display.scale).await;
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
/// `None` means nothing was written — the target could not be identified as one of ours — which
/// the caller must not paper over, because the alternative reading of a silent failure is
/// "applied", and that is what let the host tell a client a ratio the display was never set to.
/// A `Some` that is not the requested percentage is a display that will not go that far, and the
/// session records what it reached so the next edit compares against the truth.
async fn apply_display_scale(target: &session::DisplayTarget, percent: u32) -> Option<u32> {
    let t = target.clone();
    let name = target.device_name.clone();
    let scale = percent.clamp(MIN_DISPLAY_SCALE, MAX_EFFECTIVE_SCALE);
    let res =
        tokio::task::spawn_blocking(move || match pipeline::set_display_scale_owned(&t, scale) {
            Ok(applied) => Some(applied),
            Err(e) => {
                teprintln!("could not set scale for {name}: {e}");
                None
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

/// Wait for a display's reported geometry to stop moving after a mode change.
///
/// `expected` is what we asked for, in monitor coordinates. Passing it turns the common case —
/// the mode landed immediately — from "read, sleep 75 ms, read again, compare" into a single
/// read, which is 75 ms off every join that touches a display.
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

    /// One display-affecting edit, end to end across the two halves of the mechanism: the host
    /// bumps the epoch, the client offers again on the id it already holds, and the host
    /// recognises that offer as the same session.
    ///
    /// The three things asserted are the three the owner watched go wrong: the join id does not
    /// change, the display id does not change, and the mode change is one mode change rather
    /// than a removal and a create.
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

        // The desktop UI raised the device's refresh rate: a class-(d) edit, so every tab of
        // this device is told to renegotiate.
        assert_eq!(
            session::bump_reconfig_epoch_for_device(&sessions, &token),
            1
        );

        // The client answers by offering again and presenting the id it holds.
        let continued = continuing_join(&sessions, &join, &token);
        assert_eq!(
            continued.as_deref(),
            Some(join.as_str()),
            "a renegotiation must land on the session it came from, not on a new one"
        );

        // Which means `start_session` finds a display to reuse rather than creating one.
        let existing = session::get_live_display(&sessions, &join).expect("the display survives");
        assert_eq!(existing.display_id, before.display_id);

        // One mode change: the geometry the client asked for is unchanged, so only the refresh
        // rate moves and `display_params()` — what decides whether the display is touched at
        // all — reports exactly that difference.
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

        // And nothing ended: same join, same display id, no kick.
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
    /// created, so a DPR off that ladder — or one asked for after the client's viewport changed —
    /// is a `DISP_CHANGE_BADMODE` and nothing moves. Recording the requested geometry and scale
    /// anyway is what made that unrecoverable: the next save compares equal, decides there is
    /// nothing to do, and the client keeps rendering at a size the display never had.
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
        // A DPR change: twice the geometry, twice the scale.
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

        // The display refused it: it is still 1920x1080 and still at 100 %, and both halves of
        // that have to survive into the session or the retry never happens.
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

        // And a display that took the mode but would not scale past 175 % records 175 %, so the
        // next save at 200 % is still a change and is tried again.
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

        // Once the session is gone the id is worth nothing, which is what makes a stale id in a
        // reloaded tab harmless.
        session::remove_session(&sessions, &join);
        assert_eq!(continuing_join(&sessions, &join, &mine), None);
    }

    /// A class-(c) change is visible in `/reconfig` without moving either epoch, which is the
    /// whole reason it costs no renegotiation.
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
    fn a_relayed_join_is_cross_network_however_its_client_id_is_spelled() {
        // The regression this pins: the relay forwards an opaque client id, never an address,
        // and classifying a join by parsing that id put every cross-network client on the
        // same-subnet transport constants. The id's spelling is the relay's business — it has
        // been `c-1` and `c_Lu6CRIn28D7GxgD8qMpTow` — so nothing here may depend on it.
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
        // `cloud.rs` builds the relayed list as `ice_with_turn(fallback_ice_servers())`, and the
        // direct handler builds it as `ice_for(locality)`. Now that a relayed join classifies as
        // cross-network the two must agree, or a tunnelled client and a direct one would gather
        // against different servers.
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
        // The relay's deployed client page polls `/reconfig` with no `?join=`, so the lookup was
        // made against "" — which matches nothing and answers epoch 0 / kick 0 for ever. A kick
        // raised in the desktop UI never reached a device that joined over the internet.
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

        // A client that does know its join id is still answered on it, not on the fallback.
        let named: serde_json::Value =
            serde_json::from_str(&process_relay_reconfig(&state, &join, "c-7")).expect("json");
        assert_eq!(named["kick"], 1);

        // An unknown relay client is not silently mapped onto somebody else's session.
        let stranger: serde_json::Value =
            serde_json::from_str(&process_relay_reconfig(&state, "", "c-99")).expect("json");
        assert_eq!(stranger["kick"], 0);
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
        // Keyed by device token, so the same device reconnecting — in a new tab, from a new
        // address — is recognised as the one that already holds the slot.
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
