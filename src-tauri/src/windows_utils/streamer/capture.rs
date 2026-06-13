use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result, anyhow, bail};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows_capture::capture::{Context, GraphicsCaptureApiHandler};
use windows_capture::encoder::ImageFormat;
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::monitor::Monitor;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

pub fn set_dpi_awareness() {
    if let Err(e) = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) } {
        eprintln!("SetProcessDpiAwarenessContext failed (likely already set): {e}");
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
        Ok(_) => bail!(
            "DWM composition disabled, install Desktop Experience on Server"
        ),
        Err(_) => {
            eprintln!("DwmIsCompositionEnabled failed (non-fatal)");
            Ok(())
        }
    }
}

pub fn select_monitor(requested: u32) -> Result<(Monitor, MonitorInfo)> {
    let monitors = Monitor::enumerate().context("enumerating monitors")?;
    if monitors.is_empty() {
        bail!("no displays found");
    }

    println!("{} display(s) detected:", monitors.len());
    for (i, m) in monitors.iter().enumerate() {
        let name = m.name().unwrap_or_else(|_| "<unknown>".into());
        let gpu = m.device_string().unwrap_or_else(|_| "<unknown gpu>".into());
        let w = m.width().unwrap_or(0);
        let h = m.height().unwrap_or(0);
        let hz = m.refresh_rate().unwrap_or(0);
        println!("  display[{i}]: {name} -- {gpu} -- {w}x{h}@{hz}Hz");
    }

    let index = if (requested as usize) < monitors.len() {
        requested
    } else {
        eprintln!("requested display {requested} absent, falling back to 0");
        0
    };

    let monitor = monitors[index as usize];
    let info = MonitorInfo {
        index,
        name: monitor.name().unwrap_or_else(|_| "<unknown>".into()),
        gpu: monitor.device_string().unwrap_or_else(|_| "<unknown gpu>".into()),
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

pub fn set_display_resolution(device_name: &str, width: u32, height: u32, refresh: u32) -> Result<()> {
    set_display_mode(device_name, width, height, refresh, false)
}

pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<()> {
    use windows::Win32::Graphics::Gdi::{
        CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
        DM_DISPLAYFREQUENCY, DM_DISPLAYORIENTATION, DM_PELSHEIGHT, DM_PELSWIDTH, DMDO_DEFAULT,
        ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
    let name = PCWSTR(wide.as_ptr());

    let mut devmode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    unsafe {
        let _ = EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut devmode);
    }
    let (pels_w, pels_h) = if portrait { (height, width) } else { (width, height) };
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    devmode.dmPelsWidth = pels_w;
    devmode.dmPelsHeight = pels_h;
    devmode.dmDisplayFrequency = refresh;
    devmode.Anonymous1.Anonymous2.dmDisplayOrientation = DMDO_DEFAULT;
    devmode.dmFields |=
        DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY | DM_DISPLAYORIENTATION;

    let result =
        unsafe { ChangeDisplaySettingsExW(name, Some(&devmode), None, CDS_UPDATEREGISTRY, None) };
    if result == DISP_CHANGE_SUCCESSFUL {
        Ok(())
    } else {
        bail!("ChangeDisplaySettingsExW({device_name}, {pels_w}x{pels_h}@{refresh}, portrait={portrait}) -> {result:?}");
    }
}

pub fn set_display_orientation(device_name: &str, portrait: bool) -> Result<()> {
    use windows::Win32::Graphics::Gdi::{
        CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL,
        DM_DISPLAYORIENTATION, DM_PELSHEIGHT, DM_PELSWIDTH, DMDO_90, DMDO_DEFAULT,
        ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = device_name.encode_utf16().chain(std::iter::once(0)).collect();
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
    devmode.Anonymous1.Anonymous2.dmDisplayOrientation = if portrait { DMDO_90 } else { DMDO_DEFAULT };
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

const DPI_PERCENT_VALUES: [u32; 12] =
    [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

fn source_path_for_device(
    device_name: &str,
) -> Result<(windows::Win32::Foundation::LUID, u32)> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
        GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
    };

    let mut path_count = 0u32;
    let mut mode_count = 0u32;
    unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
            .ok()
            .map_err(|e| anyhow!("GetDisplayConfigBufferSizes: {e}"))?;
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .map_err(|e| anyhow!("QueryDisplayConfig: {e}"))?;
    }

    for path in paths.iter().take(path_count as usize) {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }
        let end = source
            .viewGdiDeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(source.viewGdiDeviceName.len());
        let gdi = String::from_utf16_lossy(&source.viewGdiDeviceName[..end]);
        if gdi == device_name {
            return Ok((path.sourceInfo.adapterId, path.sourceInfo.id));
        }
    }
    bail!("no active path matched GDI name {device_name}")
}

pub fn set_display_scale(device_name: &str, percent: u32) -> Result<()> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_DEVICE_INFO_TYPE,
        DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo,
    };

    let (adapter_id, source_id) = source_path_for_device(device_name)?;

    let target = percent.clamp(100, 500);
    let target_idx = DPI_PERCENT_VALUES
        .iter()
        .position(|&p| p >= target)
        .unwrap_or(DPI_PERCENT_VALUES.len() - 1);

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

    let recommended_idx = -get.min_scale_rel;
    let desired_rel =
        (target_idx as i32 - recommended_idx).clamp(get.min_scale_rel, get.max_scale_rel.max(0));

    let set = DisplayConfigSetDpi {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(DISPLAYCONFIG_DEVICE_INFO_SET_DPI),
            size: std::mem::size_of::<DisplayConfigSetDpi>() as u32,
            adapterId: adapter_id,
            id: source_id,
        },
        scale_rel: desired_rel,
    };
    let header_ptr = (&set as *const DisplayConfigSetDpi)
        .cast::<DISPLAYCONFIG_DEVICE_INFO_HEADER>();
    if unsafe { DisplayConfigSetDeviceInfo(header_ptr) } != 0 {
        bail!("DisplayConfigSetDeviceInfo(SET_DPI) failed for {device_name}");
    }
    Ok(())
}

pub fn select_monitor_by_device_name(device_name: &str) -> Result<(Monitor, MonitorInfo)> {
    let monitors = Monitor::enumerate().context("enumerating monitors")?;
    for (i, monitor) in monitors.iter().enumerate() {
        if monitor.device_name().ok().as_deref() == Some(device_name) {
            let info = MonitorInfo {
                index: i as u32,
                name: monitor.name().unwrap_or_else(|_| "<unknown>".into()),
                gpu: monitor.device_string().unwrap_or_else(|_| "<unknown gpu>".into()),
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
        *self.result.lock().unwrap() = Some(ProbeResult { width: w, height: h });
        capture_control.stop();
        Ok(())
    }
}

pub fn probe_to_png(requested_monitor: u32, path: &str) -> Result<()> {
    let (monitor, info) = select_monitor(requested_monitor)?;
    println!(
        "capturing display[{}] '{}' ({}) {}x{} -> {}",
        info.index, info.name, info.gpu, info.width, info.height, path
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
            println!("captured {}x{} frame -> {}", r.width, r.height, path);
            Ok(())
        }
        None => bail!("capture ended without producing a frame"),
    }
}
