use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tokio_tungstenite::tungstenite::Message;

use crate::streamer::config::Config;
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
    Pong { ts: u64 },
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
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
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
    Ping { ts: u64 },
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
}

pub type SharedCloudStatusSink = Arc<dyn CloudStatusSink>;

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
    pub fn spawn(
        config: CloudConfig,
        server_config: Config,
        sink: SharedCloudStatusSink,
    ) -> Self {
        let stop = Arc::new(Notify::new());
        let stop_for_thread = stop.clone();
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
            })
            .ok();
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

async fn run_loop(config: CloudConfig, server_config: Config, sink: SharedCloudStatusSink) {
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
                teprintln!("[cloud] relay connection error: {e:#}");
                sink.report(CloudState::Offline, format!("Relay unreachable: {e}"));
            }
        }

        let jitter_ms = rand::random::<u64>() % 1000;
        tokio::time::sleep(Duration::from_millis(backoff_secs * 1000 + jitter_ms)).await;
        backoff_secs = (backoff_secs * 2).min(MAX_BACKOFF_SECS);
    }
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
    tx.send(Message::Text(serde_json::to_string(&register)?.into()))
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
                if let Some(()) = handle_relay_message(state, sink, tx, parsed)? {}
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
) -> Result<Option<()>> {
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
            bail!("register_error: {code}");
        }
        RelayToHost::SignalRequest {
            request_id,
            client_id,
            method,
            path,
            body,
            ice_servers,
            ..
        } => {
            let state = state.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let resp =
                    dispatch(&state, request_id, &client_id, &method, &path, &body, ice_servers)
                        .await;
                match serde_json::to_string(&resp) {
                    Ok(text) => {
                        let _ = tx.send(Message::Text(text.into()));
                    }
                    Err(e) => teprintln!("[cloud] failed to serialize signal_response: {e}"),
                }
            });
        }
        RelayToHost::ClientGone { client_id } => {
            crate::streamer::server::process_leave(state, &client_id);
        }
        RelayToHost::Ping { ts } => {
            if let Ok(text) = serde_json::to_string(&HostToRelay::Pong { ts }) {
                let _ = tx.send(Message::Text(text.into()));
            }
        }
        RelayToHost::Shutdown { reason, .. } => {
            tprintln!("[cloud] relay draining ({reason}); reconnecting");
            bail!("relay shutdown: {reason}");
        }
    }
    Ok(Some(()))
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

    let (status, content_type, out_body): (u16, &'static str, String) = match (method, path) {
        ("POST", "/whep") => {
            let base: Vec<RTCIceServer> = if ice_servers.is_empty() {
                state.fallback_ice_servers()
            } else {
                ice_servers.into_iter().map(IceServerWire::into_rtc).collect()
            };
            let ice = state.ice_with_turn(base);
            let r = server::process_whep(state, client_id, body.as_bytes(), ice).await;
            (r.status, r.content_type, r.body)
        }
        ("GET", "/reconfig") => (200, "application/json", server::process_reconfig(state, client_id)),
        ("POST", "/leave") => {
            server::process_leave(state, client_id);
            (204, "text/plain", String::new())
        }
        _ => {
            teprintln!("[cloud] unsupported tunneled request {method} {path}");
            (404, "text/plain", "not found".to_string())
        }
    };

    let mut headers = HashMap::new();
    headers.insert("content-type".to_string(), content_type.to_string());
    HostToRelay::SignalResponse {
        request_id,
        status,
        headers,
        body: out_body,
    }
}
