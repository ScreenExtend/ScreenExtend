//! The virtual display driver's host process: how big it has grown, and restarting it.
//!
//! The bundled IddCx driver runs in its own `WUDFHost.exe` (the INF disables process sharing)
//! and leaks ~47-124 MB of commit, ~37 handles and ~5 threads per display creation or mode
//! change. At ~36 GB / ~3,200 threads `SetDisplayConfig` fails with error 31 and every join times
//! out. Only a device restart frees it, which needs Administrator (`drivers restart` behind UAC).
//!
//! Finding our `WUDFHost.exe` unelevated: `OpenProcess` is refused for a LocalService host, but
//! `GetNamedPipeServerProcessId` on `\\.\pipe\virtualdisplaydriver` names the process serving the
//! pipe, and `NtQuerySystemInformation(SystemProcessInformation)` reads its counters without a
//! handle. Only when the pipe cannot name its server does this fall back to the largest
//! `WUDFHost.exe`, and the result says so.

use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::{DisplayDriverHealth, DriverRestartOutcome};

/// Default commit threshold, MB.
///
/// The host is ~10 MB after a boot, grows ~47 MB per 1080p display creation (94-124 MB at hi-res
/// or with a DPR ladder) and fails at ~36 GB. 8 GB is ~170 ordinary joins or mode changes (weeks
/// of normal use) and a quarter of the failure point.
pub const DEFAULT_THRESHOLD_MB: u32 = 8 * 1024;

/// Default thread threshold: a quarter of the ~3,200 threads at failure. At the measured ~86
/// threads per GB that is ~9 GB, so it only matters for a leak that grows threads faster than
/// commit.
pub const DEFAULT_THRESHOLD_THREADS: u32 = 800;

/// Overrides the commit threshold, in MB, for testing. Setting it switches the thread criterion
/// off unless [`THRESHOLD_THREADS_ENV`] is set too, so a large value really does suppress the
/// prompt and a small one really does force it.
pub const THRESHOLD_MB_ENV: &str = "SE_DRIVER_RESTART_THRESHOLD_MB";
pub const THRESHOLD_THREADS_ENV: &str = "SE_DRIVER_RESTART_THRESHOLD_THREADS";

const DRIVER_PIPE: &str = r"\\.\pipe\virtualdisplaydriver";
const WUDFHOST: &str = "wudfhost.exe";

/// How long `drivers restart` waits for the restarted driver to serve its pipe again.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the desktop app waits for the elevated subcommand. The UAC prompt is answered
/// inside `ShellExecuteExW`, before this starts; what it covers is the child's own startup, the
/// restart and its [`READY_TIMEOUT`].
const ELEVATED_TIMEOUT: Duration = Duration::from_secs(120);

/// How long the desktop app waits for an elevated `installdrivers` / `removedrivers`. `nefconc`
/// creates or removes a devnode and stages or uninstalls the driver package, which is tens of
/// seconds on a slow disk, not minutes.
pub const DRIVER_COMMAND_TIMEOUT: Duration = Duration::from_secs(180);

// exit codes of `drivers restart`: 2 is usage and 3 is `doctor`'s, so these stay clear of both
pub const EXIT_RESTARTED: i32 = 0;
pub const EXIT_FAILED: i32 = 1;
pub const EXIT_NOT_ELEVATED: i32 = 4;
pub const EXIT_REFUSED: i32 = 5;
pub const EXIT_TIMEOUT: i32 = 6;
pub const EXIT_NEEDS_REBOOT: i32 = 7;
pub const EXIT_NOT_INSTALLED: i32 = 8;

/// The `reason` an exit code stands for, when the subcommand left no report behind.
pub fn reason_for_exit(code: i32) -> &'static str {
    match code {
        EXIT_RESTARTED => "restarted",
        EXIT_NOT_ELEVATED => "notElevated",
        EXIT_REFUSED => "refused",
        EXIT_TIMEOUT => "timeout",
        EXIT_NEEDS_REBOOT => "needsReboot",
        EXIT_NOT_INSTALLED => "notInstalled",
        _ => "failed",
    }
}

// what the machine reports

