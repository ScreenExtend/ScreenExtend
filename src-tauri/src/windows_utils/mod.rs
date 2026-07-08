pub mod compatibility;
pub mod device_reporter;
pub mod hosted_network;
pub mod networking;
pub mod streamer;
pub mod virtual_display;
pub mod driver_ipc;
pub mod windows_capture;

use elevated_command::Command;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::path::BaseDirectory;
use tauri::Manager;
use tauri::State;
use crate::streamer::cloud::{CloudClient, CloudConfig, CloudState, CloudStatusSink, SharedCloudStatusSink};
use crate::streamer::session::{
    self, DeviceOverride, SessionAuth, SharedDeviceOverrides, SharedDeviceReporter, SharedServerPorts,
    SharedSessions, SharedTurnConfig, SharedVirtualDisplay, UserTurnConfig,
};
use crate::streamer::{Config, Streamer};
use device_reporter::TauriDeviceReporter;
use networking::NetworkInfo;
use virtual_display::WindowsVirtualDisplay;

pub struct StreamerHandle {
    handle: axum_server::Handle,
}

pub struct AppState {
    pub virtual_display: SharedVirtualDisplay,
    pub stop_hosted_network: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub hosted_network_running: Mutex<bool>,
    pub network_adapters: Mutex<Vec<NetworkInfo>>,
    pub streamers: Mutex<HashMap<String, StreamerHandle>>,
    pub session_auth: SessionAuth,
    pub device_reporter: SharedDeviceReporter,
    pub device_overrides: SharedDeviceOverrides,
    pub sessions: SharedSessions,
    pub disconnect_grace: session::SharedDisconnectGrace,
    pub user_turn: SharedTurnConfig,
    pub server_ports: SharedServerPorts,
    pub cloud: Mutex<Option<CloudClient>>,
    pub cloud_status: Arc<Mutex<(String, String)>>,
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

pub fn set_display_topology_extend() {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, GetDisplayConfigBufferSizes,
        QueryDisplayConfig, SetDisplayConfig, QDC_ALL_PATHS, SDC_APPLY, SDC_TOPOLOGY_EXTEND,
    };

    const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

    fn source_key(p: &DISPLAYCONFIG_PATH_INFO) -> (u32, i32, u32) {
        (p.sourceInfo.adapterId.LowPart, p.sourceInfo.adapterId.HighPart, p.sourceInfo.id)
    }
    fn target_key(p: &DISPLAYCONFIG_PATH_INFO) -> (u32, i32, u32) {
        (p.targetInfo.adapterId.LowPart, p.targetInfo.adapterId.HighPart, p.targetInfo.id)
    }

    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    if let Err(e) = unsafe {
        GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count)
    }
    .ok()
    {
        teprintln!("[display] extend: GetDisplayConfigBufferSizes failed ({e})");
        return;
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    if let Err(e) = unsafe {
        QueryDisplayConfig(
            QDC_ALL_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    }
    .ok()
    {
        teprintln!("[display] extend: QueryDisplayConfig failed ({e})");
        return;
    }
    paths.truncate(path_count as usize);

    let active: Vec<&DISPLAYCONFIG_PATH_INFO> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0)
        .collect();

    let active_targets: std::collections::HashSet<_> =
        active.iter().map(|p| target_key(p)).collect();
    let active_sources: std::collections::HashSet<_> =
        active.iter().map(|p| source_key(p)).collect();
    let is_cloned = active_targets.len() >= 2 && active_sources.len() < active_targets.len();

    let has_inactive_display = paths.iter().any(|p| {
        let inactive = p.flags & DISPLAYCONFIG_PATH_ACTIVE == 0;
        let available = unsafe { p.targetInfo.targetAvailable.as_bool() };
        inactive && available && !active_targets.contains(&target_key(p))
    });

    if !is_cloned && !has_inactive_display {
        tprintln!(
            "[display] already extended ({} monitor(s), {} source(s)); nothing to do",
            active_targets.len(),
            active_sources.len()
        );
        return;
    }

    let reason = if is_cloned { "duplicate/clone topology" } else { "an inactive display" };
    tprintln!(
        "[display] {reason} detected ({} active monitor(s)); applying extend topology",
        active_targets.len()
    );

    let result = unsafe {
        SetDisplayConfig(
            None::<&[DISPLAYCONFIG_PATH_INFO]>,
            None::<&[DISPLAYCONFIG_MODE_INFO]>,
            SDC_TOPOLOGY_EXTEND | SDC_APPLY,
        )
    };
    if result == 0 {
        tprintln!("[display] extend topology applied");
    } else {
        teprintln!("[display] failed to apply extend topology (SetDisplayConfig win32 error {result})");
    }
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }
    tauri::async_runtime::spawn_blocking(set_display_topology_extend);
    let virtual_display =
        tauri::async_runtime::spawn_blocking(WindowsVirtualDisplay::new_shared).await;

    let virtual_display = match virtual_display {
        Ok(Some(vd)) => vd,
        _ => return false,
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
        streamers: Mutex::new(HashMap::new()),
        session_auth: SessionAuth::default(),
        device_reporter,
        device_overrides,
        sessions,
        disconnect_grace: session::new_shared_disconnect_grace(),
        user_turn: session::new_shared_turn_config(),
        server_ports: session::new_shared_server_ports(),
        cloud: Mutex::new(None),
        cloud_status: Arc::new(Mutex::new(("connecting".to_string(), String::new()))),
    };
    app_handle.manage(state);
    true
}

