use objc2_core_graphics::{
    CGDisplayCopyDisplayMode, CGDisplayMode, CGDisplayPixelsHigh, CGDisplayPixelsWide,
    CGError, CGGetActiveDisplayList, CGMainDisplayID,
};

use super::DisplayId;

const MAX_DISPLAYS: usize = 16;

pub fn active_displays() -> Vec<DisplayId> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count: u32 = 0;
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

pub fn select_display(monitor: u32) -> Option<DisplayId> {
    if monitor == 0 {
        return Some(unsafe { CGMainDisplayID() });
    }
    let displays = active_displays();
    displays
        .get((monitor - 1) as usize)
        .copied()
        .or_else(|| displays.first().copied())
}

pub fn display_by_name(device_name: &str) -> Option<DisplayId> {
    let id: DisplayId = device_name.trim().parse().ok()?;
    active_displays().into_iter().find(|&d| d == id)
}

pub fn display_name(display: DisplayId) -> String {
    display.to_string()
}

pub fn display_geometry(display: DisplayId) -> (u32, u32, u32) {
    let w = unsafe { CGDisplayPixelsWide(display) } as u32;
    let h = unsafe { CGDisplayPixelsHigh(display) } as u32;
    let refresh = display_refresh_hz(display);
    (w, h, refresh)
}

fn display_refresh_hz(display: DisplayId) -> u32 {
    unsafe {
        let mode: Option<objc2_core_foundation::CFRetained<CGDisplayMode>> =
            CGDisplayCopyDisplayMode(display);
        match mode {
            Some(m) => CGDisplayMode::refresh_rate(Some(&m)).round() as u32,
            None => 0,
        }
    }
}

pub fn device_names() -> Vec<String> {
    active_displays().into_iter().map(display_name).collect()
}

pub fn dimensions_by_name(device_name: &str) -> Option<(u32, u32)> {
    let display = display_by_name(device_name)?;
    let (w, h, _) = display_geometry(display);
    Some((w, h))
}
