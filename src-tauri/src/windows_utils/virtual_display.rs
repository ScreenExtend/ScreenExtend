use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::driver_ipc::{sync::DriverClient, Mode, Monitor, DEFAULT_PIPE_NAME};

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

/// Registered on every display we create so a device can be switched between these rates later
/// without a hot-unplug (an IDD monitor's mode list is fixed at create time). 120 is kept even
/// for 60 Hz clients on purpose: an indirect display has no hardware vblank, so this list is
/// also what bounds DWM's compose cadence for the monitor.
const COMMON_REFRESH_RATES: [u32; 2] = [60, 120];

/// Log a driver round trip that took longer than this. Display attach sits on the join path a
/// user waits through, so a slow driver should be visible rather than merely felt.
const SLOW_COMMIT: Duration = Duration::from_millis(250);

/// Which display backend this host will actually use, probed rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBackend {
    /// The bundled indirect display driver is installed and answering.
    IddDriver,
    /// The driver's pipe does not exist: it was never installed, or its device node is gone.
    DriverNotInstalled,
    /// The pipe exists but the driver did not answer a state request.
    DriverNotResponding,
}

impl DisplayBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IddDriver => "IddDriver",
            Self::DriverNotInstalled => "DriverNotInstalled",
            Self::DriverNotResponding => "DriverNotResponding",
        }
    }
}

fn pipe_path() -> String {
    format!(r"\\.\pipe\{DEFAULT_PIPE_NAME}")
}

/// Set once this process has actually talked to the driver, so the probe does not have to open
/// a second connection to a pipe we are already holding — the driver serves one client at a
/// time, and a second connect from the same process fails while `setup`'s client is alive.
static CONNECTED: OnceLock<()> = OnceLock::new();

/// Probe the display backend. Never cached negatively: `drivers install` flips this at runtime.
pub fn probe_display_backend() -> DisplayBackend {
    if CONNECTED.get().is_some() {
        return DisplayBackend::IddDriver;
    }
    if !std::path::Path::new(&pipe_path()).exists() {
        return DisplayBackend::DriverNotInstalled;
    }
    // `DriverClient::new` blocks on driver_ipc's own tokio runtime, which panics if the calling
    // thread belongs to a runtime. Tauri commands can, so give it a plain thread of its own.
    let connected = std::thread::spawn(|| match DriverClient::new() {
        Ok(_) => None,
        Err(e) => Some(e.to_string()),
    })
    .join();
    match connected {
        Ok(None) => {
            let _ = CONNECTED.set(());
            DisplayBackend::IddDriver
        }
        Ok(Some(e)) => {
            teprintln!("[virtual-display] driver pipe exists but did not answer: {e}");
            DisplayBackend::DriverNotResponding
        }
        Err(_) => {
            teprintln!("[virtual-display] the driver probe panicked");
            DisplayBackend::DriverNotResponding
        }
    }
}

#[derive(Debug)]
struct Inner {
    client: DriverClient,
    owned: Vec<Monitor>,
}

#[derive(Debug)]
pub struct WindowsVirtualDisplay {
    inner: Mutex<Inner>,
    /// Serialises driver round trips, so `inner` is never held across one.
    ///
    /// There is one named pipe, so commits are serial whatever we do; what this buys is that a
    /// slow or wedged driver no longer stalls *readers* of the monitor list — the display-name
    /// correlation on the join path, and `registered_refresh_rates` — behind a blocking mutex
    /// held across a `RUNTIME.block_on`. The round trip is bounded (5 s connect, 2 s write) but
    /// bounded is not short: `DISPLAY_ATTACH_TIMEOUT` is 5 s for the whole attach.
    commit: Mutex<()>,
}

impl WindowsVirtualDisplay {
    /// Send a monitor list to the driver without holding `inner` across the round trip.
    ///
    /// The caller must already hold `commit`, which is what makes reading `owned`, computing a
    /// new list and storing it back atomic enough: no other commit can interleave.
    fn commit_unlocked(&self, monitors: Vec<Monitor>) -> Result<(), String> {
        // An independent client over the same connection, so `set_monitors` mutates a state of
        // its own rather than the shared one — which is what lets the guard go before the wait.
        let mut client = self.inner.lock().unwrap().client.duplicate();
        let started = Instant::now();
        let result = client
            .set_monitors(&monitors)
            .map_err(|e| format!("set monitors: {e}"))
            .and_then(|()| client.notify().map_err(|e| format!("notify driver: {e}")));
        let elapsed = started.elapsed();
        if elapsed >= SLOW_COMMIT {
            tprintln!("[virtual-display] driver commit took {elapsed:?}");
        }
        if result.is_ok() {
            self.inner.lock().unwrap().owned = monitors;
        }
        result
    }
}

