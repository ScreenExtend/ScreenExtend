pub mod audio;
pub mod compatibility;
pub mod hosted_network;
pub mod networking;
pub mod permissions;
pub mod streamer;
pub mod virtual_display;

use crate::DisplayCapacity;
use std::process::Command;

pub struct AppState {}

fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub fn focus_main_window(window: &tauri::WebviewWindow) {
    let _ = window.set_focus();
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    command_exists("nmcli")
}

#[tauri::command]
#[specta::specta]
pub fn install_audio_driver(_app: tauri::AppHandle) -> String {
    "unsupported".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn uninstall_audio_driver() -> String {
    "unsupported".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn audio_driver_status() -> String {
    "unsupported".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn set_legacy_volume_key_proxy(_enabled: bool) {}

#[tauri::command]
#[specta::specta]
pub fn set_device_audio_output(
    _state: tauri::State<'_, AppState>,
    _ip: String,
    _device_id: String,
) {
}

#[tauri::command]
#[specta::specta]
pub fn get_device_audio_outputs(
    _state: tauri::State<'_, AppState>,
    _ip: String,
) -> crate::streamer::session::AudioOutputsReport {
    crate::streamer::session::AudioOutputsReport::default()
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
        backend: "unsupported".to_string(),
    }
}
