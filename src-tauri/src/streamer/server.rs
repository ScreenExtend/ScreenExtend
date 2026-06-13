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
use super::session::{self, DeviceInfo, DeviceOverride};
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
struct AppState {
    config: Arc<Config>,
    ice_servers: Arc<Vec<RTCIceServer>>,
    ice_json: Arc<String>,
    net_json: Arc<String>,
}

pub async fn run(config: Config, handle: Option<axum_server::Handle>) -> Result<()> {
    let handle = handle.unwrap_or_else(axum_server::Handle::new);

    let state = AppState {
        ice_servers: Arc::new(build_ice_servers(&config)),
        ice_json: Arc::new(build_ice_json(&config)),
        net_json: Arc::new(build_net_json(&config)),
        config: Arc::new(config.clone()),
    };

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

    let http = axum_server::bind(http_addr)
        .handle(handle.clone())
        .serve(app.clone().into_make_service_with_connect_info::<SocketAddr>());
    let https = axum_server::bind_rustls(https_addr, tls_config)
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
        state.ice_json.as_ref().clone(),
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
    let ip = peer.ip().to_string();
    let (epoch, kick) = state
        .config
        .sessions
        .as_ref()
        .map(|s| (session::reconfig_epoch(s, &ip), session::kick_epoch(s, &ip)))
        .unwrap_or((0, 0));
    (
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::json!({ "epoch": epoch, "kick": kick }).to_string(),
    )
        .into_response()
}

async fn leave(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Response {
    let ip = peer.ip().to_string();
    if let Some(s) = state.config.sessions.as_ref() {
        println!("leave beacon from {ip}; tearing down session");
        session::signal_leave(s, &ip);
    }
    StatusCode::NO_CONTENT.into_response()
}

fn build_net_json(config: &Config) -> String {
    serde_json::json!({ "httpsPort": config.https_port }).to_string()
}

fn build_ice_json(config: &Config) -> String {
    let mut servers: Vec<serde_json::Value> = Vec::new();
    if !config.stun_urls.is_empty() {
        servers.push(serde_json::json!({ "urls": config.stun_urls }));
    }
    if let (Some(url), Some(user), Some(cred)) =
        (&config.turn_url, &config.turn_username, &config.turn_credential)
    {
        servers.push(serde_json::json!({
            "urls": [url], "username": user, "credential": cred
        }));
    }
    serde_json::json!({ "iceServers": servers }).to_string()
}

async fn whep(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let req: JoinRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid join request: {e}")).into_response();
        }
    };

    let client_ip = peer.ip().to_string();

    println!(
        "join request: device={:?}, session={}, screen={}x{}, sdp_bytes={}",
        req.device_name,
        req.session_id,
        req.width,
        req.height,
        req.sdp.len()
    );

    let auth = match state.config.session_auth.as_ref() {
        Some(auth) if auth.validate(&req.session_id, &req.otp) => auth,
        _ => {
            println!("join rejected: invalid session id or OTP");
            return (StatusCode::UNAUTHORIZED, "invalid session id or OTP").into_response();
        }
    };
    let _ = auth;

    match start_session(&state, &req, &client_ip).await {
        Ok(answer) => {
            println!("join accepted: WHEP answer generated ({} bytes)", answer.len());
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/sdp")],
                answer,
            )
                .into_response()
        }
        Err(e) => {
            eprintln!("join failed: {e:?}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("join failed: {e}"),
            )
                .into_response()
        }
    }
}

async fn start_session(state: &AppState, req: &JoinRequest, client_ip: &str) -> Result<String> {
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
            println!("stopping previous capture for {client_ip} before starting a new session");
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

    let portrait = override_for_ip.map(|o| o.orientation_portrait).unwrap_or(false);
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
                    eprintln!("could not apply display mode to {name}: {e}");
                }
                apply_display_scale(&name, override_for_ip).await;
                wait_for_display_settle(&name).await;
                println!(
                    "virtual display id={} settings changed in place via Windows APIs ({width}x{height}@{refresh})",
                    prev.display_id
                );
            } else {
                println!(
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
                println!("virtual display created (id={display_id}, {width}x{height}@{refresh})");

                match wait_for_new_monitor(&before).await {
                    Some(name) => {
                        println!("virtual display id={display_id} attached as {name}");
                        (display_id, name)
                    }
                    None => {
                        remove_display_async(client, display_id).await;
                        bail!("virtual display {display_id} did not attach within timeout");
                    }
                }
            };

            let _ = tokio::task::spawn_blocking(pipeline::set_display_topology_extend).await;

            {
                let name = device_name.clone();
                let res = tokio::task::spawn_blocking(move || {
                    pipeline::set_display_mode(&name, width, height, refresh, portrait)
                })
                .await;
                match res {
                    Ok(Ok(())) => println!(
                        "virtual display {device_name} set to {width}x{height}@{refresh} (portrait={portrait})"
                    ),
                    Ok(Err(e)) => eprintln!("could not force {device_name} to {width}x{height}: {e}"),
                    Err(e) => eprintln!("set-mode task for {device_name} panicked: {e}"),
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

    let (closed_tx, closed_rx) = oneshot::channel();
    let answer = match webrtc_session::handle_whep_offer(
        req.sdp.clone(),
        &session.pipeline,
        state.ice_servers.as_ref().clone(),
        Some(closed_tx),
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
            println!(
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
            println!("session for display id={display_id} ({device_name}) superseded; keeping display");
            return;
        }
        println!(
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
            eprintln!("could not set scale for {name}: {e}");
        }
    })
    .await;
    if let Err(e) = res {
        eprintln!("apply-display-scale task for {device_name} panicked: {e}");
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
            eprintln!(
                "TURN relay configured ({url}) — MUST be local/regional to preserve latency"
            );
        }
        (Some(_), _, _) => {
            eprintln!("TURN_URL set but credentials missing, TURN disabled");
        }
        _ => {}
    }

    if servers.is_empty() {
        println!("ICE servers: none configured -> host candidates only (same-network)");
    } else {
        for s in &servers {
            println!(
                "ICE server configured (urls={:?}, has_creds={})",
                s.urls,
                !s.username.is_empty()
            );
        }
    }

    servers
}

fn log_urls(lan_ip: Option<&str>, http_port: u16, https_port: u16, self_signed: bool) {
    println!("server listening — HTTP :{http_port}, HTTPS :{https_port}");
    match lan_ip {
        Some(ip) => {
            println!("  LAN (open this first):  http://{ip}:{http_port}/");
            println!("  LAN (secure / WebCodecs): https://{ip}:{https_port}/");
        }
        None => println!("  LAN IP not set; use this machine's IP manually (or pass --lan-ip)"),
    }
    println!(
        "  local:  http://localhost:{http_port}/   health: http://localhost:{http_port}/health"
    );
    if self_signed {
        println!(
            "HTTPS uses a self-signed dev cert: browser shows a one-time warning, accept to proceed; \
             supply --tls-cert/--tls-key for a trusted cert"
        );
    }
}