/// One process, as `NtQuerySystemInformation` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostProcess {
    pub pid: u32,
    pub image: String,
    /// Private commit (`PrivatePageCount`): what Task Manager shows as "Commit size" and .NET
    /// as `PrivateMemorySize64`.
    pub commit_bytes: u64,
    pub threads: u32,
    pub handles: u32,
}

impl HostProcess {
    pub fn commit_mb(&self) -> u32 {
        (self.commit_bytes / (1024 * 1024)).min(u64::from(u32::MAX)) as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identified {
    /// The process serving the driver's named pipe.
    PipeServer,
    /// The largest `WUDFHost.exe`: a guess, used only when the pipe could not name its server.
    LargestWudfHost,
}

impl Identified {
    pub fn as_str(self) -> &'static str {
        match self {
            Identified::PipeServer => "pipeServer",
            Identified::LargestWudfHost => "largestWudfHost",
        }
    }
}

/// What opening the driver's pipe said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipeProbe {
    /// It answered, and this pid serves it.
    Server(u32),
    /// It exists but every instance is taken, or it would not name its server.
    Present,
    /// Nothing serves that name: the driver is not installed or not running.
    Absent,
}

fn is_wudfhost(image: &str) -> bool {
    image.eq_ignore_ascii_case(WUDFHOST)
}

