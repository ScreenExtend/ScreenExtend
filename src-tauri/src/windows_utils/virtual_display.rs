use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::driver_ipc::{
    error::{ConnectionError, InitError},
    sync::DriverClient,
    Mode, Monitor, DEFAULT_PIPE_NAME,
};

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
    /// The driver is installed and healthy, and another ScreenExtend process is using it.
    ///
    /// There is nothing to repair here, and that is the whole point of the variant. It used to
    /// arrive as `DriverNotResponding`, so the startup gate removed and reinstalled a perfectly
    /// good signed driver — one elevation prompt each — probed again, found the same thing, and
    /// went round.
    ///
    /// Two ways in, and the measured one is the second. `ERROR_PIPE_BUSY` if the driver ever
    /// runs out of pipe instances; and a state request that times out, which is what actually
    /// happens: `report_second_client_behaviour` opens twelve concurrent clients against the
    /// bundled driver on Windows 11 26200 without complaint *while it is idle*, but
    /// `report_mode_change_cost` measures a display create at ~4.9 s — during which the driver
    /// answers nobody, and the 5 s handshake deadline in `driver_ipc` expires. A `doctor` or a
    /// second launch that lands in that window used to call a busy driver a dead one.
    DriverBusy {
        /// The pid holding it, where we could name one. `None` is "somebody is, and we could
        /// not work out who" — a `probe --live` run, or an instance that is still exiting.
        holder: Option<u32>,
    },
    /// The pipe exists, accepted a connection, and then failed the protocol handshake.
    DriverNotResponding,
}

impl DisplayBackend {
    /// The stable wire name. `DriverBusy` deliberately does not carry the pid: the string is
    /// read by `doctor --json`, the compatibility card and the startup gate, and a value that
    /// changes with the pid could not be compared against anything.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IddDriver => "IddDriver",
            Self::DriverNotInstalled => "DriverNotInstalled",
            Self::DriverBusy { .. } => "DriverBusy",
            Self::DriverNotResponding => "DriverNotResponding",
        }
    }

    /// The pid holding the driver's pipe, when this is the busy state and we could name one.
    pub fn holder(self) -> Option<u32> {
        match self {
            Self::DriverBusy { holder } => holder,
            _ => None,
        }
    }
}

fn pipe_path() -> String {
    pipe_path_for(DEFAULT_PIPE_NAME)
}

