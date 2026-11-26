use std::sync::{Arc, Mutex};

use crate::windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use crate::windows_capture::encoder::ImageFormat;
use crate::windows_capture::frame::Frame;
use crate::windows_capture::graphics_capture_api::InternalCaptureControl;
use crate::windows_capture::monitor::Monitor;
use crate::windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use anyhow::{anyhow, bail, Context as _, Result};
use windows::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::streamer::session::DisplayTarget;
use crate::windows_utils::virtual_display::{resolve_owned_display, ResolvedDisplay};

pub fn set_dpi_awareness() {
    // once the process is per-monitor v2, asking again is refused with an error
    let already = unsafe {
        AreDpiAwarenessContextsEqual(
            GetThreadDpiAwarenessContext(),
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        )
    };
    if already.as_bool() {
        return;
    }
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        teprintln!("SetProcessDpiAwarenessContext failed (likely already set): {e}");
    }
}

#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub index: u32,
    pub name: String,
    pub gpu: String,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

pub fn check_dwm_composition() -> Result<()> {
    use windows::Win32::Graphics::Dwm::DwmIsCompositionEnabled;
    match unsafe { DwmIsCompositionEnabled() } {
        Ok(enabled) if enabled.as_bool() => Ok(()),
        Ok(_) => bail!("DWM composition disabled, install Desktop Experience on Server"),
        Err(_) => {
            teprintln!("DwmIsCompositionEnabled failed (non-fatal)");
            Ok(())
        }
    }
}

pub fn select_monitor(requested: u32) -> Result<(Monitor, MonitorInfo)> {
    let monitors = Monitor::enumerate().context("enumerating monitors")?;
    if monitors.is_empty() {
        bail!("no displays found");
    }

    tprintln!("{} display(s) detected:", monitors.len());
    for (i, m) in monitors.iter().enumerate() {
        let name = m.name().unwrap_or_else(|_| "<unknown>".into());
        let gpu = m.device_string().unwrap_or_else(|_| "<unknown gpu>".into());
        let w = m.width().unwrap_or(0);
        let h = m.height().unwrap_or(0);
        let hz = m.refresh_rate().unwrap_or(0);
        tprintln!("  display[{i}]: {name} -- {gpu} -- {w}x{h}@{hz}Hz");
    }

    let index = if (requested as usize) < monitors.len() {
        requested
    } else {
        teprintln!("requested display {requested} absent, falling back to 0");
        0
    };

    let monitor = monitors[index as usize];
    let info = MonitorInfo {
        index,
        name: monitor.name().unwrap_or_else(|_| "<unknown>".into()),
        gpu: monitor
            .device_string()
            .unwrap_or_else(|_| "<unknown gpu>".into()),
        width: monitor.width().context("monitor width")?,
        height: monitor.height().context("monitor height")?,
        refresh_hz: monitor.refresh_rate().unwrap_or(0),
    };
    Ok((monitor, info))
}

pub fn monitor_device_names() -> Vec<String> {
    match Monitor::enumerate() {
        Ok(monitors) => monitors
            .iter()
            .filter_map(|m| m.device_name().ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

pub fn monitor_dimensions(device_name: &str) -> Option<(u32, u32)> {
    let monitors = Monitor::enumerate().ok()?;
    let monitor = monitors
        .iter()
        .find(|m| m.device_name().ok().as_deref() == Some(device_name))?;
    Some((monitor.width().ok()?, monitor.height().ok()?))
}

/// [`monitor_dimensions`] for a display named by its stable identity rather than by whichever
/// `\\.\DISPLAYn` it held when the session started. `None` when the display cannot be
/// identified: the alternative is recording some other monitor's geometry as this session's.
pub fn monitor_dimensions_owned(target: &DisplayTarget) -> Option<(u32, u32)> {
    let resolved = resolve_owned_display(target).ok()?;
    monitor_dimensions(&resolved.gdi_name)
}

pub fn monitor_rect(device_name: &str) -> Option<(i32, i32, u32, u32)> {
    use std::mem;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
    };

    struct Ctx {
        want: Vec<u16>,
        found: Option<(i32, i32, u32, u32)>,
    }

    unsafe extern "system" fn cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rc: *mut RECT,
        lparam: LPARAM,
    ) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        let mut mi = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: mem::size_of::<MONITORINFOEXW>() as u32,
                rcMonitor: RECT::default(),
                rcWork: RECT::default(),
                dwFlags: 0,
            },
            szDevice: [0; 32],
        };
        if GetMonitorInfoW(hmon, (&raw mut mi).cast()).as_bool() {
            let len = mi.szDevice.iter().position(|&c| c == 0).unwrap_or(32);
            if mi.szDevice[..len] == ctx.want[..] {
                let r = mi.monitorInfo.rcMonitor;
                ctx.found = Some((
                    r.left,
                    r.top,
                    (r.right - r.left).max(1) as u32,
                    (r.bottom - r.top).max(1) as u32,
                ));
                return BOOL(0);
            }
        }
        BOOL(1)
    }

    let want: Vec<u16> = device_name.encode_utf16().collect();
    let mut ctx = Ctx { want, found: None };
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&raw mut ctx as isize));
    }
    ctx.found
}

