use crate::PermissionStatus;

#[tauri::command]
#[specta::specta]
pub fn check_permissions() -> Vec<PermissionStatus> {
    Vec::new()
}

#[tauri::command]
#[specta::specta]
pub fn request_permission(_key: String) -> bool {
    true
}

#[tauri::command]
#[specta::specta]
pub fn open_permission_settings(_key: String) {}

/// No per-process permission cache to escape here either; the command exists to
/// keep the surface identical across the three platform modules.
#[tauri::command]
#[specta::specta]
pub fn relaunch_app(app: tauri::AppHandle) {
    app.restart();
}
