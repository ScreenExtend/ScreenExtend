use std::process::Command as StdCommand;
use serde::{Serialize, Deserialize};
use crate::windows_utils::AppState;
use driver_ipc::{Monitor, Mode};
use elevated_command::Command;
use tauri_specta::Event;
use specta::Type;
use tauri::State;

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct VirtualDisplayConfig {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

#[tauri::command]
#[specta::specta]
pub fn install_drivers() -> bool {
    let exe_path = match std::env::current_exe() {
        Ok(exe_path) => exe_path.into_os_string().into_string().unwrap(),
        _ => {"".to_string()}
    };
    let mut cmd = StdCommand::new(exe_path);
    cmd.arg("installdrivers");
    let _ = Command::new(cmd).output();
    true
}

#[tauri::command]
#[specta::specta]
pub async fn create_display(state: State<'_, AppState>, config: VirtualDisplayConfig) -> Result<i32, ()> {
    let mut client = state.driver_client.lock().await;
    let id = client.new_id(None).unwrap();
    let mode = Mode {
        width: config.width,
        height: config.height,
        refresh_rates: vec![config.refresh_rate],
    };
    let new_monitor = Monitor {
        id,
        enabled: true,
        name: Some(config.name),
        modes: vec![mode],
    };
    match client.add(new_monitor) {
        Ok(()) => {
            match client.notify().await {
                Ok(()) => Ok(id as i32),
                Err(_) => Ok(-1)
            }
        },
        Err(_) => Ok(-1)
    }
}

#[tauri::command]
#[specta::specta]
pub async fn update_display(state: State<'_, AppState>, display_id: u32, config: VirtualDisplayConfig) -> Result<bool, ()> {
    let mut client = state.driver_client.lock().await;
    if let Some(monitor) = client.find_monitor_mut_unchecked(display_id) {
        monitor.name = Some(config.name);
        monitor.modes = vec![Mode {
            width: config.width,
            height: config.height,
            refresh_rates: vec![config.refresh_rate],
        }];
        if let Err(_) = client.notify().await {
            return Ok(false);
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

#[tauri::command]
#[specta::specta]
pub async fn remove_display(state: State<'_, AppState>, display_id: u32) -> Result<bool, ()> {
    let mut client = state.driver_client.lock().await;
    client.remove(&[display_id]);
    match client.notify().await {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn remove_all_displays(state: State<'_, AppState>) -> Result<bool, ()> {
    let mut client = state.driver_client.lock().await;
    client.remove_all();
    match client.notify().await {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}