/// Pick the driver's host out of a process list. Pure, so the choice can be tested.
///
/// The pipe server wins when it is a `WUDFHost.exe`; anything else serving that pipe (a test
/// mock, say) is not trusted. Without one, the largest `WUDFHost.exe`: a leaking driver host is
/// the largest by gigabytes, and otherwise nothing is over the threshold anyway.
pub fn select_driver_host(
    procs: &[HostProcess],
    pipe_server: Option<u32>,
) -> Option<(&HostProcess, Identified)> {
    if let Some(pid) = pipe_server {
        if let Some(p) = procs.iter().find(|p| p.pid == pid && is_wudfhost(&p.image)) {
            return Some((p, Identified::PipeServer));
        }
    }
    procs
        .iter()
        .filter(|p| is_wudfhost(&p.image))
        .max_by_key(|p| p.commit_bytes)
        .map(|p| (p, Identified::LargestWudfHost))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Thresholds {
    pub commit_mb: u32,
    /// `0` disables the thread criterion.
    pub threads: u32,
}

impl Default for Thresholds {
    fn default() -> Self {
        Thresholds {
            commit_mb: DEFAULT_THRESHOLD_MB,
            threads: DEFAULT_THRESHOLD_THREADS,
        }
    }
}

impl Thresholds {
    /// The thresholds with the two environment overrides applied. Pure over its inputs.
    pub fn from_values(mb: Option<&str>, threads: Option<&str>) -> Self {
        let parse = |v: Option<&str>| v.and_then(|s| s.trim().parse::<u32>().ok());
        let mb = parse(mb).filter(|v| *v > 0);
        let threads = parse(threads);
        match (mb, threads) {
            (None, None) => Thresholds::default(),
            (Some(mb), None) => Thresholds {
                commit_mb: mb,
                threads: 0,
            },
            (mb, Some(threads)) => Thresholds {
                commit_mb: mb.unwrap_or(DEFAULT_THRESHOLD_MB),
                threads,
            },
        }
    }

    pub fn from_env() -> Self {
        Self::from_values(
            crate::envcfg::var(THRESHOLD_MB_ENV).ok().as_deref(),
            crate::envcfg::var(THRESHOLD_THREADS_ENV).ok().as_deref(),
        )
    }

    pub fn exceeded(&self, commit_mb: u32, threads: u32) -> bool {
        commit_mb >= self.commit_mb || (self.threads > 0 && threads >= self.threads)
    }
}

/// The verdict, from what was read. Pure.
///
/// An absent pipe means the driver is not running, so whatever the largest `WUDFHost.exe`
/// weighs it is somebody else's and restarting ours would not touch it.
pub fn assess(
    procs: &[HostProcess],
    pipe: PipeProbe,
    thresholds: Thresholds,
) -> DisplayDriverHealth {
    let mut health = DisplayDriverHealth {
        supported: true,
        identified_by: "none".to_string(),
        threshold_mb: thresholds.commit_mb,
        threshold_threads: thresholds.threads,
        ..Default::default()
    };
    if pipe == PipeProbe::Absent {
        health.detail = "the display driver is not running; nothing to measure".to_string();
        return health;
    }
    let server = match pipe {
        PipeProbe::Server(pid) => Some(pid),
        _ => None,
    };
    let Some((host, how)) = select_driver_host(procs, server) else {
        health.detail = "no WUDFHost.exe is running".to_string();
        return health;
    };
    health.pid = Some(host.pid);
    health.identified_by = how.as_str().to_string();
    health.commit_mb = host.commit_mb();
    health.threads = host.threads;
    health.handles = host.handles;
    health.needs_restart = thresholds.exceeded(health.commit_mb, health.threads);
    let threads_limit = if thresholds.threads > 0 {
        format!(" or {} threads", thresholds.threads)
    } else {
        String::new()
    };
    health.detail = format!(
        "WUDFHost pid {} ({}): {} MB commit, {} threads, {} handles; restart at {} MB{}{}",
        host.pid,
        match how {
            Identified::PipeServer => "serves the driver's pipe",
            Identified::LargestWudfHost => "largest WUDFHost; the pipe would not name its server",
        },
        health.commit_mb,
        health.threads,
        health.handles,
        thresholds.commit_mb,
        threads_limit,
        if health.needs_restart {
            ". RESTART NEEDED"
        } else {
            ""
        }
    );
    health
}

/// Every process on the machine, with its commit, threads and handles. No process handle is
/// opened, so a LocalService `WUDFHost.exe` is as readable as our own.
pub fn process_list() -> Result<Vec<HostProcess>, String> {
    use windows::Wdk::System::SystemInformation::{
        NtQuerySystemInformation, SystemProcessInformation,
    };
    use windows::Win32::System::WindowsProgramming::SYSTEM_PROCESS_INFORMATION;

    const STATUS_INFO_LENGTH_MISMATCH: i32 = 0xC000_0004_u32 as i32;
    let mut size: usize = 1 << 20;
    for _ in 0..8 {
        // u64 elements so every entry, which the kernel aligns to 8, is aligned in memory too
        let mut buf = vec![0u64; size / 8];
        let mut needed = 0u32;
        let status = unsafe {
            NtQuerySystemInformation(
                SystemProcessInformation,
                buf.as_mut_ptr().cast(),
                (buf.len() * 8) as u32,
                &mut needed,
            )
        };
        if status.0 == STATUS_INFO_LENGTH_MISMATCH {
            size = (needed as usize).max(size * 2) + (64 << 10);
            continue;
        }
        if status.0 < 0 {
            return Err(format!(
                "NtQuerySystemInformation failed: 0x{:08x}",
                status.0
            ));
        }
        let base = buf.as_ptr().cast::<u8>();
        let end = buf.len() * 8;
        let mut out = Vec::new();
        let mut offset = 0usize;
        loop {
            if offset + std::mem::size_of::<SYSTEM_PROCESS_INFORMATION>() > end {
                break;
            }
            // SAFETY: in bounds (checked above) and 8-aligned (the kernel aligns every entry and
            // the buffer is u64-backed).
            let entry = unsafe { &*base.add(offset).cast::<SYSTEM_PROCESS_INFORMATION>() };
            let name = &entry.ImageName;
            let image = if name.Buffer.is_null() || name.Length == 0 {
                String::new()
            } else {
                // SAFETY: the kernel points `Buffer` into this same buffer, `Length` bytes long.
                let chars = unsafe {
                    std::slice::from_raw_parts(name.Buffer.0, usize::from(name.Length) / 2)
                };
                String::from_utf16_lossy(chars)
            };
            out.push(HostProcess {
                pid: entry.UniqueProcessId.0 as usize as u32,
                image,
                commit_bytes: entry.PrivatePageCount as u64,
                threads: entry.NumberOfThreads,
                handles: entry.HandleCount,
            });
            if entry.NextEntryOffset == 0 {
                break;
            }
            offset += entry.NextEntryOffset as usize;
        }
        return Ok(out);
    }
    Err("NtQuerySystemInformation kept asking for a larger buffer".to_string())
}

/// Ask the driver's pipe who serves it. Opens one client handle and closes it at once, the same
/// thing the backend probe's existence test does to this pipe.
pub fn probe_pipe_server() -> PipeProbe {
    probe_pipe_server_at(Path::new(DRIVER_PIPE))
}

fn probe_pipe_server_at(path: &Path) -> PipeProbe {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;

    match std::fs::OpenOptions::new().read(true).open(path) {
        Ok(file) => {
            let mut pid = 0u32;
            let handle = HANDLE(file.as_raw_handle());
            match unsafe { GetNamedPipeServerProcessId(handle, &mut pid) } {
                Ok(()) if pid != 0 => PipeProbe::Server(pid),
                _ => PipeProbe::Present,
            }
        }
        Err(e) => match e.raw_os_error() {
            // ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND
            Some(2) | Some(3) => PipeProbe::Absent,
            // ERROR_PIPE_BUSY, ERROR_SEM_TIMEOUT, or anything else from a name that exists
            _ => PipeProbe::Present,
        },
    }
}

/// The whole read: what `display_driver_health`, `doctor` and the startup gate see.
pub fn read_health() -> DisplayDriverHealth {
    let pipe = probe_pipe_server();
    let procs = match process_list() {
        Ok(p) => p,
        Err(e) => {
            teprintln!("[driver-health] {e}");
            return DisplayDriverHealth {
                supported: true,
                identified_by: "none".to_string(),
                detail: format!("could not read the process list: {e}"),
                ..Default::default()
            };
        }
    };
    let health = assess(&procs, pipe, Thresholds::from_env());
    if health.identified_by == Identified::LargestWudfHost.as_str() {
        tprintln!(
            "[driver-health] the driver's pipe would not name its server; falling back to the \
             largest WUDFHost.exe, which may not be the display driver's host"
        );
    }
    tprintln!("[driver-health] {}", health.detail);
    health
}

/// The same read as [`read_health`], silent unless the driver host is over threshold.
///
/// For the places the startup gate never reaches: `serve`, which has no gate, and a host left
/// running for days. At the failure point every join waits out the attach timeout, and this
/// line is the only thing in the log that points at the driver. Costs one
/// `NtQuerySystemInformation` and one pipe open, off the frame path.
pub fn warn_if_over_threshold(context: &str) -> Option<DisplayDriverHealth> {
    let pipe = probe_pipe_server();
    let procs = process_list().ok()?;
    let health = assess(&procs, pipe, Thresholds::from_env());
    if health.needs_restart {
        teprintln!(
            "[driver-health] {context}: {}. Restart the display driver (`ScreenExtend drivers \
             restart`, or restart the app and accept its prompt) before joins start failing.",
            health.detail
        );
    }
    Some(health)
}

// the restart itself (elevated)

/// Why a running host stands in the way of a restart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The host this data directory belongs to has set up (it holds a driver client) and may be
    /// streaming.
    HostConnected { pid: Option<u32>, joins: usize },
    /// The driver lists virtual displays and another ScreenExtend process is alive to own them.
    DisplaysAttached { monitors: usize, others: Vec<u32> },
    /// The driver's display list could not be read and another ScreenExtend process is alive.
    Unknown { others: Vec<u32>, error: String },
}

