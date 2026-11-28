//! The commands and event behind the Rearrange Displays dialog. The work is the active OS's
//! `display_layout` module; this file only moves it off the UI thread and forwards its changes.

use std::collections::HashMap;
use std::sync::{Mutex, Once, OnceLock};

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

/// Names the join each of our displays streams to, by the monitor identity its session recorded.
fn link_joins(app: &tauri::AppHandle, layout: &mut DisplayLayout) {
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    {
        use crate::streamer::lock::LockExt;
        use tauri::Manager;
        // before `setup` there is no state and no session to link
        let Some(state) = app.try_state::<crate::AppState>() else {
            return;
        };
        let links: Vec<(String, String)> = state
            .sessions
            .lock_recover()
            .iter()
            .filter_map(|(join, s)| {
                let path = &s.live_display.as_ref()?.device_path;
                (!path.is_empty()).then(|| (path.to_ascii_lowercase(), join.clone()))
            })
            .collect();
        for d in layout.displays.iter_mut().filter(|d| d.ours) {
            let id = d.id.to_ascii_lowercase();
            d.join_id = links
                .iter()
                .find(|(path, _)| *path == id)
                .map(|(_, join)| join.clone());
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (app, layout);
}

#[tauri::command]
#[specta::specta]
pub async fn get_display_layout(app: tauri::AppHandle) -> Result<DisplayLayout, String> {
    // a full display-configuration query can take hundreds of milliseconds
    let mut layout = tauri::async_runtime::spawn_blocking(platform::Arranger::read)
        .await
        .map_err(|e| e.to_string())??;
    link_joins(&app, &mut layout);
    Ok(layout)
}

#[tauri::command]
#[specta::specta]
pub async fn apply_display_layout(
    app: tauri::AppHandle,
    placements: Vec<DisplayPlacement>,
) -> Result<DisplayLayout, String> {
    let mut layout =
        tauri::async_runtime::spawn_blocking(move || platform::Arranger::apply(&placements))
            .await
            .map_err(|e| e.to_string())??;
    link_joins(&app, &mut layout);
    Ok(layout)
}

/// Starts the OS's watcher once, for the desktop window.
pub fn start_watching(app: tauri::AppHandle) {
    static STARTED: Once = Once::new();
    STARTED.call_once(move || {
        let forward = Box::new(move |mut layout: DisplayLayout| {
            link_joins(&app, &mut layout);
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

/// Where a device's display sits, relative to the main display at (0, 0), as the user last
/// placed it on the Devices page. Keyed by the device token: an address is recycled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Type)]
#[serde(rename_all = "camelCase")]
pub struct DevicePlacement {
    pub token: String,
    pub x: i32,
    pub y: i32,
}

fn saved_placements() -> &'static Mutex<HashMap<String, (i32, i32)>> {
    static SAVED: OnceLock<Mutex<HashMap<String, (i32, i32)>>> = OnceLock::new();
    SAVED.get_or_init(Default::default)
}

/// Replaces every saved placement. The desktop UI owns them in `config.json` and hands the whole
/// set over at startup and after every Apply; `serve` replays them from the same file.
pub fn replace_placements(placements: Vec<DevicePlacement>) {
    let mut map = saved_placements().lock().unwrap_or_else(|p| p.into_inner());
    map.clear();
    for p in placements.into_iter().filter(|p| !p.token.is_empty()) {
        map.insert(p.token, (p.x, p.y));
    }
}

/// The placements saved on `config.json`'s `devices[]` rows: a row needs a token and both
/// coordinates as integers, anything else is skipped.
pub fn placements_from_rows(rows: &[serde_json::Value]) -> Vec<DevicePlacement> {
    rows.iter()
        .filter_map(|row| {
            let token = row.get("token")?.as_str()?.to_string();
            let at = row.get("placement")?;
            let x = i32::try_from(at.get("x")?.as_i64()?).ok()?;
            let y = i32::try_from(at.get("y")?.as_i64()?).ok()?;
            (!token.is_empty()).then_some(DevicePlacement { token, x, y })
        })
        .collect()
}

#[tauri::command]
#[specta::specta]
pub fn set_device_placements(placements: Vec<DevicePlacement>) {
    replace_placements(placements);
}

fn placement_for(token: &str) -> Option<(i32, i32)> {
    saved_placements()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(token)
        .copied()
}

/// Puts a device's newly attached display back where the user last placed it. Positions only,
/// through the same arranger the Devices page uses, so the main display stays main and the
/// result still touches the other displays. Never fails the join: a refusal is logged. True when
/// it moved the display.
pub fn restore_placement(token: &str, device_path: &str) -> bool {
    let Some((x, y)) = placement_for(token) else {
        return false;
    };
    if device_path.is_empty() {
        return false;
    }
    let layout = match platform::Arranger::read() {
        Ok(layout) if layout.can_apply => layout,
        Ok(_) => return false,
        Err(e) => {
            teprintln!("[display-layout] could not read the layout to restore a placement: {e}");
            return false;
        }
    };
    let Some(display) = layout
        .displays
        .iter()
        .find(|d| d.id.eq_ignore_ascii_case(device_path))
    else {
        return false;
    };
    if (display.x, display.y) == (x, y) {
        return false;
    }
    let id = display.id.clone();
    match platform::Arranger::apply(&[DisplayPlacement { id, x, y }]) {
        Ok(applied) => {
            let landed = applied
                .displays
                .iter()
                .find(|d| d.id.eq_ignore_ascii_case(device_path))
                .map(|d| (d.x, d.y));
            tprintln!(
                "[display-layout] restored this device's saved placement ({x}, {y}); landed at {landed:?}"
            );
            true
        }
        Err(e) => {
            teprintln!("[display-layout] could not restore this device's placement: {e}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_placements_are_replaced_whole_and_keyed_by_token() {
        replace_placements(vec![
            DevicePlacement {
                token: "a".into(),
                x: 1920,
                y: 0,
            },
            DevicePlacement {
                token: "".into(),
                x: 5,
                y: 5,
            },
        ]);
        assert_eq!(placement_for("a"), Some((1920, 0)));
        assert_eq!(
            placement_for(""),
            None,
            "a row with no token names no device"
        );
        replace_placements(vec![DevicePlacement {
            token: "b".into(),
            x: -1280,
            y: 0,
        }]);
        assert_eq!(
            placement_for("a"),
            None,
            "a placement left out of the new set is forgotten"
        );
        assert_eq!(placement_for("b"), Some((-1280, 0)));
        replace_placements(Vec::new());
    }

    #[test]
    fn placements_are_read_from_saved_rows_with_a_token_and_whole_coordinates() {
        let rows: Vec<serde_json::Value> = serde_json::from_str(
            r#"[
                {"token": "a", "placement": {"x": 1920, "y": -40}},
                {"token": "", "placement": {"x": 1, "y": 1}},
                {"ip": "10.0.0.2", "placement": {"x": 1, "y": 1}},
                {"token": "b"},
                {"token": "c", "placement": {"x": 1.5, "y": 0}},
                {"token": "d", "placement": "left"}
            ]"#,
        )
        .unwrap();
        assert_eq!(
            placements_from_rows(&rows),
            vec![DevicePlacement {
                token: "a".into(),
                x: 1920,
                y: -40
            }]
        );
    }
}
