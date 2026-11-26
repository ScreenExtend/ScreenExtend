//! Where the host keeps its per-user state, and the test-only switch that moves it.
//!
//! `SCREENEXTEND_DATA_DIR`, set to a non-empty path, replaces the app-data, app-local-data and
//! app-config directories with that one directory (lock, control file, `config.json`, exit
//! breadcrumb, dev TLS certificate), so hosts with different values run side by side (as
//! `scripts/test-handler/` does) and a CLI verb run with the same value addresses that host.
//! Unset, every function here returns Tauri's own path. The JS side of `tauri-plugin-store` is
//! not redirected. A shipped build does not read it ([`crate::envcfg`]) and always uses Tauri's
//! paths.
//!
//! Setting it also puts the Windows display backend into shared-driver mode
//! (`windows_utils::virtual_display`): the driver's monitor list is machine-wide and each commit
//! replaces it, so an isolated host merges its monitors in instead of overwriting.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::{Manager, Runtime};

pub const DATA_DIR_ENV: &str = "SCREENEXTEND_DATA_DIR";

const CONFIG_FILE: &str = "config.json";

/// The redirected directory, or `None` when the switch is unset or empty. Read once.
pub fn override_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let raw = crate::envcfg::var_os(DATA_DIR_ENV)?;
        if raw.is_empty() {
            return None;
        }
        let path = PathBuf::from(raw);
        let path = std::path::absolute(&path).unwrap_or(path);
        if let Err(e) = std::fs::create_dir_all(&path) {
            teprintln!("[data-dir] cannot create {}: {e}", path.display());
        }
        Some(path)
    })
    .as_deref()
}

/// Whether this process runs with its state redirected (and therefore beside other hosts).
pub fn isolated() -> bool {
    override_dir().is_some()
}

pub fn local_data_dir<R: Runtime, M: Manager<R>>(app: &M) -> tauri::Result<PathBuf> {
    match override_dir() {
        Some(dir) => Ok(dir.to_path_buf()),
        None => app.path().app_local_data_dir(),
    }
}

pub fn data_dir<R: Runtime, M: Manager<R>>(app: &M) -> tauri::Result<PathBuf> {
    match override_dir() {
        Some(dir) => Ok(dir.to_path_buf()),
        None => app.path().app_data_dir(),
    }
}

pub fn config_dir<R: Runtime, M: Manager<R>>(app: &M) -> tauri::Result<PathBuf> {
    match override_dir() {
        Some(dir) => Ok(dir.to_path_buf()),
        None => app.path().app_config_dir(),
    }
}

/// The path to hand `tauri-plugin-store`. The plugin joins it onto the app-data directory, and
/// joining an absolute path yields that path, so the redirected store is just an absolute one.
pub fn config_store() -> PathBuf {
    match override_dir() {
        Some(dir) => dir.join(CONFIG_FILE),
        None => PathBuf::from(CONFIG_FILE),
    }
}
