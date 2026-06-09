pub mod hosted_network;
pub mod networking;
pub mod virtual_display;
pub mod streamer;

use std::process::Command;

pub struct AppState {}

fn command_exists(cmd: &str) -> bool {
    Command::new("which")
        .arg(cmd)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    command_exists("nmcli")
}
