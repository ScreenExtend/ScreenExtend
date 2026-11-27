//! The commands and event behind the Rearrange Displays dialog. The work is the active OS's
//! `display_layout` module; this file only moves it off the UI thread and forwards its changes.

use std::sync::Once;

use serde::{Deserialize, Serialize};
use specta::Type;
use tauri_specta::Event;

use crate::display_layout::{DisplayArranger, DisplayLayout, DisplayPlacement};

#[cfg(target_os = "linux")]
use crate::linux_utils::display_layout as platform;
#[cfg(target_os = "macos")]
use crate::macos_utils::display_layout as platform;
#[cfg(target_os = "windows")]
use crate::windows_utils::display_layout as platform;

/// The arrangement changed: a display was added, removed or moved, by anyone.
#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct DisplayLayoutChanged(pub DisplayLayout);

#[tauri::command]
#[specta::specta]
pub async fn get_display_layout() -> Result<DisplayLayout, String> {
    // a full display-configuration query can take hundreds of milliseconds
    tauri::async_runtime::spawn_blocking(platform::Arranger::read)
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
#[specta::specta]
pub async fn apply_display_layout(
    placements: Vec<DisplayPlacement>,
) -> Result<DisplayLayout, String> {
    tauri::async_runtime::spawn_blocking(move || platform::Arranger::apply(&placements))
        .await
        .map_err(|e| e.to_string())?
}

/// Starts the OS's watcher once, for the desktop window.
pub fn start_watching(app: tauri::AppHandle) {
    static STARTED: Once = Once::new();
    STARTED.call_once(move || {
        let forward = Box::new(move |layout: DisplayLayout| {
            if let Err(e) = DisplayLayoutChanged(layout).emit(&app) {
                teprintln!("[display-layout] could not emit DisplayLayoutChanged: {e}");
            }
        });
        match platform::watch(forward) {
            // started once for the life of the app: dropping the handle would stop it
            Ok(watcher) => {
                let _ = Box::leak(Box::new(watcher));
            }
            Err(e) => teprintln!("[display-layout] not watching for display changes: {e}"),
        }
    });
}
