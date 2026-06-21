//! Display enumeration via CoreGraphics (`CGDirectDisplay`).
//!
//! The macOS analogue of `windows_utils::streamer::capture`'s monitor helpers.
//! A "device name" here is simply the `CGDirectDisplayID` rendered as a decimal
//! string — stable for the life of a display connection, which is all the
//! server's per-display bookkeeping needs.

use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGError, CGGetActiveDisplayList, CGMainDisplayID,
};

use super::DisplayId;

const MAX_DISPLAYS: usize = 16;

/// Active display IDs, main display first.
pub fn active_displays() -> Vec<DisplayId> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count: u32 = 0;
    // SAFETY: writes up to MAX_DISPLAYS ids and the count.
    let err = unsafe {
        CGGetActiveDisplayList(
            MAX_DISPLAYS as u32,
            ids.as_mut_ptr(),
            &mut count,
        )
    };
    if err != CGError::Success {
        teprintln!("[display] CGGetActiveDisplayList failed: CGError {}", err.0);
        return Vec::new();
    }
    ids[..count as usize].to_vec()
}

/// Resolve the configured monitor index to a `CGDirectDisplayID`. Index 0 (and
/// the default) maps to the main display; otherwise it is a 1-based index into
/// the active-display list, matching the server's monitor numbering.
pub fn select_display(monitor: u32) -> Option<DisplayId> {
    if monitor == 0 {
        // SAFETY: always returns the main display id.
        return Some(unsafe { CGMainDisplayID() });
    }
    let displays = active_displays();
    displays
        .get((monitor - 1) as usize)
        .copied()
        .or_else(|| displays.first().copied())
}

/// Parse a device name (decimal `CGDirectDisplayID`) and confirm it is active.
pub fn display_by_name(device_name: &str) -> Option<DisplayId> {
    let id: DisplayId = device_name.trim().parse().ok()?;
    active_displays().into_iter().find(|&d| d == id)
}

/// Stable identifier string for a display (its `CGDirectDisplayID`).
pub fn display_name(display: DisplayId) -> String {
    display.to_string()
}

/// `(native_width, native_height, refresh_hz)` for a display. Refresh is 0 for
/// displays that report no fixed rate (e.g. some internal panels); callers
/// substitute a default.
pub fn display_geometry(display: DisplayId) -> (u32, u32, u32) {
    // SAFETY: CG display geometry getters take a display id and return sizes.
    let w = unsafe { CGDisplayPixelsWide(display) } as u32;
    let h = unsafe { CGDisplayPixelsHigh(display) } as u32;
    let refresh = display_refresh_hz(display);
    (w, h, refresh)
}

fn display_refresh_hz(display: DisplayId) -> u32 {
    // SAFETY: copies the current display mode (may be None) and reads its rate.
    unsafe {
        let mode: Option<objc2_core_foundation::CFRetained<CGDisplayMode>> =
            CGDisplayCopyDisplayMode(display);
        match mode {
            Some(m) => CGDisplayMode::refresh_rate(Some(&m)).round() as u32,
            None => 0,
        }
    }
}

/// Active display IDs as device-name strings (for the server's monitor list).
pub fn device_names() -> Vec<String> {
    active_displays().into_iter().map(display_name).collect()
}

/// Native pixel size of a display by device name.
pub fn dimensions_by_name(device_name: &str) -> Option<(u32, u32)> {
    let display = display_by_name(device_name)?;
    let (w, h, _) = display_geometry(display);
    Some((w, h))
}
