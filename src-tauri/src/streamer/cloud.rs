use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::streamer::config::Config;
use crate::streamer::lock::LockExt;
use crate::streamer::server::AppState;
use crate::streamer::webrtc_session::RTCIceServer;

#[allow(dead_code)]
pub const CLOUD_DOMAIN: &str = "session.screenextend.app";
pub const CLOUD_WS_URL: &str = "wss://session.screenextend.app/host/v1/connect";
pub const RELAY_PROTOCOL_VERSION: u32 = 1;

const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_BACKOFF_SECS: u64 = 30;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCapabilities {
    pub webcodecs: bool,
    #[serde(rename = "h264Profiles")]
    pub h264_profiles: Vec<String>,
}

impl Default for HostCapabilities {
    fn default() -> Self {
        Self {
            webcodecs: true,
            h264_profiles: vec!["baseline".into(), "main".into(), "high".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostToRelay {
    Register {
        protocol: u32,
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "hostVersion")]
        host_version: String,
        #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
        display_name: Option<String>,
        capabilities: HostCapabilities,
    },
    SignalResponse {
        #[serde(rename = "requestId")]
        request_id: String,
        status: u16,
        headers: HashMap<String, String>,
        body: String,
    },
    Unregister {
        #[serde(rename = "sessionId")]
        session_id: String,
    },
    Pong {
        ts: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RemoteHint {
    #[serde(rename = "ipHash", default)]
    pub ip_hash: String,
    #[serde(rename = "geoHint", default)]
    pub geo_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IceServerWire {
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}

impl IceServerWire {
    fn into_rtc(self) -> RTCIceServer {
        RTCIceServer {
            urls: self.urls,
            username: self.username,
            credential: self.credential,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum RelayToHost {
    Registered {
        #[serde(rename = "sessionId")]
        session_id: String,
        #[serde(rename = "joinUrl")]
        join_url: String,
        #[serde(rename = "heartbeatSec", default)]
        heartbeat_sec: u32,
    },
    RegisterError {
        #[serde(rename = "sessionId", default)]
        session_id: String,
        code: String,
        #[serde(default)]
        message: String,
    },
    SignalRequest {
        #[serde(rename = "requestId")]
        request_id: String,
        #[serde(rename = "sessionId", default)]
        session_id: String,
        #[serde(rename = "clientId")]
        client_id: String,
        method: String,
        path: String,
        #[serde(default)]
        query: String,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        body: String,
        #[serde(rename = "iceServers", default)]
        ice_servers: Vec<IceServerWire>,
        #[serde(default)]
        remote: RemoteHint,
    },
    ClientGone {
        #[serde(rename = "clientId")]
        client_id: String,
    },
    Ping {
        ts: u64,
    },
    Shutdown {
        #[serde(default)]
        reason: String,
        #[serde(rename = "reconnectAfterMs", default)]
        reconnect_after_ms: u64,
    },
}

#[allow(dead_code)]
pub mod register_error_code {
    pub const SESSION_TAKEN: &str = "session_taken";
    pub const INVALID_SESSION: &str = "invalid_session";
    pub const VERSION_UNSUPPORTED: &str = "version_unsupported";
    pub const RATE_LIMITED: &str = "rate_limited";
    pub const UNAUTHORIZED: &str = "unauthorized";
}

/// A registration the relay refused, carrying the code as a value rather than as spelling.
///
/// The reconnect loop used to decide whether to rotate the user's join code by asking whether the
/// error's *formatted text* contained `SESSION_TAKEN`. Nothing enforced that spelling: one
/// `format!` that echoed a relay message would have rotated a working session id out from under
/// the user, and the join code on screen would silently stop matching the one the client has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterRejected {
    pub code: String,
}

impl std::fmt::Display for RegisterRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "register_error: {}", self.code)
    }
}

impl std::error::Error for RegisterRejected {}

impl RegisterRejected {
    pub fn is_session_taken(&self) -> bool {
        self.code == register_error_code::SESSION_TAKEN
    }
}

/// Whether this error is the relay saying the session id is already registered elsewhere.
fn is_session_taken(err: &anyhow::Error) -> bool {
    err.chain()
        .filter_map(|e| e.downcast_ref::<RegisterRejected>())
        .any(RegisterRejected::is_session_taken)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CloudState {
    Disabled,
    Connecting,
    Registered,
    Offline,
    Error,
}

impl CloudState {
    pub fn as_str(self) -> &'static str {
        match self {
            CloudState::Disabled => "disabled",
            CloudState::Connecting => "connecting",
            CloudState::Registered => "registered",
            CloudState::Offline => "offline",
            CloudState::Error => "error",
        }
    }
}

pub trait CloudStatusSink: Send + Sync + std::fmt::Debug {
    fn report(&self, state: CloudState, detail: String);
    fn session_rotated(&self, session_id: String);
}

pub type SharedCloudStatusSink = Arc<dyn CloudStatusSink>;

fn generate_session_id() -> String {
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    (0..12)
        .map(|_| ALPHABET[(rand::random::<u8>() as usize) % ALPHABET.len()] as char)
        .collect()
}

#[derive(Debug, Clone)]
pub struct CloudConfig {
    pub ws_url: String,
    pub session_id: String,
    pub host_version: String,
    pub display_name: Option<String>,
}

impl CloudConfig {
    pub fn new(session_id: String) -> Self {
        Self {
            ws_url: CLOUD_WS_URL.to_string(),
            session_id,
            host_version: env!("CARGO_PKG_VERSION").to_string(),
            display_name: None,
        }
    }
}

pub struct CloudClient {
    stop: Arc<Notify>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CloudClient {
    pub fn spawn(config: CloudConfig, server_config: Config, sink: SharedCloudStatusSink) -> Self {
        let stop = Arc::new(Notify::new());
        let stop_for_thread = stop.clone();
        let sink_for_error = Arc::clone(&sink);
        let handle = std::thread::Builder::new()
            .name("cloud-relay-client".to_string())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .on_thread_start(super::platform::tune_transport_thread)
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        teprintln!("[cloud] failed to build runtime: {e}");
                        return;
                    }
                };
                rt.block_on(async move {
                    tokio::select! {
                        _ = run_loop(config, server_config, sink) => {}
                        _ = stop_for_thread.notified() => {
                            tprintln!("[cloud] stop requested; shutting down relay client");
                        }
                    }
                });
            });
        let handle = match handle {
            Ok(h) => Some(h),
            Err(e) => {
                // Discarding this left `handle` as `None`, so `stop()` silently did nothing and
                // the desktop UI sat on the "connecting" state it was given at startup forever,
                // with no line anywhere saying why.
                teprintln!("[cloud] could not start the relay client thread: {e}");
                sink_for_error.report(
                    CloudState::Error,
                    format!("relay client did not start: {e}"),
                );
                None
            }
        };
        Self { stop, handle }
    }

    pub fn stop(&mut self) {
        self.stop.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for CloudClient {
    fn drop(&mut self) {
        self.stop();
    }
}

fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

async fn run_loop(mut config: CloudConfig, server_config: Config, sink: SharedCloudStatusSink) {
    let session_auth = server_config.session_auth.clone();
    let state = Arc::new(AppState::new(server_config));
    let mut backoff_secs = 1u64;

    loop {
        sink.report(
            CloudState::Connecting,
            format!("Contacting relay {}…", config.ws_url),
        );
        match connect_and_serve(&config, &state, &sink).await {
            Ok(()) => {
                tprintln!("[cloud] relay connection closed; reconnecting");
                backoff_secs = 1;
            }
            Err(e) => {
                if is_session_taken(&e) {
                    let new_id = generate_session_id();
                    tprintln!(
                        "[cloud] session id '{}' still held by relay; rotating to '{}'",
                        config.session_id,
                        new_id
                    );
                    config.session_id = new_id.clone();
                    if let Some(auth) = session_auth.as_ref() {
                        *auth.session_id.lock_recover() = new_id.clone();
                    }
                    sink.session_rotated(new_id);
                    backoff_secs = 1;
                } else {
                    teprintln!("[cloud] relay connection error: {e:#}");
                    sink.report(CloudState::Offline, format!("Relay unreachable: {e}"));
                }
            }
        }

        let jitter_ms = rand::random::<u64>() % 1000;
        tokio::time::sleep(Duration::from_millis(backoff_secs * 1000 + jitter_ms)).await;
        backoff_secs = next_backoff_secs(backoff_secs);
    }
}

/// The reconnect schedule: double, then hold at [`MAX_BACKOFF_SECS`].
///
/// Extracted so the schedule can be asserted without a relay and without waiting for it. The
/// caller adds up to a second of jitter on top, which is what stops every host that lost the
/// same relay from reconnecting in lockstep.
fn next_backoff_secs(current: u64) -> u64 {
    current.saturating_mul(2).min(MAX_BACKOFF_SECS)
}

async fn connect_and_serve(
    config: &CloudConfig,
    state: &Arc<AppState>,
    sink: &SharedCloudStatusSink,
) -> Result<()> {
    install_crypto_provider();

    tprintln!(
        "[cloud] connecting to relay {} (protocol v{})",
        config.ws_url,
        RELAY_PROTOCOL_VERSION
    );
    let (ws, _resp) = tokio_tungstenite::connect_async(&config.ws_url)
        .await
        .map_err(|e| anyhow!("connect {}: {e}", config.ws_url))?;
    let (mut write, mut read) = ws.split();

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let register = HostToRelay::Register {
        protocol: RELAY_PROTOCOL_VERSION,
        session_id: config.session_id.clone(),
        host_version: config.host_version.clone(),
        display_name: config.display_name.clone(),
        capabilities: HostCapabilities::default(),
    };
    tx.send(Message::Text(serde_json::to_string(&register)?))
        .map_err(|_| anyhow!("failed to queue register message"))?;

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write.send(msg).await.is_err() {
                break;
            }
        }
    });