impl WindowsVirtualDisplay {
    /// Connect to the driver, clearing any monitors a previous run left behind.
    ///
    /// The failure this most often reports is "the signed indirect display driver was never
    /// installed", which used to be flattened into `None` and never logged.
    pub fn new_shared_reported() -> Result<SharedVirtualDisplay, String> {
        let mut client = DriverClient::new().map_err(|e| {
            if std::path::Path::new(&pipe_path()).exists() {
                format!(
                    "the display driver did not answer on {} ({e}); it may be starting, stopped, or wedged",
                    pipe_path()
                )
            } else {
                format!(
                    "the ScreenExtend virtual display driver is not installed ({} does not exist). \
                     Install it with `ScreenExtend drivers install` (needs Administrator). Underlying error: {e}",
                    pipe_path()
                )
            }
        })?;
        let _ = CONNECTED.set(());
        client.remove_all();
        if let Err(e) = client.notify() {
            teprintln!("[virtual-display] could not clear leftover monitors at startup: {e}");
        }
        Ok(Arc::new(Self {
            inner: Mutex::new(Inner {
                client,
                owned: Vec::new(),
            }),
            commit: Mutex::new(()),
        }))
    }

    pub fn new_shared() -> Option<SharedVirtualDisplay> {
        match Self::new_shared_reported() {
            Ok(shared) => Some(shared),
            Err(e) => {
                teprintln!("[virtual-display] {e}");
                None
            }
        }
    }

    /// The refresh rates registered on a display we own. A live IDD monitor cannot be given new
    /// ones, so anything outside this list needs the display recreating.
    pub fn registered_refresh_rates(&self, id: u32) -> Vec<u32> {
        let inner = self.inner.lock().unwrap();
        inner
            .owned
            .iter()
            .find(|m| m.id == id)
            .map(|m| {
                let mut rates: Vec<u32> = m
                    .modes
                    .iter()
                    .flat_map(|mode| mode.refresh_rates.iter().copied())
                    .collect();
                rates.sort_unstable();
                rates.dedup();
                rates
            })
            .unwrap_or_default()
    }
}

/// One active display path, as `QueryDisplayConfig` describes it.
///
/// `gdi_name` is the `\\.\DISPLAYn` every other Win32 display call wants; `friendly` is the name
/// out of the monitor's EDID, which for our own virtual displays is the name we asked the driver
/// to synthesise. Correlating the two is what lets a join skip the monitor-diff poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPath {
    pub gdi_name: String,
    pub friendly: String,
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Every active display path, with its GDI device name and its EDID friendly name.
pub fn active_display_paths() -> Vec<DisplayPath> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
        DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    let mut n_path = 0u32;
    let mut n_mode = 0u32;
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut n_path, &mut n_mode) }
        != ERROR_SUCCESS
    {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_path as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_mode as usize];
    if unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut n_path,
            paths.as_mut_ptr(),
            &mut n_mode,
            modes.as_mut_ptr(),
            None,
        )
    } != ERROR_SUCCESS
    {
        return Vec::new();
    }
    paths.truncate(n_path as usize);

    let mut out = Vec::with_capacity(paths.len());
    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != ERROR_SUCCESS.0 as i32 {
            continue;
        }
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: windows::Win32::Devices::Display::DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        let friendly = if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) }
            == ERROR_SUCCESS.0 as i32
        {
            wide_to_string(&target.monitorFriendlyDeviceName)
        } else {
            String::new()
        };
        out.push(DisplayPath {
            gdi_name: wide_to_string(&source.viewGdiDeviceName),
            friendly,
        });
    }
    out
}

/// Which `\\.\DISPLAYn` carries the monitor whose EDID friendly name is `want`.
///
/// `None` when nothing matches, and — deliberately — also when more than one path does. An
/// ambiguous answer is worse than no answer here: the caller's fallback is a correct-but-slower
/// monitor diff, while a wrong name would point the capture at somebody else's screen.
///
/// The comparison is a prefix match because the EDID monitor-name descriptor is 13 bytes, so a
/// longer name we ask the driver for comes back truncated.
pub fn gdi_name_for_friendly(paths: &[DisplayPath], want: &str) -> Option<String> {
    let mut hit = None;
    for p in paths {
        if p.friendly.is_empty() {
            continue;
        }
        if want == p.friendly || want.starts_with(p.friendly.as_str()) {
            if hit.is_some() {
                return None;
            }
            hit = Some(p.gdi_name.clone());
        }
    }
    hit
}

