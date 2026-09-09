pub mod airplay;
pub mod audio;
pub mod compatibility;
pub mod device_reporter;
pub mod hosted_network;
pub mod networking;
pub mod objc_exception;
pub mod permissions;
pub mod streamer;
pub mod virtual_display;

use crate::streamer::cloud::{
    CloudClient, CloudConfig, CloudState, CloudStatusSink, SharedCloudStatusSink,
};
use crate::streamer::lock::LockExt;
use crate::streamer::session::{
    self, DeviceOverride, SessionAuth, SharedApprovedDevices, SharedBannedDevices,
    SharedDeviceOverrides, SharedDeviceReporter, SharedOtpLimiter, SharedServerPorts,
    SharedSessions, SharedTurnConfig, SharedVirtualDisplay, UserTurnConfig,
};
use crate::streamer::{Config, Streamer};
use crate::DisplayCapacity;
use device_reporter::TauriDeviceReporter;
use networking::NetworkInfo;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::path::BaseDirectory;
use tauri::Manager;
use tauri::State;
use virtual_display::new_shared_controller;

pub struct StreamerHandle {
    handle: axum_server::Handle,
}

pub struct AppState {
    pub virtual_display: SharedVirtualDisplay,
    pub stop_hosted_network: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub hosted_network_running: Mutex<bool>,
    pub network_adapters: Mutex<Vec<NetworkInfo>>,
    pub local_ips: session::SharedLocalIps,
    pub streamers: Mutex<HashMap<String, StreamerHandle>>,
    pub session_auth: SessionAuth,
    pub device_reporter: SharedDeviceReporter,
    pub device_overrides: SharedDeviceOverrides,
    pub sessions: SharedSessions,
    pub disconnect_grace: session::SharedDisconnectGrace,
    pub user_turn: SharedTurnConfig,
    pub stun_urls: session::SharedStunConfig,
    pub banned_devices: SharedBannedDevices,
    pub approved_devices: SharedApprovedDevices,
    pub otp_limiter: SharedOtpLimiter,
    pub server_ports: SharedServerPorts,
    pub disable_gpu_encode: Arc<std::sync::atomic::AtomicBool>,
    pub cloud: Mutex<Option<CloudClient>>,
    pub cloud_status: Arc<Mutex<(String, String)>>,
    pub audio_hub: crate::streamer::audio::SharedAudioHub,
}

pub type SharedCloudStatus = Arc<Mutex<(String, String)>>;

#[derive(Debug)]
pub struct TauriCloudStatusSink {
    app: tauri::AppHandle,
    status: SharedCloudStatus,
}

impl TauriCloudStatusSink {
    pub fn new_shared(app: tauri::AppHandle, status: SharedCloudStatus) -> SharedCloudStatusSink {
        Arc::new(Self { app, status })
    }
}

impl CloudStatusSink for TauriCloudStatusSink {
    fn report(&self, state: CloudState, detail: String) {
        use tauri_specta::Event;
        *self.status.lock().unwrap() = (state.as_str().to_string(), detail.clone());
        let payload = crate::CloudStatusChange {
            state: state.as_str().to_string(),
            detail,
        };
        if let Err(e) = payload.emit(&self.app) {
            teprintln!("[cloud] failed to emit status event: {e:?}");
        }
    }

    fn session_rotated(&self, session_id: String) {
        use tauri_specta::Event;
        let payload = crate::SessionIdChange { session_id };
        if let Err(e) = payload.emit(&self.app) {
            teprintln!("[cloud] failed to emit session id change event: {e:?}");
        }
    }
}