#[tauri::command]
#[specta::specta]
pub fn set_device_override(
    state: State<'_, AppState>,
    ip: String,
    scale: u32,
    orientation: String,
    refresh_rate: u32,
    video_scale: u32,
    video_quality: u32,
) {
    use crate::streamer::server::{
        MAX_DISPLAY_SCALE, MAX_REFRESH_RATE, MIN_DISPLAY_SCALE, MIN_REFRESH_RATE,
    };
    use crate::streamer::config::ScalePercent;

    state.device_overrides.lock().unwrap().insert(
        ip.clone(),
        DeviceOverride {
            scale: scale.clamp(MIN_DISPLAY_SCALE, MAX_DISPLAY_SCALE),
            orientation_portrait: orientation == "Portrait",
            refresh_rate: refresh_rate.clamp(MIN_REFRESH_RATE, MAX_REFRESH_RATE),
            video_scale: ScalePercent::new(video_scale).percent(),
            video_quality: video_quality.clamp(1, 51) as u8,
        },
    );
    session::bump_reconfig_epoch(&state.sessions, &ip);
}

#[tauri::command]
#[specta::specta]
pub fn remove_device_override(state: State<'_, AppState>, ip: String) {
    state.device_overrides.lock().unwrap().remove(&ip);
    session::bump_kick_epoch(&state.sessions, &ip);
    session::signal_leave(&state.sessions, &ip);
}

#[tauri::command]
#[specta::specta]
pub fn set_disconnect_grace(state: State<'_, AppState>, seconds: u32) {
    let secs = (seconds as u64)
        .clamp(session::MIN_DISCONNECT_GRACE_SECS, session::MAX_DISCONNECT_GRACE_SECS);
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

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone, Default, specta::Type)]
pub struct TurnConfig {
    pub urls: String,
    pub username: String,
    pub credential: String,
}