impl Refusal {
    pub fn sentence(&self) -> String {
        match self {
            Refusal::HostConnected { pid, joins } => format!(
                "a ScreenExtend host{} is running and connected to the display driver ({joins} \
                 live session(s)); restarting the driver would cut it off and remove any \
                 displays it holds. Quit it first (`ScreenExtend stop`), then try again.",
                pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
            ),
            Refusal::DisplaysAttached { monitors, others } => format!(
                "the display driver has {monitors} virtual display(s) attached and ScreenExtend \
                 is running (pid {}); restarting the driver would remove them. Quit every \
                 ScreenExtend host first, then try again.",
                join_pids(others)
            ),
            Refusal::Unknown { others, error } => format!(
                "could not read the display driver's display list ({error}) while ScreenExtend is \
                 running (pid {}); quit it first, then try again.",
                join_pids(others)
            ),
        }
    }
}

fn join_pids(pids: &[u32]) -> String {
    pids.iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a restart may go ahead. Pure over what was asked.
///
/// - `stats`: the running instance's `STATS` reply, if one answered. Its `joins` key exists only
///   once `setup` has run and the host holds a driver client, so its presence alone is a
///   refusal, whatever the count: an idle host would be left holding a dead pipe. The desktop
///   app's own startup gate asks *before* `setup`, which is why it gets through.
/// - `running_pid`: that instance's pid, from `PING`.
/// - `monitors`: the driver's own display list, which is machine-wide.
/// - `others`: other processes running this executable, not counting this one.
pub fn restart_refusal(
    stats: Option<&serde_json::Value>,
    running_pid: Option<u32>,
    monitors: Result<usize, String>,
    others: &[u32],
) -> Option<Refusal> {
    if let Some(joins) = stats.and_then(|s| s.get("joins")) {
        let count = joins
            .as_array()
            .map(Vec::len)
            .or_else(|| joins.as_object().map(|o| o.len()))
            .unwrap_or(0);
        return Some(Refusal::HostConnected {
            pid: running_pid,
            joins: count,
        });
    }
    let sessions = stats
        .and_then(|s| s.get("sessions"))
        .and_then(|s| s.as_object())
        .map_or(0, |o| o.len());
    if sessions > 0 {
        return Some(Refusal::HostConnected {
            pid: running_pid,
            joins: sessions,
        });
    }
    // the instance that answered without `joins` has not set up (it is the desktop app asking
    // from its startup gate), so it owns no displays and is not "another" host
    let others: Vec<u32> = others
        .iter()
        .copied()
        .filter(|p| Some(*p) != running_pid)
        .collect();
    if others.is_empty() {
        // monitors left in the list belong to a host that crashed with them attached, and the
        // restart is what clears them
        return None;
    }
    match monitors {
        Ok(0) => None,
        Ok(n) => Some(Refusal::DisplaysAttached {
            monitors: n,
            others,
        }),
        Err(error) => Some(Refusal::Unknown { others, error }),
    }
}

/// One devnode of ours, by its instance id.
#[derive(Debug, Clone)]
pub struct DriverDevice {
    pub instance_id: String,
}

/// Split a `REG_MULTI_SZ` (UTF-16) into its strings.
pub fn multi_sz(units: &[u16]) -> Vec<String> {
    units
        .split(|u| *u == 0)
        .filter(|s| !s.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// RAII for an `HDEVINFO`.
struct DevInfoSet(windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        use windows::Win32::Devices::DeviceAndDriverInstallation::SetupDiDestroyDeviceInfoList;
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

fn display_class_set() -> Result<DevInfoSet, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiGetClassDevsW, DIGCF_PRESENT, GUID_DEVCLASS_DISPLAY,
    };
    unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVCLASS_DISPLAY),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT,
        )
    }
    .map(DevInfoSet)
    .map_err(|e| format!("SetupDiGetClassDevsW: {e}"))
}