pub fn focus_main_window(window: &tauri::WebviewWindow) {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};

    unsafe {
        let ns_app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        if !ns_app.is_null() {
            let _: () = msg_send![ns_app, activateIgnoringOtherApps: true];
        }
    }

    match window.ns_window() {
        Ok(ptr) if !ptr.is_null() => unsafe {
            let ns_window = ptr as *mut AnyObject;
            let nil: *mut AnyObject = std::ptr::null_mut();
            let _: () = msg_send![ns_window, makeKeyAndOrderFront: nil];
        },
        Ok(_) => {}
        Err(e) => teprintln!("[window] focus: failed to get ns_window: {e}"),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }

    audio::legacy::routing::recover_on_launch();
    let virtual_display = tauri::async_runtime::spawn_blocking(new_shared_controller).await;

    // `setup` returns a bare bool, so the only place the reason can survive is the log backlog —
    // and until now `Ok(None)` and a panicking `new_shared_controller` were indistinguishable, with
    // the panic payload discarded.
    let virtual_display = match virtual_display {
        Ok(Some(vd)) => vd,
        Ok(None) => {
            teprintln!(
                "[setup] no virtual-display backend could be constructed on this Mac \
                 (display tier: {}). Nothing else in setup ran.",
                virtual_display::probe_display_backend().as_str()
            );
            return false;
        }
        Err(e) => {
            teprintln!("[setup] building the virtual-display controller panicked: {e}");
            return false;
        }
    };

    let device_overrides: SharedDeviceOverrides = Arc::new(Mutex::new(HashMap::new()));
    let device_reporter: SharedDeviceReporter =
        TauriDeviceReporter::new_shared(app_handle.clone(), device_overrides.clone());
    let sessions: SharedSessions = Arc::new(Mutex::new(HashMap::new()));

    let state = AppState {
        virtual_display,
        stop_hosted_network: Mutex::new(None),
        hosted_network_running: Mutex::new(false),
        network_adapters: Mutex::new(Vec::new()),
        local_ips: session::new_shared_local_ips(),
        streamers: Mutex::new(HashMap::new()),
        session_auth: SessionAuth::default(),
        device_reporter,
        device_overrides,
        sessions,
        disconnect_grace: session::new_shared_disconnect_grace(),
        user_turn: session::new_shared_turn_config(),
        stun_urls: session::new_shared_stun_config(),
        banned_devices: session::new_shared_banned_devices(),
        approved_devices: session::new_shared_approved_devices(),
        otp_limiter: session::new_shared_otp_limiter(),
        server_ports: session::new_shared_server_ports(),
        disable_gpu_encode: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        cloud: Mutex::new(None),
        cloud_status: Arc::new(Mutex::new(("connecting".to_string(), String::new()))),
        audio_hub: crate::streamer::audio::AudioHub::new(),
    };
    // The saved STUN list, read from the same store the CLI writes, so `stun set`, `serve` and
    // the desktop app all agree. Absent means "keep the built-in default"; an empty list is the
    // deliberate "no STUN at all", which a LAN-only host has every reason to choose.
    if let Some(urls) = crate::cli::saved_stun_urls(&app_handle) {
        if urls.is_empty() {
            tprintln!("[ice] STUN disabled by configuration; host candidates only");
        } else {
            tprintln!("[ice] STUN servers from configuration: {}", urls.join(", "));
        }
        *state.stun_urls.lock_recover() = urls;
    }

    app_handle.manage(state);
    start_session_watchdog(app_handle.clone());
    true
}

/// How often the watchdog wakes. Cheap enough to be frequent: the AirPlay latch is an
/// atomic read and the screen-recording read is the window-list probe, 0.6 ms warm.
const WATCHDOG_TICK: Duration = Duration::from_secs(5);

