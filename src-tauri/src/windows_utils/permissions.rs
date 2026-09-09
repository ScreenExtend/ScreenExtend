use crate::PermissionStatus;

/// Nothing on Windows is gated behind a user-granted permission, so the list is empty and the
/// probe costs nothing. It exists so the command surface matches macOS.
pub fn probe_permissions() -> Vec<PermissionStatus> {
    Vec::new()
}

/// Counterpart to the macOS warm-up; there is no probe here to warm.
pub fn prime() {}

#[tauri::command]
#[specta::specta]
pub async fn check_permissions() -> Vec<PermissionStatus> {
    probe_permissions()
}

#[tauri::command]
#[specta::specta]
pub async fn request_permission(_key: String) -> bool {
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