/// The mode Windows has actually settled on for a display device, so a caller that asked for
/// 120 Hz can see whether it got it.
pub fn current_display_mode(device_name: &str) -> Option<(u32, u32, u32)> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};

    let wide: Vec<u16> = device_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut devmode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    let ok =
        unsafe { EnumDisplaySettingsW(PCWSTR(wide.as_ptr()), ENUM_CURRENT_SETTINGS, &mut devmode) };
    if !ok.as_bool() {
        return None;
    }
    Some((
        devmode.dmPelsWidth,
        devmode.dmPelsHeight,
        devmode.dmDisplayFrequency,
    ))
}

/// Run `device_name` at `refresh_rate` without touching its resolution, and report the rate
/// actually in effect afterwards.
///
/// The compose cadence of an indirect display is synthesised from the mode's refresh rate, so
/// this is the knob that decides how often the desktop for that monitor is recomposed — which
/// is independent of how many frames we choose to encode and send. Only a rate the display
/// registered at create time can be selected; see `registered_refresh_rates`.
pub fn set_refresh_rate(device_name: &str, refresh_rate: u32) -> Result<u32, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
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
    if !unsafe { EnumDisplaySettingsW(name, ENUM_CURRENT_SETTINGS, &mut devmode) }.as_bool() {
        return Err(format!("EnumDisplaySettingsW({device_name}) failed"));
    }
    if devmode.dmDisplayFrequency == refresh_rate {
        return Ok(refresh_rate);
    }

    // Measured on the bundled IDD: asking for DM_DISPLAYFREQUENCY alone is rejected with
    // DISP_CHANGE_BADMODE. The resolution has to travel with the rate, even when it is unchanged.
    devmode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    devmode.dmDisplayFrequency = refresh_rate;
    devmode.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;

    let result =
        unsafe { ChangeDisplaySettingsExW(name, Some(&devmode), None, CDS_UPDATEREGISTRY, None) };
    let achieved = current_display_mode(device_name)
        .map(|(_, _, hz)| hz)
        .unwrap_or(0);
    if result != DISP_CHANGE_SUCCESSFUL {
        return Err(format!(
            "ChangeDisplaySettingsExW({device_name}, {refresh_rate} Hz) -> {result:?}; \
             still running at {achieved} Hz"
        ));
    }
    Ok(achieved)
}

impl VirtualDisplayController for WindowsVirtualDisplay {
    fn create_display(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
    ) -> Result<u32, String> {
        self.create_display_with_modes(name, width, height, refresh_rate, &[])
    }

    fn create_display_with_modes(
        &self,
        name: String,
        width: u32,
        height: u32,
        refresh_rate: u32,
        extra_modes: &[(u32, u32)],
    ) -> Result<u32, String> {
        // Held for the whole allocate-commit-record sequence, so no other commit can hand out
        // the same id; `inner` itself is taken only for the moments that touch its fields.
        let _commit = self.commit.lock().unwrap();
        let mut monitors = self.inner.lock().unwrap().owned.clone();
        let id = (0u32..)
            .find(|candidate| !monitors.iter().any(|m| m.id == *candidate))
            .ok_or_else(|| "no free display id".to_string())?;

        let mut dims: Vec<(u32, u32)> = Vec::new();
        let mut candidates: Vec<(u32, u32)> = Vec::with_capacity(1 + extra_modes.len());
        candidates.push((width, height));
        candidates.extend_from_slice(extra_modes);
        for (w, h) in candidates {
            if w < 2 || h < 2 {
                continue;
            }
            if !dims.contains(&(w, h)) {
                dims.push((w, h));
            }
            if w != h && !dims.contains(&(h, w)) {
                dims.push((h, w));
            }
        }

        let mut refresh_rates = vec![refresh_rate];
        for rate in COMMON_REFRESH_RATES {
            if !refresh_rates.contains(&rate) {
                refresh_rates.push(rate);
            }
        }

        let modes: Vec<Mode> = dims
            .iter()
            .map(|&(w, h)| Mode {
                width: w,
                height: h,
                refresh_rates: refresh_rates.clone(),
            })
            .collect();

        let name = if monitors
            .iter()
            .any(|m| m.name.as_deref() == Some(name.as_str()))
        {
            format!("{name} ({id})")
        } else {
            name
        };

        // The mode list is fixed for the life of the monitor, so say what it is: anything a
        // later reconfig asks for that is not in here needs the display recreating.
        tprintln!(
            "[virtual-display] registering display id={id} with {} resolutions x {:?} Hz: {:?}",
            dims.len(),
            refresh_rates,
            dims
        );

        monitors.push(Monitor {
            id,
            enabled: true,
            name: Some(name),
            modes,
        });
        self.commit_unlocked(monitors)?;
        Ok(id)
    }

