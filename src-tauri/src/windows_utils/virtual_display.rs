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

impl Inner {
    fn commit(&mut self, monitors: Vec<Monitor>) -> Result<(), String> {
        let started = Instant::now();
        self.client
            .set_monitors(&monitors)
            .map_err(|e| format!("set monitors: {e}"))?;
        self.owned = monitors;
        let result = self
            .client
            .notify()
            .map_err(|e| format!("notify driver: {e}"));
        let elapsed = started.elapsed();
        if elapsed >= SLOW_COMMIT {
            tprintln!("[virtual-display] driver commit took {elapsed:?}");
        }
        result
    }
}

#[derive(Debug)]
pub struct WindowsVirtualDisplay {
    inner: Mutex<Inner>,
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
        let mut inner = self.inner.lock().unwrap();
        let mut monitors = inner.owned.clone();
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
        inner.commit(monitors)?;
        Ok(id)
    }

    fn remove_display(&self, id: u32) {
        let mut inner = self.inner.lock().unwrap();
        let mut monitors = inner.owned.clone();
        monitors.retain(|m| m.id != id);
        if let Err(e) = inner.commit(monitors) {
            teprintln!("virtual_display: remove({id}) failed: {e}");
        }
    }

    fn remove_all_displays(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.client.remove_all();
        inner.owned.clear();
        if let Err(e) = inner.client.notify() {
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
