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

/// Restart the app so it comes back as a fresh process. On macOS this is how a
/// newly granted permission is picked up; here there is no per-process permission
/// cache to escape, so the plain restart is enough.
#[tauri::command]
#[specta::specta]
pub fn relaunch_app(app: tauri::AppHandle) {
    app.restart();
}