    /// Correlate one of our monitors to its `\\.\DISPLAYn` without waiting for it to appear.
    ///
    /// The trait default returns `None`, which sends `server.rs` down `wait_for_new_monitor`:
    /// a 100 ms poll of `EnumDisplayDevices` against a snapshot taken before the create,
    /// serialised across every concurrent join by `DISPLAY_CORRELATION_LOCK` and bounded only
    /// by the 5 s attach timeout — all of it between "the device scans the QR" and "a picture
    /// appears". `QueryDisplayConfig` answers the same question directly when the monitor's
    /// EDID name is unambiguous, and returns `None` when it is not, so the diff still covers
    /// the cases this cannot.
    fn display_device_name(&self, id: u32) -> Option<String> {
        let want = {
            let inner = self.inner.lock().unwrap();
            inner
                .owned
                .iter()
                .find(|m| m.id == id)?
                .name
                .clone()
                .filter(|n| !n.trim().is_empty())?
        };
        gdi_name_for_friendly(&active_display_paths(), &want)
    }

    fn remove_display(&self, id: u32) {
        let _commit = self.commit.lock().unwrap();
        let mut monitors = self.inner.lock().unwrap().owned.clone();
        monitors.retain(|m| m.id != id);
        if let Err(e) = self.commit_unlocked(monitors) {
            teprintln!("virtual_display: remove({id}) failed: {e}");
        }
    }

    fn remove_all_displays(&self) {
        let _commit = self.commit.lock().unwrap();
        let mut client = self.inner.lock().unwrap().client.duplicate();
        client.remove_all();
        let result = client.notify();
        self.inner.lock().unwrap().owned.clear();
        if let Err(e) = result {
            teprintln!("virtual_display: notify after remove_all failed: {e:?}");
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn backend_probe_names_are_stable() {
        assert_eq!(DisplayBackend::IddDriver.as_str(), "IddDriver");
        assert_eq!(
            DisplayBackend::DriverNotInstalled.as_str(),
            "DriverNotInstalled"
        );
    }

    #[test]
    fn an_ambiguous_friendly_name_resolves_to_nothing() {
        let paths = vec![
            DisplayPath {
                gdi_name: r"\\.\DISPLAY1".to_string(),
                friendly: "ScreenExtend".to_string(),
            },
            DisplayPath {
                gdi_name: r"\\.\DISPLAY2".to_string(),
                friendly: "ScreenExtend".to_string(),
            },
            DisplayPath {
                gdi_name: r"\\.\DISPLAY3".to_string(),
                friendly: "Dell U2720Q".to_string(),
            },
        ];
        // Two monitors answer to the same truncated EDID name, so the caller must fall back to
        // the diff rather than be handed one of them at random.
        assert_eq!(gdi_name_for_friendly(&paths, "ScreenExtend - iPad"), None);
        assert_eq!(
            gdi_name_for_friendly(&paths, "Dell U2720Q"),
            Some(r"\\.\DISPLAY3".to_string())
        );
        assert_eq!(gdi_name_for_friendly(&paths, "Nothing"), None);
        // An empty EDID name matches nothing, rather than matching everything by prefix.
        let blank = vec![DisplayPath {
            gdi_name: r"\\.\DISPLAY1".to_string(),
            friendly: String::new(),
        }];
        assert_eq!(gdi_name_for_friendly(&blank, "anything"), None);
    }

    /// Dumps what `QueryDisplayConfig` says about every active display, which is the input the
    /// `display_device_name` correlation runs on. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn report_active_display_paths() {
        for p in active_display_paths() {
            println!("{} <- EDID friendly name {:?}", p.gdi_name, p.friendly);
        }
    }

    /// Times the two ways of answering "which `\\.\DISPLAYn` did my new virtual display become":
    /// the `QueryDisplayConfig` correlation against the `EnumDisplayDevices` diff `server.rs`
    /// falls back to. Needs the driver installed; creates three real displays for a few seconds.
    /// Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn report_display_name_correlation() {
        let Ok(display) = WindowsVirtualDisplay::new_shared_reported() else {
            println!("driver unavailable; skipping");
            return;
        };
        let names = [
            "ScreenExtend - alpha",
            "ScreenExtend - beta",
            "ScreenExtend - gamma",
        ];
        let mut ids = Vec::new();
        for name in names {
            let before = crate::windows_utils::streamer::capture::monitor_device_names();
            let started = Instant::now();
            let id = display
                .create_display(name.to_string(), 1280, 720, 60)
                .expect("create display");
            ids.push(id);

            // The lookup, polled the same way the diff is so the two are comparable.
            let mut looked_up = None;
            let mut lookup_ms = f64::NAN;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(n) = display.display_device_name(id) {
                    looked_up = Some(n);
                    lookup_ms = started.elapsed().as_secs_f64() * 1000.0;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            // The diff, from the same start instant.
            let mut diffed = None;
            let mut diff_ms = f64::NAN;
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                let now = crate::windows_utils::streamer::capture::monitor_device_names();
                if let Some(n) = now.iter().find(|n| !before.contains(n)) {
                    diffed = Some(n.clone());
                    diff_ms = started.elapsed().as_secs_f64() * 1000.0;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }

            println!(
                "{name}: lookup={looked_up:?} in {lookup_ms:.0}ms, diff={diffed:?} in {diff_ms:.0}ms"
            );
        }
        for p in active_display_paths() {
            println!("  path {} <- friendly {:?}", p.gdi_name, p.friendly);
        }
        for id in ids {
            display.remove_display(id);
        }
    }

    /// Reports which display backend this machine probes to. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn report_display_backend() {
        println!(
            "pipe {} exists: {}",
            pipe_path(),
            std::path::Path::new(&pipe_path()).exists()
        );
        match DriverClient::new() {
            Ok(_) => println!("DriverClient::new() => ok"),
            Err(e) => println!("DriverClient::new() => {e}"),
        }
        println!(
            "probe_display_backend() => {}",
            probe_display_backend().as_str()
        );
    }