/// Walk the present Display-class devices and call `f` on each one whose hardware ids include
/// ours (`Root\VirtualDisplayDriver`, the id the installer creates the devnode with).
fn for_each_driver_device(
    mut f: impl FnMut(
        &DevInfoSet,
        &windows::Win32::Devices::DeviceAndDriverInstallation::SP_DEVINFO_DATA,
        String,
    ),
) -> Result<usize, String> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiEnumDeviceInfo, SetupDiGetDeviceInstanceIdW, SetupDiGetDeviceRegistryPropertyW,
        SPDRP_HARDWAREID, SP_DEVINFO_DATA,
    };
    let set = display_class_set()?;
    let mut found = 0;
    for index in 0.. {
        let mut data = SP_DEVINFO_DATA {
            cbSize: std::mem::size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) }.is_err() {
            break;
        }
        let mut bytes = vec![0u8; 2048];
        let mut needed = 0u32;
        if unsafe {
            SetupDiGetDeviceRegistryPropertyW(
                set.0,
                &data,
                SPDRP_HARDWAREID,
                None,
                Some(&mut bytes),
                Some(&mut needed),
            )
        }
        .is_err()
        {
            continue;
        }
        bytes.truncate(needed as usize);
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        if !multi_sz(&units)
            .iter()
            .any(|id| id.eq_ignore_ascii_case(crate::VDD_HARDWARE_ID))
        {
            continue;
        }
        let mut id = vec![0u16; 512];
        let mut id_len = 0u32;
        let instance_id = if unsafe {
            SetupDiGetDeviceInstanceIdW(set.0, &data, Some(&mut id), Some(&mut id_len))
        }
        .is_ok()
        {
            let len = (id_len as usize).saturating_sub(1).min(id.len());
            String::from_utf16_lossy(&id[..len])
        } else {
            String::new()
        };
        found += 1;
        f(&set, &data, instance_id);
    }
    Ok(found)
}