#[tauri::command]
#[specta::specta]
pub fn set_turn_config(state: State<'_, AppState>, urls: String, username: String, credential: String) {
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
pub fn set_server_ports(state: State<'_, AppState>, http_port: u16, https_port: u16) -> ServerPorts {
    let http = if http_port == 0 { session::DEFAULT_HTTP_PORT } else { http_port };
    let https = if https_port == 0 { session::DEFAULT_HTTPS_PORT } else { https_port };

    let (cur_http, cur_https) = state.server_ports.get();
    if http == https {
        teprintln!("[streamer] rejecting server port change: HTTP and HTTPS must differ ({http})");
        return ServerPorts { http: cur_http, https: cur_https };
    }
    if http == cur_http && https == cur_https {
        return ServerPorts { http, https };
    }

    state.server_ports.set(http, https);
    tprintln!("[streamer] server ports changed to HTTP :{http}, HTTPS :{https}; restarting streamers");

    {
        let mut streamers = state.streamers.lock().unwrap();
        for (ip, streamer) in streamers.drain() {
            tprintln!("[streamer] stopping streamer bound to {ip} for port change");
            streamer.handle.shutdown();
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    sync_streamers(&state);

    ServerPorts { http, https }
}

#[tauri::command]
#[specta::specta]
pub fn install_drivers(app: tauri::AppHandle) -> bool {
    let resource_path = |file: &str| {
        app.path()
            .resolve(file, BaseDirectory::Resource)
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap()
    };
    let exe_path = match std::env::current_exe() {
        Ok(exe_path) => exe_path.into_os_string().into_string().unwrap(),
        _ => "".to_string(),
    };
    let mut cmd = StdCommand::new(exe_path);
    cmd.arg("installdrivers");
    let mut admincmd = Command::new(cmd);
    let mut fincmd = admincmd.name("ScreenExtend".to_string());
    if let Ok(icon_bytes) = std::fs::read(&resource_path("icons/icon.icns")) {
        fincmd = fincmd.icon(icon_bytes);
    }
    let _ = fincmd.output().unwrap();
    true
}

#[tauri::command]
#[specta::specta]
pub fn remove_drivers(app: tauri::AppHandle) -> bool {
    let resource_path = |file: &str| {
        app.path()
            .resolve(file, BaseDirectory::Resource)
            .unwrap()
            .into_os_string()
            .into_string()
            .unwrap()
    };
    let exe_path = match std::env::current_exe() {
        Ok(exe_path) => exe_path.into_os_string().into_string().unwrap(),
        _ => "".to_string(),
    };
    let mut cmd = StdCommand::new(exe_path);
    cmd.arg("removedrivers");
    let mut admincmd = Command::new(cmd);
    let mut fincmd = admincmd.name("ScreenExtend".to_string());
    if let Ok(icon_bytes) = std::fs::read(&resource_path("icons/icon.icns")) {
        fincmd = fincmd.icon(icon_bytes);
    }
    let _ = fincmd.output().unwrap();
    true
}

pub fn remove_all_displays(client: &SharedVirtualDisplay) {
    let client = client.clone();
    let _ = std::thread::spawn(move || client.remove_all_displays()).join();
}

#[tauri::command]
#[specta::specta]
pub fn set_session_credentials(state: State<'_, AppState>, session_id: String, otp: String) {
    *state.session_auth.session_id.lock().unwrap() = session_id;
    *state.session_auth.otp.lock().unwrap() = otp;
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
    *state.cloud_status.lock().unwrap() = (CloudState::Disabled.as_str().to_string(), String::new());
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

pub fn sync_streamers(state: &AppState) {
    let desired: Vec<(String, Ipv4Addr)> = {
        let adapters = state.network_adapters.lock().unwrap();
        adapters
            .iter()
            .flat_map(|adapter| adapter.ip_addresses.iter())
            .filter_map(|ip| ip.parse::<Ipv4Addr>().ok().map(|addr| (ip.clone(), addr)))
            .collect()
    };

    let (http_port, https_port) = state.server_ports.get();

    let mut streamers = state.streamers.lock().unwrap();

    streamers.retain(|ip, streamer| {
        if desired.iter().any(|(desired_ip, _)| desired_ip == ip) {
            true
        } else {
            tprintln!("[streamer] stopping streamer bound to {ip}");
            streamer.handle.graceful_shutdown(Some(Duration::from_secs(1)));
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
            port: http_port,
            https_port,
            virtual_display: Some(state.virtual_display.clone()),
            session_auth: Some(state.session_auth.clone()),
            device_reporter: Some(state.device_reporter.clone()),
            device_overrides: Some(state.device_overrides.clone()),
            sessions: Some(state.sessions.clone()),
            disconnect_grace: Some(state.disconnect_grace.clone()),
            user_turn: Some(state.user_turn.clone()),
            ..Config::default()
        };

        let thread_handle = handle.clone();
        let ip_for_log = ip.clone();
        std::thread::spawn(move || {
            if let Err(e) = Streamer::new(config).run_with_handle(thread_handle) {
                teprintln!("[streamer] streamer bound to {ip_for_log} exited: {e}");
            }
        });

        tprintln!("[streamer] started streamer bound to {ip}");
        streamers.insert(ip, StreamerHandle { handle });
    }
}
