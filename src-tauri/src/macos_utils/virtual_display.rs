use crate::macos_utils::AppState;
use serde::{Serialize, Deserialize};
use specta::Type;
use tauri::State;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct VirtualDisplayConfig {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

#[tauri::command]
#[specta::specta]
pub fn install_drivers() -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub async fn create_display(state: State<'_, AppState>, config: VirtualDisplayConfig) -> Result<i32, ()> {
    Ok(1)
}

#[tauri::command]
#[specta::specta]
pub async fn update_display(state: State<'_, AppState>, display_id: u32, config: VirtualDisplayConfig) -> Result<bool, ()> {
    Ok(true)
}

#[tauri::command]
#[specta::specta]
pub async fn remove_display(state: State<'_, AppState>, display_id: u32) -> Result<bool, ()> {
    Ok(true)
}

#[tauri::command]
#[specta::specta]
pub async fn remove_all_displays(state: State<'_, AppState>) -> Result<bool, ()> {
    Ok(true)
}