pub fn set_display_resolution(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
) -> Result<()> {
    set_display_mode(device_name, width, height, refresh, false).map(|_| ())
}

/// Change one of **our** displays' mode. Returns the refresh rate the display is actually running
/// at afterwards.
///
/// The name is a hint: Windows hands a `\\.\DISPLAYn` straight back out when the monitor holding
/// it detaches, and `ChangeDisplaySettingsExW` resizes whatever it finds there. So the target is
/// re-derived from the display's stable identity before every write (the retry runs for up to a
/// second while displays can attach and detach) and refused if it does not resolve to a display
/// this host created.
///
/// An IDD monitor's mode list is fixed when it is created, so an unregistered rate is refused and
/// the fallback applies the resolution at the rate the display is already on. That rate is what
/// is returned, never the requested one: a session that records a cadence the display never had
/// makes every later save compare equal and change nothing.
pub fn set_display_mode_owned(
    target: &DisplayTarget,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<u32> {
    let badmode_falls_back = super::tune::on("display_mode_badmode_fast_fallback");
    let gdi_name = set_display_mode_retrying(target, width, height, refresh, portrait, |plan| {
        plan_mode_write(plan, badmode_falls_back)
    })?;
    note_display_disturbed(&target.device_path);
    Ok(
        crate::windows_utils::virtual_display::current_display_mode(&gdi_name)
            .map(|(_, _, hz)| hz)
            .filter(|hz| *hz > 0)
            .unwrap_or(refresh),
    )
}

pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<u32> {
    set_display_mode_owned(
        &DisplayTarget {
            device_path: String::new(),
            device_name: device_name.to_string(),
        },
        width,
        height,
        refresh,
        portrait,
    )
}

/// Where a mode write stands after one `ChangeDisplaySettingsExW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModeWritePlan {
    /// The write that was just made carried the refresh rate.
    with_rate: bool,
    succeeded: bool,
    /// The write failed with `DISP_CHANGE_BADMODE`.
    badmode: bool,
    /// The display's own mode list has this size at some rate, but not at the one asked for:
    /// the rate was never registered and no retry can reach it. False when the list could not
    /// be read or does not have the size at all (a display that is still settling).
    rate_unregistered: bool,
    /// The rate-less write has already been tried once, early, and failed.
    early_fallback_failed: bool,
    expired: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeWriteStep {
    Done,
    /// Write the mode again with the rate, after a short wait.
    Retry,
    /// Write the mode without the rate, now.
    WithoutRate,
    Fail,
}

/// What to do after one mode write.
///
/// A rate the mode list does not have at that size is refused with `DISP_CHANGE_BADMODE` every
/// time, so with `badmode_falls_back` it goes to the rate-less fallback at once; if that fails
/// too (the resolution itself was refused) the retry resumes. A `BADMODE` for a size@rate the
/// list *does* have is a display still settling: it is retried with the rate for the whole
/// window so the rate asked for can still be reached. Every other failure is retried until the
/// window closes.
fn plan_mode_write(plan: ModeWritePlan, badmode_falls_back: bool) -> ModeWriteStep {
    if plan.succeeded {
        return ModeWriteStep::Done;
    }
    if !plan.with_rate {
        // the fallback failed: early, go back to retrying the full mode; at the end, give up
        return if plan.expired {
            ModeWriteStep::Fail
        } else {
            ModeWriteStep::Retry
        };
    }
    if plan.expired {
        return ModeWriteStep::WithoutRate;
    }
    if badmode_falls_back && plan.badmode && plan.rate_unregistered && !plan.early_fallback_failed {
        return ModeWriteStep::WithoutRate;
    }
    ModeWriteStep::Retry
}

/// Whether `modes` (width, height, rate) has `width`x`height` at some rate but never at `refresh`.
/// A size missing altogether says nothing about the rate, so it is not "unregistered".
fn rate_is_unregistered(modes: &[(u32, u32, u32)], width: u32, height: u32, refresh: u32) -> bool {
    let mut size_listed = false;
    for &(w, h, hz) in modes {
        if (w, h) == (width, height) {
            if hz == refresh {
                return false;
            }
            size_listed = true;
        }
    }
    size_listed
}

/// Every mode the display lists, as (width, height, rate).
fn display_modes(device_name: &str) -> Vec<(u32, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        EnumDisplaySettingsW, DEVMODEW, ENUM_DISPLAY_SETTINGS_MODE,
    };

    let wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut modes = Vec::new();
    // an IDD monitor lists a few dozen modes: the cap only bounds a driver that never says stop
    for index in 0..4096u32 {
        let mut devmode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        let listed = unsafe {
            EnumDisplaySettingsW(
                PCWSTR(wide.as_ptr()),
                ENUM_DISPLAY_SETTINGS_MODE(index),
                &mut devmode,
            )
        };
        if !listed.as_bool() {
            break;
        }
        modes.push((
            devmode.dmPelsWidth,
            devmode.dmPelsHeight,
            devmode.dmDisplayFrequency,
        ));
    }
    modes
}