fn pipe_path_for(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// `ERROR_PIPE_BUSY`: the pipe exists, and every instance of it already has a client.
const ERROR_PIPE_BUSY: i32 = 231;

/// `ERROR_SEM_TIMEOUT`, what a `WaitNamedPipe` that ran out of time reports. Same meaning as
/// [`ERROR_PIPE_BUSY`] for our purposes — occupied, not dead.
const ERROR_SEM_TIMEOUT: i32 = 121;

/// `ERROR_FILE_NOT_FOUND` / `ERROR_PATH_NOT_FOUND`: nothing is serving this name.
const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PATH_NOT_FOUND: i32 = 3;

fn is_busy_os_error(code: i32) -> bool {
    code == ERROR_PIPE_BUSY || code == ERROR_SEM_TIMEOUT
}

fn is_absent_os_error(code: i32) -> bool {
    code == ERROR_FILE_NOT_FOUND || code == ERROR_PATH_NOT_FOUND
}

/// Set once this process has actually talked to the driver.
///
/// A host that is already holding a working client has its answer and does not need to spend a
/// connect and a 5 s handshake deadline asking again — least of all while its own
/// `create_display` has the driver busy, which is the window this whole module now has to be
/// careful about.
static CONNECTED: OnceLock<()> = OnceLock::new();

/// Probe the display backend. Never cached negatively: `drivers install` flips this at runtime.
pub fn probe_display_backend() -> DisplayBackend {
    if CONNECTED.get().is_some() {
        return DisplayBackend::IddDriver;
    }
    match probe_pipe(DEFAULT_PIPE_NAME) {
        DisplayBackend::IddDriver => {
            let _ = CONNECTED.set(());
            DisplayBackend::IddDriver
        }
        other => other,
    }
}

/// Classify a failed [`DriverClient::new`].
///
/// The `Path::exists()` test this replaced could not tell any of these apart, and worse, it got
/// the busy case exactly backwards: `std::fs::metadata` opens the file, so a pipe whose every
/// instance is taken reports **not found**. Asking the pipe itself and reading the error is the
/// only way to separate "never installed" from "installed and occupied" from "wedged".
///
/// The handshake arm is the one that is not a simple error-code mapping, and it is the arm the
/// reported fault actually takes. The bundled driver answers a state request within a
/// millisecond when idle and not at all while it is attaching a monitor, which takes seconds —
/// so a timeout means "occupied" whenever another of our own processes is alive to be occupying
/// it, and reinstalling the driver cannot help while one is.
fn classify_init_error(e: &InitError) -> DisplayBackend {
    match e {
        InitError::Connect(ConnectionError::Failed(io)) => match io.raw_os_error() {
            Some(code) if is_busy_os_error(code) => DisplayBackend::DriverBusy {
                holder: driver_pipe_holder(),
            },
            Some(code) if is_absent_os_error(code) => DisplayBackend::DriverNotInstalled,
            _ => DisplayBackend::DriverNotResponding,
        },
        InitError::RequestState(_) => match driver_pipe_holder() {
            Some(pid) => DisplayBackend::DriverBusy { holder: Some(pid) },
            None => DisplayBackend::DriverNotResponding,
        },
    }
}

/// The classification itself, against one pipe name.
///
/// Split out from [`probe_display_backend`] so the busy path can be exercised against a pipe of
/// the test's own — reproducing it against the real driver means fighting the running host for
/// it, on exactly the machine where that must not happen.
fn probe_pipe(name: &str) -> DisplayBackend {
    // `DriverClient::new` blocks on driver_ipc's own tokio runtime, which panics if the calling
    // thread belongs to a runtime. Tauri commands can, so give it a plain thread of its own.
    let owned = name.to_string();
    let connected = std::thread::spawn(move || match DriverClient::new_with(&owned) {
        Ok(_) => Ok(()),
        Err(e) => Err((classify_init_error(&e), e.to_string())),
    })
    .join();
    match connected {
        Ok(Ok(())) => DisplayBackend::IddDriver,
        Ok(Err((DisplayBackend::DriverBusy { holder }, e))) => {
            match holder {
                Some(pid) => tprintln!(
                    "[virtual-display] the driver is installed and healthy; pid {pid} has its \
                     pipe ({e})"
                ),
                None => tprintln!(
                    "[virtual-display] the driver is installed and healthy; another process has \
                     its pipe ({e})"
                ),
            }
            DisplayBackend::DriverBusy { holder }
        }
        Ok(Err((DisplayBackend::DriverNotInstalled, e))) => {
            tprintln!(
                "[virtual-display] {} does not exist ({e})",
                pipe_path_for(name)
            );
            DisplayBackend::DriverNotInstalled
        }
        Ok(Err((_, e))) => {
            teprintln!("[virtual-display] driver pipe exists but did not answer: {e}");
            DisplayBackend::DriverNotResponding
        }
        Err(_) => {
            teprintln!("[virtual-display] the driver probe panicked");
            DisplayBackend::DriverNotResponding
        }
    }
}

/// Which process is holding the driver's pipe, when we can name one.
///
/// The single-instance control socket is asked first, because that answer is both authoritative
/// and actionable: a GUI or a `serve` host replies to `PING` with its pid, and is the only kind
/// of holder we can then offer to quit. Our own pid is not an answer — by the time the desktop
/// UI probes, the control server it will find may well be this process's own. Anything without
/// a control socket (`probe --live`, an instance still exiting) is found by looking for another
/// copy of this executable.
pub fn driver_pipe_holder() -> Option<u32> {
    let me = std::process::id();
    if let Some(pid) = crate::single_instance::running_instance_pid() {
        if pid != me {
            return Some(pid);
        }
    }
    sibling_processes().into_iter().next()
}

/// Every other process running this same executable, oldest pid first.
fn sibling_processes() -> Vec<u32> {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::ProcessStatus::EnumProcesses;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let Some(mine) = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()))
    else {
        return Vec::new();
    };
    let me = std::process::id();

    let mut pids = vec![0u32; 2048];
    let mut needed = 0u32;
    if unsafe {
        EnumProcesses(
            pids.as_mut_ptr(),
            (pids.len() * std::mem::size_of::<u32>()) as u32,
            &mut needed,
        )
    }
    .is_err()
    {
        return Vec::new();
    }
    pids.truncate(needed as usize / std::mem::size_of::<u32>());

    let mut out = Vec::new();
    for pid in pids {
        if pid == 0 || pid == me {
            continue;
        }
        // Limited information is enough for the image name and is the access a same-user
        // process is granted even when it runs at a different integrity level.
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            continue;
        };
        let mut buf = [0u16; 260];
        let mut len = buf.len() as u32;
        let named = unsafe {
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        if named.is_err() {
            continue;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        if path
            .rsplit(['\\', '/'])
            .next()
            .is_some_and(|f| f.to_lowercase() == mine)
        {
            out.push(pid);
        }
    }
    out.sort_unstable();
    out
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
            if matches!(classify_init_error(&e), DisplayBackend::DriverBusy { .. }) {
                // Not a fault. The driver serves one client at a time and somebody else has it,
                // so the answer is to close them, never to reinstall anything.
                match driver_pipe_holder() {
                    Some(pid) => format!(
                        "the display driver is installed and healthy, but another ScreenExtend \
                         process (pid {pid}) is already using it. Quit that instance and try again."
                    ),
                    None => "the display driver is installed and healthy, but another \
                             ScreenExtend process is already using it. Quit it and try again."
                        .to_string(),
                }
            } else if std::path::Path::new(&pipe_path()).exists() {
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
        assert_eq!(
            DisplayBackend::DriverNotResponding.as_str(),
            "DriverNotResponding"
        );
        // The pid rides in `holder`, never in the wire name — the name is compared against a
        // literal by `doctor --json` and the startup gate.
        assert_eq!(
            DisplayBackend::DriverBusy { holder: None }.as_str(),
            "DriverBusy"
        );
        assert_eq!(
            DisplayBackend::DriverBusy { holder: Some(7) }.as_str(),
            "DriverBusy"
        );
        assert_eq!(
            DisplayBackend::DriverBusy { holder: Some(7) }.holder(),
            Some(7)
        );
        assert_eq!(DisplayBackend::IddDriver.holder(), None);
    }

    /// The distinction the whole fix rests on: a refused connect can mean "occupied", and only
    /// two OS errors mean that. Anything else is a driver that is genuinely not answering, and
    /// treating it as busy would hide a real fault behind "quit the other instance".
    #[test]
    fn only_the_pipe_busy_errors_mean_occupied() {
        assert!(is_busy_os_error(ERROR_PIPE_BUSY));
        assert!(is_busy_os_error(ERROR_SEM_TIMEOUT));
        // ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND.
        for code in [5, 109, 0, ERROR_FILE_NOT_FOUND] {
            assert!(!is_busy_os_error(code), "{code} must not read as busy");
        }
        assert!(is_absent_os_error(ERROR_FILE_NOT_FOUND));
        assert!(is_absent_os_error(ERROR_PATH_NOT_FOUND));
        assert!(!is_absent_os_error(ERROR_PIPE_BUSY));

        let connect_error = |code| {
            InitError::Connect(ConnectionError::Failed(std::io::Error::from_raw_os_error(
                code,
            )))
        };
        assert!(matches!(
            classify_init_error(&connect_error(ERROR_PIPE_BUSY)),
            DisplayBackend::DriverBusy { .. }
        ));
        assert_eq!(
            classify_init_error(&connect_error(ERROR_FILE_NOT_FOUND)),
            DisplayBackend::DriverNotInstalled
        );
        // ERROR_ACCESS_DENIED: the pipe is there and would not have us. Not a state anything
        // can install its way out of, so it stays the honest "not answering".
        assert_eq!(
            classify_init_error(&connect_error(5)),
            DisplayBackend::DriverNotResponding
        );
        // A handshake that timed out with nobody else on the machine to be holding the driver
        // is a driver that is genuinely wedged.
        let handshake = InitError::RequestState(crate::driver_ipc::error::RequestError::Timeout(
            Duration::from_secs(5),
        ));
        if driver_pipe_holder().is_none() {
            assert_eq!(
                classify_init_error(&handshake),
                DisplayBackend::DriverNotResponding
            );
        } else {
            crate::test_support::skip(
                "the wedged-driver classification",
                "another ScreenExtend process is running, which is the busy case by definition",
            );
        }
    }

    /// A pipe whose single instance is already taken reads as `DriverBusy`, and a pipe that
    /// accepts a connection and then says nothing reads as `DriverNotResponding`.
    ///
    /// Neither needs the real driver: the classification is about what the OS says to a
    /// connect, so a named pipe of the test's own reproduces it exactly — and reproducing it
    /// with the real driver would mean fighting the running host for it, on the one machine
    /// where that matters.
    #[test]
    fn an_occupied_pipe_is_busy_and_a_silent_one_is_not_responding() {
        use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("tokio runtime");

        let taken = format!("screenextend-probe-busy-{}", std::process::id());
        let taken_path = pipe_path_for(&taken);
        let _server = rt
            .block_on(async {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .max_instances(1)
                    .create(&taken_path)
            })
            .expect("create a one-instance pipe server");
        // One client occupies the only instance, exactly as the running host does.
        let _client = rt
            .block_on(async { ClientOptions::new().open(&taken_path) })
            .expect("occupy the pipe");
        match probe_pipe(&taken) {
            DisplayBackend::DriverBusy { .. } => {}
            other => panic!("an occupied pipe classified as {other:?}, not DriverBusy"),
        }

        // A pipe that lets us in and then says nothing: a wedged driver, unless one of our own
        // processes is up, in which case it is the busy case by definition.
        let silent = format!("screenextend-probe-silent-{}", std::process::id());
        let silent_path = pipe_path_for(&silent);
        let _mute = rt
            .block_on(async {
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .max_instances(2)
                    .create(&silent_path)
            })
            .expect("create a pipe server that never answers");
        if driver_pipe_holder().is_none() {
            assert_eq!(probe_pipe(&silent), DisplayBackend::DriverNotResponding);
        } else {
            crate::test_support::skip(
                "the wedged-driver probe",
                "another ScreenExtend process is running, so a silent pipe reads as busy",
            );
        }

        // And a name nothing is serving is a driver that was never installed.
        assert_eq!(
            probe_pipe("screenextend-probe-absent"),
            DisplayBackend::DriverNotInstalled
        );
    }

    /// What the *real* driver does to a second client, measured rather than assumed.
    ///
    /// This is the state the owner hit: one client already open, a second asking. It prints the
    /// error the second connect gets, so the classification above can be checked against the
    /// shipped driver rather than against a pipe of our own making. Needs the driver installed
    /// and nothing else using it. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "requires the virtual display driver installed and idle; run with --ignored --nocapture"]
    fn report_second_client_behaviour() {
        println!(
            "pipe {} exists: {}",
            pipe_path(),
            std::path::Path::new(&pipe_path()).exists()
        );
        println!(
            "first probe (nothing holding it): {:?}",
            probe_pipe(DEFAULT_PIPE_NAME)
        );
        let Ok(held) = DriverClient::new() else {
            crate::test_support::skip_because(
                "the second-client probe",
                "the virtual display driver is not installed, or is already in use",
            );
            return;
        };
        println!("holding one client open; asking again from a second connection");
        let mut open = vec![held];
        for n in 2..=12 {
            let started = Instant::now();
            let next = std::thread::spawn(DriverClient::new)
                .join()
                .expect("probe thread");
            match next {
                Ok(client) => {
                    println!("  client {n}: ok after {:?}", started.elapsed());
                    open.push(client);
                }
                Err(e) => {
                    println!("  client {n}: {e} after {:?}", started.elapsed());
                    println!("    raw os error: {:?}", raw_os_error_of(&e));
                    break;
                }
            }
        }
        println!("  {} clients open at once", open.len());
        println!(
            "  Path::exists() while held: {}",
            std::path::Path::new(&pipe_path()).exists()
        );
        println!(
            "  probe_pipe() while held:   {:?}",
            probe_pipe(DEFAULT_PIPE_NAME)
        );
        drop(open);
    }

    #[cfg(test)]
    fn raw_os_error_of(e: &InitError) -> Option<i32> {
        match e {
            InitError::Connect(ConnectionError::Failed(io)) => io.raw_os_error(),
            InitError::RequestState(_) => None,
        }
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
    #[ignore = "read-only report of QueryDisplayConfig output; run with --ignored --nocapture"]
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
    #[ignore = "requires the virtual display driver installed; creates three real displays; run with --ignored --nocapture"]
    fn report_display_name_correlation() {
        let Ok(display) = WindowsVirtualDisplay::new_shared_reported() else {
            crate::test_support::skip_because(
                "windows virtual display",
                "the virtual display driver is not installed",
            );
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

    /// Times the two ways a connected device's geometry can change.
    ///
    /// A resolution already in the display's registered mode list is a `ChangeDisplaySettingsEx`
    /// on a live monitor. One that is not needs the *mode list* to change, and the bundled
    /// driver's `notify()` implements that as `IddCxMonitorDeparture` followed by
    /// `IddCxMonitorCreate` + `IddCxMonitorArrival` — a full hot-unplug and re-plug, with the
    /// desktop reflowing every window and the capture session rebuilt. That is why
    /// `dpr_mode_ladder` pre-registers the whole ladder at create time.
    ///
    /// `IddCxMonitorUpdateModes` would replace the second path with an in-place update; it is
    /// IddCx 1.0 API, below the IddCx 1.2 the bundled INF declares, and upstream simply never
    /// used it. This harness is the before half of that comparison. Needs the driver installed;
    /// creates a real display for a few seconds. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "requires the virtual display driver installed; creates a real display; run with --ignored --nocapture"]
    fn report_mode_change_cost() {
        use crate::windows_utils::streamer::capture::{monitor_device_names, set_display_mode};

        let Ok(display) = WindowsVirtualDisplay::new_shared_reported() else {
            crate::test_support::skip_because(
                "windows virtual display",
                "the virtual display driver is not installed",
            );
            return;
        };
        let before = monitor_device_names();
        let id = display
            .create_display_with_modes(
                "ScreenExtend mode probe".to_string(),
                1920,
                1080,
                60,
                &[(1280, 720)],
            )
            .expect("create display");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut name = None;
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(n) = monitor_device_names()
                .into_iter()
                .find(|n| !before.contains(n))
            {
                name = Some(n);
                break;
            }
        }
        let Some(name) = name else {
            println!("the new display never appeared");
            display.remove_display(id);
            return;
        };
        println!("{name}: created, now at {:?}", current_display_mode(&name));

        // In the registered ladder: no mode-list change, so no departure.
        for (w, h) in [(1280u32, 720u32), (1920, 1080)] {
            let t = Instant::now();
            let applied = set_display_mode(&name, w, h, 60, false);
            println!(
                "  in-ladder {w}x{h}: {:?} in {:.1} ms -> {:?}",
                applied,
                t.elapsed().as_secs_f64() * 1000.0,
                current_display_mode(&name)
            );
        }

        // Outside it: the only way to reach a new mode today is to take the display away and
        // build a new one, which is what a mode-list change costs.
        let t = Instant::now();
        display.remove_display(id);
        // Wait for the old monitor to actually go, or the name search below would match the
        // display we just asked to be removed and report a time that is not the reattach.
        let gone = Instant::now() + Duration::from_secs(15);
        while Instant::now() < gone && monitor_device_names().iter().any(|n| n == &name) {
            std::thread::sleep(Duration::from_millis(20));
        }
        println!(
            "  departure alone: {:.1} ms",
            t.elapsed().as_secs_f64() * 1000.0
        );
        let recreated = display
            .create_display_with_modes(
                "ScreenExtend mode probe".to_string(),
                1600,
                900,
                60,
                &[(1280, 720)],
            )
            .expect("recreate display");
        let mut new_name = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
            if let Some(n) = monitor_device_names()
                .into_iter()
                .find(|n| !before.contains(n))
            {
                new_name = Some(n);
                break;
            }
        }
        println!(
            "  out-of-ladder 1600x900 (remove + recreate + reattach): {:.1} ms -> {new_name:?}",
            t.elapsed().as_secs_f64() * 1000.0
        );
        if let Some(n) = &new_name {
            let t = Instant::now();
            let applied = set_display_mode(n, 1600, 900, 60, false);
            println!(
                "  then set 1600x900: {applied:?} in {:.1} ms -> {:?}",
                t.elapsed().as_secs_f64() * 1000.0,
                current_display_mode(n)
            );
        }
        display.remove_display(recreated);
    }

    /// Reports which display backend this machine probes to. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "read-only report of the probed display backend; run with --ignored --nocapture"]
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
    #[ignore = "requires the virtual display driver installed; creates a real display; run with --ignored --nocapture"]
    fn report_selected_refresh_rate() {
        let Ok(display) = WindowsVirtualDisplay::new_shared_reported() else {
            crate::test_support::skip_because(
                "windows virtual display",
                "the virtual display driver is not installed",
            );
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
