pub mod hosted_network;
pub mod virtual_display;

use driver_ipc::DriverClient;
use std::sync::{Arc, Mutex};
use tauri::Manager;
use tokio::sync::Mutex as TokioMutex;

pub struct AppState {
    pub driver_client: Arc<TokioMutex<DriverClient>>,
    pub stop_hosted_network: Arc<Mutex<Option<Box<dyn Fn() + Send + Sync>>>>,
}

#[tauri::command]
#[specta::specta]
pub async fn setup(app_handle: tauri::AppHandle) -> bool {
    match DriverClient::new().await {
        Ok(client) => {
            let state = AppState {
                driver_client: Arc::new(TokioMutex::new(client)),
                stop_hosted_network: Arc::new(Mutex::new(None)),
            };
            app_handle.manage(state);
            true
        }
        Err(_) => false,
    }
}