/// The write loop of [`set_display_mode_owned`]. Returns the GDI name the successful write went
/// to.
fn set_display_mode_retrying(
    target: &DisplayTarget,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
    plan: impl Fn(ModeWritePlan) -> ModeWriteStep,
) -> Result<String> {
    use windows::Win32::Graphics::Gdi::{DISP_CHANGE_BADMODE, DISP_CHANGE_SUCCESSFUL};

    let (pels_w, pels_h) = if portrait {
        (height, width)
    } else {
        (width, height)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let mut with_rate = true;
    let mut early_fallback_failed = false;
    let mut last_with_rate = None;
    // read on the first `BADMODE` only: the mode list is fixed at create
    let mut unregistered_rate = None;
    loop {
        let resolved = resolve_owned_display(target)?;
        let result = write_display_mode(&resolved.gdi_name, pels_w, pels_h, refresh, with_rate);
        if with_rate {
            last_with_rate = Some(result);
        }
        let badmode = result == DISP_CHANGE_BADMODE;
        let rate_unregistered = with_rate
            && badmode
            && *unregistered_rate.get_or_insert_with(|| {
                rate_is_unregistered(&display_modes(&resolved.gdi_name), pels_w, pels_h, refresh)
            });
        let step = plan(ModeWritePlan {
            with_rate,
            succeeded: result == DISP_CHANGE_SUCCESSFUL,
            badmode,
            rate_unregistered,
            early_fallback_failed,
            expired: std::time::Instant::now() >= deadline,
        });
        match step {
            ModeWriteStep::Done => {
                if !with_rate {
                    teprintln!(
                        "[display] {} rejected {pels_w}x{pels_h}@{refresh} ({:?}); applied \
                         {pels_w}x{pels_h} at its current refresh rate instead",
                        resolved.gdi_name,
                        last_with_rate.unwrap_or(result)
                    );
                }
                return Ok(resolved.gdi_name);
            }
            ModeWriteStep::WithoutRate => with_rate = false,
            ModeWriteStep::Retry => {
                if !with_rate {
                    early_fallback_failed = true;
                }
                with_rate = true;
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            ModeWriteStep::Fail => bail!(
                "ChangeDisplaySettingsExW({}, {pels_w}x{pels_h}@{refresh}, portrait={portrait}) \
                 -> {:?}",
                resolved.gdi_name,
                last_with_rate.unwrap_or(result)
            ),
        }
    }
}

/// One `ChangeDisplaySettingsExW` of `pels_w`x`pels_h` (already oriented), with or without the
/// refresh rate. `DM_DISPLAYFREQUENCY` must travel with the size or it is refused even at a
/// registered mode.
fn write_display_mode(
    device_name: &str,
    pels_w: u32,
    pels_h: u32,
    refresh: u32,
    with_rate: bool,
) -> windows::Win32::Graphics::Gdi::DISP_CHANGE {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW, DMDO_DEFAULT,
        DM_DISPLAYFREQUENCY, DM_DISPLAYORIENTATION, DM_PELSHEIGHT, DM_PELSWIDTH,
        ENUM_CURRENT_SETTINGS,
    };

    let wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name = PCWSTR(wide.as_ptr());

    let mut devmode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe {
        let _ = EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut devmode);
    }
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    devmode.dmPelsWidth = pels_w;
    devmode.dmPelsHeight = pels_h;
    devmode.Anonymous1.Anonymous2.dmDisplayOrientation = DMDO_DEFAULT;
    devmode.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYORIENTATION;
    if with_rate {
        devmode.dmDisplayFrequency = refresh;
        devmode.dmFields |= DM_DISPLAYFREQUENCY;
    } else {
        devmode.dmDisplayFrequency = 0;
        devmode.dmFields &= !DM_DISPLAYFREQUENCY;
    }
    unsafe { ChangeDisplaySettingsExW(name, Some(&devmode), None, CDS_UPDATEREGISTRY, None) }
}

