pub mod device_reporter;
pub mod hosted_network;
pub mod networking;
pub mod streamer;
pub mod virtual_display;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tauri::State;
use crate::streamer::cloud::{CloudClient, CloudConfig, CloudState, CloudStatusSink, SharedCloudStatusSink};
use crate::streamer::session::{
    self, DeviceOverride, SessionAuth, SharedDeviceOverrides, SharedDeviceReporter, SharedSessions,
    SharedVirtualDisplay,
};
use crate::streamer::{Config, Streamer};
use device_reporter::TauriDeviceReporter;
use networking::NetworkInfo;
use virtual_display::MacosVirtualDisplay;

pub struct StreamerHandle {
    handle: axum_server::Handle,
}

pub struct AppState {
    pub virtual_display: SharedVirtualDisplay,
    pub stop_hosted_network: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub current_user: Mutex<String>,
    pub hosted_network_running: Mutex<bool>,
    pub network_adapters: Mutex<Vec<NetworkInfo>>,
    pub streamers: Mutex<HashMap<String, StreamerHandle>>,
    pub session_auth: SessionAuth,
    pub device_reporter: SharedDeviceReporter,
    pub device_overrides: SharedDeviceOverrides,
    pub sessions: SharedSessions,
    pub disconnect_grace: session::SharedDisconnectGrace,
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
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }
    let virtual_display =
        tauri::async_runtime::spawn_blocking(MacosVirtualDisplay::new_shared).await;

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
        current_user: Mutex::new("".to_string()),
        hosted_network_running: Mutex::new(false),
        network_adapters: Mutex::new(Vec::new()),
        streamers: Mutex::new(HashMap::new()),
        session_auth: SessionAuth::default(),
        device_reporter,
        device_overrides,
        sessions,
        disconnect_grace: session::new_shared_disconnect_grace(),
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

#[tauri::command]
#[specta::specta]
pub fn install_drivers(_app: tauri::AppHandle) -> bool {
    // No Windows-style virtual display drivers on macOS.
    true
}

#[tauri::command]
#[specta::specta]
pub fn remove_drivers(_app: tauri::AppHandle) -> bool {
    // No Windows-style virtual display drivers on macOS.
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
pub fn get_cloud_status(state: State<'_, AppState>) -> crate::CloudStatusChange {
    let (status, detail) = state.cloud_status.lock().unwrap().clone();
    crate::CloudStatusChange {
        state: status,
        detail,
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_current_user(state: State<'_, AppState>, current_user: String) {
    *state.current_user.lock().unwrap() = current_user;
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
            virtual_display: Some(state.virtual_display.clone()),
            session_auth: Some(state.session_auth.clone()),
            device_reporter: Some(state.device_reporter.clone()),
            device_overrides: Some(state.device_overrides.clone()),
            sessions: Some(state.sessions.clone()),
            disconnect_grace: Some(state.disconnect_grace.clone()),
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