    let result = read_loop(state, sink, &tx, &mut read).await;

    writer.abort();
    result
}

type WsRead = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
>;

async fn read_loop(
    state: &Arc<AppState>,
    sink: &SharedCloudStatusSink,
    tx: &mpsc::UnboundedSender<Message>,
    read: &mut WsRead,
) -> Result<()> {
    loop {
        let next = tokio::time::timeout(HEARTBEAT_TIMEOUT, read.next()).await;
        let msg = match next {
            Err(_) => bail!("relay heartbeat timeout (no frame in {HEARTBEAT_TIMEOUT:?})"),
            Ok(None) => return Ok(()),
            Ok(Some(Err(e))) => bail!("websocket error: {e}"),
            Ok(Some(Ok(m))) => m,
        };

        match msg {
            Message::Text(text) => {
                let parsed: RelayToHost = match serde_json::from_str(text.as_str()) {
                    Ok(p) => p,
                    Err(e) => {
                        teprintln!("[cloud] ignoring malformed relay frame: {e}");
                        continue;
                    }
                };
                handle_relay_message(state, sink, tx, parsed)?;
            }
            Message::Ping(payload) => {
                let _ = tx.send(Message::Pong(payload));
            }
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
}

fn handle_relay_message(
    state: &Arc<AppState>,
    sink: &SharedCloudStatusSink,
    tx: &mpsc::UnboundedSender<Message>,
    msg: RelayToHost,
) -> Result<()> {
    match msg {
        RelayToHost::Registered { join_url, .. } => {
            tprintln!("[cloud] registered; clients can join at {join_url}");
            sink.report(CloudState::Registered, format!("Registered · {join_url}"));
        }
        RelayToHost::RegisterError { code, message, .. } => {
            teprintln!("[cloud] registration rejected ({code}): {message}");
            sink.report(
                CloudState::Error,
                format!("Registration rejected: {code} {message}"),
            );
            return Err(RegisterRejected { code }.into());
        }
        RelayToHost::SignalRequest {
            request_id,
            client_id,
            method,
            path,
            query,
            body,
            ice_servers,
            ..
        } => {
            // The relay splits the client's URL into `path` and `query`. `dispatch` matches the
            // route on the path and reads `?join=` from the query, so the two are put back
            // together here; dropping the query is what made every relayed `/audio-outputs`
            // (and a `/leave` naming its join) arrive with no join to attach to.
            let path = request_path(&path, &query);
            let state = state.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let resp = dispatch(
                    &state,
                    request_id,
                    &client_id,
                    &method,
                    &path,
                    &body,
                    ice_servers,
                )
                .await;
                match serde_json::to_string(&resp) {
                    Ok(text) => {
                        // The writer loop breaks on its first send error, so a dropped
                        // `signal_response` is a join that hangs with nothing logged.
                        if tx.send(Message::Text(text)).is_err() {
                            teprintln!(
                                "[cloud] relay writer is gone; the WHEP answer for {client_id} was not delivered"
                            );
                        }
                    }
                    Err(e) => teprintln!("[cloud] failed to serialize signal_response: {e}"),
                }
            });
        }
        RelayToHost::ClientGone { client_id } => {
            crate::streamer::server::process_relay_client_gone(state, &client_id);
        }
        RelayToHost::Ping { ts } => {
            if let Ok(text) = serde_json::to_string(&HostToRelay::Pong { ts }) {
                let _ = tx.send(Message::Text(text));
            }
        }
        RelayToHost::Shutdown { reason, .. } => {
            tprintln!("[cloud] relay draining ({reason}); reconnecting");
            bail!("relay shutdown: {reason}");
        }
    }
    Ok(())
}