pub fn set_display_orientation(device_name: &str, portrait: bool) -> Result<()> {
    let resolved = resolve_owned_display(&DisplayTarget {
        device_path: String::new(),
        device_name: device_name.to_string(),
    })?;
    set_display_orientation_resolved(&resolved.gdi_name, portrait)
}

fn set_display_orientation_resolved(device_name: &str, portrait: bool) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DMDO_90, DMDO_DEFAULT, DM_DISPLAYORIENTATION, DM_PELSHEIGHT,
        DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
    };

    let wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let name = PCWSTR(wide.as_ptr());

    let mut devmode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe {
        if !EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut devmode).as_bool() {
            bail!("EnumDisplaySettingsW({device_name}) failed");
        }
    }

    let current = unsafe { devmode.Anonymous1.Anonymous2.dmDisplayOrientation };
    let is_portrait_now = current == DMDO_90 || current.0 == 3;
    if is_portrait_now == portrait {
        return Ok(());
    }

    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    devmode.Anonymous1.Anonymous2.dmDisplayOrientation =
        if portrait { DMDO_90 } else { DMDO_DEFAULT };
    let w = devmode.dmPelsWidth;
    let h = devmode.dmPelsHeight;
    devmode.dmPelsWidth = h;
    devmode.dmPelsHeight = w;
    devmode.dmFields |= DM_DISPLAYORIENTATION | DM_PELSWIDTH | DM_PELSHEIGHT;

    let result =
        unsafe { ChangeDisplaySettingsExW(name, Some(&devmode), None, CDS_UPDATEREGISTRY, None) };
    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        bail!("ChangeDisplaySettingsExW orientation({device_name}) -> {result:?}");
    }
}

#[repr(C)]
struct DisplayConfigGetDpi {
    header: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_HEADER,
    min_scale_rel: i32,
    cur_scale_rel: i32,
    max_scale_rel: i32,
}

#[repr(C)]
struct DisplayConfigSetDpi {
    header: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_HEADER,
    scale_rel: i32,
}

const DISPLAYCONFIG_DEVICE_INFO_GET_DPI: i32 = -3;
const DISPLAYCONFIG_DEVICE_INFO_SET_DPI: i32 = -4;

pub const DPI_PERCENT_VALUES: [u32; 12] =
    [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

/// The DPI band Windows reports for one display, as `(current, minimum, maximum)` step indices
/// into [`DPI_PERCENT_VALUES`]. Read-only: `virtual_display::log_display_inventory` prints it so
/// a report of a changed screen zoom can be checked against it. The two
/// `DISPLAYCONFIG_DEVICE_INFO_TYPE` values this and the writer use are undocumented.
pub fn read_dpi_band(adapter_id: (u32, i32), source_id: u32) -> Option<(i32, i32, i32)> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_DEVICE_INFO_TYPE,
    };
    use windows::Win32::Foundation::LUID;

    let mut get = DisplayConfigGetDpi {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DISPLAYCONFIG_DEVICE_INFO_GET_DPI),
            size: std::mem::size_of::<DisplayConfigGetDpi>() as u32,
            adapterId: LUID {
                LowPart: adapter_id.0,
                HighPart: adapter_id.1,
            },
            id: source_id,
        },
        min_scale_rel: 0,
        cur_scale_rel: 0,
        max_scale_rel: 0,
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut get.header) } != 0 {
        return None;
    }
    let recommended = -get.min_scale_rel;
    Some((
        recommended + get.cur_scale_rel,
        recommended + get.min_scale_rel,
        recommended + get.max_scale_rel,
    ))
}

/// One step index as a percentage, clamped into the table.
pub fn dpi_percent_at(index: i32) -> u32 {
    DPI_PERCENT_VALUES
        .get(index.clamp(0, DPI_PERCENT_VALUES.len() as i32 - 1) as usize)
        .copied()
        .unwrap_or(100)
}

/// What one pass of [`ensure_dpi_at`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DpiState {
    /// The display is where it is going to be: either at the requested step, or at the closest
    /// step it is willing to take. `at` is the index it actually holds.
    Settled { at: i32, requested: bool },
    /// A write went out; read it back on the next pass.
    Wrote,
}