/// How often authorization is actually re-read while a session is streaming.
const TCC_REPROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Watch two things that can change under a live session and that nothing else notices.
///
/// **Screen-recording consent.** macOS 15 re-asks roughly every 30 days and there is no
/// supported way to suppress it. A host left running as a permanent second monitor -- the
/// product's whole use case -- will meet that with a device connected, and if the user
/// dismisses the dialog the capture APIs go on delivering frames: they just contain
/// wallpaper. So the host reports a healthy stream while the client shows a blank desktop.
/// `ensure_screen_recording_access` catches this at pipeline start, which is every join
/// and every refresh, and nowhere in between -- this is the in-between. Only a definite
/// `Some(true) -> Some(false)` transition acts; `None` means "could not read" and must
/// never be treated as a refusal.
///
/// **AirPlay geometry.** On that display tier macOS owns the display and moves it under
/// us; the sender re-sends codec data when it does, which `MirrorStats` latches. Bumping
/// the reconfig epoch hands that to the client on its next `/reconfig` poll instead of
/// waiting for a size poll to notice.
fn start_session_watchdog(app_handle: tauri::AppHandle) {
    std::thread::Builder::new()
        .name("se-session-watchdog".into())
        .spawn(move || {
            let airplay_tier = virtual_display::probe_display_backend()
                == virtual_display::DisplayBackend::AirPlayFallback;
            let mut authorized: Option<bool> = None;
            let mut last_probe = Instant::now();
            loop {
                std::thread::sleep(WATCHDOG_TICK);
                let Some(state) = app_handle.try_state::<AppState>() else {
                    continue;
                };

                if airplay_tier && airplay::take_geometry_changed() {
                    let ips: Vec<String> = state.sessions.lock().unwrap().keys().cloned().collect();
                    if !ips.is_empty() {
                        tprintln!(
                            "[airplay] macOS changed the display geometry; asking {} client(s) \
                             to renegotiate",
                            ips.len()
                        );
                    }
                    for ip in ips {
                        session::bump_reconfig_epoch(&state.sessions, &ip);
                    }
                }

                let streaming = {
                    let sessions = state.sessions.lock().unwrap();
                    sessions.values().any(|s| s.live_display.is_some())
                };
                if !streaming {
                    // Nothing to protect, and the next join re-reads authorization anyway.
                    authorized = None;
                    continue;
                }
                if last_probe.elapsed() < TCC_REPROBE_INTERVAL {
                    continue;
                }
                last_probe = Instant::now();
                let now = permissions::screen_recording_live();
                if authorized == Some(true) && now == Some(false) {
                    teprintln!(
                        "[tcc] Screen Recording authorization was revoked while streaming. \
                         macOS 15 and later re-ask about every 30 days; capture would keep \
                         delivering frames of the wallpaper, so the sessions are being ended \
                         instead. Re-approve ScreenExtend under System Settings -> Privacy & \
                         Security -> Screen Recording, then reconnect."
                    );
                    crate::streamer::session::broadcast_bye(
                        &state.sessions,
                        session::ByeReason::Kicked,
                        crate::BYE_FLUSH_TIMEOUT,
                    );
                    let ips: Vec<String> = state.sessions.lock().unwrap().keys().cloned().collect();
                    for ip in ips {
                        session::bump_kick_epoch(&state.sessions, &ip);
                        session::signal_leave(&state.sessions, &ip);
                    }
                }
                if now.is_some() {
                    authorized = now;
                }
            }
        })
        .ok();
}

