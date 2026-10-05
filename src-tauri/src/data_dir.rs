//! Where the host keeps its per-user state, and the test-only switch that moves it.
//!
//! `SCREENEXTEND_DATA_DIR`, when set to a non-empty path, replaces the app-data, app-local-data
//! and app-config directories with that one directory. Everything that makes a host a
//! *singleton* lives there — `screenextend.lock`, the control socket's `screenextend.ctrl`,
//! `config.json`, the exit breadcrumb and the dev TLS certificate — so two hosts started with
//! two different values can run side by side, and every CLI verb run with the same value
//! (`status`, `stop`, `devices set`, `logs`, `config …`) addresses that one host.
//!
//! It exists for `scripts/test-handler/`, which runs up to four hosts at once. Unset, every
//! function here returns exactly what Tauri's path resolver returns, so a normal install never
//! sees it. The desktop UI's own store calls (the JS side of `tauri-plugin-store`) are not
//! redirected; the switch is for `serve` and the CLI.
//!
//! Setting it also puts the Windows display backend into shared-driver mode
//! (`windows_utils::virtual_display`): the driver keeps one monitor list for the whole machine
//! and each commit replaces it, so a host that does not own the driver alone must merge its
//! monitors into the live list instead of overwriting it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::{Manager, Runtime};

pub const DATA_DIR_ENV: &str = "SCREENEXTEND_DATA_DIR";

const CONFIG_FILE: &str = "config.json";

/// The redirected directory, or `None` when the switch is unset or empty. Read once.
pub fn override_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let raw = std::env::var_os(DATA_DIR_ENV)?;
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