/// Decide what to do about one display's DPI, given what `GET_DPI` reported. Pure, so the rule
/// can be tested on a machine with one display.
///
/// `min_rel` and `max_rel` are the offsets Windows reports from the *recommended* step to the
/// lowest and highest it will accept: a target outside that band has already been refused.
fn plan_dpi(min_rel: i32, cur_rel: i32, max_rel: i32, target_idx: i32) -> DpiState {
    let recommended_idx = -min_rel;
    let current_idx = recommended_idx + cur_rel;
    let reachable_rel = (target_idx - recommended_idx).clamp(min_rel, max_rel.max(min_rel));
    let reachable_idx = recommended_idx + reachable_rel;
    if current_idx == reachable_idx {
        // `requested` tells "at the step asked for" from "as close as this display goes":
        // rewriting a refused step is a full display reconfiguration per attempt
        return DpiState::Settled {
            at: current_idx,
            requested: current_idx == target_idx,
        };
    }
    DpiState::Wrote
}

/// Set the per-display DPI of one resolved target.
///
/// `resolved` carries the `adapterId`+`sourceId` pair from the same `QueryDisplayConfig` sweep
/// that proved this is one of our displays, so the identity verified and the identity written
/// cannot drift apart. Never resolve the pair again here by GDI name: the name is the one thing
/// about a display that is not stable.
fn ensure_dpi_at(resolved: &ResolvedDisplay, target_idx: i32) -> Result<DpiState> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_HEADER,
        DISPLAYCONFIG_DEVICE_INFO_TYPE,
    };
    use windows::Win32::Foundation::LUID;

    let device_name = resolved.gdi_name.as_str();
    let adapter_id = LUID {
        LowPart: resolved.adapter_id.0,
        HighPart: resolved.adapter_id.1,
    };
    let source_id = resolved.source_id;

    let mut get = DisplayConfigGetDpi {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DISPLAYCONFIG_DEVICE_INFO_GET_DPI),
            size: std::mem::size_of::<DisplayConfigGetDpi>() as u32,
            adapterId: adapter_id,
            id: source_id,
        },
        min_scale_rel: 0,
        cur_scale_rel: 0,
        max_scale_rel: 0,
    };
    if unsafe { DisplayConfigGetDeviceInfo(&mut get.header) } != 0 {
        bail!("DisplayConfigGetDeviceInfo(GET_DPI) failed for {device_name}");
    }

    if let settled @ DpiState::Settled { .. } = plan_dpi(
        get.min_scale_rel,
        get.cur_scale_rel,
        get.max_scale_rel,
        target_idx,
    ) {
        return Ok(settled);
    }

    let recommended_idx = -get.min_scale_rel;
    let desired_rel = (target_idx - recommended_idx)
        .clamp(get.min_scale_rel, get.max_scale_rel.max(get.min_scale_rel));
    let set = DisplayConfigSetDpi {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DISPLAYCONFIG_DEVICE_INFO_SET_DPI),
            size: std::mem::size_of::<DisplayConfigSetDpi>() as u32,
            adapterId: adapter_id,
            id: source_id,
        },
        scale_rel: desired_rel,
    };
    let header_ptr =
        (&set as *const DisplayConfigSetDpi).cast::<DISPLAYCONFIG_DEVICE_INFO_HEADER>();
    if unsafe { DisplayConfigSetDeviceInfo(header_ptr) } != 0 {
        bail!("DisplayConfigSetDeviceInfo(SET_DPI) failed for {device_name}");
    }
    tprintln!(
        "[display] SET_DPI on {device_name}: relative step {} -> {desired_rel}",
        get.cur_scale_rel
    );
    Ok(DpiState::Wrote)
}

/// When each display this host commands was last disturbed: attached, or given a mode or a DPI.
/// A monitor refuses capture items for a while after either, and that is a property of the
/// display, so every attempt on it waits against the same clock.
fn disturbed() -> &'static Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static AT: std::sync::OnceLock<Mutex<std::collections::HashMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    AT.get_or_init(Default::default)
}

/// Record that the display at `device_path` was just attached or written to.
pub fn note_display_disturbed(device_path: &str) {
    note_display_disturbed_at(device_path, std::time::Instant::now());
}

pub(crate) fn note_display_disturbed_at(device_path: &str, at: std::time::Instant) {
    if device_path.is_empty() {
        return;
    }
    let mut map = disturbed().lock().unwrap_or_else(|p| p.into_inner());
    // nothing older than a minute still bounds a wait
    map.retain(|_, t| t.elapsed() < std::time::Duration::from_secs(60));
    let entry = map.entry(device_path.to_string()).or_insert(at);
    *entry = (*entry).max(at);
}