/// The relay sends the client's URL as separate `path` and `query` fields; `dispatch` matches
/// the route on the path and reads `?join=` from the query, so the two are rejoined first.
fn request_path(path: &str, query: &str) -> String {
    if query.is_empty() {
        path.to_string()
    } else {
        format!("{path}?{query}")
    }
}

async fn dispatch(
    state: &AppState,
    request_id: String,
    client_id: &str,
    method: &str,
    path: &str,
    body: &str,
    ice_servers: Vec<IceServerWire>,
) -> HostToRelay {
    use crate::streamer::server;

    // The relay hands the path through as the client wrote it, so a `?join=` rides along with
    // it and has to be split off before the route is matched.
    let (route, query) = match path.split_once('?') {
        Some((r, q)) => (r, q),
        None => (path, ""),
    };
    let relay_join = query
        .split('&')
        .filter_map(|pair| pair.strip_prefix("join="))
        .next()
        .unwrap_or_default()
        .to_string();

    let (status, content_type, out_body, device_token, join_id): (
        u16,
        &'static str,
        String,
        Option<String>,
        Option<String>,
    ) = match (method, route) {
        ("POST", "/whep") => {
            let base: Vec<RTCIceServer> = if ice_servers.is_empty() {
                state.fallback_ice_servers()
            } else {
                ice_servers
                    .into_iter()
                    .map(IceServerWire::into_rtc)
                    .collect()
            };
            let ice = state.ice_with_turn(base);
            let r = server::process_whep(
                state,
                server::JoinOrigin::relayed(client_id),
                body.as_bytes(),
                ice,
            )
            .await;
            (r.status, r.content_type, r.body, r.device_token, r.resource)
        }
        ("GET", "/reconfig") => (
            200,
            "application/json",
            server::process_relay_reconfig(state, &relay_join, client_id),
            None,
            None,
        ),
        ("POST", "/leave") => {
            if relay_join.is_empty() {
                // A client that names no join is one that predates the id. The relay knows
                // which of its clients this is, and that maps onto joins without ever touching
                // an address.
                server::process_relay_client_gone(state, client_id);
            } else {
                server::process_leave(state, &relay_join);
            }
            (204, "text/plain", String::new(), None, None)
        }
        // The live relay is not known to forward this today (its page is older, and the routes
        // above are what it was measured to tunnel). Handled anyway, so a relay or page that
        // does send it gets the device's speakers onto the host's sheet instead of a 404; a
        // body that names no join has nothing to attach to.
        ("POST", "/audio-outputs") => {
            if !relay_join.is_empty() {
                server::process_audio_outputs(state, &relay_join, body.as_bytes());
            }
            (204, "text/plain", String::new(), None, None)
        }
        _ => {
            teprintln!("[cloud] unsupported tunneled request {method} {path}");
            (404, "text/plain", "not found".to_string(), None, None)
        }
    };

    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), content_type.to_string());
    if let Some(token) = device_token {
        headers.insert("x-device-token".to_string(), token);
    }
    // The relay has no URL space, so there is no `Location` to read the id out of; the header is
    // how a tunnelled client learns which session it is.
    if let Some(join) = join_id {
        headers.insert("x-join-id".to_string(), join);
    }
    HostToRelay::SignalResponse {
        request_id,
        status,
        headers,
        body: out_body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The relay is a separate service that this repo does not build, so the wire format is a
    // contract with something nothing here can compile against. Every field name below is the
    // JSON key the relay reads or writes; renaming one is a silent protocol break — serde will
    // happily serialise the new name and the relay will read a default.

    fn field<'a>(v: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
        v.get(key).unwrap_or_else(|| panic!("no `{key}` in {v}"))
    }

    #[test]
    fn the_relays_query_field_is_rejoined_to_the_path_before_dispatch() {
        assert_eq!(
            request_path("/audio-outputs", "join=abc"),
            "/audio-outputs?join=abc"
        );
        assert_eq!(request_path("/leave", ""), "/leave");
    }

    #[test]
    fn register_carries_the_protocol_version_and_camel_case_keys() {
        let msg = HostToRelay::Register {
            protocol: RELAY_PROTOCOL_VERSION,
            session_id: "ABCDEFGHJKLM".into(),
            host_version: "0.6.3".into(),
            display_name: Some("Studio PC".into()),
            capabilities: HostCapabilities::default(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(field(&v, "type"), "register");
        assert_eq!(field(&v, "protocol"), 1);
        assert_eq!(field(&v, "sessionId"), "ABCDEFGHJKLM");
        assert_eq!(field(&v, "hostVersion"), "0.6.3");
        assert_eq!(field(&v, "displayName"), "Studio PC");
        assert_eq!(field(field(&v, "capabilities"), "webcodecs"), true);
        assert!(field(field(&v, "capabilities"), "h264Profiles")
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "baseline"));
    }

    #[test]
    fn an_absent_display_name_is_omitted_rather_than_sent_as_null() {
        // `skip_serializing_if` is what keeps a host with no name from advertising `null` as
        // one; the relay treats a present key as a name.
        let msg = HostToRelay::Register {
            protocol: RELAY_PROTOCOL_VERSION,
            session_id: "ABCDEFGHJKLM".into(),
            host_version: "0.6.3".into(),
            display_name: None,
            capabilities: HostCapabilities::default(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert!(v.get("displayName").is_none(), "{v}");
    }

    #[test]
    fn a_signal_response_carries_the_request_id_it_answers() {
        // The relay multiplexes every client's WHEP round trip over one socket, so a response
        // with the wrong id is a client that never gets an answer.
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/sdp".to_string());
        let msg = HostToRelay::SignalResponse {
            request_id: "req-42".into(),
            status: 200,
            headers,
            body: "v=0\r\n".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&msg).unwrap()).unwrap();
        assert_eq!(field(&v, "type"), "signal_response");
        assert_eq!(field(&v, "requestId"), "req-42");
        assert_eq!(field(&v, "status"), 200);
        assert_eq!(
            field(field(&v, "headers"), "content-type"),
            "application/sdp"
        );
    }

    #[test]
    fn unregister_and_pong_keep_their_snake_case_tags() {
        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&HostToRelay::Unregister {
                session_id: "ABCDEFGHJKLM".into(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(field(&v, "type"), "unregister");
        assert_eq!(field(&v, "sessionId"), "ABCDEFGHJKLM");

        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&HostToRelay::Pong { ts: 17 }).unwrap())
                .unwrap();
        assert_eq!(field(&v, "type"), "pong");
        assert_eq!(field(&v, "ts"), 17);
    }

    #[test]
    fn every_relay_frame_this_host_understands_parses() {
        // Exactly the frames the relay sends, spelled as it sends them.
        let frames = [
            r#"{"type":"registered","sessionId":"ABCDEFGHJKLM","joinUrl":"https://session.screenextend.app/j/ABCDEFGHJKLM","heartbeatSec":20}"#,
            r#"{"type":"register_error","sessionId":"ABCDEFGHJKLM","code":"session_taken","message":"held by another host"}"#,
            r#"{"type":"client_gone","clientId":"c-9"}"#,
            r#"{"type":"ping","ts":1710000000}"#,
            r#"{"type":"shutdown","reason":"maintenance","reconnectAfterMs":5000}"#,
        ];
        for raw in frames {
            serde_json::from_str::<RelayToHost>(raw)
                .unwrap_or_else(|e| panic!("relay frame did not parse: {raw}\n{e}"));
        }
    }

    #[test]
    fn a_signal_request_parses_with_its_ice_servers_and_remote_hint() {
        let raw = r#"{
            "type":"signal_request","requestId":"req-1","sessionId":"ABCDEFGHJKLM",
            "clientId":"c-1","method":"POST","path":"/whep","query":"",
            "headers":{"content-type":"application/json"},"body":"{}",
            "iceServers":[{"urls":["turn:relay.example:3478"],"username":"u","credential":"c"}],
            "remote":{"ipHash":"deadbeef","geoHint":"EU"}
        }"#;
        match serde_json::from_str::<RelayToHost>(raw).expect("signal_request") {
            RelayToHost::SignalRequest {
                request_id,
                client_id,
                path,
                ice_servers,
                remote,
                ..
            } => {
                assert_eq!(request_id, "req-1");
                assert_eq!(client_id, "c-1");
                assert_eq!(path, "/whep");
                assert_eq!(ice_servers.len(), 1);
                let rtc = ice_servers.into_iter().next().unwrap().into_rtc();
                assert_eq!(rtc.urls, vec!["turn:relay.example:3478".to_string()]);
                assert_eq!(rtc.username, "u");
                assert_eq!(rtc.credential, "c");
                assert_eq!(remote.geo_hint, "EU");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_minimal_signal_request_parses_with_defaults() {
        // Everything but the ids and the method/path is `#[serde(default)]` on purpose: a relay
        // that stops sending an optional field must not take the host offline.
        let raw = r#"{"type":"signal_request","requestId":"r","clientId":"c","method":"GET","path":"/ice-config"}"#;
        match serde_json::from_str::<RelayToHost>(raw).expect("minimal signal_request") {
            RelayToHost::SignalRequest {
                query,
                headers,
                body,
                ice_servers,
                remote,
                ..
            } => {
                assert!(query.is_empty());
                assert!(headers.is_empty());
                assert!(body.is_empty());
                assert!(ice_servers.is_empty());
                assert!(remote.ip_hash.is_empty());
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn an_unknown_frame_type_is_an_error_the_read_loop_can_skip() {
        // The read loop logs and continues on a parse failure rather than dropping the
        // connection, which is what lets the relay add a frame type before the host knows it.
        assert!(serde_json::from_str::<RelayToHost>(r#"{"type":"whats_this","x":1}"#).is_err());
        assert!(serde_json::from_str::<RelayToHost>("not json at all").is_err());
    }

    #[test]
    fn the_session_taken_code_is_the_one_the_reconnect_loop_matches_on() {
        // The reconnect loop rotates the session id off `RegisterRejected`, whose `code` is this
        // constant, so the constant and the wire spelling have to be the same thing. The three
        // tests at the end of this module cover the matching; this one covers the parsing.
        let raw = r#"{"type":"register_error","code":"session_taken","message":""}"#;
        match serde_json::from_str::<RelayToHost>(raw).expect("register_error") {
            RelayToHost::RegisterError { code, .. } => {
                assert_eq!(code, register_error_code::SESSION_TAKEN);
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn the_backoff_doubles_and_then_holds() {
        // A relay that is down stays down; the schedule has to stop growing or a host that lost
        // it overnight comes back hours late.
        let mut secs = 1u64;
        let mut seen = vec![secs];
        for _ in 0..10 {
            secs = next_backoff_secs(secs);
            seen.push(secs);
        }
        assert_eq!(&seen[..6], &[1, 2, 4, 8, 16, 30]);
        assert!(seen.iter().all(|s| *s <= MAX_BACKOFF_SECS));
        assert_eq!(*seen.last().unwrap(), MAX_BACKOFF_SECS);
        // And it never overflows back to a tight loop, whatever it is handed.
        assert_eq!(next_backoff_secs(u64::MAX), MAX_BACKOFF_SECS);
        assert_eq!(next_backoff_secs(0), 0);
    }

    #[test]
    fn the_heartbeat_timeout_is_longer_than_the_relays_own_interval() {
        // The relay advertises `heartbeatSec` (20 s in practice) and the host bails when no
        // frame arrives inside this window. Making them equal would drop a healthy connection
        // on the first late ping.
        assert!(HEARTBEAT_TIMEOUT >= Duration::from_secs(40));
        assert!(HEARTBEAT_TIMEOUT > Duration::from_secs(MAX_BACKOFF_SECS));
    }

    #[test]
    fn a_generated_session_id_has_no_ambiguous_characters() {
        // The id is read off a QR code or typed from a screen. `0`/`O` and `1`/`I`/`l` are the
        // pairs people get wrong, and a mistyped id is an unexplained join failure.
        for _ in 0..64 {
            let id = generate_session_id();
            assert_eq!(id.len(), 12);
            for c in id.chars() {
                assert!(c.is_ascii_uppercase() || c.is_ascii_digit(), "{id}");
                assert!(!"01IO".contains(c), "{id} contains an ambiguous character");
            }
        }
        assert_ne!(generate_session_id(), generate_session_id());
    }

    #[test]
    fn the_cloud_state_strings_are_the_ones_the_ui_switches_on() {
        // These reach the desktop UI through `CloudStatusChange` and are compared as strings.
        assert_eq!(CloudState::Disabled.as_str(), "disabled");
        assert_eq!(CloudState::Connecting.as_str(), "connecting");
        assert_eq!(CloudState::Registered.as_str(), "registered");
        assert_eq!(CloudState::Offline.as_str(), "offline");
        assert_eq!(CloudState::Error.as_str(), "error");
    }

    #[test]
    fn the_relay_url_is_a_wss_url_on_the_documented_domain() {
        assert!(CLOUD_WS_URL.starts_with("wss://"), "{CLOUD_WS_URL}");
        assert!(CLOUD_WS_URL.contains(CLOUD_DOMAIN), "{CLOUD_WS_URL}");
        assert!(
            CLOUD_WS_URL.contains(&format!("/v{RELAY_PROTOCOL_VERSION}/")),
            "the path should carry the protocol version the register frame claims"
        );
    }

    #[test]
    fn the_relays_session_taken_code_is_recognised_through_context() {
        let e: anyhow::Error = RegisterRejected {
            code: register_error_code::SESSION_TAKEN.to_string(),
        }
        .into();
        assert!(is_session_taken(&e.context("relay handshake")));
    }

    #[test]
    fn another_register_error_does_not_rotate_the_session_id() {
        let e: anyhow::Error = RegisterRejected {
            code: register_error_code::RATE_LIMITED.to_string(),
        }
        .into();
        assert!(!is_session_taken(&e));
    }

    #[test]
    fn an_unrelated_error_that_merely_spells_the_code_does_not_rotate_it() {
        // The exact regression the typed error exists to prevent: any future `format!` that
        // echoes a relay message used to be enough to rotate a working join code.
        let e = anyhow!("websocket error: relay said session_taken about somebody else");
        assert!(!is_session_taken(&e));
        assert!(e.to_string().contains(register_error_code::SESSION_TAKEN));
    }
}
