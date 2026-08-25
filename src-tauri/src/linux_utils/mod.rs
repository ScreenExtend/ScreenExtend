pub mod audio;
pub mod compatibility;
pub mod hosted_network;
pub mod networking;
pub mod permissions;
pub mod streamer;
pub mod virtual_display;

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

// System-audio driver commands are the macOS 10.15–12.x legacy tier only
// (PRD-macos-legacy-audio §8.4); inert on Linux.
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