/// How much of `bound` is left to wait for `target` to hand out a capture item, counted from
/// when it was last disturbed. A display with no record gets the whole bound.
pub fn item_wait_remaining(
    target: &DisplayTarget,
    bound: std::time::Duration,
) -> std::time::Duration {
    let map = disturbed().lock().unwrap_or_else(|p| p.into_inner());
    match map.get(&target.device_path) {
        Some(at) if !target.device_path.is_empty() => bound.saturating_sub(at.elapsed()),
        _ => bound,
    }
}

/// The scale, in percent, the window manager is actually applying to the monitor `target`
/// names now, read as a per-monitor-aware thread sees it.
///
/// `GET_DPI` reads the display configuration, which takes a `SET_DPI` at once; the window manager
/// applies it afterwards and announces it to every top-level window with `WM_SETTINGCHANGE`.
/// Until then a new monitor's capture item can be refused. This is the read of the second half.
pub fn effective_scale_percent(target: &DisplayTarget) -> Option<u32> {
    use windows::Win32::UI::HiDpi::{
        GetDpiForMonitor, SetThreadDpiAwarenessContext, MDT_EFFECTIVE_DPI,
    };
    let resolved = resolve_owned_display(target).ok()?;
    let (monitor, _) = select_monitor_by_device_name(&resolved.gdi_name).ok()?;
    let hmon = windows::Win32::Graphics::Gdi::HMONITOR(monitor.as_raw_hmonitor());
    // a thread that is not per-monitor aware is told 96 for every monitor
    let previous =
        unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    let read = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    if !previous.is_invalid() {
        unsafe { SetThreadDpiAwarenessContext(previous) };
    }
    read.ok()?;
    (dpi_x != 0).then(|| (dpi_x * 100 + 48) / 96)
}

/// What the window manager reported for a monitor while a scale write was waited for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleLanding {
    pub first: Option<u32>,
    pub last: Option<u32>,
    /// How long until it read the written scale, when it did within the bound.
    pub landed_after: Option<std::time::Duration>,
}

/// Wait, at most `bound`, until the window manager applies `percent` to `target`'s monitor.
pub fn wait_for_effective_scale(
    target: &DisplayTarget,
    percent: u32,
    bound: std::time::Duration,
) -> ScaleLanding {
    wait_for_scale_with(
        percent,
        bound,
        std::time::Duration::from_millis(50),
        &mut || effective_scale_percent(target),
    )
}

/// [`wait_for_effective_scale`] with the read given, so the rule can be tested without a display.
pub(crate) fn wait_for_scale_with(
    percent: u32,
    bound: std::time::Duration,
    poll: std::time::Duration,
    read: &mut dyn FnMut() -> Option<u32>,
) -> ScaleLanding {
    let started = std::time::Instant::now();
    let mut first = None;
    loop {
        let now = read();
        let first = *first.get_or_insert(now);
        if now == Some(percent) {
            return ScaleLanding {
                first,
                last: now,
                landed_after: Some(started.elapsed()),
            };
        }
        if started.elapsed() + poll > bound {
            return ScaleLanding {
                first,
                last: now,
                landed_after: None,
            };
        }
        std::thread::sleep(poll);
    }
}

pub fn set_display_scale(device_name: &str, percent: u32) -> Result<u32> {
    set_display_scale_owned(
        &DisplayTarget {
            device_path: String::new(),
            device_name: device_name.to_string(),
        },
        percent,
    )
}