/// Our driver's devnodes. Normally exactly one (`ROOT\DISPLAY\0000`).
pub fn find_driver_devices() -> Result<Vec<DriverDevice>, String> {
    let mut out = Vec::new();
    for_each_driver_device(|_, _, instance_id| out.push(DriverDevice { instance_id }))?;
    Ok(out)
}

#[derive(Debug)]
pub enum RestartError {
    NeedsReboot(String),
    Api(String),
}

/// Restart every devnode of ours in place: `DIF_PROPERTYCHANGE` with `DICS_PROPCHANGE`, the
/// class-installer request Device Manager and devcon's `restart` make. Stopping the device ends
/// its `WUDFHost.exe` (process sharing is disabled) and everything that leaked into it. Needs
/// Administrator. A device that refuses to stop leaves `DI_NEEDREBOOT` behind, which is
/// reported rather than called a success.
pub fn restart_driver_devices() -> Result<usize, RestartError> {
    use windows::Win32::Devices::DeviceAndDriverInstallation::{
        SetupDiCallClassInstaller, SetupDiGetDeviceInstallParamsW, SetupDiSetClassInstallParamsW,
        DICS_FLAG_CONFIGSPECIFIC, DICS_PROPCHANGE, DIF_PROPERTYCHANGE, DI_NEEDREBOOT,
        DI_NEEDRESTART, SP_CLASSINSTALL_HEADER, SP_DEVINSTALL_PARAMS_W, SP_PROPCHANGE_PARAMS,
    };
    let mut failure: Option<RestartError> = None;
    let mut restarted = 0usize;
    let found = for_each_driver_device(|set, data, instance_id| {
        if failure.is_some() {
            return;
        }
        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: std::mem::size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: DICS_PROPCHANGE,
            Scope: DICS_FLAG_CONFIGSPECIFIC,
            HwProfile: 0,
        };
        let set_params = unsafe {
            SetupDiSetClassInstallParamsW(
                set.0,
                Some(data),
                Some(&params.ClassInstallHeader),
                std::mem::size_of::<SP_PROPCHANGE_PARAMS>() as u32,
            )
        };
        if let Err(e) = set_params {
            failure = Some(RestartError::Api(format!(
                "SetupDiSetClassInstallParamsW({instance_id}): {e}"
            )));
            return;
        }
        if let Err(e) = unsafe { SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set.0, Some(data)) }
        {
            failure = Some(RestartError::Api(format!(
                "SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, {instance_id}): {e}"
            )));
            return;
        }
        // SAFETY: a plain C struct; zero is a valid starting value and cbSize is set below.
        let mut install: SP_DEVINSTALL_PARAMS_W = unsafe { std::mem::zeroed() };
        install.cbSize = std::mem::size_of::<SP_DEVINSTALL_PARAMS_W>() as u32;
        if unsafe { SetupDiGetDeviceInstallParamsW(set.0, Some(data), &mut install) }.is_ok()
            && (install.Flags.0 & (DI_NEEDREBOOT.0 | DI_NEEDRESTART.0)) != 0
        {
            failure = Some(RestartError::NeedsReboot(format!(
                "{instance_id} could not be stopped while Windows was using it; restart the \
                 computer to free the driver's memory"
            )));
            return;
        }
        restarted += 1;
    })
    .map_err(RestartError::Api)?;
    if let Some(f) = failure {
        return Err(f);
    }
    if found == 0 {
        return Err(RestartError::Api(
            "no ScreenExtend display driver device is present".to_string(),
        ));
    }
    Ok(restarted)
}

