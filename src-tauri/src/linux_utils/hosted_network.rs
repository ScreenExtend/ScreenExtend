use crate::linux_utils::AppState;
use tauri::State;

#[tauri::command]
#[specta::specta]
pub fn start_hosted_network(state: State<'_, AppState>, name: &str, password: &str) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn stop_hosted_network(state: State<'_, AppState>) -> bool {
    true
}