    /// Measures how the bundled IDD behaves around refresh rates. Requires the driver installed;
    /// it creates a real display for a few seconds. Run with `--ignored --nocapture`.
    ///
    /// On Windows 11 26200 with the bundled driver this reports:
    ///   - a new monitor comes up at 1280x720@60 — Windows picks its own default, not the
    ///     highest advertised rate, so registering 120 Hz costs nothing;
    ///   - at 1280x720 (a mode we never registered, i.e. a DWM virtual mode) 120 Hz is refused
    ///     with DISP_CHANGE_BADMODE;
    ///   - at 1920x1080, which we did register with [60, 120], the rate change is accepted and
    ///     `EnumDisplaySettingsW` reads back 1920x1080@120.
    #[test]
    #[ignore]
    fn report_selected_refresh_rate() {
        let Ok(display) = WindowsVirtualDisplay::new_shared_reported() else {
            println!("driver unavailable; skipping");
            return;
        };
        let before = crate::windows_utils::streamer::capture::monitor_device_names();
        let id = display
            .create_display("ScreenExtend rate probe".to_string(), 1920, 1080, 60)
            .expect("create display");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut new: Vec<String> = Vec::new();
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(500));
            let after = crate::windows_utils::streamer::capture::monitor_device_names();
            new = after.into_iter().filter(|n| !before.contains(n)).collect();
            if !new.is_empty() {
                break;
            }
        }
        for name in &new {
            println!(
                "{name}: mode Windows chose = {:?}",
                current_display_mode(name)
            );
            println!(
                "{name}: set_refresh_rate(120) at that mode -> {:?}",
                set_refresh_rate(name, 120)
            );
            let applied = crate::windows_utils::streamer::capture::set_display_mode(
                name, 1920, 1080, 60, false,
            );
            println!("{name}: set_display_mode(1920x1080@60) -> {applied:?}");
            std::thread::sleep(Duration::from_secs(2));
            println!("{name}: now at {:?}", current_display_mode(name));
            println!(
                "{name}: set_refresh_rate(120) at 1920x1080 -> {:?}",
                set_refresh_rate(name, 120)
            );
            std::thread::sleep(Duration::from_secs(2));
            println!("{name}: finally at {:?}", current_display_mode(name));
        }
        if new.is_empty() {
            println!("no new display device name appeared");
        }
        display.remove_display(id);
    }
}
