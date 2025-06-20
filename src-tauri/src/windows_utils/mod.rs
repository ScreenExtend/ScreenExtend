pub mod hosted_network;
pub mod virtual_display;
pub mod networking;

use driver_ipc::DriverClient;
use std::sync::Mutex;
use tauri::Manager;
use tokio::sync::Mutex as TokioMutex;
use tauri::State;

pub struct AppState {
    pub driver_client: TokioMutex<DriverClient>,
    pub stop_hosted_network: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    pub current_user: Mutex<String>,
    pub hosted_network_running: Mutex<bool>,
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    if app_handle.try_state::<AppState>().is_some() {
        return true;
    }
    match DriverClient::new().await {
        Ok(client) => {
            let state = AppState {
                driver_client: TokioMutex::new(client),
                stop_hosted_network: Mutex::new(None),
                current_user: Mutex::new("".to_string()),
                hosted_network_running: Mutex::new(false),
            };
            app_handle.manage(state);
            true
        }
        Err(_) => false,
    }
}

#[tauri::command]
#[specta::specta]
pub fn set_current_user(state: State<'_, AppState>, current_user: String) {
    *state.current_user.lock().unwrap() = current_user;
}
