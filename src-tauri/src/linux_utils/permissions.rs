use crate::PermissionStatus;

/// Always empty: nothing on Linux needs a user-granted permission. Exists to match macOS.
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

/// No per-process permission cache to escape here either; the command exists to
/// keep the surface identical across the three platform modules.
#[tauri::command]
#[specta::specta]
pub fn relaunch_app(app: tauri::AppHandle) {
    crate::exit_reason::record("relaunch_app: the desktop UI asked for a restart");
    app.restart();
}