#[tauri::command]
#[specta::specta]
#[allow(clippy::too_many_arguments)]
pub fn set_device_override(
    state: State<'_, AppState>,
    ip: String,
    scale: u32,
    orientation: String,
    refresh_rate: u32,
    video_scale: u32,
    video_quality: u32,
    control_enabled: bool,
    dpr: f64,
    audio_enabled: bool,
) {
    use crate::streamer::config::ScalePercent;
    use crate::streamer::platform::max_display_dpr;
    use crate::streamer::server::{
        MAX_DISPLAY_SCALE, MAX_REFRESH_RATE, MIN_DISPLAY_SCALE, MIN_REFRESH_RATE,
    };

    let dpr = if dpr.is_finite() {
        dpr.clamp(1.0, max_display_dpr())
    } else {
        1.0
    };
    let next = DeviceOverride {
        scale: scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE),
        orientation_portrait: orientation == "Portrait",
        refresh_rate: refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE),
        video_scale: ScalePercent::new(video_scale).percent(),
        video_quality: video_quality.clamp(1, 51) as u8,
        control_enabled,
        dpr,
        audio_enabled,
    };
    // Which overrides need the session rebuilt, and which only need the display touched. The
    // client answers a reconfig epoch bump by tearing the whole session down — peer connection,
    // encoder, capture, injector thread — and paying the full join cost. Everything below except
    // the display scale is an encoder-session parameter, so that is unavoidable for them. The
    // scale is not: it changes the extended desktop's DPI, not the captured geometry, so the
    // stream the client is already decoding is exactly the stream it wants.
    let (needs_rebuild, scale_only) = {
        let mut overrides = state.device_overrides.lock_recover();
        let verdict = match overrides.get(&ip) {
            Some(old) => {
                let rebuild = old.orientation_portrait != next.orientation_portrait
                    || old.refresh_rate != next.refresh_rate
                    || old.video_scale != next.video_scale
                    || old.video_quality != next.video_quality
                    || (old.dpr - next.dpr).abs() > f64::EPSILON;
                (rebuild, !rebuild && old.scale != next.scale)
            }
            None => (true, false),
        };
        overrides.insert(ip.clone(), next);
        verdict
    };
    if needs_rebuild {
        session::bump_reconfig_epoch(&state.sessions, &ip);
    } else if scale_only {
        let sessions = state.sessions.clone();
        let effective = ((next.dpr * next.scale as f64).round() as u32).clamp(
            MIN_DISPLAY_SCALE,
            crate::streamer::server::MAX_EFFECTIVE_SCALE,
        );
        tauri::async_runtime::spawn(async move {
            if !crate::streamer::server::apply_scale_live(&sessions, &ip, effective).await {
                // Nothing live to touch, so there is nothing to apply it to; the next join reads
                // it back out of the override map anyway.
                tprintln!("display scale for {ip} saved; no live display to apply it to");
            }
        });
    }
}

#[tauri::command]
#[specta::specta]
pub fn refresh_device_stream(state: State<'_, AppState>, ip: String) -> bool {
    if session::get_live_display(&state.sessions, &ip).is_none() {
        tprintln!("stream refresh skipped: {ip} has no live display attached");
        return false;
    }
    session::bump_reconfig_epoch(&state.sessions, &ip);
    tprintln!("stream refresh requested for {ip}; client will renegotiate on its next poll");
    true
}

#[tauri::command]
#[specta::specta]
pub fn set_device_audio_output(state: State<'_, AppState>, ip: String, device_id: String) {
    let sel = if device_id.trim().is_empty() {
        None
    } else {
        Some(device_id)
    };
    session::set_selected_audio_output(&state.sessions, &ip, sel);
}

#[tauri::command]
#[specta::specta]
pub fn get_device_audio_outputs(
    state: State<'_, AppState>,
    ip: String,
) -> crate::streamer::session::AudioOutputsReport {
    session::audio_outputs_report(&state.sessions, &ip)
}

#[tauri::command]
#[specta::specta]
pub fn remove_device_override(state: State<'_, AppState>, ip: String) {
    state.device_overrides.lock().unwrap().remove(&ip);
    session::send_bye(&state.sessions, &ip, session::ByeReason::Kicked);
    session::bump_kick_epoch(&state.sessions, &ip);
    session::signal_leave(&state.sessions, &ip);
}