/// Wait until the driver serves its pipe from a process other than `old_pid` and answers the
/// protocol. Returns the new host's pid.
pub fn wait_until_ready(old_pid: Option<u32>, timeout: Duration) -> Option<u32> {
    let deadline = Instant::now() + timeout;
    loop {
        if let PipeProbe::Server(pid) = probe_pipe_server() {
            if Some(pid) != old_pid
                && super::virtual_display::probe_display_backend_fresh()
                    == super::virtual_display::DisplayBackend::IddDriver
            {
                return Some(pid);
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// What the elevated child writes for the desktop app, which cannot read its stdout.
pub fn write_report(path: &Path, outcome: &DriverRestartOutcome) {
    match serde_json::to_string(outcome) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                teprintln!("[driver-restart] could not write {}: {e}", path.display());
            }
        }
        Err(e) => teprintln!("[driver-restart] could not encode the report: {e}"),
    }
}

// the desktop app's half (unelevated)

/// Set while an elevated driver operation this process started is still running, including one
/// it stopped waiting for. Install, remove and restart all reconfigure the same devnode, so a
/// second one must not start over the first.
static ELEVATED_IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Holds [`ELEVATED_IN_FLIGHT`] until dropped.
#[derive(Debug)]
pub struct ElevatedSlot(());

impl Drop for ElevatedSlot {
    fn drop(&mut self) {
        ELEVATED_IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// Claim the one elevated-operation slot, or `None` while another operation holds it.
pub fn claim_elevated_slot() -> Option<ElevatedSlot> {
    ELEVATED_IN_FLIGHT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| ElevatedSlot(()))
}

/// How an elevated run of this executable ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Elevated {
    /// The UAC prompt was dismissed.
    Declined,
    /// It could not be started.
    Failed(String),
    /// Another elevated driver operation from this process is still running.
    Busy,
    /// It ran to completion with this exit code.
    Exited(i32),
    /// It was still running at the deadline. It is left to finish (killing an elevated process
    /// halfway through a devnode change is worse than letting it complete) and watched to the
    /// end, so the slot stays taken until it really has.
    TimedOut,
}

/// Run this executable elevated with `params`, and wait for it.
///
/// The `elevated-command` crate cannot do this: on Windows it is a bare `ShellExecuteW("runas")`,
/// which returns as soon as the process is created, with no exit code and no handle to wait on.
/// `ShellExecuteExW` with `SEE_MASK_NOCLOSEPROCESS` is the same UAC route and hands back both.
///
/// `leftover` is removed once the child has exited, on the timeout path too.
pub fn run_elevated(
    params: &str,
    owner: Option<isize>,
    timeout: Duration,
    leftover: Option<PathBuf>,
) -> Elevated {
    use windows::core::{HSTRING, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, HWND, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject, INFINITE};
    use windows::Win32::UI::Shell::{
        ShellExecuteExW, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS,
        SHELLEXECUTEINFOW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let Some(slot) = claim_elevated_slot() else {
        return Elevated::Busy;
    };
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            return Elevated::Failed(format!("could not resolve the executable path: {e}"));
        }
    };
    let file = HSTRING::from(exe.as_os_str());
    let params = HSTRING::from(params);
    let verb = HSTRING::from("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        hwnd: HWND(owner.unwrap_or(0) as *mut core::ffi::c_void),
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    if let Err(e) = unsafe { ShellExecuteExW(&mut info) } {
        if e.code() == ERROR_CANCELLED.to_hresult() {
            return Elevated::Declined;
        }
        return Elevated::Failed(format!("could not start the elevated process: {e}"));
    }
    if info.hProcess.is_invalid() {
        return Elevated::Failed(
            "the elevated process started but returned no process to wait on".to_string(),
        );
    }
    let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX - 1);
    let waited = unsafe { WaitForSingleObject(info.hProcess, millis) };
    if waited != WAIT_OBJECT_0 {
        // watched to the end on its own thread, which holds the slot so nothing this process
        // starts later can race it, then removes what the child was going to leave behind
        let raw = info.hProcess.0 as usize;
        let spawned = std::thread::Builder::new()
            .name("se-elevated-watch".into())
            .spawn(move || {
                let process = HANDLE(raw as *mut core::ffi::c_void);
                let _ = unsafe { WaitForSingleObject(process, INFINITE) };
                let _ = unsafe { CloseHandle(process) };
                if let Some(path) = leftover {
                    let _ = std::fs::remove_file(path);
                }
                drop(slot);
                tprintln!("[drivers] the elevated operation that outlived its deadline has exited");
            });
        if spawned.is_err() {
            // the closure (and the slot in it) is dropped with the failed spawn, so the app is
            // not wedged; the process handle is leaked rather than closed under a child that may
            // still be running
            teprintln!("[drivers] could not start a watcher for the timed-out elevated process");
        }
        return Elevated::TimedOut;
    }
    let mut code = 0u32;
    let got = unsafe { GetExitCodeProcess(info.hProcess, &mut code) };
    let _ = unsafe { CloseHandle(info.hProcess) };
    drop(slot);
    if got.is_err() {
        return Elevated::Exited(EXIT_FAILED);
    }
    Elevated::Exited(code as i32)
}