/// Change one of **our** displays' DPI. Returns the scale the display is actually at afterwards.
///
/// The target is re-resolved from the stable identity on every iteration of the read-back loop:
/// it runs for up to two seconds while the desktop is being reconfigured, and a pair resolved
/// before a mode change can name somebody else's display by the time the write goes out. A
/// target that stops resolving ends the loop with an error.
///
/// A display reports the band of steps it will take, and a step outside it is already refused.
/// Every `SET_DPI` is a full display reconfiguration and the read-back of a refused step can
/// never match, so the loop stops at the closest step the display will take and says so once.
pub fn set_display_scale_owned(target: &DisplayTarget, percent: u32) -> Result<u32> {
    let want = percent.clamp(100, 500);
    let target_idx = DPI_PERCENT_VALUES
        .iter()
        .position(|&p| p >= want)
        .unwrap_or(DPI_PERCENT_VALUES.len() - 1) as i32;
    let device_name = &target.device_name;
    let percent_at = |idx: i32| {
        DPI_PERCENT_VALUES
            .get(idx.clamp(0, DPI_PERCENT_VALUES.len() as i32 - 1) as usize)
            .copied()
            .unwrap_or(100)
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    let mut stable = 0u32;
    let mut settled_at = target_idx;
    let mut ever_set = false;
    let mut last_err: Option<anyhow::Error> = None;
    loop {
        match resolve_owned_display(target).and_then(|r| ensure_dpi_at(&r, target_idx)) {
            Ok(DpiState::Settled { at, requested }) => {
                settled_at = at;
                if !requested {
                    teprintln!(
                        "[display] {device_name} will not scale past {}%; {want}% was asked for \
                         and {}% is what it is at",
                        percent_at(at),
                        percent_at(at)
                    );
                    return Ok(percent_at(at));
                }
                stable += 1;
                if stable >= 5 {
                    return Ok(percent_at(at));
                }
            }
            Ok(DpiState::Wrote) => {
                stable = 0;
                ever_set = true;
                note_display_disturbed(&target.device_path);
            }
            Err(e) => {
                stable = 0;
                last_err = Some(e);
            }
        }
        if std::time::Instant::now() >= deadline {
            return match last_err {
                Some(e) if !ever_set && stable == 0 => Err(e),
                // written but never read back as settled: the client may be told a DPR the host
                // has not applied
                _ => {
                    if ever_set {
                        teprintln!(
                            "[display] {device_name} accepted a scale of {want}% but never \
                             reported it back within 2s; the client's DPR may not match"
                        );
                    }
                    Ok(percent_at(settled_at))
                }
            };
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

pub fn select_monitor_by_device_name(device_name: &str) -> Result<(Monitor, MonitorInfo)> {
    let monitors = Monitor::enumerate().context("enumerating monitors")?;
    for (i, monitor) in monitors.iter().enumerate() {
        if monitor.device_name().ok().as_deref() == Some(device_name) {
            let info = MonitorInfo {
                index: i as u32,
                name: monitor.name().unwrap_or_else(|_| "<unknown>".into()),
                gpu: monitor
                    .device_string()
                    .unwrap_or_else(|_| "<unknown gpu>".into()),
                width: monitor.width().context("monitor width")?,
                height: monitor.height().context("monitor height")?,
                refresh_hz: monitor.refresh_rate().unwrap_or(0),
            };
            return Ok((*monitor, info));
        }
    }
    bail!("monitor with device name {device_name} not found")
}
struct ProbeResult {
    width: u32,
    height: u32,
}

struct ProbeHandler {
    path: String,
    result: Arc<Mutex<Option<ProbeResult>>>,
}

impl GraphicsCaptureApiHandler for ProbeHandler {
    type Flags = (String, Arc<Mutex<Option<ProbeResult>>>);
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let (path, result) = ctx.flags;
        Ok(Self { path, result })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        capture_control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let (w, h) = (frame.width(), frame.height());
        frame.save_as_image(&self.path, ImageFormat::Png)?;
        // inside a WinRT delegate: a panic here would unwind across `extern "system"` frames
        *self
            .result
            .lock()
            .map_err(|_| "probe result mutex poisoned")? = Some(ProbeResult {
            width: w,
            height: h,
        });
        capture_control.stop();
        Ok(())
    }
}

pub fn probe_to_png(requested_monitor: u32, path: &str) -> Result<()> {
    let (monitor, info) = select_monitor(requested_monitor)?;
    tprintln!(
        "capturing display[{}] '{}' ({}) {}x{} -> {}",
        info.index,
        info.name,
        info.gpu,
        info.width,
        info.height,
        path
    );

    let result = Arc::new(Mutex::new(None));
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Default,
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        (path.to_string(), result.clone()),
    );

    ProbeHandler::start(settings).map_err(|e| anyhow!("graphics capture failed: {e}"))?;

    let captured = result
        .lock()
        .map_err(|_| anyhow!("probe result mutex poisoned"))?
        .take();
    match captured {
        Some(r) => {
            tprintln!("captured {}x{} frame -> {}", r.width, r.height, path);
            Ok(())
        }
        None => bail!("capture ended without producing a frame"),
    }
}

#[cfg(test)]
mod mode_write_tests {
    use super::{plan_mode_write, rate_is_unregistered, ModeWritePlan, ModeWriteStep};

    /// A plan for a `BADMODE` (when `badmode`) on a rate the display does not list.
    fn plan(with_rate: bool, badmode: bool, early_failed: bool, expired: bool) -> ModeWritePlan {
        ModeWritePlan {
            with_rate,
            succeeded: false,
            badmode,
            rate_unregistered: badmode,
            early_fallback_failed: early_failed,
            expired,
        }
    }

    /// A display still settling refuses a size@rate it does list. It keeps the retry, so the
    /// requested rate can still be reached inside the window; the fast fallback would land it at
    /// the current rate for good.
    #[test]
    fn a_listed_rate_refused_while_settling_keeps_the_retry() {
        let settling = ModeWritePlan {
            rate_unregistered: false,
            ..plan(true, true, false, false)
        };
        assert_eq!(plan_mode_write(settling, true), ModeWriteStep::Retry);
        let expired = ModeWritePlan {
            expired: true,
            ..settling
        };
        assert_eq!(plan_mode_write(expired, true), ModeWriteStep::WithoutRate);
    }

    #[test]
    fn only_a_size_listed_without_the_rate_is_unregistered() {
        let modes = [(1920, 1080, 60), (1920, 1080, 120), (2560, 1440, 60)];
        assert!(!rate_is_unregistered(&modes, 1920, 1080, 120));
        assert!(rate_is_unregistered(&modes, 1920, 1080, 144));
        assert!(rate_is_unregistered(&modes, 2560, 1440, 120));
        // a size the list does not have yet says nothing about the rate
        assert!(!rate_is_unregistered(&modes, 1280, 720, 60));
        assert!(!rate_is_unregistered(&[], 1920, 1080, 60));
    }

    /// A rate the display never registered is refused the same way every time, so it is not
    /// re-sent for the whole window before the rate-less fallback.
    #[test]
    fn an_unregistered_rate_goes_straight_to_the_fallback() {
        assert_eq!(
            plan_mode_write(plan(true, true, false, false), true),
            ModeWriteStep::WithoutRate
        );
        // switched off: retry until the window closes
        assert_eq!(
            plan_mode_write(plan(true, true, false, false), false),
            ModeWriteStep::Retry
        );
    }

    #[test]
    fn a_refused_resolution_keeps_the_retry() {
        // the early fallback failed too, so the size itself was refused: retry the full mode
        assert_eq!(
            plan_mode_write(plan(false, true, false, false), true),
            ModeWriteStep::Retry
        );
        assert_eq!(
            plan_mode_write(plan(true, true, true, false), true),
            ModeWriteStep::Retry
        );
        // other failures retry, then fall back, then give up
        assert_eq!(
            plan_mode_write(plan(true, false, false, false), true),
            ModeWriteStep::Retry
        );
        assert_eq!(
            plan_mode_write(plan(true, false, false, true), true),
            ModeWriteStep::WithoutRate
        );
        assert_eq!(
            plan_mode_write(plan(false, false, true, true), true),
            ModeWriteStep::Fail
        );
        let ok = ModeWritePlan {
            succeeded: true,
            ..plan(true, false, false, false)
        };
        assert_eq!(plan_mode_write(ok, true), ModeWriteStep::Done);
    }
}

#[cfg(test)]
mod dpi_tests {
    use super::{plan_dpi, DpiState, DPI_PERCENT_VALUES};

    /// The arithmetic that decides whether a DPI write goes out at all.
    ///
    /// The band Windows reports is relative to the display's *recommended* step, so the same
    /// request is reachable on one display and refused on another. A refused step must not be
    /// rewritten: each write is a full display reconfiguration (windows move, the shell repaints).
    #[test]
    fn an_unreachable_scale_is_written_once_and_then_left_alone() {
        // the bundled virtual display at 1920x1080 recommends 100 % and goes no further than
        // 175 %; 200 % is asked for
        let target_200 = 4;
        assert_eq!(
            plan_dpi(0, 0, 3, target_200),
            DpiState::Wrote,
            "the first pass writes the closest step it will take"
        );
        // Windows read back 175 % (`cur_rel = 3`), as far as it goes: settled at 175 %, not at
        // what was asked for
        assert_eq!(
            plan_dpi(0, 3, 3, target_200),
            DpiState::Settled {
                at: 3,
                requested: false
            }
        );
        assert_eq!(DPI_PERCENT_VALUES[3], 175);

        // the same display at 3840x2160, where Windows recommends 200 % and the band is
        // 100 %..300 %: the request is reachable and settling means settling at it
        assert_eq!(
            plan_dpi(-4, 0, 4, target_200),
            DpiState::Settled {
                at: 4,
                requested: true
            }
        );
        assert_eq!(plan_dpi(-4, -4, 4, target_200), DpiState::Wrote);
        assert_eq!(DPI_PERCENT_VALUES[4], 200);

        // "below the band" does not arise: Windows reports a minimum at or below the recommended
        // step and `DPI_PERCENT_VALUES` starts at 100 %. The clamp only closes the arithmetic.

        // a display that reports a single step is settled wherever it is, and is never written
        assert_eq!(
            plan_dpi(0, 0, 0, 9),
            DpiState::Settled {
                at: 0,
                requested: false
            }
        );
        assert_eq!(
            plan_dpi(0, 0, 0, 0),
            DpiState::Settled {
                at: 0,
                requested: true
            }
        );
    }
}
