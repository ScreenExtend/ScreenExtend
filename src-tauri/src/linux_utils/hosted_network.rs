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

#[tauri::command]
#[specta::specta]
pub fn is_hosted_network(state: State<'_, AppState>) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn get_hosted_network_status(state: State<'_, AppState>) -> crate::HostedNetworkReport {
    crate::HostedNetworkReport {
        state: crate::HostedNetworkReport::NOT_STARTED.to_string(),
        secured: true,
        ..Default::default()
    }
}
