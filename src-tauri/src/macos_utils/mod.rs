pub mod hosted_network;
pub mod networking;
pub mod virtual_display;

pub struct AppState {}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    true
}
