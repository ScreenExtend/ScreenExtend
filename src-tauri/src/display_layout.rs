//! The in-app display arrangement: where every connected display sits on the desktop, and moving
//! them. Positions only: no mode, scale, rotation, mirroring or primary change ever goes through
//! here. Each OS implements [`DisplayArranger`] in its `*_utils::display_layout` module.

use serde::{Deserialize, Serialize};

/// One display as the arrangement sees it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct ArrangedDisplay {
    /// Stable across reboots: the monitor device path on Windows, the display UUID on macOS.
    pub id: String,
    /// What the user sees in the system's own settings.
    pub name: String,
    /// Top-left on the desktop, in `units`. The primary display is at (0, 0).
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
    /// A virtual display this host created for a client.
    pub ours: bool,
    /// Displays showing the same picture share a group and move together.
    pub mirror_group: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DisplayLayout {
    pub displays: Vec<ArrangedDisplay>,
    /// "pixels" on Windows, "points" on macOS.
    pub units: String,
    /// Whether this session may move displays at all (false over Remote Desktop, or unsupported).
    pub can_apply: bool,
    /// Why `can_apply` is false, for the UI.
    pub reason: Option<String>,
}

/// A requested position for one display, by [`ArrangedDisplay::id`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "camelCase")]
pub struct DisplayPlacement {
    pub id: String,
    pub x: i32,
    pub y: i32,
}

/// Implemented once per OS.
pub trait DisplayArranger {
    /// The current arrangement, read fresh every time.
    fn read() -> Result<DisplayLayout, String>;
    /// Moves displays to `placements` and returns the layout the OS actually applied. The
    /// primary display stays primary: the whole layout is translated so it is at (0, 0).
    /// Displays not named keep their position. Changes persist across reboots.
    fn apply(placements: &[DisplayPlacement]) -> Result<DisplayLayout, String>;
}