/// Run `drivers restart` elevated and wait for it: setup must not race a driver that is
/// mid-restart.
pub fn restart_elevated(owner: Option<isize>) -> DriverRestartOutcome {
    let fail = |reason: &str, detail: String| {
        teprintln!("[driver-restart] {reason}: {detail}");
        DriverRestartOutcome {
            ok: false,
            reason: reason.to_string(),
            detail,
            ..Default::default()
        }
    };

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let report = std::env::temp_dir().join(format!(
        "screenextend-driver-restart-{}-{nonce}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&report);

    let params = format!("drivers restart --report \"{}\"", report.display());
    tprintln!("[driver-restart] asking for elevation to restart the display driver");
    let code = match run_elevated(&params, owner, ELEVATED_TIMEOUT, Some(report.clone())) {
        Elevated::Exited(code) => code,
        Elevated::Declined => {
            return fail(
                "declined",
                "the administrator prompt was dismissed".to_string(),
            )
        }
        Elevated::Failed(detail) => return fail("failed", detail),
        Elevated::Busy => {
            return fail(
                "timeout",
                "an earlier display-driver operation started by this app is still running; \
                 wait for it to finish, then try again"
                    .to_string(),
            )
        }
        Elevated::TimedOut => {
            return fail(
                "timeout",
                format!(
                    "the elevated restart did not finish within {} s. It is still running and \
                     may yet complete; wait for it rather than starting another",
                    ELEVATED_TIMEOUT.as_secs()
                ),
            )
        }
    };

    let written = std::fs::read_to_string(&report)
        .ok()
        .and_then(|s| serde_json::from_str::<DriverRestartOutcome>(&s).ok());
    let _ = std::fs::remove_file(&report);
    let mut outcome = written.unwrap_or_else(|| DriverRestartOutcome {
        ok: code == EXIT_RESTARTED,
        reason: reason_for_exit(code).to_string(),
        detail: format!("`drivers restart` exited with {code} and left no report"),
        ..Default::default()
    });
    // the exit code is the verdict; the report only explains it
    outcome.ok = code == EXIT_RESTARTED;
    if outcome.ok {
        tprintln!(
            "[driver-restart] restarted: {} MB → {} MB",
            outcome.commit_mb_before.unwrap_or_default(),
            outcome.commit_mb_after.unwrap_or_default()
        );
    } else {
        teprintln!(
            "[driver-restart] {} (exit {code}): {}",
            outcome.reason,
            outcome.detail
        );
    }
    outcome
}

#[cfg(test)]
#[path = "test/driver_health.rs"]
mod tests;