#[tauri::command]
#[specta::specta]
pub fn set_device_banned(state: State<'_, AppState>, token: String, ip: String, banned: bool) {
    let key = if token.trim().is_empty() {
        ip.clone()
    } else {
        token
    };
    if banned {
        state.banned_devices.lock().unwrap().insert(key);
        if !ip.is_empty() {
            session::send_bye(&state.sessions, &ip, session::ByeReason::Kicked);
            session::bump_kick_epoch(&state.sessions, &ip);
            session::signal_leave(&state.sessions, &ip);
        }
        tprintln!("device {ip} banned; existing session (if any) kicked");
    } else {
        state.banned_devices.lock().unwrap().remove(&key);
        tprintln!("device {ip} unbanned");
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_device_approved(state: State<'_, AppState>, token: String, approved: bool) {
    if token.trim().is_empty() {
        return;
    }
    if approved {
        state.approved_devices.lock().unwrap().insert(token);
        tprintln!("device token approved for auto-join");
    } else {
        state.approved_devices.lock().unwrap().remove(&token);
        tprintln!("device token auto-join approval revoked");
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_disconnect_grace(state: State<'_, AppState>, seconds: u32) {
    let secs = (seconds as u64).clamp(
        session::MIN_DISCONNECT_GRACE_SECS,
        session::MAX_DISCONNECT_GRACE_SECS,
    );
    state
        .disconnect_grace
        .store(secs, std::sync::atomic::Ordering::Relaxed);
    tprintln!("disconnect grace set to {secs}s");
}

#[tauri::command]
#[specta::specta]
pub fn get_disconnect_grace(state: State<'_, AppState>) -> u32 {
    state
        .disconnect_grace
        .load(std::sync::atomic::Ordering::Relaxed) as u32
}

#[tauri::command]
#[specta::specta]
pub fn get_display_capacity(state: State<'_, AppState>) -> DisplayCapacity {
    let max = state.virtual_display.max_concurrent_displays();
    let in_use = session::live_display_count(&state.sessions);
    DisplayCapacity {
        max: max.map(|m| m as u32),
        in_use: in_use as u32,
        full: max.is_some_and(|m| in_use >= m),
        backend: virtual_display::probe_display_backend()
            .as_str()
            .to_string(),
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, specta::Type)]
pub struct TurnConfig {
    pub urls: String,
    pub username: String,
    pub credential: String,
}

#[tauri::command]
#[specta::specta]
pub fn set_turn_config(
    state: State<'_, AppState>,
    urls: String,
    username: String,
    credential: String,
) {
    let urls: Vec<String> = urls
        .split(',')
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty())
        .collect();
    let enabled = !urls.is_empty();
    *state.user_turn.lock().unwrap() = UserTurnConfig {
        urls,
        username: username.trim().to_string(),
        credential: credential.trim().to_string(),
    };
    if enabled {
        tprintln!("TURN relay configured from settings");
    } else {
        tprintln!("TURN relay cleared (none configured)");
    }
}

#[tauri::command]
#[specta::specta]
pub fn get_turn_config(state: State<'_, AppState>) -> TurnConfig {
    let cfg = state.user_turn.lock().unwrap();
    TurnConfig {
        urls: cfg.urls.join(","),
        username: cfg.username.clone(),
        credential: cfg.credential.clone(),
    }
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Copy, specta::Type)]
pub struct ServerPorts {
    pub http: u16,
    pub https: u16,
}

#[tauri::command]
#[specta::specta]
pub fn get_server_ports(state: State<'_, AppState>) -> ServerPorts {
    let (http, https) = state.server_ports.get();
    ServerPorts { http, https }
}

#[tauri::command]
#[specta::specta]
pub fn set_server_ports(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    http_port: u16,
    https_port: u16,
) -> ServerPorts {
    let http = if http_port == 0 {
        session::DEFAULT_HTTP_PORT
    } else {
        http_port
    };
    let https = if https_port == 0 {
        session::DEFAULT_HTTPS_PORT
    } else {
        https_port
    };

    let (cur_http, cur_https) = state.server_ports.get();
    if http == https {
        teprintln!("[streamer] rejecting server port change: HTTP and HTTPS must differ ({http})");
        return ServerPorts {
            http: cur_http,
            https: cur_https,
        };
    }
    if http == cur_http && https == cur_https {
        return ServerPorts { http, https };
    }

    state.server_ports.set(http, https);
    tprintln!(
        "[streamer] server ports changed to HTTP :{http}, HTTPS :{https}; restarting streamers"
    );

    // Tell the connected clients why their stream is about to end, rather than dropping the
    // transport under them.
    let connected: Vec<String> = state.sessions.lock().unwrap().keys().cloned().collect();
    for ip in connected {
        session::send_bye(&state.sessions, &ip, session::ByeReason::HostExit);
    }

    {
        let mut streamers = state.streamers.lock().unwrap();
        for (ip, streamer) in streamers.drain() {
            tprintln!("[streamer] stopping streamer bound to {ip} for port change");
            streamer
                .handle
                .graceful_shutdown(Some(Duration::from_millis(500)));
        }
    }
    // Waiting for the old listeners to release their ports used to be a 300 ms
    // `std::thread::sleep` inside a synchronous command, i.e. on the main thread, with
    // `sync_streamers` and its per-adapter runtime spin-up behind it. The command now returns
    // immediately and the restart finishes on its own thread.
    std::thread::spawn(move || {
        use tauri::Manager;
        std::thread::sleep(Duration::from_millis(300));
        match app.try_state::<AppState>() {
            Some(state) => sync_streamers(&state),
            None => teprintln!("[streamer] port change: app state went away before the restart"),
        }
    });

    ServerPorts { http, https }
}

#[tauri::command]
#[specta::specta]
pub fn get_disable_gpu_encode(state: State<'_, AppState>) -> bool {
    state
        .disable_gpu_encode
        .load(std::sync::atomic::Ordering::Relaxed)
}

#[tauri::command]
#[specta::specta]
pub fn set_disable_gpu_encode(state: State<'_, AppState>, disabled: bool) {
    state
        .disable_gpu_encode
        .store(disabled, std::sync::atomic::Ordering::Relaxed);
}

#[tauri::command]
#[specta::specta]
pub fn install_drivers(_app: tauri::AppHandle) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn remove_drivers(_app: tauri::AppHandle) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn install_audio_driver(app: tauri::AppHandle) -> String {
    let pkg = app
        .path()
        .resolve("resources/ScreenExtendAudio.pkg", BaseDirectory::Resource);
    match pkg {
        Ok(path) => {
            if !path.exists() {
                // `resolve` only joins a path, so an app bundle built without the package
                // staged reaches `install_pkg` and fails there with nothing naming the cause.
                teprintln!(
                    "[audio] this build does not carry the audio driver package ({} is \
                     missing). Build it with macos/ScreenExtendAudio/scripts/release.sh \
                     before `tauri build`, or the Install button can only fail.",
                    path.display()
                );
            }
            audio::legacy::installer::install_pkg(&path)
                .as_str()
                .to_string()
        }
        Err(e) => {
            teprintln!("[audio] could not resolve installer package: {e}");
            "failed".to_string()
        }
    }
}

#[tauri::command]
#[specta::specta]
pub fn uninstall_audio_driver() -> String {
    audio::legacy::installer::uninstall().as_str().to_string()
}

#[tauri::command]
#[specta::specta]
pub fn audio_driver_status() -> String {
    use audio::legacy::probe::LegacyState;
    match audio::legacy::installer::health() {
        LegacyState::Ready => "ready".to_string(),
        LegacyState::NeedsInstall => "needs_install".to_string(),
        LegacyState::InstalledButUnhealthy => "installed_but_unhealthy".to_string(),
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_legacy_volume_key_proxy(enabled: bool) {
    audio::legacy::volume_keys::set_enabled(enabled);
}

pub fn remove_all_displays(client: &SharedVirtualDisplay) {
    client.remove_all_displays();
}

#[tauri::command]
#[specta::specta]
pub fn set_session_credentials(state: State<'_, AppState>, session_id: String, otp: String) {
    *state.session_auth.session_id.lock().unwrap() = session_id;
    *state.session_auth.otp.lock().unwrap() = otp;
    state.otp_limiter.reset();
}

#[tauri::command]
#[specta::specta]
pub fn register_cloud_session(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    session_id: String,
) {
    let server_config = Config {
        virtual_display: Some(state.virtual_display.clone()),
        session_auth: Some(state.session_auth.clone()),
        device_reporter: Some(state.device_reporter.clone()),
        device_overrides: Some(state.device_overrides.clone()),
        sessions: Some(state.sessions.clone()),
        disconnect_grace: Some(state.disconnect_grace.clone()),
        user_turn: Some(state.user_turn.clone()),
        stun_urls: state.stun_urls.lock_recover().clone(),
        banned_devices: Some(state.banned_devices.clone()),
        approved_devices: Some(state.approved_devices.clone()),
        otp_limiter: Some(state.otp_limiter.clone()),
        audio_hub: Some(state.audio_hub.clone()),
        gpu_encode_toggle: Some(state.disable_gpu_encode.clone()),
        ..Config::default()
    };
    *state.cloud_status.lock().unwrap() = ("connecting".to_string(), String::new());
    let sink = TauriCloudStatusSink::new_shared(app, state.cloud_status.clone());
    let client = CloudClient::spawn(CloudConfig::new(session_id), server_config, sink);
    let mut guard = state.cloud.lock().unwrap();
    if let Some(mut prev) = guard.take() {
        prev.stop();
    }
    *guard = Some(client);
}

#[tauri::command]
#[specta::specta]
pub fn unregister_cloud_session(state: State<'_, AppState>) {
    let mut guard = state.cloud.lock().unwrap();
    if let Some(mut prev) = guard.take() {
        prev.stop();
        tprintln!("[cloud] public sessions disabled; relay client stopped");
    }
    *state.cloud_status.lock().unwrap() =
        (CloudState::Disabled.as_str().to_string(), String::new());
}

#[tauri::command]
#[specta::specta]
pub fn get_cloud_status(state: State<'_, AppState>) -> crate::CloudStatusChange {
    let (status, detail) = state.cloud_status.lock().unwrap().clone();
    crate::CloudStatusChange {
        state: status,
        detail,
    }
}

fn display_slot_holder(state: &AppState) -> Option<String> {
    let max = state.virtual_display.max_concurrent_displays()?;
    if session::live_display_count(&state.sessions) < max {
        return None;
    }
    session::display_holder_host_ip(&state.sessions)
}

/// Whether an IPv6 address is one a client can reach by typing it into a browser.
///
/// Link-local (`fe80::/10`) is not: it needs a zone index (`%en0`), and no URL carries one. The
/// loopback and unspecified addresses are not either. Global unicast and unique-local are, and on
/// an IPv6-only network they are the only way to host at all.
fn servable_v6(ip: &std::net::Ipv6Addr) -> bool {
    !ip.is_loopback() && !ip.is_unspecified() && ip.segments()[0] & 0xffc0 != 0xfe80
}

pub fn sync_streamers(state: &AppState) {
    let (desired, unusable, fresh_local_ips) = {
        let adapters = state.network_adapters.lock().unwrap();
        let all = || {
            adapters
                .iter()
                .flat_map(|adapter| adapter.ip_addresses.iter())
        };

        // IPv4 first, then whatever IPv6 a browser can actually reach. An IPv6-only network used
        // to produce no listener at all and no diagnostic, because every v6 address failed
        // `parse::<Ipv4Addr>()` and was dropped with `.ok()`.
        let mut desired: Vec<(String, IpAddr)> = Vec::new();
        let mut unusable: Vec<String> = Vec::new();
        for ip in all() {
            match ip.parse::<IpAddr>() {
                Ok(addr @ IpAddr::V4(_)) => desired.push((ip.clone(), addr)),
                Ok(IpAddr::V6(v6)) if servable_v6(&v6) => {
                    desired.push((ip.clone(), IpAddr::V6(v6)))
                }
                _ => unusable.push(ip.clone()),
            }
        }
        desired.sort_by_key(|(_, addr)| addr.is_ipv6());

        let fresh_local_ips = crate::streamer::server::collect_local_ips(all().map(String::as_str));
        (desired, unusable, fresh_local_ips)
    };

    *state.local_ips.lock().unwrap() = fresh_local_ips;

    if !unusable.is_empty() {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let first_time = !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed);
        if first_time || desired.is_empty() {
            teprintln!(
                "[streamer] {} address(es) are not being served ({}): a link-local IPv6 address                  needs a zone index that no browser URL can carry{}",
                unusable.len(),
                unusable.join(", "),
                if desired.is_empty() {
                    ", and no other address is available, so nothing is listening and no device can join"
                } else {
                    ""
                }
            );
        }
    }

    let desired = match display_slot_holder(state) {
        Some(host_ip) => {
            let kept: Vec<_> = desired
                .into_iter()
                .filter(|(ip, _)| *ip == host_ip)
                .collect();
            if kept.is_empty() {
                tprintln!(
                    "[streamer] display slot is taken by a client on an adapter that is gone; serving nobody until it frees up"
                );
            }
            kept
        }
        None => desired,
    };

    let (http_port, https_port) = state.server_ports.get();

    let mut streamers = state.streamers.lock().unwrap();

    streamers.retain(|ip, streamer| {
        if desired.iter().any(|(desired_ip, _)| desired_ip == ip) {
            true
        } else {
            tprintln!("[streamer] stopping streamer bound to {ip}");
            streamer
                .handle
                .graceful_shutdown(Some(Duration::from_secs(1)));
            false
        }
    });

    for (ip, addr) in desired {
        if streamers.contains_key(&ip) {
            continue;
        }

        let handle = axum_server::Handle::new();
        let config = Config {
            bind_ip: addr,
            lan_ip: Some(ip.clone()),
            local_ips: Some(state.local_ips.clone()),
            port: http_port,
            https_port,
            virtual_display: Some(state.virtual_display.clone()),
            session_auth: Some(state.session_auth.clone()),
            device_reporter: Some(state.device_reporter.clone()),
            device_overrides: Some(state.device_overrides.clone()),
            sessions: Some(state.sessions.clone()),
            disconnect_grace: Some(state.disconnect_grace.clone()),
            user_turn: Some(state.user_turn.clone()),
            stun_urls: state.stun_urls.lock_recover().clone(),
            banned_devices: Some(state.banned_devices.clone()),
            approved_devices: Some(state.approved_devices.clone()),
            otp_limiter: Some(state.otp_limiter.clone()),
            audio_hub: Some(state.audio_hub.clone()),
            gpu_encode_toggle: Some(state.disable_gpu_encode.clone()),
            ..Config::default()
        };

        let thread_handle = handle.clone();
        let ip_for_log = ip.clone();
        // One thread per adapter, but they all drive the single shared transport runtime
        // (`streamer::transport_runtime`) rather than each building one of their own.
        // A discarded `JoinHandle` also discarded the spawn error, so a thread that never
        // started left `streamers` claiming an adapter was served with nothing listening.
        let spawned = std::thread::Builder::new()
            .name(format!("se-adapter-{ip}"))
            .spawn(move || {
                if let Err(e) = Streamer::new(config).run_with_handle(thread_handle) {
                    teprintln!("[streamer] streamer bound to {ip_for_log} exited: {e}");
                }
            });
        if let Err(e) = spawned {
            teprintln!("[streamer] could not start a streamer for {ip}: {e}");
            continue;
        }

        tprintln!("[streamer] started streamer bound to {ip}");
        streamers.insert(ip, StreamerHandle { handle });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;

    #[test]
    fn only_reachable_ipv6_addresses_are_served() {
        // The one this dev box reports, and the reason an IPv6-only network still cannot host
        // from it: fe80::/10 needs a zone index no URL can carry.
        assert!(!servable_v6(
            &"fe80::844:907:64ca:8224".parse::<Ipv6Addr>().unwrap()
        ));
        assert!(!servable_v6(&Ipv6Addr::LOCALHOST));
        assert!(!servable_v6(&Ipv6Addr::UNSPECIFIED));
        // Global unicast and unique-local are what an IPv6-only network actually hands out.
        assert!(servable_v6(&"2001:db8::1".parse::<Ipv6Addr>().unwrap()));
        assert!(servable_v6(&"fd00::1".parse::<Ipv6Addr>().unwrap()));
    }
}
