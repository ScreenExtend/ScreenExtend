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
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

use crate::streamer::session::DisplayTarget;
use crate::windows_utils::virtual_display::{resolve_owned_display, ResolvedDisplay};

pub fn set_dpi_awareness() {
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

/// The same, for a display named by its stable identity rather than by whichever `\\.\DISPLAYn`
/// it held when the session started.
///
/// `None` when the display cannot be identified, which is the honest answer — the caller's
/// alternative is to read some *other* monitor's geometry and record it as this session's.
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

/// Change one of **our** displays' mode.
///
/// The name is a hint. Windows hands a `\\.\DISPLAYn` straight back out when the monitor holding
/// it detaches, so a name cached at create time can address a different monitor by the time a
/// reconfigure runs — and `ChangeDisplaySettingsExW` will cheerfully resize whatever it finds
/// there. The target is therefore re-derived from the display's stable identity immediately
/// before the write, and refused outright if it does not resolve to a display this host created.
///
/// Returns the refresh rate the display is **actually** running at afterwards. An IDD monitor's
/// mode list is fixed when it is created, so a rate that was never registered is refused — and
/// the fallback below then applies the resolution at whatever rate the display is already on.
/// That is the right thing to do, and reporting it as the requested rate is not: the session
/// would record a cadence the display never had, and every later save would compare equal to it
/// and decide there was nothing left to change.
pub fn set_display_mode_owned(
    target: &DisplayTarget,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<u32> {
    let resolved = resolve_owned_display(target)?;
    set_display_mode_resolved(&resolved.gdi_name, width, height, refresh, portrait)?;
    Ok(
        crate::windows_utils::virtual_display::current_display_mode(&resolved.gdi_name)
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

fn set_display_mode_resolved(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DMDO_DEFAULT, DM_DISPLAYFREQUENCY, DM_DISPLAYORIENTATION,
        DM_PELSHEIGHT, DM_PELSWIDTH, ENUM_CURRENT_SETTINGS,
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
    let (pels_w, pels_h) = if portrait {
        (height, width)
    } else {
        (width, height)
    };
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    devmode.dmPelsWidth = pels_w;
    devmode.dmPelsHeight = pels_h;
    devmode.dmDisplayFrequency = refresh;
    devmode.Anonymous1.Anonymous2.dmDisplayOrientation = DMDO_DEFAULT;
    devmode.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_DISPLAYORIENTATION;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    let last = loop {
        let result = unsafe {
            ChangeDisplaySettingsExW(name, Some(&devmode), None, CDS_UPDATEREGISTRY, None)
        };
        if result == DISP_CHANGE_SUCCESSFUL {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            break result;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    devmode.dmFields &= !DM_DISPLAYFREQUENCY;
    devmode.dmDisplayFrequency = 0;
    let result =
        unsafe { ChangeDisplaySettingsExW(name, Some(&devmode), None, CDS_UPDATEREGISTRY, None) };
    if result == DISP_CHANGE_SUCCESSFUL {
        teprintln!(
            "[display] {device_name} rejected {pels_w}x{pels_h}@{refresh} ({last:?}); \
             applied {pels_w}x{pels_h} at its current refresh rate instead"
        );
        return Ok(());
    }

    bail!("ChangeDisplaySettingsExW({device_name}, {pels_w}x{pels_h}@{refresh}, portrait={portrait}) -> {last:?}");
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
/// into [`DPI_PERCENT_VALUES`].
///
/// Read-only, and the one number a report of "my screen's zoom changed" can be checked against —
/// which is what `virtual_display::log_display_inventory` prints it for. The `#[repr(C)]` pair
/// and the two undocumented `DISPLAYCONFIG_DEVICE_INFO_TYPE` values live here, beside the writer
/// that uses them, rather than being spelled out a second time somewhere else.
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

/// Decide what to do about one display's DPI, given what `GET_DPI` reported.
///
/// Split out because the arithmetic is the whole of the rule and the Win32 calls around it cannot
/// be exercised on a machine with one display. `min_rel` and `max_rel` are the offsets Windows
/// reports from the *recommended* step to the lowest and highest it will accept, so a target
/// outside that band is not a target — it is a request the display has already refused.
fn plan_dpi(min_rel: i32, cur_rel: i32, max_rel: i32, target_idx: i32) -> DpiState {
    let recommended_idx = -min_rel;
    let current_idx = recommended_idx + cur_rel;
    let reachable_rel = (target_idx - recommended_idx).clamp(min_rel, max_rel.max(min_rel));
    let reachable_idx = recommended_idx + reachable_rel;
    if current_idx == reachable_idx {
        // Settled. Note the second field: *equal to what was asked for* is a different answer
        // from *as close as this display goes*, and the caller has to be able to tell them
        // apart — rewriting a step the display has already refused is a full display
        // reconfiguration per attempt, twenty of them over the two-second window, and the
        // desktop is reshuffled by every one.
        return DpiState::Settled {
            at: current_idx,
            requested: current_idx == target_idx,
        };
    }
    DpiState::Wrote
}

/// Set the per-display DPI of one resolved target.
///
/// `resolved` carries the `adapterId`+`sourceId` pair that came back from the *same*
/// `QueryDisplayConfig` sweep that proved this is one of our displays, so the identity the caller
/// verified and the identity this writes to cannot drift apart between the two calls. Resolving
/// the pair again here, by GDI name, is what made that possible — and a GDI name is the one
/// thing about a display that is not stable.
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
    Ok(DpiState::Wrote)
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

/// Change one of **our** displays' DPI.
///
/// The target is re-resolved from the stable identity on *every* iteration of the read-back
/// loop, not once before it: the loop runs for up to two seconds across a window in which the
/// desktop is being reconfigured, and a target resolved before a mode change is exactly the
/// stale pair that can name somebody else's display by the time the write goes out. A target
/// that stops resolving ends the loop with an error rather than falling through to another
/// display.
///
/// Returns the scale the display is **actually** at afterwards, which is not always the one that
/// was asked for: a display reports the band of steps it will take, and a step outside it is a
/// request it has already refused. Rewriting a refused step until a deadline expires is not a
/// retry — every `SET_DPI` is a full display reconfiguration, the read-back can never match, and
/// twenty of them over two seconds is a desktop being rearranged twenty times while the user
/// watches. So the loop stops at the closest step the display will take and says so once.
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
            }
            Err(e) => {
                stable = 0;
                last_err = Some(e);
            }
        }
        if std::time::Instant::now() >= deadline {
            return match last_err {
                Some(e) if !ever_set && stable == 0 => Err(e),
                // Written but never read back as settled. Reporting this as success let the
                // caller tell the client a DPR the host had not applied; the client then renders
                // at a ratio that does not match the display's real scale.
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
        *self.result.lock().unwrap() = Some(ProbeResult {
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

    let captured = result.lock().unwrap().take();
    match captured {
        Some(r) => {
            tprintln!("captured {}x{} frame -> {}", r.width, r.height, path);
            Ok(())
        }
        None => bail!("capture ended without producing a frame"),
    }
}

#[cfg(test)]
mod dpi_tests {
    use super::{plan_dpi, DpiState, DPI_PERCENT_VALUES};

    /// The arithmetic that decides whether a DPI write goes out at all.
    ///
    /// The band Windows reports is relative to the display's *recommended* step, so the same
    /// request is reachable on one display and refused on another — and a refused one used to be
    /// rewritten every 100 ms until a two-second deadline expired. Each of those writes is a full
    /// display reconfiguration: the desktop is rearranged, windows are moved and the shell
    /// repaints, twenty times, for a value the display was never going to take.
    #[test]
    fn an_unreachable_scale_is_written_once_and_then_left_alone() {
        // The bundled virtual display as measured at 1920x1080: recommended is 100 % and it will
        // go no further than 175 %. 200 % was asked for.
        let target_200 = 4;
        assert_eq!(
            plan_dpi(0, 0, 3, target_200),
            DpiState::Wrote,
            "the first pass writes the closest step it will take"
        );
        // Windows read back 175 %, which is `cur_rel = 3`. That is as far as it goes, so this is
        // settled — at 175 %, not at what was asked for.
        assert_eq!(
            plan_dpi(0, 3, 3, target_200),
            DpiState::Settled {
                at: 3,
                requested: false
            }
        );
        assert_eq!(DPI_PERCENT_VALUES[3], 175);

        // The same display at 3840x2160, where Windows recommends 200 % and the band is
        // 100 %..300 %: now the request is reachable and settling means settling at it.
        assert_eq!(
            plan_dpi(-4, 0, 4, target_200),
            DpiState::Settled {
                at: 4,
                requested: true
            }
        );
        assert_eq!(plan_dpi(-4, -4, 4, target_200), DpiState::Wrote);
        assert_eq!(DPI_PERCENT_VALUES[4], 200);

        // Windows reports a minimum at or below the recommended step on every display measured
        // here, and `DPI_PERCENT_VALUES` starts at 100 %, so "below the band" is not a case that
        // arises in practice — the clamp covers it only so the arithmetic has no open end.

        // A display that reports a single step is settled wherever it is, and is never written.
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
