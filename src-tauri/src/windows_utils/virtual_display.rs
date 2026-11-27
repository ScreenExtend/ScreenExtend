use anyhow::bail;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::streamer::lock::LockExt;

use crate::driver_ipc::{
    error::{ConnectionError, InitError},
    host::{self, LinkOptions},
    sync::DriverClient,
    v2, Handshake, Mode, Monitor, DEFAULT_PIPE_NAME,
};

use crate::streamer::session::{DisplayTarget, SharedVirtualDisplay, VirtualDisplayController};

/// Registered on every display so a device can switch rates without a hot-unplug: an IDD
/// monitor's mode list is fixed at create time. 120 stays even for 60 Hz clients: an indirect
/// display has no hardware vblank, so this list also bounds DWM's compose cadence.
const COMMON_REFRESH_RATES: [u32; 2] = [60, 120];

/// Log a driver round trip slower than this: display attach is on the join path a user waits on.
const SLOW_COMMIT: Duration = Duration::from_millis(250);

/// Which display backend this host will use, probed rather than assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayBackend {
    /// The bundled indirect display driver is installed and answering.
    IddDriver,
    /// The driver's pipe does not exist: it was never installed, or its device node is gone.
    DriverNotInstalled,
    /// The driver is installed and healthy, and another ScreenExtend process is using it.
    /// Nothing to repair: reinstalling a good driver cannot free a pipe someone else holds.
    ///
    /// Reached by `ERROR_PIPE_BUSY` (every pipe instance taken; twelve concurrent clients did not
    /// exhaust the bundled driver's), or by a handshake state request that times out while another
    /// of our processes is alive.
    DriverBusy {
        /// The pid holding it, when we could name one. `None`: a holder we could not identify
        /// (a `probe --live` run, or an instance still exiting).
        holder: Option<u32>,
    },
    /// The pipe exists, accepted a connection, and then failed the protocol handshake.
    DriverNotResponding,
}

impl DisplayBackend {
    /// The stable wire name. `DriverBusy` carries no pid: `doctor --json`, the compatibility
    /// card and the startup gate compare this string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IddDriver => "IddDriver",
            Self::DriverNotInstalled => "DriverNotInstalled",
            Self::DriverBusy { .. } => "DriverBusy",
            Self::DriverNotResponding => "DriverNotResponding",
        }
    }

    /// The pid holding the driver's pipe, when busy and the holder could be named.
    pub fn holder(self) -> Option<u32> {
        match self {
            Self::DriverBusy { holder } => holder,
            _ => None,
        }
    }
}

#[cfg(test)]
fn pipe_path() -> String {
    pipe_path_for(DEFAULT_PIPE_NAME)
}

fn pipe_path_for(name: &str) -> String {
    format!(r"\\.\pipe\{name}")
}

/// `ERROR_PIPE_BUSY`: the pipe exists, and every instance of it already has a client.
const ERROR_PIPE_BUSY: i32 = 231;

/// `ERROR_SEM_TIMEOUT`: what a timed-out `WaitNamedPipe` reports. Like [`ERROR_PIPE_BUSY`],
/// occupied, not dead.
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

/// Set while this process holds a driver connection that last worked.
///
/// Saves a probe (a connect plus a 5 s handshake deadline) while this host's own
/// `create_display` may have the driver busy. Cleared when a commit finds the connection dead
/// and a reconnect fails. A slow driver (a write that timed out with nothing sent) leaves it
/// set: busy, not gone.
static CONNECTED: AtomicBool = AtomicBool::new(false);

/// [`probe_display_backend`] for a caller that must not block: the dashboard's
/// `get_display_capacity`, a sync command polled on the UI's main thread.
///
/// A fresh probe can cost a 5 s handshake deadline, so this returns the last answer and
/// refreshes it on its own thread once it is older than 3 s. Only the first ask in a process
/// probes inline.
pub fn probe_display_backend_cached() -> DisplayBackend {
    const FRESH_FOR: Duration = Duration::from_secs(3);
    static LAST: Mutex<Option<(Instant, DisplayBackend)>> = Mutex::new(None);
    static REFRESHING: AtomicBool = AtomicBool::new(false);

    if CONNECTED.load(Ordering::Acquire) {
        return DisplayBackend::IddDriver;
    }
    let last = *LAST.lock_recover();
    match last {
        Some((at, backend)) => {
            if at.elapsed() >= FRESH_FOR && !REFRESHING.swap(true, Ordering::AcqRel) {
                let spawned = std::thread::Builder::new()
                    .name("display-backend-probe".to_string())
                    .spawn(|| {
                        let fresh = probe_display_backend();
                        *LAST.lock_recover() = Some((Instant::now(), fresh));
                        REFRESHING.store(false, Ordering::Release);
                    });
                if spawned.is_err() {
                    REFRESHING.store(false, Ordering::Release);
                }
            }
            backend
        }
        None => {
            let fresh = probe_display_backend();
            *LAST.lock_recover() = Some((Instant::now(), fresh));
            fresh
        }
    }
}

/// Probe the display backend. Never cached negatively: `drivers install` flips this at runtime.
pub fn probe_display_backend() -> DisplayBackend {
    if CONNECTED.load(Ordering::Acquire) {
        return DisplayBackend::IddDriver;
    }
    match probe_pipe(DEFAULT_PIPE_NAME) {
        DisplayBackend::IddDriver => {
            CONNECTED.store(true, Ordering::Release);
            DisplayBackend::IddDriver
        }
        other => other,
    }
}

/// Classify a failed [`DriverClient::new`].
///
/// Do not test the pipe with `Path::exists()`: `std::fs::metadata` opens the file, so a pipe
/// whose every instance is taken reports not found. Only the connect error separates "never
/// installed" from "occupied" from "wedged".
///
/// An idle driver answers a state request within a millisecond, so a handshake timeout means
/// "occupied" while another of our processes is alive, and "not responding" otherwise.
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

/// The classification itself, against one pipe name, so a test can exercise the busy path on a
/// pipe of its own instead of the real driver's.
fn probe_pipe(name: &str) -> DisplayBackend {
    // `DriverClient::new` blocks on driver_ipc's own tokio runtime, which panics on a thread that
    // already belongs to one (a Tauri command's can), so it gets a plain thread
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
/// The single-instance control socket is asked first: a GUI or `serve` host answers `PING` with
/// its pid and is the only holder we can offer to quit. Our own pid is not an answer (the
/// control server found may be this process's). A holder with no control socket (`probe --live`,
/// an instance still exiting) is found as another copy of this executable.
pub fn driver_pipe_holder() -> Option<u32> {
    let me = std::process::id();
    if let Some(pid) = crate::single_instance::running_instance_pid() {
        if pid != me {
            return Some(pid);
        }
    }
    sibling_processes().into_iter().next()
}

/// The backend as the driver answers right now, ignoring this process's cached "connected".
///
/// For `drivers restart`, which has to see the restarted driver answer before it may report
/// success; [`probe_display_backend`] would return the answer it got before the restart.
pub fn probe_display_backend_fresh() -> DisplayBackend {
    probe_pipe(DEFAULT_PIPE_NAME)
}

/// How many monitors the driver itself says are in its list, asked over a fresh connection.
///
/// The list is machine-wide: it holds every virtual display any ScreenExtend host on this
/// machine has attached, which is what `drivers restart` must not take away from under one.
pub fn driver_monitor_count() -> Result<usize, String> {
    std::thread::spawn(|| {
        let client = DriverClient::new().map_err(|e| e.to_string())?;
        client.request_live_state().map(|m| m.len())
    })
    .join()
    .map_err(|_| "the driver state request panicked".to_string())?
}

/// What the driver says about itself on its versioned pipe, for `doctor`: protocol, version,
/// build and capabilities. `None` when it serves no versioned pipe or did not answer `Hello`.
/// The `Hello` declares no owner, so this connection never holds or removes a display.
pub fn driver_protocol_report() -> Option<serde_json::Value> {
    std::thread::spawn(|| {
        let hello = v2::Hello {
            client: Some(format!("ScreenExtend {} doctor", env!("CARGO_PKG_VERSION"))),
            owner: None,
        };
        let client =
            DriverClient::new_host(DEFAULT_PIPE_NAME, Some(&hello), Handshake::PROBE).ok()?;
        let n = client.negotiated();
        Some(serde_json::json!({
            "protocol": n.protocol,
            "driver": n.driver.clone()?,
            "build": n.build.clone(),
            "iddcxVersion": n.iddcx_version,
            "capabilities": n.capabilities.names(),
        }))
    })
    .join()
    .ok()
    .flatten()
}

/// Every other process running this same executable, oldest pid first.
pub(crate) fn sibling_processes() -> Vec<u32> {
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
        // limited information covers the image name, and a same-user process is granted it at any
        // integrity level
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
    /// Every monitor of ours in the driver's list, including released ones still held idle.
    owned: Vec<Monitor>,
    /// Ids this host has finished with that stay in the driver's list, idle: taking one out
    /// would break the contiguous run the driver insists on (see [`compact`]), and removing `1`
    /// from `[0, 1, 2]` detaches *every* display. Each goes once every id above it has. Default
    /// mode only: shared mode keeps the same thing machine-wide in its registry.
    released: Vec<u32>,
}

/// The device path each driver id's monitor was claimed under, so a released id can be found on
/// the desktop and a captured monitor can be matched to its id in the driver's diagnostics.
/// Recorded by [`claim_display_identity`] for the id in [`PENDING_CLAIM`].
static ID_PATHS: Mutex<Option<std::collections::HashMap<u32, String>>> = Mutex::new(None);

/// The id the last create handed out, until its join claims the monitor.
///
/// A create and its claim happen under the join's `DISPLAY_CORRELATION_LOCK` (`server.rs`), which
/// every removal takes too, so the next claim is this create's.
static PENDING_CLAIM: Mutex<Option<u32>> = Mutex::new(None);

/// Device paths of released monitors that are held in the driver's list for contiguity and have
/// been taken off the desktop ([`park_released_displays`]). Nothing activates them again
/// ([`plan_driver_paths`] skips them), and nothing claims them ([`attached_display_paths`]).
static PARKED: Mutex<Option<std::collections::HashSet<String>>> = Mutex::new(None);

fn is_parked(device_path: &str) -> bool {
    PARKED
        .lock_recover()
        .as_ref()
        .is_some_and(|set| set.contains(device_path))
}

fn any_parked() -> bool {
    PARKED
        .lock_recover()
        .as_ref()
        .is_some_and(|set| !set.is_empty())
}

#[derive(Debug)]
pub struct WindowsVirtualDisplay {
    inner: Mutex<Inner>,
    /// Serialises driver round trips, so `inner` is never held across one.
    ///
    /// A slow or wedged driver then cannot stall readers of the monitor list
    /// (`registered_refresh_rates`, the reporting harnesses). A default-mode commit is one pipe
    /// write (bounded at 2 s); a reconnect adds a connect and a handshake (5 s deadline).
    /// Shared-driver mode also holds it through its cross-process lock and confirm poll.
    commit: Mutex<()>,
    /// Shared-driver mode (`SCREENEXTEND_DATA_DIR` set): merge into the driver's machine-wide
    /// monitor list instead of replacing it. See `mod shared`.
    shared: bool,
    /// The pipe this host connected on, so a reconnect goes back to the same one.
    pipe: String,
    /// Take a released monitor that has to stay in the driver's list off the desktop
    /// (`park_released_displays`). Default mode on the real driver only, and off unless
    /// `SE_TUNE_ON=park_released_displays`: without it a held monitor stays on the desktop,
    /// idle, and no topology write is made for it.
    park: bool,
    /// Which of the versioned driver features this host may use, read once.
    link: LinkOptions,
    /// The current connection's driver accepted this host as an owner: it removes the host's
    /// displays once the host's last connection has been gone for the grace period.
    owned_by_driver: AtomicBool,
    /// Where the encoder choice lives, for the render adapter. `None` asks for nothing.
    encoder: Option<EncoderSource>,
    render: Mutex<RenderState>,
    /// The adapters a render target is picked from; DXGI's list outside tests.
    candidates: fn() -> Vec<host::AdapterCandidate>,
    /// Whether duplication owns the render preference, so the encoder-follow request stands
    /// down; `pipeline::duplication_owns_render` outside tests.
    duplication_owns: fn() -> bool,
    /// The driver has owned this host's displays on some connection: from then on a connection
    /// that does not own them (lost, or landed on the unversioned pipe) is repaired at once, for
    /// as long as the host lives.
    ownership_wanted: AtomicBool,
    /// The link watcher is running.
    watching: AtomicBool,
    /// The last reconnect found no versioned pipe at all (the driver is restarting, or was
    /// replaced by one without it): the watcher then retries slowly.
    v2_gone: AtomicBool,
    /// The host stopped: the watcher ends.
    stopped: AtomicBool,
    /// This controller, for starting the watcher from wherever ownership is first negotiated.
    this: std::sync::OnceLock<std::sync::Weak<Self>>,
    /// When the first display was created; the link-drop hook counts from it.
    first_create: Mutex<Option<Instant>>,
}

/// Developer hooks for the driver link, each honoured only with `SE_TEST_HOOKS=1`.
struct LinkHooks {
    /// `SE_DRIVER_FIRST_LINK_V1=1`: the first connection acts as if the driver served no
    /// versioned pipe, so ownership can only be negotiated on a later reconnect.
    first_link_v1: bool,
    /// `SE_DRIVER_SLOW_HELLO=<n>`: the next `n` reconnect `Hello`s are not answered in time.
    slow_hellos: std::sync::atomic::AtomicU32,
    /// `SE_DRIVER_DROP_LINK_AFTER_S=<s>[,<s>...]`: see `start_link_drop_hook`.
    drop_link_after: Option<Vec<u64>>,
}

impl LinkHooks {
    fn take_slow_hello(&self) -> bool {
        let mut n = self.slow_hellos.load(Ordering::Acquire);
        while n > 0 {
            match self.slow_hellos.compare_exchange_weak(
                n,
                n - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(now) => n = now,
            }
        }
        false
    }
}

fn hooks() -> &'static LinkHooks {
    static HOOKS: std::sync::OnceLock<LinkHooks> = std::sync::OnceLock::new();
    HOOKS.get_or_init(|| {
        use crate::windows_utils::streamer::tune::hook_var;
        LinkHooks {
            first_link_v1: hook_var("SE_DRIVER_FIRST_LINK_V1").as_deref() == Some("1"),
            slow_hellos: std::sync::atomic::AtomicU32::new(
                hook_var("SE_DRIVER_SLOW_HELLO")
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0),
            ),
            drop_link_after: hook_var("SE_DRIVER_DROP_LINK_AFTER_S")
                .map(|v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect()),
        }
    })
}

/// The real host's controller, for the capture path's requests to the driver.
static CONTROLLER: Mutex<Option<std::sync::Weak<WindowsVirtualDisplay>>> = Mutex::new(None);

impl Drop for WindowsVirtualDisplay {
    fn drop(&mut self) {
        // ends the link watcher even between two waits
        self.stopped.store(true, Ordering::Release);
    }
}

/// The host's encoder settings, shared with `AppState`, read when a display is about to be
/// created: the encoder a session will start is the one these name at that moment.
#[derive(Debug, Clone)]
pub struct EncoderSource {
    pub vendor: Arc<Mutex<crate::streamer::config::EncoderVendor>>,
    pub software: Arc<AtomicBool>,
}

impl EncoderSource {
    fn current(&self) -> crate::streamer::config::EncoderVendor {
        if self.software.load(Ordering::Relaxed) {
            crate::streamer::config::EncoderVendor::Software
        } else {
            *self.vendor.lock_recover()
        }
    }
}

/// What this host knows about the driver's render adapter preference.
#[derive(Debug, Default)]
struct RenderState {
    /// What the driver last reported; `None` until it has said.
    reported: Option<Option<v2::Luid>>,
    /// What the newest session asked for, re-sent after a reconnect.
    wanted: Option<Option<v2::Luid>>,
    /// This host set the preference the driver holds, so it clears it when it stops.
    set_here: bool,
    /// Values the driver refused; each is logged once and never sent again.
    refused: Vec<Option<v2::Luid>>,
}

enum CommitError {
    /// Refused before anything was written: a bad list. Reconnecting cannot help.
    Invalid(String),
    /// The driver is alive and did not take the write within its deadline, and nothing of it
    /// went out. The connection is still in step; replacing it would only race a second
    /// connection's list against whatever the first still has queued, and its handshake would
    /// time out against the same busy driver.
    Busy(String),
    /// The pipe is closed or out of step, or the shared-mode lock failed. Worth one reconnect.
    Pipe(String),
}

impl CommitError {
    fn into_message(self) -> String {
        match self {
            CommitError::Invalid(e) | CommitError::Busy(e) | CommitError::Pipe(e) => e,
        }
    }
}

impl WindowsVirtualDisplay {
    fn send_once(
        &self,
        mut client: DriverClient,
        monitors: &[Monitor],
    ) -> Result<Vec<Monitor>, CommitError> {
        if self.shared {
            let previous: Vec<u32> = self
                .inner
                .lock_recover()
                .owned
                .iter()
                .map(|m| m.id)
                .collect();
            return shared::commit(&mut client, &previous, monitors.to_vec())
                .map_err(CommitError::Pipe);
        }
        client
            .set_monitors(monitors)
            .map_err(|e| CommitError::Invalid(format!("set monitors: {e}")))?;
        client.notify().map_err(|e| {
            let busy = e.is_busy();
            let message = format!("notify driver: {e}");
            if busy {
                CommitError::Busy(message)
            } else {
                CommitError::Pipe(message)
            }
        })?;
        Ok(monitors.to_vec())
    }

    /// Open a fresh connection to the driver and make it the one every later commit uses.
    ///
    /// The driver's `WUDFHost.exe` can restart under a running host (a UMDF host crash, the
    /// devnode disabled and re-enabled, a driver update, `drivers install` from a terminal), and
    /// the connection made at setup is then dead for good.
    fn reconnect(&self, why: &str) -> Option<DriverClient> {
        match self.connect_fresh(Handshake::RELINK) {
            Ok(fresh) => Some(self.install_link(fresh, why)),
            Err(e) => {
                teprintln!(
                    "[virtual-display] the driver connection failed ({why}) and a reconnect \
                     failed too: {e}"
                );
                None
            }
        }
    }

    /// Open a new host connection, with the same token as before: inside the grace period the
    /// driver keeps this host's displays rather than removing them. Takes no lock.
    fn connect_fresh(&self, how: Handshake) -> Result<DriverClient, String> {
        // a driver that went away may have come back as a different devnode
        forget_driver_instances();
        let hello = host::hello_for(self.link, host::owner_token());
        DriverClient::new_host(&self.pipe, hello.as_ref(), how).map_err(|e| e.to_string())
    }

    /// Make `fresh` the connection every later commit uses. The caller holds `commit`.
    fn install_link(&self, fresh: DriverClient, why: &str) -> DriverClient {
        let server = if self.pipe == DEFAULT_PIPE_NAME {
            match super::driver_health::probe_pipe_server() {
                super::driver_health::PipeProbe::Server(pid) => format!("pid {pid}"),
                _ => "a server that would not name itself".to_string(),
            }
        } else {
            format!("pipe {}", self.pipe)
        };
        tprintln!(
            "[virtual-display] the driver connection failed ({why}); reconnected, now served by \
             {server}"
        );
        self.adopt_link(&fresh, "after a reconnect");
        // the preference lives as long as the driver's host process, which may be new
        let wanted = {
            let mut render = self.render.lock_recover();
            if render.wanted.is_some() {
                render.reported = read_preference(&fresh);
            }
            render.wanted
        };
        if let Some(want) = wanted {
            let live = self.inner.lock_recover().owned.len();
            self.apply_render(&fresh, want, live);
        }
        let copy = fresh.duplicate();
        self.inner.lock_recover().client = fresh;
        if self.park {
            unpark_after_reconnect();
        }
        copy
    }

    /// Send a monitor list to the driver without holding `inner` across the round trip.
    ///
    /// The caller must hold `commit`: that is what makes reading `owned`, computing a new list
    /// and storing it back atomic. A dead connection gets one reconnect and one retry with the
    /// whole list, which after a driver restart also re-attaches the displays this host holds. A
    /// busy driver (a write that timed out with nothing sent) keeps its connection and the error
    /// is returned: the next commit sends the whole list again.
    fn commit_unlocked(&self, monitors: Vec<Monitor>) -> Result<Vec<Monitor>, String> {
        // an independent client over the same connection: `set_monitors` mutates its own state,
        // so the guard can go before the wait
        let client = self.inner.lock_recover().client.duplicate();
        let started = Instant::now();
        let result = match self.send_once(client, &monitors) {
            Ok(done) => Ok(done),
            Err(CommitError::Invalid(e)) => Err(CommitError::Invalid(e)),
            Err(CommitError::Busy(e)) => Err(CommitError::Busy(e)),
            Err(CommitError::Pipe(first)) => match self.reconnect(&first) {
                Some(fresh) => self.send_once(fresh, &monitors).map_err(|e| match e {
                    // the fresh connection's handshake answered: the driver is only slow
                    CommitError::Busy(e) => {
                        CommitError::Busy(format!("{first}; reconnected, then: {e}"))
                    }
                    e => CommitError::Pipe(format!(
                        "{first}; and again after reconnecting: {}",
                        e.into_message()
                    )),
                }),
                None => Err(CommitError::Pipe(first)),
            },
        };
        let elapsed = started.elapsed();
        if elapsed >= SLOW_COMMIT {
            tprintln!("[virtual-display] driver commit took {elapsed:?}");
        }
        // `CONNECTED` describes the real driver; a host on a test's own pipe leaves it alone
        let real = self.pipe == DEFAULT_PIPE_NAME;
        match result {
            Ok(done) => {
                if real {
                    CONNECTED.store(true, Ordering::Release);
                }
                self.inner.lock_recover().owned = done.clone();
                Ok(done)
            }
            Err(CommitError::Pipe(e)) => {
                if real {
                    CONNECTED.store(false, Ordering::Release);
                }
                Err(e)
            }
            Err(CommitError::Busy(e)) => {
                teprintln!(
                    "[virtual-display] the driver is not draining its pipe; keeping the \
                     connection, and the next commit re-sends the whole list: {e}"
                );
                Err(e)
            }
            Err(CommitError::Invalid(e)) => Err(e),
        }
    }
}

impl WindowsVirtualDisplay {
    /// Connect to the driver, clearing any monitors a previous run left behind.
    /// The most common failure is a driver that was never installed.
    pub fn new_shared_reported() -> Result<SharedVirtualDisplay, String> {
        Self::new_shared_following(None)
    }

    /// [`new_shared_reported`](Self::new_shared_reported), with the encoder settings the render
    /// adapter follows.
    pub fn new_shared_following(
        encoder: Option<EncoderSource>,
    ) -> Result<SharedVirtualDisplay, String> {
        let display = Arc::new(Self::connect_with(
            DEFAULT_PIPE_NAME,
            crate::data_dir::isolated(),
            LinkOptions::from_switches(),
            encoder,
        )?);
        Self::watch_link(&display);
        *CONTROLLER.lock_recover() = Some(Arc::downgrade(&display));
        Ok(display)
    }

    #[cfg(test)]
    fn connect_reported(pipe: &str, shared: bool) -> Result<Self, String> {
        Self::connect_with(pipe, shared, LinkOptions::from_switches(), None)
    }

    fn connect_with(
        pipe: &str,
        shared: bool,
        link: LinkOptions,
        encoder: Option<EncoderSource>,
    ) -> Result<Self, String> {
        let pipe_path = pipe_path_for(pipe);
        let hello = host::hello_for(link, host::owner_token());
        let first = Handshake {
            skip_v2: hooks().first_link_v1,
            ..Handshake::FIRST
        };
        let mut client = DriverClient::new_host(pipe, hello.as_ref(), first).map_err(|e| {
            if matches!(classify_init_error(&e), DisplayBackend::DriverBusy { .. }) {
                // not a fault: another process has the driver (mid-attach, or holding every pipe
                // instance), so the fix is to close it, never to reinstall
                match driver_pipe_holder() {
                    Some(pid) => format!(
                        "the display driver is installed and healthy, but another ScreenExtend \
                         process (pid {pid}) is already using it. Quit that instance and try again."
                    ),
                    None => "the display driver is installed and healthy, but another \
                             ScreenExtend process is already using it. Quit it and try again."
                        .to_string(),
                }
            } else if std::path::Path::new(&pipe_path).exists() {
                format!(
                    "the display driver did not answer on {pipe_path} ({e}); it may be starting, stopped, or wedged"
                )
            } else {
                format!(
                    "the ScreenExtend virtual display driver is not installed ({pipe_path} does not exist). \
                     Install it with `ScreenExtend drivers install` (needs Administrator). Underlying error: {e}"
                )
            }
        })?;
        if pipe == DEFAULT_PIPE_NAME {
            CONNECTED.store(true, Ordering::Release);
        }
        forget_driver_instances();
        let park = !shared
            && pipe == DEFAULT_PIPE_NAME
            && crate::windows_utils::streamer::tune::candidate("park_released_displays");
        let display = Self {
            inner: Mutex::new(Inner {
                client: client.duplicate(),
                owned: Vec::new(),
                released: Vec::new(),
            }),
            commit: Mutex::new(()),
            shared,
            pipe: pipe.to_string(),
            park,
            link,
            owned_by_driver: AtomicBool::new(false),
            encoder,
            render: Mutex::new(RenderState::default()),
            candidates: render_candidates,
            // tests set it themselves: the real rule reads process-wide state other tests change
            duplication_owns: if cfg!(test) {
                || false
            } else {
                crate::windows_utils::streamer::pipeline::duplication_owns_render
            },
            ownership_wanted: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            v2_gone: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            this: std::sync::OnceLock::new(),
            first_create: Mutex::new(None),
        };
        display.adopt_link(&client, "at startup");
        if link.render_follows_encoder {
            display.describe_render_adapter(&client);
        }
        display.clear_stale_preference(&client);
        if shared {
            // other hosts' monitors live in the same driver list: never clear it, reclaim only what
            // exited hosts left behind
            if let Err(e) = shared::commit(&mut client, &[], Vec::new()) {
                teprintln!("[virtual-display] shared driver: startup reclaim failed: {e}");
            }
        } else {
            startup_clear(&mut client);
        }
        Ok(display)
    }

    /// Note what a (new) connection agreed with the driver, and say it once.
    fn adopt_link(&self, client: &DriverClient, when: &str) {
        let negotiated = client.negotiated();
        let owned = self.link.owned_monitors && negotiated.owns_monitors();
        self.owned_by_driver.store(owned, Ordering::Release);
        if owned {
            self.ownership_wanted.store(true, Ordering::Release);
            self.ensure_watcher();
        }
        if self.link.owned_monitors {
            match (&negotiated.owner, owned) {
                (Some(owner), true) => {
                    tprintln!(
                        "[virtual-display] owned monitors {when}: the driver removes this host's \
                         displays {} ms after its last connection closes (owner {}…){}",
                        owner.grace_ms,
                        owner.token_prefix,
                        if owner.monitors.is_empty() {
                            String::new()
                        } else {
                            format!("; it still holds {:?} for this host", owner.monitors)
                        }
                    );
                }
                _ => tprintln!(
                    "[virtual-display] owned monitors unavailable {when} ({}): a host that dies \
                     leaves its displays attached until the next host clears them",
                    if negotiated.driver.is_none() {
                        "no versioned Hello was answered"
                    } else if !negotiated.has(v2::CAP_OWNED_MONITORS) {
                        "the driver does not offer owned_monitors"
                    } else {
                        "the driver did not accept the owner"
                    }
                ),
            }
        }
    }

    /// Remember this controller and start the link watcher if the driver already owns its
    /// displays. Ownership first negotiated later, on a reconnect, starts it from `adopt_link`.
    fn watch_link(display: &Arc<Self>) {
        let _ = display.this.set(Arc::downgrade(display));
        if display.owned_by_driver.load(Ordering::Acquire) {
            display.ensure_watcher();
        }
        if let Some(times) = hooks().drop_link_after.clone() {
            Self::start_link_drop_hook(display, times);
        }
    }

    /// Start the link watcher, once. It reconnects when the connection dies, and keeps
    /// reconnecting while the connection does not own this host's displays, so the driver sees
    /// the token again inside the grace period instead of removing displays that are still
    /// streaming. A host the driver never owned has no watcher: the next commit reconnects.
    fn ensure_watcher(&self) {
        let Some(weak) = self.this.get().cloned() else {
            return;
        };
        if self.watching.swap(true, Ordering::AcqRel) {
            return;
        }
        let spawned = std::thread::Builder::new()
            .name("driver-link".to_string())
            .spawn(move || {
                let mut attempt = 0u32;
                loop {
                    let Some(display) = weak.upgrade() else {
                        return;
                    };
                    if display.stopped.load(Ordering::Acquire) {
                        return;
                    }
                    if display.link_needs_repair() {
                        let done = display.relink_attempt(attempt);
                        let slow = display.v2_gone.load(Ordering::Acquire);
                        drop(display);
                        if done {
                            attempt = 0;
                        } else {
                            attempt += 1;
                            let pause = if slow {
                                host::RELINK_NO_V2_PAUSE
                            } else {
                                host::relink_backoff(attempt)
                            };
                            std::thread::sleep(pause);
                        }
                        continue;
                    }
                    attempt = 0;
                    let mut watch = display.inner.lock_recover().client.closed_watch();
                    drop(display);
                    // bounded, so a stopped or dropped controller ends this thread
                    DriverClient::wait_closed(&mut watch, Duration::from_secs(1));
                }
            });
        if let Err(e) = spawned {
            self.watching.store(false, Ordering::Release);
            teprintln!("[virtual-display] could not start the driver link watcher: {e}");
        }
    }

    /// The connection is dead, or it does not own this host's displays while the driver has
    /// owned them before.
    fn link_needs_repair(&self) -> bool {
        self.inner.lock_recover().client.is_dead()
            || (self.ownership_wanted.load(Ordering::Acquire)
                && !self.owned_by_driver.load(Ordering::Acquire))
    }

    /// One reconnect attempt, made without the commit lock; the lock is taken only to put the
    /// new connection in and re-send the list. `true` when nothing is left to repair.
    fn relink_attempt(&self, attempt: u32) -> bool {
        let mut how = Handshake::RELINK;
        if hooks().take_slow_hello() {
            // a Hello not answered in time, to exercise the path that lands elsewhere
            how.unanswered = true;
        }
        let fresh = match self.connect_fresh(how) {
            Ok(fresh) => fresh,
            Err(e) => {
                if attempt == 0 {
                    teprintln!(
                        "[virtual-display] reconnecting to the driver failed: {e}; retrying"
                    );
                }
                return false;
            }
        };
        let negotiated = fresh.negotiated().clone();
        let owns = self.link.owned_monitors && negotiated.owns_monitors();
        if negotiated.driver.is_some() && !negotiated.has(v2::CAP_OWNED_MONITORS) {
            // the driver answered and does not offer ownership: retrying cannot bring it back
            if self.ownership_wanted.swap(false, Ordering::AcqRel) {
                tprintln!(
                    "[virtual-display] the driver no longer offers owned_monitors; this host's \
                     displays are no longer removed by the driver if it dies"
                );
            }
        }
        if negotiated.v2_absent {
            if !self.v2_gone.swap(true, Ordering::AcqRel) {
                tprintln!(
                    "[virtual-display] the driver serves no versioned pipe now; retrying every {} \
                     s while this host's displays are not owned",
                    host::RELINK_NO_V2_PAUSE.as_secs()
                );
            }
        } else {
            self.v2_gone.store(false, Ordering::Release);
        }
        let _commit = self.commit.lock_recover();
        let dead = self.inner.lock_recover().client.is_dead();
        if !dead && !self.link_needs_repair() {
            // a commit's own reconnect got there first
            return true;
        }
        if owns || dead {
            self.install_link(fresh, "the connection to the driver was lost");
            let owned = self.inner.lock_recover().owned.clone();
            if let Err(e) = self.commit_unlocked(owned) {
                teprintln!("[virtual-display] re-sending the display list failed: {e}");
            }
        }
        if owns || !self.ownership_wanted.load(Ordering::Acquire) {
            return true;
        }
        if attempt == 0 {
            teprintln!(
                "[virtual-display] the new driver connection does not own this host's displays; \
                 trying the versioned pipe again before the driver's {} ms grace runs out",
                host::OWNER_GRACE_MS
            );
        }
        false
    }

    /// A developer hook (`SE_DRIVER_DROP_LINK_AFTER_S=<s>[,<s>...]`, with `SE_TEST_HOOKS=1`):
    /// at each time after the first display is created, drop the driver connection the way a
    /// lost pipe does, while the driver stays up. A plain connection with no `Hello` takes its
    /// place, so the driver sees this host's token go and arms the grace. Where no watcher runs
    /// yet, the reconnect a commit would make after a pipe error follows at once.
    fn start_link_drop_hook(display: &Arc<Self>, times: Vec<u64>) {
        let weak = Arc::downgrade(display);
        let _ = std::thread::Builder::new()
            .name("driver-link-drop".to_string())
            .spawn(move || {
                let started = loop {
                    let Some(display) = weak.upgrade() else {
                        return;
                    };
                    if let Some(at) = *display.first_create.lock_recover() {
                        break at;
                    }
                    drop(display);
                    std::thread::sleep(Duration::from_millis(200));
                };
                for at in times {
                    let due = started + Duration::from_secs(at);
                    if let Some(wait) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(wait);
                    }
                    let Some(display) = weak.upgrade() else {
                        return;
                    };
                    display.simulate_link_loss();
                }
            });
    }

    fn simulate_link_loss(&self) {
        let _commit = self.commit.lock_recover();
        let plain = match DriverClient::new_host(&self.pipe, None, Handshake::FIRST) {
            Ok(c) => c,
            Err(e) => {
                teprintln!("[virtual-display] link-drop hook: no plain connection ({e})");
                return;
            }
        };
        tprintln!(
            "[virtual-display] link-drop hook: dropping the driver connection (the driver stays \
             up)"
        );
        self.adopt_link(&plain, "after the link-drop hook");
        self.inner.lock_recover().client = plain;
        if self.watching.load(Ordering::Acquire) {
            return;
        }
        // no watcher: do what the next commit would after a pipe error
        match self.connect_fresh(Handshake::RELINK) {
            Ok(fresh) => {
                self.install_link(fresh, "the link-drop hook");
                let owned = self.inner.lock_recover().owned.clone();
                if let Err(e) = self.commit_unlocked(owned) {
                    teprintln!("[virtual-display] re-sending the display list failed: {e}");
                }
            }
            Err(e) => teprintln!("[virtual-display] link-drop hook: reconnect failed: {e}"),
        }
    }

    /// Say where the driver composes at startup, and learn its current preference.
    fn describe_render_adapter(&self, client: &DriverClient) {
        let negotiated = client.negotiated();
        if !negotiated.has(v2::CAP_RENDER_ADAPTER) {
            tprintln!(
                "[virtual-display] render adapter: the driver does not offer render_adapter; \
                 Windows chooses where the virtual displays are composed"
            );
            return;
        }
        let reported = read_preference(client);
        self.render.lock_recover().reported = reported;
        tprintln!(
            "[virtual-display] render adapter: following the encoder; the driver's preference is \
             {}",
            match reported {
                Some(Some(luid)) => adapter_label(luid),
                Some(None) => "none (Windows chooses)".to_string(),
                None => "not reported".to_string(),
            }
        );
    }

    /// Before a display is created: point the driver at the adapter the session will need.
    /// With DXGI duplication as the capture, that is the adapter DXGI lists outputs under, since
    /// duplication delivers nothing from any other; otherwise, with the render switch on, the
    /// adapter the session's encoder is expected to run on. `live` is how many of this host's
    /// displays exist now.
    fn choose_render_adapter(&self, live: usize) {
        let client = self.inner.lock_recover().client.duplicate();
        if !client.negotiated().has(v2::CAP_RENDER_ADAPTER) {
            return;
        }
        let dxgi = (self.duplication_owns)();
        let listing = if dxgi { dxgi_listing_adapter() } else { None };
        let encoder_target = match self.encoder.as_ref().map(EncoderSource::current) {
            Some(vendor) if self.link.render_follows_encoder => {
                let candidates = (self.candidates)();
                host::render_target(vendor, &candidates).map(|a| a.luid)
            }
            _ => None,
        };
        let Some(want) = host::render_want(
            dxgi,
            listing,
            self.link.render_follows_encoder,
            encoder_target,
        ) else {
            return;
        };
        if dxgi {
            tprintln!(
                "[virtual-display] render adapter: the capture is DXGI duplication, which only \
                 delivers from the adapter DXGI lists the output under; asking for {}",
                want.map_or("no preference".to_string(), adapter_label)
            );
        }
        self.render.lock_recover().wanted = Some(want);
        self.apply_render(&client, want, live);
    }

    /// The encoder a session actually started runs on `adapter` (`None`: the CPU). With the
    /// render switch on, the preference follows it when it differs from the one predicted before
    /// the display existed (`auto` can end on another vendor or on x264). Never while DXGI
    /// duplication owns the preference, which needs the listing adapter whatever encodes; asked
    /// again under the commit lock, right before the request.
    fn encoder_started(&self, adapter: Option<v2::Luid>) {
        if !self.link.render_follows_encoder || (self.duplication_owns)() {
            return;
        }
        let _commit = self.commit.lock_recover();
        if (self.duplication_owns)() {
            return;
        }
        let predicted = self.render.lock_recover().wanted;
        if predicted == Some(adapter) {
            return;
        }
        let client = self.inner.lock_recover().client.duplicate();
        if !client.negotiated().has(v2::CAP_RENDER_ADAPTER) {
            return;
        }
        tprintln!(
            "[virtual-display] render adapter: the encoder that started runs on {}, not on the \
             adapter predicted from the setting; following it",
            adapter.map_or("the CPU".to_string(), adapter_label)
        );
        self.render.lock_recover().wanted = Some(adapter);
        let live = self.inner.lock_recover().owned.len();
        self.apply_render(&client, adapter, live);
    }

    /// At start-up: a preference left in the driver by a host that is gone (one that crashed
    /// cannot clear its own) is cleared, whatever the render switch says, so Windows chooses
    /// again. In shared mode only while no other host's display is listed.
    fn clear_stale_preference(&self, client: &DriverClient) {
        let n = client.negotiated();
        if !n.has(v2::CAP_RENDER_ADAPTER) || !n.has(v2::CAP_DIAGNOSTICS) {
            return;
        }
        let reported = read_preference(client);
        let Some(Some(stale)) = reported else {
            self.render.lock_recover().reported = reported;
            return;
        };
        if self.shared {
            match client.request_live_state() {
                Ok(list) if list.is_empty() => {}
                _ => {
                    self.render.lock_recover().reported = reported;
                    return;
                }
            }
        }
        match client.set_render_adapter(None) {
            Ok(reply) => {
                self.render.lock_recover().reported = Some(reply.preferred);
                tprintln!(
                    "[virtual-display] render adapter: cleared a preference for {} left by an \
                     earlier host",
                    adapter_label(stale)
                );
            }
            Err(e) => teprintln!("[virtual-display] render adapter: a stale preference stays: {e}"),
        }
    }

    /// Send what [`host::render_step`] decides, at most twice: a refused adapter becomes "no
    /// preference", which may itself need sending.
    fn apply_render(&self, client: &DriverClient, want: Option<v2::Luid>, live: usize) {
        for _ in 0..2 {
            let (step, reported) = {
                let render = self.render.lock_recover();
                (
                    host::render_step(want, render.reported, &render.refused),
                    render.reported,
                )
            };
            let host::RenderStep::Send(luid) = step else {
                return;
            };
            let describe = |l: Option<v2::Luid>| match l {
                Some(l) => adapter_label(l),
                None => "no preference (Windows chooses)".to_string(),
            };
            if live > 0 {
                tprintln!(
                    "[virtual-display] render adapter: {} -> {} with {live} display(s) of this \
                     host live; Windows re-creates their swap chains, and the newest session's \
                     encoder wins",
                    reported.map_or("unknown".to_string(), describe),
                    describe(luid)
                );
            }
            match client.set_render_adapter(luid) {
                Ok(reply) => {
                    let mut render = self.render.lock_recover();
                    render.reported = Some(reply.preferred);
                    render.set_here = reply.preferred.is_some();
                    drop(render);
                    tprintln!(
                        "[virtual-display] render adapter: {}",
                        match (reply.preferred, reply.description) {
                            (Some(l), Some(d)) => format!("composing on {d} ({l})"),
                            (Some(l), None) => format!("composing on {}", adapter_label(l)),
                            (None, _) => "preference cleared; Windows chooses".to_string(),
                        }
                    );
                    return;
                }
                Err(e) if e.code().is_some() => {
                    self.render.lock_recover().refused.push(luid);
                    teprintln!(
                        "[virtual-display] render adapter: the driver refused {}: {e}; treating \
                         it as no preference",
                        describe(luid)
                    );
                }
                Err(e) => {
                    teprintln!("[virtual-display] render adapter: the request failed: {e}");
                    return;
                }
            }
        }
    }

    /// The host is stopping: take back a preference it set, so the next host starts from
    /// Windows' choice. Left alone while another host's displays remain on the driver.
    fn clear_render_on_stop(&self) {
        let set_here = {
            let render = self.render.lock_recover();
            render.set_here && render.reported != Some(None)
        };
        if !set_here {
            return;
        }
        let client = self.inner.lock_recover().client.duplicate();
        if self.shared {
            match client.request_live_state() {
                Ok(list) if list.is_empty() => {}
                Ok(list) => {
                    tprintln!(
                        "[virtual-display] render adapter: leaving the preference; other hosts' \
                         displays {:?} are still on the driver",
                        list.iter().map(|m| m.id).collect::<Vec<_>>()
                    );
                    return;
                }
                Err(e) => {
                    teprintln!("[virtual-display] render adapter: not cleared ({e})");
                    return;
                }
            }
        }
        match client.set_render_adapter(None) {
            Ok(reply) => {
                let mut render = self.render.lock_recover();
                render.reported = Some(reply.preferred);
                render.set_here = false;
                drop(render);
                tprintln!("[virtual-display] render adapter: preference cleared as the host stops");
            }
            Err(e) => teprintln!("[virtual-display] render adapter: clearing it failed: {e}"),
        }
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
        let inner = self.inner.lock_recover();
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
/// `gdi_name` is the `\\.\DISPLAYn` every other Win32 display call wants; `friendly` is the
/// monitor's EDID name. Ours report the bundled driver's `VirtuDisplay+`, so it identifies
/// nothing and is only ever logged.
///
/// `device_path` is the identity. A GDI name is a *slot*: Windows hands `\\.\DISPLAY3` back out
/// the moment the monitor holding it detaches, so a cached name can address a different monitor
/// (a physical one included) by the time a reconfigure writes to it. The device-interface path
/// does not move, so every write re-derives the name from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayPath {
    pub gdi_name: String,
    pub friendly: String,
    /// `\\?\DISPLAY#<hardware id>#...`, stable for as long as the monitor exists.
    pub device_path: String,
    /// The device-interface path of the adapter the target is on. Ours is the adapter of our
    /// own driver's devnode ([`on_driver_adapter`]); nothing the user owns is on it.
    pub target_adapter: String,
    /// The pair `DisplayConfigSetDeviceInfo` addresses a per-display DPI write by.
    pub adapter_id: (u32, i32),
    pub source_id: u32,
}

/// Every monitor this process created, by device path.
///
/// This is the ownership test, consulted with the adapter before every write. Output technology
/// cannot serve: the bundled IDD monitor reports `DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI` (5),
/// exactly what a real HDMI monitor reports. What separates ours is that we created it and
/// watched it appear on our own driver's adapter.
///
/// Keyed by the claiming display id as well: Windows hands a device path one display has just
/// released to the next display created, so a removal must take only its own claim with it.
static OWNED_PATHS: Mutex<Option<OwnedPaths>> = Mutex::new(None);

type OwnedPaths = std::collections::HashMap<String, std::collections::BTreeSet<u32>>;

/// The claim of a monitor whose create named no driver id (none pending at the claim).
const UNKNOWN_ID: u32 = u32::MAX;

fn owned_paths() -> std::sync::MutexGuard<'static, Option<OwnedPaths>> {
    OWNED_PATHS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Record a monitor as display `id`'s. Called once, with the path read back after it attached.
pub fn remember_owned_path(device_path: &str, id: u32) {
    if device_path.is_empty() {
        return;
    }
    owned_paths()
        .get_or_insert_with(Default::default)
        .entry(device_path.to_string())
        .or_default()
        .insert(id);
}

/// Forget display `display_id`'s claim on `device_path`, when that display is removed, so a write
/// can no longer reach it. Another display's claim on the same path stays.
pub fn forget_display_identity(display_id: u32, device_path: &str) {
    let mut owned = owned_paths();
    let Some(map) = owned.as_mut() else {
        return;
    };
    if let Some(ids) = map.get_mut(device_path) {
        ids.remove(&display_id);
        ids.remove(&UNKNOWN_ID);
        if ids.is_empty() {
            map.remove(device_path);
        }
    }
}

pub(crate) fn is_owned_path(device_path: &str) -> bool {
    owned_paths()
        .as_ref()
        .is_some_and(|map| map.get(device_path).is_some_and(|ids| !ids.is_empty()))
}

/// A resolved, verified write target: this monitor exists, it is one of ours, and these are the
/// handles to address it by *right now*.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDisplay {
    pub gdi_name: String,
    pub adapter_id: (u32, i32),
    pub source_id: u32,
}

/// Find the one active path a write to `target` may land on, or refuse.
///
/// Pure, so the rule is testable against fabricated input; the live version is
/// [`resolve_owned_display`], the only thing `set_display_mode_owned` and
/// `set_display_scale_owned` may write through.
///
/// The rules, in order:
/// * whatever the caller names must be a display **this host created**: nothing about how
///   Windows describes a display separates ours from the user's;
/// * a recorded device path must name exactly one attached monitor, or the write is refused. A
///   detached display has no name to fall back to: the name it held has already been handed to
///   whatever attached next;
/// * with no recorded path (a display created before one was read back, or a probe harness
///   calling by name) the GDI name is matched, and the monitor it lands on must still be in
///   the owned set;
/// * whatever it lands on must be on our own driver's adapter (`on_driver`). The owned set can
///   only hold such paths, so this is a second check of the same thing.
pub fn pick_owned_display(
    paths: &[DisplayPath],
    target: &DisplayTarget,
    owned: &dyn Fn(&str) -> bool,
    on_driver: &dyn Fn(&DisplayPath) -> bool,
) -> anyhow::Result<ResolvedDisplay> {
    let by_path = !target.device_path.is_empty();
    if by_path && !owned(&target.device_path) {
        bail!(
            "{} was not created by this host; refusing to change its mode or its DPI",
            target.device_path
        );
    }
    let mut matches = paths.iter().filter(|p| {
        if by_path {
            p.device_path == target.device_path
        } else {
            !target.device_name.is_empty() && p.gdi_name == target.device_name
        }
    });
    let Some(hit) = matches.next() else {
        if by_path {
            bail!(
                "virtual display {} is not attached (it last answered to {}); refusing to write to any display",
                target.device_path,
                target.device_name
            );
        }
        bail!(
            "no active display is named {}; refusing to write to any display",
            target.device_name
        );
    };
    if matches.next().is_some() {
        bail!(
            "more than one active display answers to {}; refusing to write to any of them",
            if by_path {
                &target.device_path
            } else {
                &target.device_name
            }
        );
    }
    if !owned(&hit.device_path) {
        bail!(
            "{} is {}, which this host did not create; refusing to change its mode or its DPI",
            target.device_name,
            if hit.friendly.is_empty() {
                hit.device_path.as_str()
            } else {
                hit.friendly.as_str()
            }
        );
    }
    if !on_driver(hit) {
        bail!(
            "{} ({}) is not on the ScreenExtend display driver's adapter; refusing to change its \
             mode or its DPI",
            hit.gdi_name,
            hit.device_path
        );
    }
    if by_path && hit.gdi_name != target.device_name && !target.device_name.is_empty() {
        tprintln!(
            "[display] {} now answers to {} (it was {}); writing to the monitor, not the name",
            target.device_path,
            hit.gdi_name,
            target.device_name
        );
    }
    Ok(ResolvedDisplay {
        gdi_name: hit.gdi_name.clone(),
        adapter_id: hit.adapter_id,
        source_id: hit.source_id,
    })
}

/// [`pick_owned_display`] against the live `QueryDisplayConfig` state, the live owned set and our
/// driver's adapter.
pub fn resolve_owned_display(target: &DisplayTarget) -> anyhow::Result<ResolvedDisplay> {
    let instances = driver_instances().map_err(|e| {
        anyhow::anyhow!(
            "cannot identify the display driver's adapter ({e}); refusing to write to any display"
        )
    })?;
    pick_owned_display(&active_display_paths(), target, &is_owned_path, &|p| {
        on_driver_adapter(&p.target_adapter, &instances)
    })
}

/// The device-interface path of the monitor answering to `gdi_name`, if it is provably the one
/// this create produced, or why it is not.
///
/// Three things must all hold. A mis-correlated join that recorded somebody else's monitor as
/// ours would make every later ownership check agree with the mistake:
/// * exactly one active path answers to the name (two is a duplicated source, and a write to it
///   would land on both);
/// * its monitor was not attached before the create: `before` holds every target, active or
///   not, and a snapshot that failed holds [`SNAPSHOT_FAILED`], which refuses outright;
/// * it is on our own driver's adapter. A monitor hot-plugged inside the attach window is new,
///   and is still not ours.
pub fn pick_new_identity(
    paths: &[DisplayPath],
    gdi_name: &str,
    before: &[String],
    driver_instances: &[String],
) -> Result<String, String> {
    if before.iter().any(|b| b == SNAPSHOT_FAILED) {
        return Err(
            "the snapshot taken before the create failed, so a new display cannot be \
                    told apart from one that was already there"
                .to_string(),
        );
    }
    let mut named = paths.iter().filter(|p| p.gdi_name == gdi_name);
    let Some(hit) = named.next() else {
        return Err(format!("no active display answers to {gdi_name}"));
    };
    if named.next().is_some() {
        return Err(format!(
            "more than one display answers to {gdi_name} (a duplicated desktop)"
        ));
    }
    if hit.device_path.is_empty() {
        return Err(format!(
            "Windows could not describe the monitor on {gdi_name}"
        ));
    }
    if before.iter().any(|b| b == &hit.device_path) {
        return Err(format!(
            "{} was already attached before the create",
            hit.device_path
        ));
    }
    if !on_driver_adapter(&hit.target_adapter, driver_instances) {
        return Err(format!(
            "{} is on {:?}, not on the ScreenExtend display driver's adapter",
            hit.device_path, hit.target_adapter
        ));
    }
    Ok(hit.device_path.clone())
}

/// Read back the identity of a display that has just attached and record it as ours.
///
/// `None`, with the reason in the log, when it cannot be shown to be the display this create
/// produced: the caller must not drive that display.
pub fn claim_display_identity(gdi_name: &str, before: &[String]) -> Option<String> {
    let instances = match driver_instances() {
        Ok(ids) => ids,
        Err(e) => {
            teprintln!(
                "[display] refusing to claim {gdi_name}: cannot identify the display driver's \
                 adapter ({e})"
            );
            return None;
        }
    };
    match pick_new_identity(&active_display_paths(), gdi_name, before, &instances) {
        Ok(path) => {
            let pending = *PENDING_CLAIM.lock_recover();
            remember_owned_path(&path, pending.unwrap_or(UNKNOWN_ID));
            if let Some(id) = PENDING_CLAIM.lock_recover().take() {
                ID_PATHS
                    .lock_recover()
                    .get_or_insert_with(Default::default)
                    .insert(id, path.clone());
            }
            // a monitor arriving makes Windows apply a topology for the new set, which can bring a
            // parked one back onto the desktop
            if any_parked() {
                park_released_displays("a new display attached");
            }
            Some(path)
        }
        Err(why) => {
            teprintln!("[display] refusing to claim {gdi_name} as this host's display: {why}");
            None
        }
    }
}

/// Write every attached display's identity, mode and DPI to the log, so a report that a
/// display's mode or zoom changed can be checked against what was there before the host
/// created anything. Costs one `QueryDisplayConfig`.
pub fn log_display_inventory(when: &str) {
    use crate::windows_utils::streamer::capture::{dpi_percent_at, read_dpi_band};
    let paths = active_display_paths();
    tprintln!("[display] {when}: {} attached display(s)", paths.len());
    for p in &paths {
        let mode = current_display_mode(&p.gdi_name)
            .map(|(w, h, hz)| format!("{w}x{h}@{hz}"))
            .unwrap_or_else(|| "?".to_string());
        let dpi = match read_dpi_band(p.adapter_id, p.source_id) {
            Some((cur, lo, hi)) => format!(
                "{}% (band {}%..{}%)",
                dpi_percent_at(cur),
                dpi_percent_at(lo),
                dpi_percent_at(hi)
            ),
            None => "?".to_string(),
        };
        tprintln!(
            "[display]   {} {mode} dpi={dpi} friendly={:?} ours={} path={}",
            p.gdi_name,
            p.friendly,
            is_owned_path(&p.device_path),
            p.device_path
        );
    }
}

/// What [`attached_display_paths`] returns when it could not read the topology at all.
///
/// An empty before-set would make every monitor look new and void the check that stops a join
/// claiming somebody else's monitor. The snapshot passes through `server.rs` as a plain list,
/// so the failure travels inside it as a value no device path can equal, and
/// [`pick_new_identity`] refuses on it.
pub const SNAPSHOT_FAILED: &str = "<the display snapshot taken before this create failed>";

/// Every monitor's device path, active or not, except a departed one of our own. Snapshotted
/// before a create so the identity read back afterwards can be checked against it.
///
/// Inactive targets are in it on purpose: a monitor the user switched off ("show only on 1", a
/// TV left connected but disabled) would otherwise look brand new if anything brought it up
/// during the attach window, and be claimed as ours.
///
/// Left out ([`in_before_set`]): a target on our own driver's adapter that is neither active
/// nor connected. The adapter can keep listing a departed target under its last device path,
/// and a client that rejoins under the same id brings the same monitor back under that path,
/// which must not read as "already attached before the create".
pub fn attached_display_paths() -> Vec<String> {
    match all_target_device_paths() {
        Ok(paths) => paths,
        Err(e) => {
            teprintln!(
                "[display] could not read the displays before a create ({e}); this join will \
                 refuse to claim whatever attaches rather than guess"
            );
            vec![SNAPSHOT_FAILED.to_string()]
        }
    }
}

fn all_target_device_paths() -> Result<Vec<String>, String> {
    use windows::Win32::Devices::Display::QDC_ALL_PATHS;
    let (paths, _) = query_display_config(QDC_ALL_PATHS)?;
    // without the adapter nothing can be called ours, so every target goes in: the strictest
    // snapshot, and the claim refuses on the adapter anyway
    let instances = driver_instances().unwrap_or_default();
    let mut adapters = AdapterNames::default();
    // a target is listed once per source it could take; it is active if any of them is
    let mut active_targets = std::collections::HashSet::new();
    for path in &paths {
        if path.flags & PATH_ACTIVE != 0 {
            active_targets.insert(path_key(path.targetInfo.adapterId, path.targetInfo.id));
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for path in &paths {
        let ours = on_driver_adapter(&adapters.get(path.targetInfo.adapterId), &instances);
        let active =
            active_targets.contains(&path_key(path.targetInfo.adapterId, path.targetInfo.id));
        if !in_before_set(active, path.targetInfo.targetAvailable.as_bool(), ours) {
            continue;
        }
        let key = (
            path.targetInfo.adapterId.LowPart,
            path.targetInfo.adapterId.HighPart,
            path.targetInfo.id,
        );
        if !seen.insert(key) {
            continue;
        }
        if let Some((_, device_path)) = target_name(path.targetInfo.adapterId, path.targetInfo.id) {
            if !device_path.is_empty() && !out.contains(&device_path) {
                out.push(device_path);
            }
        }
    }
    Ok(out)
}

/// Whether a target belongs in the before-create snapshot: everything except one of ours that
/// has departed (neither active nor connected). See [`attached_display_paths`].
pub fn in_before_set(active: bool, available: bool, ours: bool) -> bool {
    active || available || !ours
}

pub(crate) fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

type QueriedConfig = (
    Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO>,
    Vec<windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO>,
);

/// `QueryDisplayConfig`, with the retry Microsoft documents for it.
///
/// The topology can change between sizing the buffers and filling them (a monitor attaching or
/// departing, which is exactly when this module asks), and the call then returns
/// `ERROR_INSUFFICIENT_BUFFER`.
fn query_display_config(
    flags: windows::Win32::Devices::Display::QUERY_DISPLAY_CONFIG_FLAGS,
) -> Result<QueriedConfig, String> {
    use windows::Win32::Devices::Display::{
        GetDisplayConfigBufferSizes, QueryDisplayConfig, DISPLAYCONFIG_MODE_INFO,
        DISPLAYCONFIG_PATH_INFO,
    };
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};

    for _ in 0..5 {
        let mut n_path = 0u32;
        let mut n_mode = 0u32;
        let sized = unsafe { GetDisplayConfigBufferSizes(flags, &mut n_path, &mut n_mode) };
        if sized != ERROR_SUCCESS {
            return Err(format!(
                "GetDisplayConfigBufferSizes failed (win32 error {})",
                sized.0
            ));
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); n_path as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); n_mode as usize];
        let queried = unsafe {
            QueryDisplayConfig(
                flags,
                &mut n_path,
                paths.as_mut_ptr(),
                &mut n_mode,
                modes.as_mut_ptr(),
                None,
            )
        };
        if queried == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        if queried != ERROR_SUCCESS {
            return Err(format!(
                "QueryDisplayConfig failed (win32 error {})",
                queried.0
            ));
        }
        paths.truncate(n_path as usize);
        modes.truncate(n_mode as usize);
        return Ok((paths, modes));
    }
    Err("the display topology kept changing under QueryDisplayConfig".to_string())
}

/// A target's EDID friendly name and its device-interface path.
pub(crate) fn target_name(
    adapter: windows::Win32::Foundation::LUID,
    id: u32,
) -> Option<(String, String)> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: adapter,
            id,
        },
        ..Default::default()
    };
    (unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == ERROR_SUCCESS.0 as i32).then(
        || {
            (
                wide_to_string(&target.monitorFriendlyDeviceName),
                wide_to_string(&target.monitorDevicePath),
            )
        },
    )
}

/// An adapter's device-interface path, e.g. `\\?\ROOT#DISPLAY#0000#{5b45201d-...}`.
fn adapter_name(adapter: windows::Win32::Foundation::LUID) -> Option<String> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_ADAPTER_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;
    let mut name = DISPLAYCONFIG_ADAPTER_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
            adapterId: adapter,
            id: 0,
        },
        ..Default::default()
    };
    (unsafe { DisplayConfigGetDeviceInfo(&mut name.header) } == ERROR_SUCCESS.0 as i32)
        .then(|| wide_to_string(&name.adapterDevicePath))
}

/// [`adapter_name`], asked once per adapter per sweep.
#[derive(Default)]
struct AdapterNames(std::collections::HashMap<(u32, i32), String>);

impl AdapterNames {
    fn get(&mut self, adapter: windows::Win32::Foundation::LUID) -> String {
        self.0
            .entry((adapter.LowPart, adapter.HighPart))
            .or_insert_with(|| adapter_name(adapter).unwrap_or_default())
            .clone()
    }
}

/// The device instance id an adapter's device-interface path names:
/// `\\?\ROOT#DISPLAY#0000#{5b45201d-...}` is the device `ROOT\DISPLAY\0000`.
pub fn adapter_instance_id(interface_path: &str) -> Option<String> {
    let rest = interface_path.strip_prefix(r"\\?\")?;
    let body = &rest[..rest.rfind("#{")?];
    (!body.is_empty()).then(|| body.replace('#', "\\"))
}

/// Whether `adapter_path` is the adapter of one of our driver's devnodes.
///
/// "Not attached before the create" says only that a monitor is new, and one hot-plugged inside
/// the attach window is new too. Only a display this host created has a target on the indirect
/// display adapter our driver's devnode (hardware id `Root\VirtualDisplayDriver`) creates.
pub fn on_driver_adapter(adapter_path: &str, driver_instances: &[String]) -> bool {
    adapter_instance_id(adapter_path)
        .is_some_and(|id| driver_instances.iter().any(|d| d.eq_ignore_ascii_case(&id)))
}

/// Our driver's devnode instance ids, and when they were read. Cleared whenever the host
/// (re)connects to the driver: a reinstall can have renumbered the devnode.
static DRIVER_INSTANCES: Mutex<Option<(Instant, Vec<String>)>> = Mutex::new(None);

fn forget_driver_instances() {
    *DRIVER_INSTANCES.lock_recover() = None;
}

/// Our driver's devnode instance ids, from SetupAPI by hardware id. Cached: a reconfigure
/// resolves its target several times, and the devnode only changes on a reinstall.
fn driver_instances() -> Result<Vec<String>, String> {
    driver_instances_within(Duration::from_secs(30))
}

/// The same, read now. A topology write asks this way.
fn driver_instances_fresh() -> Result<Vec<String>, String> {
    driver_instances_within(Duration::ZERO)
}

fn driver_instances_within(fresh_for: Duration) -> Result<Vec<String>, String> {
    if let Some((at, ids)) = DRIVER_INSTANCES.lock_recover().as_ref() {
        if at.elapsed() < fresh_for {
            return Ok(ids.clone());
        }
    }
    let ids = root_instance_ids(
        super::driver_health::find_driver_devices()?
            .into_iter()
            .map(|d| d.instance_id)
            .collect(),
    )?;
    *DRIVER_INSTANCES.lock_recover() = Some((Instant::now(), ids.clone()));
    Ok(ids)
}

/// The driver is root-enumerated (`Root\VirtualDisplayDriver` makes `ROOT\DISPLAY\000n`), so an
/// instance id anywhere else is not ours to trust, whatever SetupAPI matched it on.
pub fn root_instance_ids(found: Vec<String>) -> Result<Vec<String>, String> {
    let (root, other): (Vec<String>, Vec<String>) = found
        .into_iter()
        .filter(|id| !id.is_empty())
        .partition(|id| id.len() > 5 && id[..5].eq_ignore_ascii_case("ROOT\\"));
    if !other.is_empty() {
        teprintln!(
            "[display] ignoring display driver device(s) that are not root-enumerated: {other:?}"
        );
    }
    if root.is_empty() {
        return Err("no ScreenExtend display driver device is present".to_string());
    }
    Ok(root)
}

/// Every active display path, or why it could not be read.
pub fn active_display_paths_checked() -> Result<Vec<DisplayPath>, String> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;

    let (paths, _) = query_display_config(QDC_ONLY_ACTIVE_PATHS)?;
    let mut adapters = AdapterNames::default();
    let mut out = Vec::with_capacity(paths.len());
    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
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
        // an unidentified target gets an empty device path, which is never an identity, so it is
        // never written to: `pick_owned_display` cannot match it, `pick_new_identity` refuses it
        let (friendly, device_path) =
            target_name(path.targetInfo.adapterId, path.targetInfo.id).unwrap_or_default();
        out.push(DisplayPath {
            gdi_name: wide_to_string(&source.viewGdiDeviceName),
            friendly,
            device_path,
            target_adapter: adapters.get(path.targetInfo.adapterId),
            adapter_id: (
                path.sourceInfo.adapterId.LowPart,
                path.sourceInfo.adapterId.HighPart,
            ),
            source_id: path.sourceInfo.id,
        });
    }
    Ok(out)
}

/// Every active display path. Empty, with a log line, when the topology could not be read.
pub fn active_display_paths() -> Vec<DisplayPath> {
    active_display_paths_checked().unwrap_or_else(|e| {
        teprintln!("[display] {e}");
        Vec::new()
    })
}

/// `DISPLAYCONFIG_PATH_ACTIVE`.
const PATH_ACTIVE: u32 = 0x0000_0001;
/// `DISPLAYCONFIG_PATH_MODE_IDX_INVALID`.
const MODE_IDX_INVALID: u32 = 0xffff_ffff;

/// `(adapter LUID low, high, source or target id)`.
pub type PathKey = (u32, i32, u32);

/// One row of `QueryDisplayConfig(QDC_ALL_PATHS)`, reduced to what a topology decision reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopologyRow {
    pub source: PathKey,
    pub target: PathKey,
    pub active: bool,
    /// A monitor is connected to this target.
    pub available: bool,
    /// The target is on our driver's adapter.
    pub ours: bool,
    /// One of ours that was released and taken off the desktop ([`PARKED`]): never activated,
    /// and deactivated again if Windows brings it back.
    pub parked: bool,
    /// Active, and its source sits at the desktop origin: the primary display.
    pub primary: bool,
}

/// What [`plan_driver_paths`] changes: row indices to deactivate and to activate. Every index is
/// a row on our own adapter; no other row is ever named.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TopologyPlan {
    pub deactivate: Vec<usize>,
    pub activate: Vec<usize>,
}

/// Give each of our attached monitors that has no desktop of its own a source of its own, and
/// touch nothing else.
///
/// Never `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)`: it names no display, so it re-enables every
/// monitor the user switched off (one coming up inside the attach window can be mistaken for
/// the new display) and undoes a duplicate topology the user chose. The two states one of our
/// monitors can be stuck in are handled on our own rows only:
/// * connected but inactive: activate one of its paths through a source nobody uses;
/// * cloned (active but sharing its source): move it to a free source, so it gets a
///   `\\.\DISPLAYn` of its own and a write to it cannot also land on what it was cloned with.
///
/// New sources are taken only from our own adapter. `None` when there is nothing to do.
pub fn plan_driver_paths(rows: &[TopologyRow]) -> Option<TopologyPlan> {
    use std::collections::{HashMap, HashSet};

    let mut source_use: HashMap<PathKey, usize> = HashMap::new();
    for r in rows.iter().filter(|r| r.active) {
        *source_use.entry(r.source).or_default() += 1;
    }
    let active_targets: HashSet<PathKey> =
        rows.iter().filter(|r| r.active).map(|r| r.target).collect();
    let mut used_sources: HashSet<PathKey> = source_use.keys().copied().collect();

    let mut plan = TopologyPlan::default();
    let mut needy: Vec<PathKey> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        if !(r.active && r.ours) || r.parked || needy.contains(&r.target) {
            continue;
        }
        let sharing = source_use.get_mut(&r.source).expect("counted above");
        if *sharing > 1 {
            *sharing -= 1;
            plan.deactivate.push(i);
            needy.push(r.target);
        }
    }
    for r in rows {
        if r.ours
            && !r.parked
            && r.available
            && !r.active
            && !active_targets.contains(&r.target)
            && !needy.contains(&r.target)
        {
            needy.push(r.target);
        }
    }
    for target in &needy {
        let pick = rows.iter().enumerate().find(|(_, r)| {
            r.ours
                && r.target == *target
                && !r.active
                && r.available
                && (r.source.0, r.source.1) == (r.target.0, r.target.1)
                && !used_sources.contains(&r.source)
        });
        if let Some((i, r)) = pick {
            plan.activate.push(i);
            used_sources.insert(r.source);
        }
    }
    // a cloned target that found no free source stays put: deactivating it would detach it
    let activated: Vec<PathKey> = plan.activate.iter().map(|&i| rows[i].target).collect();
    plan.deactivate
        .retain(|&i| activated.contains(&rows[i].target));
    (!plan.activate.is_empty()).then_some(plan)
}

/// Which rows to deactivate to take every parked monitor off the desktop: active rows of ours
/// that are parked and not the primary display, and nothing else. Empty rather than a plan that
/// would leave no active path at all.
pub fn plan_parking(rows: &[TopologyRow]) -> Vec<usize> {
    let drop: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.active && r.ours && r.parked && !r.primary)
        .map(|(i, _)| i)
        .collect();
    let active = rows.iter().filter(|r| r.active).count();
    if drop.len() >= active {
        return Vec::new();
    }
    drop
}

fn path_key(luid: windows::Win32::Foundation::LUID, id: u32) -> PathKey {
    (luid.LowPart, luid.HighPart, id)
}

/// The mode a path's index points at, if it is a mode of the expected kind.
fn mode_at(
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    idx: u32,
    kind: windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE,
) -> Option<&windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO> {
    if idx == MODE_IDX_INVALID {
        return None;
    }
    modes.get(idx as usize).filter(|m| m.infoType == kind)
}

fn source_mode_of(
    path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
) -> Option<SourceModeState> {
    use windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE;
    // SAFETY: without QDC_VIRTUAL_MODE_AWARE the union holds the plain mode index.
    let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    let m = mode_at(modes, idx, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)?;
    // SAFETY: the mode's infoType is SOURCE, so the union holds a source mode.
    let s = unsafe { m.Anonymous.sourceMode };
    Some(SourceModeState {
        width: s.width,
        height: s.height,
        pixel_format: s.pixelFormat.0,
        x: s.position.x,
        y: s.position.y,
    })
}

fn target_mode_of(
    path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
) -> Option<TargetModeState> {
    use windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO_TYPE_TARGET;
    // SAFETY: without QDC_VIRTUAL_MODE_AWARE the union holds the plain mode index.
    let idx = unsafe { path.targetInfo.Anonymous.modeInfoIdx };
    let m = mode_at(modes, idx, DISPLAYCONFIG_MODE_INFO_TYPE_TARGET)?;
    // SAFETY: the mode's infoType is TARGET, so the union holds a target mode.
    let v = unsafe { m.Anonymous.targetMode }.targetVideoSignalInfo;
    Some(TargetModeState {
        pixel_rate: v.pixelRate,
        h_sync: (v.hSyncFreq.Numerator, v.hSyncFreq.Denominator),
        v_sync: (v.vSyncFreq.Numerator, v.vSyncFreq.Denominator),
        active: (v.activeSize.cx, v.activeSize.cy),
        total: (v.totalSize.cx, v.totalSize.cy),
        // SAFETY: both arms of this union are one u32.
        video_standard: unsafe { v.Anonymous.videoStandard },
        scan_line: v.scanLineOrdering.0,
    })
}

/// The rows of a `QDC_ALL_PATHS` query, with ours, the parked ones and the primary marked.
///
/// `ours` is asked per adapter and `parked` only for targets that are ours; both come from the
/// caller so the marking can be tested on hand-made arrays.
pub fn topology_rows(
    paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    ours: &mut dyn FnMut(windows::Win32::Foundation::LUID) -> bool,
    parked: &mut dyn FnMut(windows::Win32::Foundation::LUID, u32) -> bool,
) -> Vec<TopologyRow> {
    let mut parked_targets = std::collections::HashMap::<PathKey, bool>::new();
    paths
        .iter()
        .map(|p| {
            let target = path_key(p.targetInfo.adapterId, p.targetInfo.id);
            let mine = ours(p.targetInfo.adapterId);
            let is_parked = mine
                && *parked_targets
                    .entry(target)
                    .or_insert_with(|| parked(p.targetInfo.adapterId, p.targetInfo.id));
            let active = p.flags & PATH_ACTIVE != 0;
            TopologyRow {
                source: path_key(p.sourceInfo.adapterId, p.sourceInfo.id),
                target,
                active,
                available: p.targetInfo.targetAvailable.as_bool(),
                ours: mine,
                parked: is_parked,
                primary: active && source_mode_of(p, modes).is_some_and(|m| m.x == 0 && m.y == 0),
            }
        })
        .collect()
}

/// A source mode as `QueryDisplayConfig` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceModeState {
    pub width: u32,
    pub height: u32,
    pub pixel_format: i32,
    pub x: i32,
    pub y: i32,
}

/// A target mode's signal timing as `QueryDisplayConfig` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetModeState {
    pub pixel_rate: u64,
    pub h_sync: (u32, u32),
    pub v_sync: (u32, u32),
    pub active: (u32, u32),
    pub total: (u32, u32),
    pub video_standard: u32,
    pub scan_line: i32,
}

/// One active path with its mode indices resolved, so two configurations (or a query and the
/// arrays about to be written) compare field by field however their mode arrays are numbered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathState {
    pub source: PathKey,
    pub target: PathKey,
    pub ours: bool,
    pub flags: u32,
    pub source_status: u32,
    pub target_status: u32,
    pub output: i32,
    pub rotation: i32,
    pub scaling: i32,
    pub refresh: (u32, u32),
    pub scan_line: i32,
    pub available: bool,
    pub source_mode: Option<SourceModeState>,
    pub target_mode: Option<TargetModeState>,
}

impl PathState {
    fn primary(&self) -> bool {
        self.source_mode.is_some_and(|m| m.x == 0 && m.y == 0)
    }
}

/// Every active path of a configuration, resolved.
pub fn path_states(
    paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    ours: &dyn Fn(windows::Win32::Foundation::LUID) -> bool,
) -> Vec<PathState> {
    paths
        .iter()
        .filter(|p| p.flags & PATH_ACTIVE != 0)
        .map(|p| PathState {
            source: path_key(p.sourceInfo.adapterId, p.sourceInfo.id),
            target: path_key(p.targetInfo.adapterId, p.targetInfo.id),
            ours: ours(p.targetInfo.adapterId),
            flags: p.flags,
            source_status: p.sourceInfo.statusFlags,
            target_status: p.targetInfo.statusFlags,
            output: p.targetInfo.outputTechnology.0,
            rotation: p.targetInfo.rotation.0,
            scaling: p.targetInfo.scaling.0,
            refresh: (
                p.targetInfo.refreshRate.Numerator,
                p.targetInfo.refreshRate.Denominator,
            ),
            scan_line: p.targetInfo.scanLineOrdering.0,
            available: p.targetInfo.targetAvailable.as_bool(),
            source_mode: source_mode_of(p, modes),
            target_mode: target_mode_of(p, modes),
        })
        .collect()
}

/// What differs between two descriptions of the same target's path.
pub fn changed_fields(before: &PathState, after: &PathState) -> Vec<&'static str> {
    let mut out = Vec::new();
    if before.source != after.source {
        out.push("source");
    }
    if before.flags != after.flags {
        out.push("path flags");
    }
    if before.source_status != after.source_status || before.target_status != after.target_status {
        out.push("status flags");
    }
    if before.output != after.output {
        out.push("output technology");
    }
    if before.rotation != after.rotation {
        out.push("rotation");
    }
    if before.scaling != after.scaling {
        out.push("scaling");
    }
    if before.refresh != after.refresh {
        out.push("refresh rate");
    }
    if before.scan_line != after.scan_line {
        out.push("scan-line ordering");
    }
    if before.available != after.available {
        out.push("availability");
    }
    match (before.source_mode, after.source_mode) {
        (Some(b), Some(a)) => {
            if (b.width, b.height) != (a.width, a.height) {
                out.push("resolution");
            }
            if b.pixel_format != a.pixel_format {
                out.push("pixel format");
            }
            if (b.x, b.y) != (a.x, a.y) {
                out.push("position");
            }
        }
        (None, None) => {}
        _ => out.push("source mode"),
    }
    if before.primary() != after.primary() {
        out.push("primary");
    }
    match (before.target_mode, after.target_mode) {
        (Some(b), Some(a)) => {
            if b.v_sync != a.v_sync {
                out.push("vertical refresh");
            }
            if (
                b.pixel_rate,
                b.h_sync,
                b.active,
                b.total,
                b.video_standard,
                b.scan_line,
            ) != (
                a.pixel_rate,
                a.h_sync,
                a.active,
                a.total,
                a.video_standard,
                a.scan_line,
            ) {
                out.push("signal timing");
            }
        }
        (None, None) => {}
        _ => out.push("target mode"),
    }
    out
}

/// One target whose path is not what it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathChange {
    pub target: PathKey,
    pub fields: Vec<&'static str>,
}

/// Every target, among those `include` selects, whose active path differs between `before` and
/// `after`: switched on, switched off, or changed in any field.
pub fn path_changes(
    before: &[PathState],
    after: &[PathState],
    include: &dyn Fn(&PathState) -> bool,
) -> Vec<PathChange> {
    let mut out = Vec::new();
    for b in before.iter().filter(|b| include(b)) {
        let fields = match after.iter().find(|a| a.target == b.target) {
            None => vec!["switched off"],
            Some(a) => changed_fields(b, a),
        };
        if !fields.is_empty() {
            out.push(PathChange {
                target: b.target,
                fields,
            });
        }
    }
    for a in after.iter().filter(|a| include(a)) {
        if !before.iter().any(|b| b.target == a.target) {
            out.push(PathChange {
                target: a.target,
                fields: vec!["switched on"],
            });
        }
    }
    out
}

/// The rules every topology write is held to, checked against the exact arrays about to be
/// handed to Windows: some display stays on, every display that is not ours is written exactly
/// as it is, and the primary display stays the primary.
pub fn check_write(before: &[PathState], written: &[PathState]) -> Result<(), String> {
    if written.is_empty() {
        return Err("it would leave no display on".to_string());
    }
    let theirs = path_changes(before, written, &|p| !p.ours);
    if !theirs.is_empty() {
        return Err(format!(
            "it would change a display this host does not own: {theirs:?}"
        ));
    }
    let primaries = |states: &[PathState]| {
        let mut v: Vec<PathKey> = states
            .iter()
            .filter(|s| s.primary())
            .map(|s| s.source)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    if primaries(before) != primaries(written) {
        return Err("it would change which display is the primary".to_string());
    }
    Ok(())
}

/// A target's preferred mode, which is what a display we activate is given.
#[derive(Clone, Copy)]
pub struct PreferredMode {
    pub width: u32,
    pub height: u32,
    pub target_mode: windows::Win32::Devices::Display::DISPLAYCONFIG_TARGET_MODE,
}

/// The arrays a plan writes: every active path exactly as queried, minus `plan.deactivate`,
/// plus `plan.activate` with an explicit mode each.
///
/// The kept paths' modes are renumbered into a compact array, so a clone group keeps its one
/// shared source mode. An activated path gets its target's preferred mode and a source against
/// the right edge of the rightmost display, top-aligned, so nothing already there has to move.
/// With every mode supplied, Windows needs no licence to change any of them.
pub fn build_topology_write(
    paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    plan: &TopologyPlan,
    preferred: &dyn Fn(usize) -> Option<PreferredMode>,
) -> Result<QueriedConfig, String> {
    use windows::Win32::Devices::Display::{
        DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_0, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
        DISPLAYCONFIG_MODE_INFO_TYPE_TARGET, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_PIXELFORMAT_32BPP, DISPLAYCONFIG_RATIONAL, DISPLAYCONFIG_ROTATION_IDENTITY,
        DISPLAYCONFIG_SCALING_IDENTITY, DISPLAYCONFIG_SOURCE_MODE,
    };
    use windows::Win32::Foundation::POINTL;

    if let Some(&bad) = plan
        .deactivate
        .iter()
        .chain(&plan.activate)
        .find(|&&i| i >= paths.len())
    {
        return Err(format!(
            "the plan names path {bad}, which the query did not return"
        ));
    }
    let mut out_modes: Vec<DISPLAYCONFIG_MODE_INFO> = Vec::new();
    let mut remap = std::collections::HashMap::<u32, u32>::new();
    let mut take = |idx: u32, out_modes: &mut Vec<DISPLAYCONFIG_MODE_INFO>| -> u32 {
        if idx == MODE_IDX_INVALID || idx as usize >= modes.len() {
            return MODE_IDX_INVALID;
        }
        *remap.entry(idx).or_insert_with(|| {
            out_modes.push(modes[idx as usize]);
            (out_modes.len() - 1) as u32
        })
    };
    let mut out_paths: Vec<DISPLAYCONFIG_PATH_INFO> = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        if p.flags & PATH_ACTIVE == 0 || plan.deactivate.contains(&i) {
            continue;
        }
        let mut kept = *p;
        // SAFETY: without QDC_VIRTUAL_MODE_AWARE the union holds the plain mode index.
        let (src, tgt) = unsafe {
            (
                p.sourceInfo.Anonymous.modeInfoIdx,
                p.targetInfo.Anonymous.modeInfoIdx,
            )
        };
        kept.sourceInfo.Anonymous.modeInfoIdx = take(src, &mut out_modes);
        kept.targetInfo.Anonymous.modeInfoIdx = take(tgt, &mut out_modes);
        out_paths.push(kept);
    }

    // right edge of the desktop as it will stand, and the top of whatever holds it
    let mut edge: Option<(i32, i32)> = None;
    for p in &out_paths {
        if let Some(m) = source_mode_of(p, &out_modes) {
            let right = m.x.saturating_add(m.width as i32);
            if edge.is_none_or(|(r, _)| right > r) {
                edge = Some((right, m.y));
            }
        }
    }

    for &i in &plan.activate {
        let p = &paths[i];
        let Some(pref) = preferred(i) else {
            return Err(format!(
                "target {} on the adapter {:?} reports no preferred mode",
                p.targetInfo.id, p.targetInfo.adapterId
            ));
        };
        let Some((x, y)) = edge else {
            return Err("no display on the desktop to place a new one beside".to_string());
        };
        let mut fresh = *p;
        fresh.flags |= PATH_ACTIVE;
        out_modes.push(DISPLAYCONFIG_MODE_INFO {
            infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
            id: p.sourceInfo.id,
            adapterId: p.sourceInfo.adapterId,
            Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                    width: pref.width,
                    height: pref.height,
                    pixelFormat: DISPLAYCONFIG_PIXELFORMAT_32BPP,
                    position: POINTL { x, y },
                },
            },
        });
        fresh.sourceInfo.Anonymous.modeInfoIdx = (out_modes.len() - 1) as u32;
        out_modes.push(DISPLAYCONFIG_MODE_INFO {
            infoType: DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
            id: p.targetInfo.id,
            adapterId: p.targetInfo.adapterId,
            Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                targetMode: pref.target_mode,
            },
        });
        fresh.targetInfo.Anonymous.modeInfoIdx = (out_modes.len() - 1) as u32;
        let signal = pref.target_mode.targetVideoSignalInfo;
        fresh.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
            Numerator: signal.vSyncFreq.Numerator,
            Denominator: signal.vSyncFreq.Denominator,
        };
        fresh.targetInfo.scanLineOrdering = signal.scanLineOrdering;
        if fresh.targetInfo.rotation.0 == 0 {
            fresh.targetInfo.rotation = DISPLAYCONFIG_ROTATION_IDENTITY;
        }
        if fresh.targetInfo.scaling.0 == 0 {
            fresh.targetInfo.scaling = DISPLAYCONFIG_SCALING_IDENTITY;
        }
        out_paths.push(fresh);
        edge = Some((x.saturating_add(pref.width as i32), y));
    }
    Ok((out_paths, out_modes))
}

/// What a topology write needs from the machine, so the whole sequence can run against
/// hand-made configurations in a test.
pub trait TopologyIo {
    fn query(&mut self) -> Result<QueriedConfig, String>;
    /// The adapter is our driver's, asked afresh for this write.
    fn ours(&mut self, adapter: windows::Win32::Foundation::LUID) -> bool;
    /// A row the plan names may still be changed: it is ours and, for parking, still parked and
    /// not in use by a session.
    fn may_write(
        &mut self,
        path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
    ) -> bool;
    fn preferred(
        &mut self,
        path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
    ) -> Option<PreferredMode>;
    /// `SetDisplayConfig` with exactly these arrays; its return code.
    fn apply(
        &mut self,
        paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
        modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    ) -> i32;
}

/// How a topology write ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Written. `collateral` lists displays that are not ours and changed anyway, which must
    /// never happen; `Err` when the configuration could not be read back to check.
    Applied {
        collateral: Result<Vec<PathChange>, String>,
    },
    /// Nothing written: the plan breaks a rule, or a row it names is no longer ours to change.
    Refused(String),
    /// Nothing written: the topology moved after the plan was made.
    Stale(Vec<PathChange>),
    /// Nothing changed: Windows would not apply the exact configuration (its error code).
    Rejected(i32),
    /// Nothing written: the configuration could not be read just before the write.
    Unreadable(String),
}

/// The whole topology write, against `io`. Refuses if the arrays fail [`check_write`], gives up
/// if anything at all moved since `snapshot`, then reads back every display that is not ours.
///
/// There is no `SDC_ALLOW_CHANGES`: every kept path is handed back with its queried modes and an
/// activated one with modes of its own, so Windows either applies precisely this or refuses it.
pub fn write_topology_with(
    io: &mut dyn TopologyIo,
    snapshot: &QueriedConfig,
    plan: &TopologyPlan,
) -> WriteOutcome {
    use std::collections::HashMap;
    let (paths, modes) = snapshot;
    for &i in plan.deactivate.iter().chain(&plan.activate) {
        let Some(p) = paths.get(i) else {
            return WriteOutcome::Refused(format!(
                "the plan names path {i}, which the query did not return"
            ));
        };
        if !io.may_write(p) {
            return WriteOutcome::Refused(format!(
                "target {} on the adapter {:?} is no longer a display this host may change",
                p.targetInfo.id, p.targetInfo.adapterId
            ));
        }
    }
    let mut preferred = HashMap::new();
    for &i in &plan.activate {
        preferred.insert(i, io.preferred(&paths[i]));
    }
    let (out_paths, out_modes) = match build_topology_write(paths, modes, plan, &|i| {
        preferred.get(&i).copied().flatten()
    }) {
        Ok(arrays) => arrays,
        Err(e) => return WriteOutcome::Refused(e),
    };

    let mut owners = Owners::default();
    owners.learn(io, paths);
    owners.learn(io, &out_paths);
    let before = path_states(paths, modes, &|a| owners.get(a));
    let written = path_states(&out_paths, &out_modes, &|a| owners.get(a));
    if let Err(e) = check_write(&before, &written) {
        return WriteOutcome::Refused(e);
    }

    let fresh = match io.query() {
        Ok(config) => config,
        Err(e) => return WriteOutcome::Unreadable(e),
    };
    owners.learn(io, &fresh.0);
    let now = path_states(&fresh.0, &fresh.1, &|a| owners.get(a));
    let mut moved = path_changes(&before, &now, &|_| true);
    // a row the plan switches on must still be there, off and connected
    for &i in &plan.activate {
        let want = &paths[i];
        let still = fresh.0.iter().any(|p| {
            path_key(p.sourceInfo.adapterId, p.sourceInfo.id)
                == path_key(want.sourceInfo.adapterId, want.sourceInfo.id)
                && path_key(p.targetInfo.adapterId, p.targetInfo.id)
                    == path_key(want.targetInfo.adapterId, want.targetInfo.id)
                && p.flags & PATH_ACTIVE == 0
                && p.targetInfo.targetAvailable.as_bool()
        });
        if !still {
            moved.push(PathChange {
                target: path_key(want.targetInfo.adapterId, want.targetInfo.id),
                fields: vec!["no longer free to switch on"],
            });
        }
    }
    if !moved.is_empty() {
        return WriteOutcome::Stale(moved);
    }

    let code = io.apply(&out_paths, &out_modes);
    if code != 0 {
        return WriteOutcome::Rejected(code);
    }
    let collateral = match io.query() {
        Ok(after) => {
            owners.learn(io, &after.0);
            let after = path_states(&after.0, &after.1, &|a| owners.get(a));
            Ok(path_changes(&before, &after, &|p| !p.ours))
        }
        Err(e) => Err(e),
    };
    WriteOutcome::Applied { collateral }
}

/// Which adapters are ours, asked once each per write.
#[derive(Default)]
struct Owners(std::collections::HashMap<(u32, i32), bool>);

impl Owners {
    fn learn(
        &mut self,
        io: &mut dyn TopologyIo,
        paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
    ) {
        for p in paths {
            let a = p.targetInfo.adapterId;
            self.0
                .entry((a.LowPart, a.HighPart))
                .or_insert_with(|| io.ours(a));
        }
    }

    fn get(&self, a: windows::Win32::Foundation::LUID) -> bool {
        self.0
            .get(&(a.LowPart, a.HighPart))
            .copied()
            .unwrap_or(false)
    }
}

/// Topology writes that left a display that is not ours different from how they found it. Each
/// one is an error in the log carrying the running count.
static FOREIGN_DISPLAY_CHANGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The live machine behind [`TopologyIo`].
struct LiveTopology {
    instances: Vec<String>,
    adapters: AdapterNames,
    parking: bool,
}

impl TopologyIo for LiveTopology {
    fn query(&mut self) -> Result<QueriedConfig, String> {
        query_display_config(windows::Win32::Devices::Display::QDC_ALL_PATHS)
    }

    fn ours(&mut self, adapter: windows::Win32::Foundation::LUID) -> bool {
        on_driver_adapter(&self.adapters.get(adapter), &self.instances)
    }

    fn may_write(
        &mut self,
        path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
    ) -> bool {
        if !self.ours(path.targetInfo.adapterId) {
            return false;
        }
        !self.parking
            || target_name(path.targetInfo.adapterId, path.targetInfo.id)
                .is_some_and(|(_, device_path)| parked_and_free(&device_path))
    }

    fn preferred(
        &mut self,
        path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
    ) -> Option<PreferredMode> {
        preferred_mode(path.targetInfo.adapterId, path.targetInfo.id)
    }

    fn apply(
        &mut self,
        paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
        modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    ) -> i32 {
        use windows::Win32::Devices::Display::{
            SetDisplayConfig, SDC_APPLY, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
        };
        unsafe {
            SetDisplayConfig(
                Some(paths),
                Some(modes),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG,
            )
        }
    }
}

fn preferred_mode(adapter: windows::Win32::Foundation::LUID, id: u32) -> Option<PreferredMode> {
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_TARGET_PREFERRED_MODE,
    };
    use windows::Win32::Foundation::ERROR_SUCCESS;
    let mut pref = DISPLAYCONFIG_TARGET_PREFERRED_MODE {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_PREFERRED_MODE>() as u32,
            adapterId: adapter,
            id,
        },
        ..Default::default()
    };
    let ok = unsafe { DisplayConfigGetDeviceInfo(&mut pref.header) } == ERROR_SUCCESS.0 as i32;
    (ok && pref.width > 0 && pref.height > 0).then_some(PreferredMode {
        width: pref.width,
        height: pref.height,
        target_mode: pref.targetMode,
    })
}

/// A target's name for the log: EDID name and device path, or its ids when Windows has neither.
fn describe_target(target: PathKey) -> String {
    let luid = windows::Win32::Foundation::LUID {
        LowPart: target.0,
        HighPart: target.1,
    };
    match target_name(luid, target.2) {
        Some((friendly, path)) if !path.is_empty() => format!("{friendly:?} ({path})"),
        _ => format!(
            "target {} on adapter {:08x}:{:08x}",
            target.2, target.1, target.0
        ),
    }
}

fn describe_changes(changes: &[PathChange]) -> String {
    changes
        .iter()
        .map(|c| format!("{}: {}", describe_target(c.target), c.fields.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The last failure a topology write logged, so a poll that retries every 100 ms says it once.
/// Holds the configuration a rejected write was built from and when, so the same configuration
/// is not offered to Windows again straight away.
static LAST_WRITE_FAILURE: Mutex<Option<(String, Option<Instant>)>> = Mutex::new(None);

/// Run [`write_topology_with`] against the machine, and log how it went. `announce` is logged
/// when a write is attempted. Returns whether the write was applied.
fn write_topology(
    snapshot: &QueriedConfig,
    plan: &TopologyPlan,
    parking: bool,
    what: &str,
    announce: &str,
) -> bool {
    const REOFFER_AFTER: Duration = Duration::from_secs(2);
    let signature = format!(
        "{what} {plan:?} {:?}",
        path_states(&snapshot.0, &snapshot.1, &|_| false)
    );
    if let Some((last, Some(at))) = LAST_WRITE_FAILURE.lock_recover().as_ref() {
        if *last == signature && at.elapsed() < REOFFER_AFTER {
            return false;
        }
    }
    // ownership is asked afresh for a write, never from the cache
    let instances = match driver_instances_fresh() {
        Ok(ids) => ids,
        Err(e) => {
            log_write_failure(what, format!("[display] not {what}: {e}"), None);
            return false;
        }
    };
    if !LAST_WRITE_FAILURE
        .lock_recover()
        .as_ref()
        .is_some_and(|(k, _)| *k == signature)
    {
        tprintln!("{announce}");
    }
    let mut io = LiveTopology {
        instances,
        adapters: AdapterNames::default(),
        parking,
    };
    match write_topology_with(&mut io, snapshot, plan) {
        WriteOutcome::Applied { collateral } => {
            *LAST_WRITE_FAILURE.lock_recover() = None;
            match collateral {
                Ok(changes) => {
                    for change in &changes {
                        let n = FOREIGN_DISPLAY_CHANGES.fetch_add(1, Ordering::Relaxed) + 1;
                        teprintln!(
                            "[display] ERROR: {} changed while {what}: {}. This host does not \
                             own it and wrote it exactly as it was ({n} such change(s) this run)",
                            describe_target(change.target),
                            change.fields.join(", ")
                        );
                    }
                }
                Err(e) => teprintln!(
                    "[display] {what}: applied, but the displays could not be read back to \
                     check the ones this host does not own: {e}"
                ),
            }
            true
        }
        WriteOutcome::Rejected(code) => {
            log_write_failure(
                what,
                format!(
                    "[display] {what}: Windows would not apply the exact configuration \
                     (SetDisplayConfig win32 error {code}); nothing was changed"
                ),
                Some(signature),
            );
            false
        }
        WriteOutcome::Stale(changes) => {
            log_write_failure(
                what,
                format!(
                    "[display] not {what}: the displays changed after they were read ({}); \
                     nothing was written",
                    describe_changes(&changes)
                ),
                None,
            );
            false
        }
        WriteOutcome::Refused(why) => {
            log_write_failure(what, format!("[display] not {what}: {why}"), None);
            false
        }
        WriteOutcome::Unreadable(e) => {
            log_write_failure(
                what,
                format!("[display] not {what}: the displays could not be read again: {e}"),
                None,
            );
            false
        }
    }
}

/// Log `line` unless it is the failure logged last. A rejected configuration is remembered by
/// its `signature`, so it is not offered again for a moment.
fn log_write_failure(what: &str, line: String, rejected: Option<String>) {
    let mut last = LAST_WRITE_FAILURE.lock_recover();
    let key = rejected
        .clone()
        .unwrap_or_else(|| format!("{what}: {line}"));
    let repeat = last.as_ref().is_some_and(|(k, _)| *k == key);
    *last = Some((key, rejected.map(|_| Instant::now())));
    drop(last);
    if !repeat {
        teprintln!("{line}");
    }
}

/// A device path that is parked and that no session has claimed since.
fn parked_and_free(device_path: &str) -> bool {
    !device_path.is_empty() && is_parked(device_path) && !is_owned_path(device_path)
}

/// [`topology_rows`] against the live machine.
fn live_rows(
    paths: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
    modes: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
    driver_instances: &[String],
) -> Vec<TopologyRow> {
    let mut adapters = AdapterNames::default();
    let check_parked = any_parked();
    topology_rows(
        paths,
        modes,
        &mut |a| on_driver_adapter(&adapters.get(a), driver_instances),
        &mut |a, id| {
            check_parked
                && target_name(a, id).is_some_and(|(_, device_path)| parked_and_free(&device_path))
        },
    )
}

/// Take every parked monitor that is on the desktop off it.
///
/// A released id in the middle of the driver's run has to stay in its list (the driver detaches
/// every display over a gap), but left active it is a screen nobody can see that is still part
/// of the user's desktop: windows stay on it and the pointer can wander onto it. Deactivating
/// its path keeps the driver's list intact. Only active rows of ours that are parked are named,
/// never the primary. Returns whether anything was applied.
fn park_released_displays(why: &str) -> bool {
    use windows::Win32::Devices::Display::QDC_ALL_PATHS;

    if !any_parked() {
        return false;
    }
    let instances = match driver_instances() {
        Ok(ids) => ids,
        Err(e) => {
            teprintln!("[display] not taking released displays off the desktop: {e}");
            return false;
        }
    };
    let snapshot = match query_display_config(QDC_ALL_PATHS) {
        Ok(config) => config,
        Err(e) => {
            teprintln!("[display] not taking released displays off the desktop: {e}");
            return false;
        }
    };
    let rows = live_rows(&snapshot.0, &snapshot.1, &instances);
    if rows
        .iter()
        .any(|r| r.active && r.ours && r.parked && r.primary)
    {
        teprintln!(
            "[display] a released ScreenExtend display is the primary display; leaving it on the \
             desktop"
        );
    }
    let off = plan_parking(&rows);
    if off.is_empty() {
        return false;
    }
    let announce = format!(
        "[display] taking {} released ScreenExtend display(s) off the desktop ({why}); they stay \
         in the driver's list, idle, until the displays above them have gone",
        off.len()
    );
    write_topology(
        &snapshot,
        &TopologyPlan {
            deactivate: off,
            activate: Vec::new(),
        },
        true,
        "taking a released ScreenExtend display off the desktop",
        &announce,
    )
}

/// [`plan_driver_paths`] against the live topology, applied through [`write_topology`].
///
/// If Windows refuses the exact configuration the display is left as Windows left it, and the
/// join's attach poll runs out and fails the join cleanly. Returns whether anything was applied.
pub fn activate_driver_paths() -> bool {
    use windows::Win32::Devices::Display::QDC_ALL_PATHS;

    let instances = match driver_instances() {
        Ok(ids) => ids,
        Err(e) => {
            teprintln!("[display] not touching the topology: {e}");
            return false;
        }
    };
    let snapshot = match query_display_config(QDC_ALL_PATHS) {
        Ok(config) => config,
        Err(e) => {
            teprintln!("[display] not touching the topology: {e}");
            return false;
        }
    };
    let rows = live_rows(&snapshot.0, &snapshot.1, &instances);
    let Some(plan) = plan_driver_paths(&rows) else {
        return false;
    };
    let announce = format!(
        "[display] giving {} ScreenExtend display(s) a desktop of their own ({} moved off a \
         shared source); every other display's path is passed through unchanged",
        plan.activate.len(),
        plan.deactivate.len()
    );
    write_topology(
        &snapshot,
        &plan,
        false,
        "activating the ScreenExtend display's path",
        &announce,
    )
}

/// Say what the user's own topology is at startup, and change none of it: a monitor the user
/// switched off or a duplicated desktop is their choice, and `SDC_TOPOLOGY_EXTEND` would undo
/// both. Our own displays get a desktop of their own when they attach
/// ([`activate_driver_paths`]).
pub fn log_topology_at_startup() {
    use windows::Win32::Devices::Display::QDC_ALL_PATHS;
    let Ok((paths, modes)) = query_display_config(QDC_ALL_PATHS) else {
        return;
    };
    let instances = driver_instances().unwrap_or_default();
    let rows = live_rows(&paths, &modes, &instances);
    let theirs: Vec<&TopologyRow> = rows.iter().filter(|r| !r.ours).collect();
    let active: Vec<&&TopologyRow> = theirs.iter().filter(|r| r.active).collect();
    let mut sources: Vec<(u32, i32, u32)> = active.iter().map(|r| r.source).collect();
    sources.sort_unstable();
    sources.dedup();
    let mut off: Vec<(u32, i32, u32)> = theirs
        .iter()
        .filter(|r| r.available && !r.active && !active.iter().any(|a| a.target == r.target))
        .map(|r| r.target)
        .collect();
    off.sort_unstable();
    off.dedup();
    if sources.len() < active.len() {
        tprintln!(
            "[display] the desktop is duplicated across {} display(s); left as it is",
            active.len()
        );
    }
    if !off.is_empty() {
        tprintln!(
            "[display] {} connected display(s) are switched off; left as they are",
            off.len()
        );
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
/// in effect afterwards.
///
/// An indirect display's compose cadence is synthesised from the mode's refresh rate, so this
/// decides how often that monitor's desktop is recomposed, independently of how many frames
/// are encoded and sent. Only a rate registered at create time can be selected; see
/// `registered_refresh_rates`.
pub fn set_refresh_rate(device_name: &str, refresh_rate: u32) -> Result<u32, String> {
    use windows::core::PCWSTR;
    use windows::Win32::Graphics::Gdi::{
        ChangeDisplaySettingsExW, EnumDisplaySettingsW, CDS_UPDATEREGISTRY, DEVMODEW,
        DISP_CHANGE_SUCCESSFUL, DM_DISPLAYFREQUENCY, DM_PELSHEIGHT, DM_PELSWIDTH,
        ENUM_CURRENT_SETTINGS,
    };

    // same rule as every other display write: the name is resolved against what is attached
    // now, and anything but one of our own displays is refused
    let device_name = &resolve_owned_display(&DisplayTarget {
        device_path: String::new(),
        device_name: device_name.to_string(),
    })
    .map_err(|e| e.to_string())?
    .gdi_name;

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

    // the bundled IDD rejects DM_DISPLAYFREQUENCY alone with DISP_CHANGE_BADMODE: the
    // resolution has to travel with the rate, even unchanged
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
        // every creation grows the driver's host process, and nothing else checks it after the
        // startup gate (`serve` has no gate). Outside the commit lock: a process-list read and a
        // pipe open, not a commit
        if self.pipe == DEFAULT_PIPE_NAME {
            super::driver_health::warn_if_over_threshold("before creating a display");
        }
        // held for the whole allocate-commit-record sequence, so no other commit can hand out
        // the same id; `inner` is taken only to touch its fields
        let _commit = self.commit.lock_recover();
        let mut monitors = self.inner.lock_recover().owned.clone();
        // before the monitor exists: a change made later re-creates its swap chain
        self.choose_render_adapter(monitors.len());
        self.first_create
            .lock_recover()
            .get_or_insert_with(Instant::now);
        // in shared-driver mode the id is machine-wide, chosen under the cross-process lock
        // inside the commit; `shared::ALLOCATE` marks the monitor that needs one
        let id = if self.shared {
            shared::ALLOCATE
        } else {
            // never a released id: that monitor is still attached, idle, and its name and device
            // path are already in the join's before-snapshot, so the attach correlation refuses it
            let ids: Vec<u32> = monitors.iter().map(|m| m.id).collect();
            allocate(&ids, &[])
        };

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

        // the mode list is fixed for the life of the monitor: anything a later reconfig asks for
        // that is not in it needs the display recreating
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
        let committed = self.commit_unlocked(monitors)?;
        let id = committed.last().map(|m| m.id).unwrap_or(id);
        // the join claims this monitor next, under the same correlation lock
        *PENDING_CLAIM.lock_recover() = Some(id);
        Ok(id)
    }

    // no `display_device_name` override: the trait default (`None`) sends the join down the
    // monitor diff. A name match cannot work: the bundled driver's EDID name is always its own
    // `VirtuDisplay+`, never the name the host asked for

    fn remove_display(&self, id: u32) {
        let _commit = self.commit.lock_recover();
        let (mut monitors, released) = {
            let inner = self.inner.lock_recover();
            (inner.owned.clone(), inner.released.clone())
        };
        if self.shared {
            monitors.retain(|m| m.id != id);
            match self.commit_unlocked(monitors) {
                Ok(_) => forget_ids(&[id]),
                Err(e) => teprintln!("virtual_display: remove({id}) failed: {e}"),
            }
            return;
        }
        let ids: Vec<u32> = monitors.iter().map(|m| m.id).collect();
        if !ids.contains(&id) {
            return;
        }
        let (keep, held) = plan_release(&ids, &released, id);
        if keep.len() == ids.len() {
            // taking it out now would leave a gap in the run, and the driver detaches *every*
            // display over a gap, every other client's stream with it
            self.inner.lock_recover().released = held;
            tprintln!(
                "[virtual-display] display id={id} released; it stays in the driver's list until \
                 the displays above it ({:?}) have gone{}",
                ids.iter().filter(|i| **i > id).collect::<Vec<_>>(),
                if self.park {
                    ", off the desktop"
                } else {
                    ", attached and idle: anything on it stays on a screen nobody can see \
                     (SE_TUNE_ON=park_released_displays takes it off the desktop)"
                }
            );
            if self.park {
                self.park_released(id);
            }
            return;
        }
        monitors.retain(|m| keep.contains(&m.id));
        match self.commit_unlocked(monitors) {
            Ok(_) => {
                self.inner.lock_recover().released = held.clone();
                let gone: Vec<u32> = ids.iter().copied().filter(|i| !keep.contains(i)).collect();
                forget_ids(&gone);
                if !held.is_empty() {
                    tprintln!(
                        "[virtual-display] display id={id} removed; still held idle: {held:?}"
                    );
                }
            }
            Err(e) => {
                // the driver still holds everything in `owned`: record this id as released too, or
                // the next commit re-sends it as an owned display and never compacts it
                let mut inner = self.inner.lock_recover();
                if !inner.released.contains(&id) {
                    inner.released.push(id);
                }
                drop(inner);
                teprintln!("virtual_display: remove({id}) failed: {e}");
                // still in the driver's list and on the desktop: the same state as a held id
                if self.park {
                    self.park_released(id);
                }
            }
        }
    }

    fn remove_all_displays(&self) {
        let _commit = self.commit.lock_recover();
        // shared mode removes only ours (other hosts sharing the driver keep theirs); the default
        // mode sends the empty list, which clears the driver
        if let Err(e) = self.commit_unlocked(Vec::new()) {
            teprintln!("virtual_display: remove_all failed: {e}");
        }
        let mut inner = self.inner.lock_recover();
        inner.owned.clear();
        inner.released.clear();
        drop(inner);
        *ID_PATHS.lock_recover() = None;
        *PENDING_CLAIM.lock_recover() = None;
        if self.park {
            *PARKED.lock_recover() = None;
        }
        self.clear_render_on_stop();
        // the host is stopping: nothing is left for the driver to keep, so the watcher ends
        self.ownership_wanted.store(false, Ordering::Release);
        self.stopped.store(true, Ordering::Release);
    }
}

impl WindowsVirtualDisplay {
    /// Take released `id`'s monitor off the desktop, if its identity is known and nobody owns it.
    fn park_released(&self, id: u32) {
        let path = ID_PATHS
            .lock_recover()
            .as_ref()
            .and_then(|m| m.get(&id).cloned());
        let Some(path) = path else {
            tprintln!(
                "[virtual-display] display id={id} was never claimed, so it cannot be found on \
                 the desktop; it stays attached and idle"
            );
            return;
        };
        // the session forgets a display's identity before it removes it: if this one is still
        // owned, somebody is still driving it
        if is_owned_path(&path) {
            teprintln!(
                "[virtual-display] display id={id} ({path}) is still recorded as in use; leaving \
                 it on the desktop"
            );
            return;
        }
        // and it must be on our driver's adapter now, not only when it was claimed
        if let Err(why) = path_on_driver_now(&path) {
            teprintln!(
                "[virtual-display] not taking display id={id} ({path}) off the desktop: {why}"
            );
            return;
        }
        PARKED
            .lock_recover()
            .get_or_insert_with(Default::default)
            .insert(path);
        park_released_displays("a display in the middle of the run was released");
    }
}

/// The driver's render adapter preference, from its diagnostics; `None` when it cannot say.
fn read_preference(client: &DriverClient) -> Option<Option<v2::Luid>> {
    if !client.negotiated().has(v2::CAP_DIAGNOSTICS) {
        return None;
    }
    match client.diagnostics() {
        Ok(diag) => v2::reported_preference(&diag),
        Err(e) => {
            teprintln!("[virtual-display] the driver's diagnostics did not answer: {e}");
            None
        }
    }
}

/// Every adapter, in the order the capture device's preference enumerates them, with the
/// indirect display adapter of our own driver marked.
fn render_candidates() -> Vec<host::AdapterCandidate> {
    use windows::Win32::Graphics::Dxgi::DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE;
    let adapters = match crate::windows_capture::d3d11::enumerate_adapters(Some(
        DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
    )) {
        Ok(adapters) => adapters,
        Err(e) => {
            teprintln!("[virtual-display] render adapter: cannot list the adapters: {e}");
            return Vec::new();
        }
    };
    let instances = driver_instances().unwrap_or_default();
    adapters
        .into_iter()
        .map(|(_, info)| {
            let luid = v2::Luid::from_packed(info.luid);
            let ours = adapter_name(windows::Win32::Foundation::LUID {
                LowPart: luid.low,
                HighPart: luid.high,
            })
            .is_some_and(|path| on_driver_adapter(&path, &instances));
            host::AdapterCandidate {
                luid,
                vendor_id: info.vendor_id,
                software: info.software,
                ours,
                description: info.description,
            }
        })
        .collect()
}

/// `name (vendor 0x..., LUID 0x...)`, the form of every `adapters:` line, or the bare LUID when no
/// adapter has it.
fn adapter_label(luid: v2::Luid) -> String {
    crate::windows_capture::d3d11::enumerate_adapters(None)
        .ok()
        .and_then(|list| {
            list.into_iter()
                .find(|(_, info)| info.luid == luid.packed())
                .map(|(_, info)| info.label())
        })
        .unwrap_or_else(|| format!("LUID {luid}"))
}

/// Which adapter Windows composes the monitor `hmonitor` on, as the driver reports it.
///
/// `None` when there is nothing to say: no diagnostics on this driver, or not one of this host's
/// displays. `Some(Err(_))` when it is ours and the driver could not say.
pub fn composing_adapter_of(hmonitor: isize) -> Option<Result<String, String>> {
    composing_adapter_of_name(&monitor_gdi_name(hmonitor)?)
}

/// [`composing_adapter_of`], for a monitor named by its `\.\DISPLAYn`.
pub fn composing_adapter_of_name(gdi: &str) -> Option<Result<String, String>> {
    Some(composing_luid_of_name(gdi)?.map(adapter_label))
}

/// The LUID Windows composes the monitor `\.\DISPLAYn` on, as the driver reports it; see
/// [`composing_adapter_of`].
pub fn composing_luid_of_name(gdi: &str) -> Option<Result<v2::Luid, String>> {
    // on the path to a session's first frame, so bounded: a slow driver or a busy connection
    // costs this log field, never the start
    const DEADLINE: Duration = Duration::from_millis(200);
    // a duplicate of the controller's own connection, held only for this request, so nothing
    // keeps the host's connection (and the driver's count for its token) alive behind it
    let link = CONTROLLER
        .lock_recover()
        .as_ref()
        .and_then(|w| w.upgrade())
        .map(|d| d.inner.lock_recover().client.duplicate())
        .filter(|c| c.negotiated().has(v2::CAP_DIAGNOSTICS))?;
    let gdi = gdi.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    // a plain thread, also because the round trip blocks on driver_ipc's own runtime, which
    // panics on a thread that already belongs to one
    let spawned = std::thread::Builder::new()
        .name("composing-adapter".to_string())
        .spawn(move || {
            let path = active_display_paths()
                .into_iter()
                .find(|p| p.gdi_name == gdi)
                .map(|p| p.device_path);
            let id = path.and_then(|path| {
                ID_PATHS
                    .lock_recover()
                    .as_ref()?
                    .iter()
                    .find(|(_, p)| **p == path)
                    .map(|(id, _)| *id)
            });
            let answer = id.map(|id| match link.diagnostics() {
                Ok(diag) => v2::composing_adapter(&diag, id)
                    .ok_or_else(|| format!("the driver reports no swap chain for display id={id}")),
                Err(e) => Err(format!("the driver's diagnostics did not answer: {e}")),
            });
            let _ = tx.send(answer);
        });
    if spawned.is_err() {
        return None;
    }
    match rx.recv_timeout(DEADLINE) {
        Ok(answer) => answer,
        Err(_) => Some(Err(format!(
            "not reported within {} ms",
            DEADLINE.as_millis()
        ))),
    }
}

/// The adapter DXGI lists outputs under in this process: the first hardware adapter, in
/// DXGI's own order, that enumerates any output, other than our driver's indirect display
/// adapter. On a hybrid laptop DXGI lists every output under the adapter the process runs on
/// (the integrated panel included, though the iGPU drives it), and the virtual displays with
/// them.
fn dxgi_listing_adapter() -> Option<v2::Luid> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    // SAFETY: plain factory creation and enumeration; every interface is released on drop.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.ok()?;
    let instances = driver_instances().unwrap_or_default();
    for i in 0.. {
        // SAFETY: as above; the loop ends at the first index DXGI does not have.
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            break;
        };
        // SAFETY: as above.
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else {
            continue;
        };
        let info = crate::windows_capture::d3d11::AdapterInfo::from_dxgi_desc1(&desc);
        let ours =
            adapter_name(desc.AdapterLuid).is_some_and(|p| on_driver_adapter(&p, &instances));
        // SAFETY: as above.
        let has_output = unsafe { adapter.EnumOutputs(0) }.is_ok();
        if has_output && !info.software && !ours {
            return Some(v2::Luid::from_packed(info.luid));
        }
    }
    None
}

/// DXGI duplication is this session's capture: make the driver compose the display `gdi` on
/// the adapter DXGI lists its output under, now that the output exists and that can be read
/// rather than predicted. Duplication delivers nothing from any other adapter, so on a
/// mismatch the preference is set to the listing adapter, which re-creates the display's swap
/// chain there, and this waits for it.
///
/// `listing` is that adapter (packed LUID); the caller reads it once per opening and asks
/// here only when it changed. The request and the wait for the driver to report the new
/// composition both fit in `budget`; no lock is held across either.
pub fn compose_for_duplication(gdi: &str, listing: i64, budget: Duration) {
    let started = Instant::now();
    let Some(display) = CONTROLLER.lock_recover().as_ref().and_then(|w| w.upgrade()) else {
        return;
    };
    let target = v2::Luid::from_packed(listing);
    let now = composing_luid_of_name(gdi);
    if let Some(Ok(l)) = &now {
        if *l == target {
            return;
        }
    }
    let client = display.inner.lock_recover().client.duplicate();
    if !client.negotiated().has(v2::CAP_RENDER_ADAPTER) {
        return;
    }
    tprintln!(
        "[virtual-display] render adapter: DXGI lists {gdi} under {}, which is not where the \
         driver composes it ({}); asking for it before duplication starts",
        adapter_label(target),
        match &now {
            Some(Ok(l)) => adapter_label(*l),
            Some(Err(why)) => format!("unknown: {why}"),
            None => "unknown".to_string(),
        }
    );
    display.render.lock_recover().wanted = Some(Some(target));
    let left = budget.saturating_sub(started.elapsed());
    if left.is_zero() || !request_composition(&client, &display.render, target, left) {
        return;
    }
    while started.elapsed() < budget {
        if let Some(Ok(l)) = composing_luid_of_name(gdi) {
            if l == target {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    teprintln!(
        "[virtual-display] render adapter: {gdi} is still not composed on {} after {} ms; \
         duplication may deliver nothing",
        adapter_label(target),
        budget.as_millis()
    );
}

/// One `SetRenderAdapter` for `target`, answered within `within` or given up on: the capture
/// path's request, which must never wait longer than its budget nor try twice. `true` when the
/// driver took it.
fn request_composition(
    client: &DriverClient,
    render: &Mutex<RenderState>,
    target: v2::Luid,
    within: Duration,
) -> bool {
    match client.set_render_adapter_within(Some(target), within) {
        Ok(reply) => {
            let mut r = render.lock_recover();
            r.reported = Some(reply.preferred);
            r.set_here = reply.preferred.is_some();
            reply.preferred == Some(target)
        }
        Err(e) => {
            if e.code().is_some() {
                render.lock_recover().refused.push(Some(target));
            }
            teprintln!(
                "[virtual-display] render adapter: asking for {} before duplication failed: {e}",
                adapter_label(target)
            );
            false
        }
    }
}

/// The encoder a session started runs on `adapter_luid` (packed; `None` for the CPU). Called
/// from the capture path, which must not wait on the driver, so the work goes to a thread.
pub fn encoder_started(adapter_luid: Option<i64>) {
    let Some(display) = CONTROLLER.lock_recover().as_ref().and_then(|w| w.upgrade()) else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("render-follow".to_string())
        .spawn(move || display.encoder_started(adapter_luid.map(v2::Luid::from_packed)));
}

/// The default-mode start-up clear. On a driver that reports owners it is targeted: monitors
/// nobody owns and those whose owner has no live connection go, another live host's stay. On
/// any other driver it clears everything.
fn startup_clear(client: &mut DriverClient) {
    let n = client.negotiated();
    if n.has(v2::CAP_OWNED_MONITORS) && n.has(v2::CAP_DIAGNOSTICS) {
        if let (Ok(diag), Ok(list)) = (client.diagnostics(), client.request_live_state()) {
            let owners = diag.get("owners");
            let live: Vec<String> = owners
                .and_then(|o| o.get("tokens"))
                .and_then(|t| t.as_array())
                .map(|t| {
                    t.iter()
                        .filter(|t| t.get("connections").and_then(|c| c.as_u64()).unwrap_or(0) > 0)
                        .filter_map(|t| t.get("token_prefix").and_then(|p| p.as_str()))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let owner_of = |id: u32| -> Option<String> {
                owners?
                    .get("monitors")?
                    .as_array()?
                    .iter()
                    .find(|m| m.get("id").and_then(|i| i.as_u64()) == Some(u64::from(id)))?
                    .get("owner_prefix")?
                    .as_str()
                    .map(str::to_string)
            };
            let monitors: Vec<(u32, Option<String>)> =
                list.iter().map(|m| (m.id, owner_of(m.id))).collect();
            let removed = host::startup_removals(&monitors, &live);
            let kept: Vec<Monitor> = list
                .iter()
                .filter(|m| !removed.contains(&m.id))
                .cloned()
                .collect();
            if !kept.is_empty() {
                tprintln!(
                    "[virtual-display] start-up clear: leaving {:?} (another live host's, or held \
                     so the list stays one run); removing {removed:?}",
                    kept.iter().map(|m| m.id).collect::<Vec<_>>()
                );
            }
            if kept.len() == list.len() {
                return;
            }
            if client.set_monitors(&kept).is_ok() {
                if let Err(e) = client.notify() {
                    teprintln!(
                        "[virtual-display] could not clear leftover monitors at startup: {e}"
                    );
                }
            }
            return;
        }
    }
    // still needed with owned monitors offered but unreadable: a driver without them never
    // removes what a dead host left
    client.remove_all();
    if let Err(e) = client.notify() {
        teprintln!("[virtual-display] could not clear leftover monitors at startup: {e}");
    }
}

fn monitor_gdi_name(hmonitor: isize) -> Option<String> {
    use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, HMONITOR, MONITORINFO, MONITORINFOEXW};
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        szDevice: [0; 32],
    };
    // SAFETY: `info` is a MONITORINFOEXW with its size set, which GetMonitorInfoW accepts in
    // place of a MONITORINFO; a stale handle only makes the call fail.
    let ok = unsafe { GetMonitorInfoW(HMONITOR(hmonitor as _), (&raw mut info).cast()) };
    ok.as_bool().then(|| wide_to_string(&info.szDevice))
}

/// After a reconnect the driver has re-arrived every monitor this host holds, and a held one
/// comes back on the desktop. Its device path is not known to survive a driver restart, and
/// taking it off again would be a topology write while Windows is re-arranging all of our
/// monitors, so the parked state is dropped: the monitor stays on, idle, until the ones above
/// it go, as with parking switched off.
fn unpark_after_reconnect() {
    let dropped = PARKED.lock_recover().take().map_or(0, |set| set.len());
    if dropped > 0 {
        tprintln!(
            "[virtual-display] the driver connection was replaced; {dropped} released display(s) \
             that were off the desktop are back on it, idle, until the displays above them have \
             gone"
        );
    }
}

/// Whether `device_path` names a target on our driver's adapter right now, by the positive test
/// ([`on_driver_adapter`]) against a fresh read of the devnode.
fn path_on_driver_now(device_path: &str) -> Result<(), String> {
    use windows::Win32::Devices::Display::QDC_ALL_PATHS;
    let instances = driver_instances_fresh()?;
    let (paths, _) = query_display_config(QDC_ALL_PATHS)?;
    let mut adapters = AdapterNames::default();
    let mut targets = Vec::new();
    for p in &paths {
        let key = path_key(p.targetInfo.adapterId, p.targetInfo.id);
        if targets.iter().any(|(k, _, _)| *k == key) {
            continue;
        }
        let path = target_name(p.targetInfo.adapterId, p.targetInfo.id)
            .map(|(_, path)| path)
            .unwrap_or_default();
        targets.push((key, path, adapters.get(p.targetInfo.adapterId)));
    }
    let listed: Vec<(String, String)> = targets.into_iter().map(|(_, p, a)| (p, a)).collect();
    check_path_on_driver(&listed, device_path, &instances)
}

/// [`path_on_driver_now`]'s rule over `(device path, adapter path)` pairs: exactly one target
/// answers to `device_path`, and it is on one of our driver's devnodes.
pub fn check_path_on_driver(
    targets: &[(String, String)],
    device_path: &str,
    driver_instances: &[String],
) -> Result<(), String> {
    if device_path.is_empty() {
        return Err("it has no device path".to_string());
    }
    let mut hits = targets.iter().filter(|(p, _)| p == device_path);
    let Some((_, adapter)) = hits.next() else {
        return Err("no target answers to it".to_string());
    };
    if hits.next().is_some() {
        return Err("more than one target answers to it".to_string());
    }
    if !on_driver_adapter(adapter, driver_instances) {
        return Err(format!(
            "it is on {adapter:?}, not on the display driver's adapter"
        ));
    }
    Ok(())
}

/// Ids that have left the driver's list: their monitors are gone, so nothing is parked under
/// them, and the path an id comes back with is a new claim.
fn forget_ids(ids: &[u32]) {
    let mut map = ID_PATHS.lock_recover();
    let Some(map) = map.as_mut() else {
        return;
    };
    let mut parked = PARKED.lock_recover();
    for id in ids {
        if let Some(path) = map.remove(id) {
            if let Some(set) = parked.as_mut() {
                set.remove(&path);
            }
        }
    }
}

/// The driver attaches a list only when its ids are one contiguous run starting at 0 or 1
/// (measured; see `mod shared`).
pub fn attachable(ids: &[u32]) -> bool {
    if ids.is_empty() {
        return true;
    }
    let mut v = ids.to_vec();
    v.sort_unstable();
    v.dedup();
    v.len() == ids.len() && v[0] <= 1 && v.windows(2).all(|w| w[1] == w[0] + 1)
}

/// The id a new monitor takes: a released one if any (reusing an attached slot), else the one
/// that extends the run.
pub fn allocate(ids: &[u32], released: &[u32]) -> u32 {
    if let Some(id) = released.iter().min() {
        return *id;
    }
    match (ids.iter().min(), ids.iter().max()) {
        (None, _) => 0,
        (Some(1), _) if !ids.contains(&0) => 0,
        (_, Some(max)) => max + 1,
        _ => 0,
    }
}

/// Remove released ids, highest first, for as long as the remaining run stays attachable.
pub fn compact(ids: &mut Vec<u32>, released: &mut Vec<u32>) -> Vec<u32> {
    let mut removed = Vec::new();
    loop {
        let mut changed = false;
        let mut cands = released.clone();
        cands.sort_unstable_by(|a, b| b.cmp(a));
        for id in cands {
            let rest: Vec<u32> = ids.iter().copied().filter(|x| *x != id).collect();
            if attachable(&rest) {
                *ids = rest;
                released.retain(|x| *x != id);
                removed.push(id);
                changed = true;
            }
        }
        if !changed {
            return removed;
        }
    }
}

/// The default mode's removal: release `id`, then take out every released id that can go
/// without leaving a gap. Returns the ids to keep in the driver's list and those still held.
pub fn plan_release(ids: &[u32], released: &[u32], id: u32) -> (Vec<u32>, Vec<u32>) {
    let mut keep = ids.to_vec();
    let mut held: Vec<u32> = released
        .iter()
        .copied()
        .filter(|r| keep.contains(r))
        .collect();
    if keep.contains(&id) && !held.contains(&id) {
        held.push(id);
    }
    compact(&mut keep, &mut held);
    held.sort_unstable();
    (keep, held)
}

/// Shared-driver mode, entered when `SCREENEXTEND_DATA_DIR` isolates this host
/// (`crate::data_dir`). Test-only: it lets `scripts/test-handler/` run several hosts at once.
///
/// The driver keeps one machine-wide monitor list and every notify replaces it, so two hosts
/// that each own it wipe each other's displays. On the shipped driver (Windows 11 26200) a
/// list attaches only when its ids are one contiguous run starting at 0 or 1: `[2]`, `[0,2]`,
/// `[0,1,3]` attach nothing, and `[0,1,2]` to `[0,2]` detaches *every* display while the
/// driver's own state reads `[0,2]`. Removing from either end of a run leaves the rest
/// attached. It sends no `Changed` event to the client that made a change, so a commit is
/// confirmed with a fresh state request.
///
/// So ids are machine-wide and dense: every commit, under a file lock all sharing hosts take,
/// merges this host's monitors into the live list and removes released ones only while the run
/// stays valid. A dead owner pid counts as released; monitors in no registry (an unisolated
/// host's) are never touched.
mod shared {
    use super::{allocate, attachable, compact, DriverClient, Monitor};
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Placeholder id on a monitor that needs one allocated inside the commit.
    pub const ALLOCATE: u32 = u32::MAX;

    fn dir() -> PathBuf {
        std::env::temp_dir().join("screenextend-shared-driver")
    }

    /// Held for a whole read-merge-notify-confirm sequence. Dropping the file releases it.
    struct Guard(#[allow(dead_code)] std::fs::File);

    /// How long a commit waits to open the lock file before failing, rather than spinning (and
    /// logging) forever on a directory it cannot write.
    const LOCK_OPEN_DEADLINE: Duration = Duration::from_secs(30);

    fn lock() -> Result<Guard, String> {
        use fs4::fs_std::FileExt;
        let path = dir().join("commit.lock");
        let _ = std::fs::create_dir_all(dir());
        let deadline = Instant::now() + LOCK_OPEN_DEADLINE;
        let mut logged = false;
        loop {
            let error = match std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
            {
                // blocks while another host holds it; that host's hold is itself bounded
                Ok(file) => match FileExt::lock_exclusive(&file) {
                    Ok(()) => return Ok(Guard(file)),
                    Err(e) => e,
                },
                Err(e) => e,
            };
            if Instant::now() >= deadline {
                return Err(format!("shared lock {}: {error}", path.display()));
            }
            if !logged {
                teprintln!(
                    "[virtual-display] shared lock {}: {error}; retrying",
                    path.display()
                );
                logged = true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// One registered monitor's owner: its pid and that process's creation time, so a pid the
    /// system has handed to somebody else after a crash does not keep a dead host's displays
    /// owned forever. `(0, 0)` means released.
    type Owner = (u32, u64);
    const RELEASED: Owner = (0, 0);

    /// id -> owner.
    type Registry = BTreeMap<u32, Owner>;

    fn registry_path() -> PathBuf {
        // versioned name: the older pid-only registry is not read, and a host that cannot parse
        // a file treats every monitor in it as somebody else's and never touches it
        dir().join("owners-v2.json")
    }

    fn read_registry() -> Registry {
        std::fs::read(registry_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn write_registry(reg: &Registry) {
        if let Ok(bytes) = serde_json::to_vec(reg) {
            if let Err(e) = std::fs::write(registry_path(), bytes) {
                teprintln!("[virtual-display] cannot write the shared registry: {e}");
            }
        }
    }

    /// A process's creation time (100 ns ticks since 1601), or `None` when it is not running.
    fn process_started(pid: u32) -> Option<u64> {
        use windows::Win32::Foundation::{CloseHandle, FILETIME, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
        else {
            return None;
        };
        let mut code = 0u32;
        let running = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok()
            && code == STILL_ACTIVE.0 as u32;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let timed =
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
                .is_ok();
        let _ = unsafe { CloseHandle(handle) };
        (running && timed)
            .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    /// Whether `owner` is still the process that registered the monitor.
    pub fn owner_alive(owner: Owner, started: &dyn Fn(u32) -> Option<u64>) -> bool {
        owner != RELEASED && started(owner.0).is_some_and(|now| owner.1 == 0 || now == owner.1)
    }

    /// Commit `ours` (this host's whole desired list; a monitor with id [`ALLOCATE`] gets one)
    /// into the machine-wide list. `previous` is what this host owned before. Returns `ours`
    /// with real ids.
    pub fn commit(
        client: &mut DriverClient,
        previous: &[u32],
        ours: Vec<Monitor>,
    ) -> Result<Vec<Monitor>, String> {
        // the driver gives a monitor to the token whose Notify created it and never moves it, so a
        // reused released id would go when the host that created it goes. Decided on the protocol,
        // not on this host's switches: another host may be an owner
        let reuse_released = reuse_released_for(client);
        commit_with(client, previous, ours, reuse_released)
    }

    /// See [`super::host::may_reuse_released`].
    pub fn reuse_released_for(client: &DriverClient) -> bool {
        super::host::may_reuse_released(client.negotiated().protocol)
    }

    fn commit_with(
        client: &mut DriverClient,
        previous: &[u32],
        mut ours: Vec<Monitor>,
        reuse_released: bool,
    ) -> Result<Vec<Monitor>, String> {
        let _guard = lock()?;
        let me: Owner = (
            std::process::id(),
            process_started(std::process::id()).unwrap_or(0),
        );
        let live = client
            .request_live_state()
            .unwrap_or_else(|_| client.refresh_state().to_vec());
        let mut reg = read_registry();
        reg.retain(|id, _| live.iter().any(|m| m.id == *id));
        for owner in reg.values_mut() {
            if *owner != RELEASED && *owner != me && !owner_alive(*owner, &process_started) {
                *owner = RELEASED;
            }
        }
        for id in previous {
            if !ours.iter().any(|m| m.id == *id) {
                reg.insert(*id, RELEASED);
            }
        }
        let mut list = live.clone();
        let mut released: Vec<u32> = reg
            .iter()
            .filter(|(_, o)| **o == RELEASED)
            .map(|(i, _)| *i)
            .collect();
        for m in ours.iter_mut() {
            if m.id == ALLOCATE {
                let ids: Vec<u32> = list.iter().map(|m| m.id).collect();
                m.id = allocate(&ids, if reuse_released { &released } else { &[] });
                released.retain(|x| *x != m.id);
            }
            reg.insert(m.id, me);
            match list.iter_mut().find(|x| x.id == m.id) {
                Some(slot) => *slot = m.clone(),
                None => list.push(m.clone()),
            }
        }
        let mut ids: Vec<u32> = list.iter().map(|m| m.id).collect();
        let removed = compact(&mut ids, &mut released);
        list.retain(|m| ids.contains(&m.id));
        for id in &removed {
            reg.remove(id);
        }
        if !attachable(&ids) {
            teprintln!(
                "[virtual-display] shared driver: the list {ids:?} is not one run from 0 or 1; \
                 the driver will attach none of it"
            );
        }
        let changed = list != live;
        if changed {
            client
                .set_monitors(&list)
                .map_err(|e| format!("set monitors: {e}"))?;
            client.notify().map_err(|e| format!("notify driver: {e}"))?;
            // hold the lock until the driver reports the list, so the next host reads it
            let want: Vec<u32> = {
                let mut v = ids.clone();
                v.sort_unstable();
                v
            };
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Ok(state) = client.request_live_state() {
                    let mut have: Vec<u32> = state.iter().map(|m| m.id).collect();
                    have.sort_unstable();
                    if have == want {
                        break;
                    }
                }
                if Instant::now() >= deadline {
                    teprintln!(
                        "[virtual-display] shared driver: the driver never reported {want:?}"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        write_registry(&reg);
        tprintln!(
            "[virtual-display] shared driver: ours {:?}, machine {:?}, released {:?}, removed {:?}",
            ours.iter().map(|m| m.id).collect::<Vec<_>>(),
            ids,
            released,
            removed
        );
        Ok(ours)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// A pid the system has recycled after a crash is a different process, and the dead
        /// host's monitors must read as released, not as owned by whoever got the pid.
        #[test]
        fn a_recycled_pid_does_not_keep_a_dead_hosts_monitors() {
            let started = |pid: u32| match pid {
                10 => Some(500),
                _ => None,
            };
            assert!(owner_alive((10, 500), &started), "same pid, same process");
            assert!(
                !owner_alive((10, 499), &started),
                "same pid, a different process"
            );
            assert!(!owner_alive((11, 500), &started), "not running");
            assert!(!owner_alive(RELEASED, &started));
            // a host that could not read its own creation time still counts as alive by pid
            assert!(owner_alive((10, 0), &started));
        }

        #[test]
        fn only_one_run_from_zero_or_one_attaches() {
            for ok in [&[][..], &[0], &[1], &[1, 2], &[0, 1, 2, 3, 4], &[2, 1, 0]] {
                assert!(attachable(ok), "{ok:?}");
            }
            for bad in [&[2][..], &[4], &[16], &[0, 2], &[0, 1, 3], &[0, 0]] {
                assert!(!attachable(bad), "{bad:?}");
            }
        }

        #[test]
        fn allocation_reuses_released_then_extends_the_run() {
            assert_eq!(allocate(&[], &[]), 0);
            assert_eq!(allocate(&[0, 1], &[]), 2);
            assert_eq!(allocate(&[1, 2], &[]), 0);
            assert_eq!(allocate(&[0, 1, 2], &[1]), 1);
        }

        #[test]
        fn a_released_middle_id_stays_until_the_top_goes() {
            let (mut ids, mut rel) = (vec![0, 1, 2], vec![1]);
            assert!(compact(&mut ids, &mut rel).is_empty());
            assert_eq!(ids, vec![0, 1, 2]);
            let (mut ids, mut rel) = (vec![0, 1, 2], vec![1, 2]);
            compact(&mut ids, &mut rel);
            assert_eq!(ids, vec![0]);
            let (mut ids, mut rel) = (vec![0, 1, 2], vec![0]);
            compact(&mut ids, &mut rel);
            assert_eq!(
                ids,
                vec![1, 2],
                "removing 0 leaves a run from 1, which attaches"
            );
            let (mut ids, mut rel) = (vec![0, 1, 2], vec![0, 1, 2]);
            compact(&mut ids, &mut rel);
            assert!(ids.is_empty());
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
        // the pid rides in `holder`, never in the wire name: `doctor --json` and the startup gate
        // compare the name against a literal
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

    /// A refused connect can mean "occupied", and only two OS errors mean that. Anything else
    /// is a driver that is not answering, and treating it as busy would hide a real fault
    /// behind "quit the other instance".
    #[test]
    fn only_the_pipe_busy_errors_mean_occupied() {
        assert!(is_busy_os_error(ERROR_PIPE_BUSY));
        assert!(is_busy_os_error(ERROR_SEM_TIMEOUT));
        // 5 is ERROR_ACCESS_DENIED, 109 ERROR_BROKEN_PIPE
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
        // ERROR_ACCESS_DENIED: the pipe is there and would not have us, which no install can
        // fix, so it stays "not answering"
        assert_eq!(
            classify_init_error(&connect_error(5)),
            DisplayBackend::DriverNotResponding
        );
        // a handshake timeout with no other process to be holding the driver: a wedged driver
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
    /// connect, so a named pipe of the test's own reproduces it exactly.
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
        // one client occupies the only instance, exactly as the running host does
        let _client = rt
            .block_on(async { ClientOptions::new().open(&taken_path) })
            .expect("occupy the pipe");
        match probe_pipe(&taken) {
            DisplayBackend::DriverBusy { .. } => {}
            other => panic!("an occupied pipe classified as {other:?}, not DriverBusy"),
        }

        // a pipe that lets us in and then says nothing: a wedged driver, unless one of our own
        // processes is up, which is the busy case by definition
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
        // another test in this binary can start or end a copy of the executable at any moment,
        // so the holder is asked on both sides of the probe
        let before = driver_pipe_holder();
        match probe_pipe(&silent) {
            DisplayBackend::DriverNotResponding => {}
            DisplayBackend::DriverBusy { .. }
                if before.is_some() || driver_pipe_holder().is_some() =>
            {
                crate::test_support::skip(
                    "the wedged-driver probe",
                    "another ScreenExtend process is running, so a silent pipe reads as busy",
                );
            }
            other => panic!("a silent pipe classified as {other:?}, not DriverNotResponding"),
        }

        assert_eq!(
            probe_pipe("screenextend-probe-absent"),
            DisplayBackend::DriverNotInstalled
        );
    }

    /// What the real driver does to a second client: one client already open, a second asking.
    /// Prints the error the second connect gets, so the classification can be checked against
    /// the shipped driver. Needs the driver installed and nothing else using it. Run with
    /// `--ignored --nocapture`.
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

    /// Who this machine says is holding the driver, and what the busy line would read.
    ///
    /// The shipped driver will not enter the busy state on demand (it answers a state request
    /// in under a millisecond, even while a monitor attaches), so the classification is checked
    /// against a pipe of our own (`an_occupied_pipe_is_busy_and_a_silent_one_is_not_responding`)
    /// and the holder lookup here, against whatever host is running. Start `ScreenExtend serve`
    /// first and this prints its pid. Run with `--ignored --nocapture`.
    #[test]
    #[ignore = "reports which process holds the display driver; start a host first; run with --ignored --nocapture"]
    fn report_driver_holder() {
        // `cli::dispatch` does this in a real process; a test binary never gets there
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let path = std::path::Path::new(&local)
                .join("app.screenextend.desktop")
                .join(crate::single_instance::CTRL_FILE);
            println!(
                "control file: {} (exists: {})",
                path.display(),
                path.exists()
            );
            crate::single_instance::remember_ctrl_path(path);
        }
        println!("this process:            {}", std::process::id());
        println!(
            "control socket answers:  {:?}",
            crate::single_instance::running_instance_pid()
        );
        println!("driver_pipe_holder():    {:?}", driver_pipe_holder());
        let backend = DisplayBackend::DriverBusy {
            holder: driver_pipe_holder(),
        };
        println!(
            "the doctor line would read: {}",
            crate::cli::driver_line(backend)
        );
    }

    #[cfg(test)]
    fn raw_os_error_of(e: &InitError) -> Option<i32> {
        match e {
            InitError::Connect(ConnectionError::Failed(io)) => io.raw_os_error(),
            InitError::RequestState(_) => None,
        }
    }

    /// Real adapter paths, as `DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME` reports them: the
    /// bundled IDD's adapter (devnode `ROOT\DISPLAY\0000`, hardware id
    /// `Root\VirtualDisplayDriver`) and an Intel GPU driving a laptop panel. The IDD monitor
    /// reports output technology HDMI, like a real HDMI monitor; the adapter tells them apart.
    const IDD_ADAPTER: &str = r"\\?\ROOT#DISPLAY#0000#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}";
    const GPU_ADAPTER: &str = r"\\?\PCI#VEN_8086&DEV_46A3&SUBSYS_13241462&REV_0C#3&11583659&1&10#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}";

    fn driver() -> Vec<String> {
        vec![r"ROOT\DISPLAY\0000".to_string()]
    }

    fn on_driver(p: &DisplayPath) -> bool {
        on_driver_adapter(&p.target_adapter, &driver())
    }

    fn path(
        gdi: &str,
        friendly: &str,
        device_path: &str,
        adapter: &str,
        source: u32,
    ) -> DisplayPath {
        DisplayPath {
            gdi_name: gdi.to_string(),
            friendly: friendly.to_string(),
            device_path: device_path.to_string(),
            target_adapter: adapter.to_string(),
            adapter_id: (0x1234, 0),
            source_id: source,
        }
    }

    /// The driver's pipe dies under a running host (its `WUDFHost.exe` restarted) and a new
    /// one comes up. The next create reconnects, re-sends everything the host holds, and
    /// succeeds. Against a mock pipe of the test's own: nothing here reaches the real driver or
    /// creates a display.
    #[test]
    fn a_host_reconnects_when_the_driver_pipe_dies() {
        use crate::driver_ipc::mock::MockServer;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let name = format!("screenextend-test-reconnect-{}", std::process::id());
        let serve = |rt: &tokio::runtime::Runtime| {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<Monitor>>();
            let mut server = rt.block_on(async { MockServer::new(&name) });
            let task = rt.spawn(async move {
                loop {
                    server.pump().await;
                    let _ = tx.send(server.state().to_vec());
                }
            });
            (task, rx)
        };

        let (first, _first_states) = serve(&rt);
        let display = WindowsVirtualDisplay::connect_reported(&name, false)
            .expect("connect to the mock driver");
        let a = display
            .create_display("a".to_string(), 1280, 720, 60)
            .expect("the first create");
        assert_eq!(a, 0);

        // the driver goes away, and a new one serves the same name
        first.abort();
        std::thread::sleep(Duration::from_millis(200));
        let (_second, second_states) = serve(&rt);

        let b = display
            .create_display("b".to_string(), 1280, 720, 60)
            .expect("the create after the driver restarted reconnects");
        assert_eq!(b, 1);
        let mut latest = Vec::new();
        while let Ok(state) = second_states.recv_timeout(Duration::from_secs(2)) {
            latest = state;
            if latest.len() == 2 {
                break;
            }
        }
        let ids: Vec<u32> = latest.iter().map(|m| m.id).collect();
        assert_eq!(ids, vec![0, 1], "the new driver holds both displays");
    }

    #[test]
    fn an_adapter_path_names_its_devnode() {
        assert_eq!(
            adapter_instance_id(IDD_ADAPTER).as_deref(),
            Some(r"ROOT\DISPLAY\0000")
        );
        assert_eq!(
            adapter_instance_id(GPU_ADAPTER).as_deref(),
            Some(r"PCI\VEN_8086&DEV_46A3&SUBSYS_13241462&REV_0C\3&11583659&1&10")
        );
        for junk in [
            "",
            r"\\?\",
            r"\\?\#{x}",
            "ROOT#DISPLAY#0000#{x}",
            r"\\?\ROOT#DISPLAY#0000",
        ] {
            assert_eq!(adapter_instance_id(junk), None, "{junk:?}");
        }
        assert!(on_driver_adapter(IDD_ADAPTER, &driver()));
        // case-insensitive: SetupAPI and the display stack disagree on case
        assert!(on_driver_adapter(&IDD_ADAPTER.to_lowercase(), &driver()));
        assert!(!on_driver_adapter(GPU_ADAPTER, &driver()));
        // another root-enumerated display adapter (a second IDD product) is not ours
        assert!(!on_driver_adapter(
            r"\\?\ROOT#DISPLAY#0001#{5b45201d-f2f2-4f3b-85bb-30ff1f953599}",
            &driver()
        ));
        assert!(!on_driver_adapter(IDD_ADAPTER, &[]));
        assert!(!on_driver_adapter("", &driver()));
    }

    /// The whole of "a display write may only land on a display we created", against fabricated
    /// `QueryDisplayConfig` output.
    ///
    /// The case that matters is the recycled name: Windows hands a departed display's
    /// `\\.\DISPLAYn` to whatever attaches next (here the laptop's own panel), and a mode or DPI
    /// write addressed by that name would land on it.
    #[test]
    fn a_write_only_ever_lands_on_a_display_we_created() {
        // measured: the IDD monitor reports a real HDMI monitor's output technology and the
        // driver's own EDID name, so only "this host created it" tells it from a user's monitor
        let ours = path(
            r"\\.\DISPLAY9",
            "VirtuDisplay+",
            r"\\?\DISPLAY#CHY0000#1&28a6823a&e&UID256",
            IDD_ADAPTER,
            8,
        );
        let panel = path(
            r"\\.\DISPLAY1",
            "",
            r"\\?\DISPLAY#BOE0A00#4&26d38288&0&UID8388688",
            GPU_ADAPTER,
            0,
        );
        let live = vec![panel.clone(), ours.clone()];
        let mine = ours.device_path.clone();
        let owned = move |p: &str| p == mine;

        // a path recorded as ours that now sits on somebody else's adapter is refused too: the
        // owned set and the adapter are two locks on the same door
        let everything = |_: &str| true;
        let panel_target = DisplayTarget {
            device_path: panel.device_path.clone(),
            device_name: panel.gdi_name.clone(),
        };
        let err = pick_owned_display(&live, &panel_target, &everything, &on_driver)
            .expect_err("the panel is never written to");
        assert!(
            err.to_string()
                .contains("not on the ScreenExtend display driver's adapter"),
            "unexpected: {err}"
        );

        // ordinary case: resolves to the name and the adapter/source pair a DPI write needs
        let target = DisplayTarget {
            device_path: ours.device_path.clone(),
            device_name: ours.gdi_name.clone(),
        };
        let hit = pick_owned_display(&live, &target, &owned, &on_driver)
            .expect("our own display resolves");
        assert_eq!(hit.gdi_name, r"\\.\DISPLAY9");
        assert_eq!(hit.source_id, 8);

        // the name moved while the display stayed: the write follows the monitor
        let renamed = vec![
            path(r"\\.\DISPLAY1", "", &panel.device_path, GPU_ADAPTER, 0),
            path(
                r"\\.\DISPLAY5",
                "VirtuDisplay+",
                &ours.device_path,
                IDD_ADAPTER,
                4,
            ),
        ];
        let hit = pick_owned_display(&renamed, &target, &owned, &on_driver)
            .expect("the monitor is still here");
        assert_eq!(hit.gdi_name, r"\\.\DISPLAY5");
        assert_eq!(hit.source_id, 4);

        // the display is gone and the panel has inherited its name: this must refuse, never
        // fall through and write a mode and a DPI to the machine's own screen
        let recycled = vec![path(
            r"\\.\DISPLAY9",
            "",
            &panel.device_path,
            GPU_ADAPTER,
            0,
        )];
        let err = pick_owned_display(&recycled, &target, &owned, &on_driver)
            .expect_err("a departed display refuses");
        assert!(
            err.to_string().contains("is not attached"),
            "unexpected: {err}"
        );

        // with a name alone (how the probe harness calls) the panel is still refused: it is not
        // in the owned set
        let name_only = DisplayTarget {
            device_path: String::new(),
            device_name: r"\\.\DISPLAY9".to_string(),
        };
        let err = pick_owned_display(&recycled, &name_only, &owned, &on_driver)
            .expect_err("a display we did not create refuses");
        assert!(
            err.to_string().contains("did not create"),
            "unexpected: {err}"
        );
        // the same name against our own display is allowed, which keeps the probe harness and
        // the `--ignored` mode tests working
        assert_eq!(
            pick_owned_display(&live, &name_only, &owned, &on_driver)
                .expect("ours by name")
                .gdi_name,
            r"\\.\DISPLAY9"
        );

        // a path that is attached but was never created here is refused before the sweep runs:
        // holding an identity does not get a caller onto another display
        let not_ours = DisplayTarget {
            device_path: panel.device_path.clone(),
            device_name: panel.gdi_name.clone(),
        };
        let err = pick_owned_display(&live, &not_ours, &owned, &on_driver)
            .expect_err("the panel refuses");
        assert!(
            err.to_string().contains("was not created by this host"),
            "unexpected: {err}"
        );

        // nothing matches: refuse rather than pick the first display on the machine
        let elsewhere = DisplayTarget {
            device_path: r"\\?\DISPLAY#CHY0000#2&other".to_string(),
            device_name: r"\\.\DISPLAY7".to_string(),
        };
        assert!(pick_owned_display(&live, &elsewhere, &owned, &on_driver).is_err());
        assert!(pick_owned_display(&[], &target, &owned, &on_driver).is_err());

        // two paths carrying one identity (a clone topology) is ambiguous: refuse
        let cloned = vec![
            ours.clone(),
            path(
                r"\\.\DISPLAY6",
                "VirtuDisplay+",
                &ours.device_path,
                IDD_ADAPTER,
                5,
            ),
        ];
        assert!(pick_owned_display(&cloned, &target, &owned, &on_driver).is_err());
    }

    /// The correlation records an identity only for a monitor that was not attached before.
    #[test]
    fn a_display_that_was_already_attached_is_never_recorded_as_ours() {
        let panel = path(
            r"\\.\DISPLAY1",
            "",
            r"\\?\DISPLAY#BOE0A00#4&26d38288&0&UID8388688",
            GPU_ADAPTER,
            0,
        );
        let fresh = path(
            r"\\.\DISPLAY9",
            "VirtuDisplay+",
            r"\\?\DISPLAY#CHY0000#1&28a6823a&e&UID256",
            IDD_ADAPTER,
            8,
        );
        let before = vec![panel.device_path.clone()];
        let after = vec![panel.clone(), fresh.clone()];

        assert_eq!(
            pick_new_identity(&after, r"\\.\DISPLAY9", &before, &driver()),
            Ok(fresh.device_path.clone())
        );
        // the monitor diff handed back the name of a display that was already there: the
        // mis-correlation must produce no identity rather than a wrong one
        assert!(pick_new_identity(&after, r"\\.\DISPLAY1", &before, &driver()).is_err());
        assert!(pick_new_identity(&after, r"\\.\DISPLAY4", &before, &driver()).is_err());
        // a path Windows could not describe is not an identity either
        let nameless = vec![path(r"\\.\DISPLAY9", "", "", IDD_ADAPTER, 8)];
        assert!(pick_new_identity(&nameless, r"\\.\DISPLAY9", &before, &driver()).is_err());
    }

    /// A laptop on "second screen only" has its panel connected but inactive. Something brings
    /// it up inside the attach window and the diff hands back its name. It is not ours: it is
    /// on the GPU's adapter, and with the inactive targets in the before-set it is not new
    /// either.
    #[test]
    fn a_monitor_that_comes_up_during_the_attach_is_never_claimed() {
        let panel = path(
            r"\\.\DISPLAY1",
            "",
            r"\\?\DISPLAY#BOE0A00#4&26d38288&0&UID8388688",
            GPU_ADAPTER,
            0,
        );
        // not in `before` at all, as an active-only snapshot would have it
        let err = pick_new_identity(
            std::slice::from_ref(&panel),
            r"\\.\DISPLAY1",
            &[],
            &driver(),
        )
        .expect_err("a monitor on the GPU's adapter is never ours");
        assert!(
            err.contains("not on the ScreenExtend display driver's adapter"),
            "{err}"
        );
        // a hot-plugged external monitor is the same case on another adapter path
        let tv = path(
            r"\\.\DISPLAY3",
            "LG TV",
            r"\\?\DISPLAY#GSM5B09#5&1&0&UID4352",
            GPU_ADAPTER,
            2,
        );
        assert!(pick_new_identity(&[tv], r"\\.\DISPLAY3", &[], &driver()).is_err());
        // with no driver devnode found, nothing can be shown to be ours
        let ours = path(
            r"\\.\DISPLAY9",
            "VirtuDisplay+",
            r"\\?\DISPLAY#CHY0000#1&28a6823a&e&UID256",
            IDD_ADAPTER,
            8,
        );
        assert!(pick_new_identity(std::slice::from_ref(&ours), r"\\.\DISPLAY9", &[], &[]).is_err());
        assert!(pick_new_identity(&[ours], r"\\.\DISPLAY9", &[], &driver()).is_ok());
    }

    /// A before-snapshot that failed must not read as "nothing was attached before".
    #[test]
    fn a_failed_snapshot_refuses_every_claim() {
        let ours = path(
            r"\\.\DISPLAY9",
            "VirtuDisplay+",
            r"\\?\DISPLAY#CHY0000#1&28a6823a&e&UID256",
            IDD_ADAPTER,
            8,
        );
        let failed = vec![SNAPSHOT_FAILED.to_string()];
        let err = pick_new_identity(&[ours], r"\\.\DISPLAY9", &failed, &driver())
            .expect_err("no snapshot, no claim");
        assert!(err.contains("snapshot"), "{err}");
    }

    /// Two targets on one source answer to one `\\.\DISPLAYn`; a write to it lands on both.
    #[test]
    fn a_duplicated_name_is_never_claimed() {
        let ours = path(
            r"\\.\DISPLAY2",
            "VirtuDisplay+",
            r"\\?\DISPLAY#CHY0000#1&28a6823a&e&UID256",
            IDD_ADAPTER,
            1,
        );
        let twin = path(
            r"\\.\DISPLAY2",
            "",
            r"\\?\DISPLAY#BOE0A00#4&26d38288&0&UID8388688",
            GPU_ADAPTER,
            1,
        );
        assert!(pick_new_identity(&[ours, twin], r"\\.\DISPLAY2", &[], &driver()).is_err());
    }

    fn row(source: u32, target: u32, active: bool, available: bool, ours: bool) -> TopologyRow {
        // adapter 1 is ours, adapter 2 the user's GPU; a row's source and target sit on the same
        // adapter, as QDC_ALL_PATHS lists them
        let adapter = if ours { 1 } else { 2 };
        TopologyRow {
            source: (adapter, 0, source),
            target: (adapter, 0, target),
            active,
            available,
            ours,
            parked: false,
            primary: false,
        }
    }

    fn parked(mut r: TopologyRow) -> TopologyRow {
        r.parked = true;
        r
    }

    /// A released monitor held in the driver's list for contiguity comes off the desktop, and
    /// nothing else is named: not the user's displays, not a live one of ours.
    #[test]
    fn a_parked_display_is_the_only_row_taken_off_the_desktop() {
        let rows = vec![
            row(0, 100, true, true, false),         // the panel
            row(1, 101, true, true, false),         // the user's external monitor
            row(0, 256, true, true, true),          // a live display of ours
            parked(row(1, 257, true, true, true)),  // released, held for the run, still active
            parked(row(2, 257, false, true, true)), // the same target's other source
            parked(row(3, 258, false, true, true)), // released and already off the desktop
        ];
        assert_eq!(plan_parking(&rows), vec![3]);
        // nothing parked and active: nothing to do
        assert!(plan_parking(&rows[..3]).is_empty());
        // never a plan that leaves no active path at all
        let alone = vec![parked(row(0, 256, true, true, true))];
        assert!(plan_parking(&alone).is_empty());
        // never the primary display
        let mut primary = rows.clone();
        primary[3].primary = true;
        assert!(plan_parking(&primary).is_empty());
    }

    /// The join's "give our display a desktop" never brings a parked one back.
    #[test]
    fn a_parked_display_is_never_activated() {
        let rows = vec![
            row(0, 100, true, true, false),
            parked(row(1, 257, false, true, true)), // released, off the desktop
            parked(row(2, 257, false, true, true)),
        ];
        assert_eq!(plan_driver_paths(&rows), None);
        // the new display beside it still gets its source
        let mut with_new = rows.clone();
        with_new.push(row(1, 258, false, true, true));
        let plan = plan_driver_paths(&with_new).expect("the new display needs a source");
        assert_eq!(plan.activate, vec![3]);
    }

    /// The before-create snapshot keeps every monitor that is connected or on the desktop, and
    /// every target that is not ours; it drops only a departed monitor of ours, whose path a
    /// rejoin given the same id comes back with.
    #[test]
    fn a_departed_display_of_ours_is_not_in_the_before_set() {
        // (active, available, ours)
        assert!(in_before_set(true, true, true), "a live display of ours");
        assert!(
            in_before_set(false, true, true),
            "ours, connected, off the desktop"
        );
        assert!(!in_before_set(false, false, true), "ours, departed");
        assert!(
            in_before_set(false, true, false),
            "the user's switched-off monitor"
        );
        assert!(
            in_before_set(false, false, false),
            "the user's unplugged monitor"
        );
        assert!(in_before_set(true, true, false), "the panel");
    }

    /// A removal whose commit fails leaves the driver holding everything it held, so the id
    /// stays recorded as released and is compacted away by the next removal, instead of being
    /// re-sent forever as a display somebody owns. Against a mock pipe of the test's own;
    /// nothing reaches the real driver.
    #[test]
    fn a_failed_removal_keeps_the_id_released() {
        use crate::driver_ipc::mock::MockServer;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let name = format!("screenextend-test-failed-remove-{}", std::process::id());
        let serve = |rt: &tokio::runtime::Runtime| {
            let (tx, rx) = std::sync::mpsc::channel::<Vec<Monitor>>();
            let mut server = rt.block_on(async { MockServer::new(&name) });
            let task = rt.spawn(async move {
                loop {
                    server.pump().await;
                    let _ = tx.send(server.state().to_vec());
                }
            });
            (task, rx)
        };

        let (first, _states) = serve(&rt);
        let display = WindowsVirtualDisplay::connect_reported(&name, false)
            .expect("connect to the mock driver");
        for n in ["a", "b", "c"] {
            display
                .create_display(n.to_string(), 1280, 720, 60)
                .expect("create");
        }

        // the driver goes away and nothing serves the pipe: the removal of the top id fails
        first.abort();
        std::thread::sleep(Duration::from_millis(200));
        display.remove_display(2);
        {
            let inner = display.inner.lock_recover();
            let owned: Vec<u32> = inner.owned.iter().map(|m| m.id).collect();
            assert_eq!(owned, vec![0, 1, 2], "the driver was never told");
            assert_eq!(inner.released, vec![2], "the id is still released");
        }

        // the driver is back, and the middle one leaves too: both go, in one commit
        let (_second, states) = serve(&rt);
        display.remove_display(1);
        let mut latest = None;
        while let Ok(state) = states.recv_timeout(Duration::from_secs(2)) {
            let ids: Vec<u32> = state.iter().map(|m| m.id).collect();
            latest = Some(ids.clone());
            if ids == vec![0] {
                break;
            }
        }
        assert_eq!(latest, Some(vec![0]));
        let inner = display.inner.lock_recover();
        assert!(inner.released.is_empty(), "{:?}", inner.released);
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    }

    const NV: v2::Luid = v2::Luid {
        low: 97388,
        high: 0,
    };
    const IGPU: v2::Luid = v2::Luid {
        low: 74978,
        high: 0,
    };

    fn fake_adapters() -> Vec<host::AdapterCandidate> {
        use crate::windows_utils::streamer::{INTEL_PCI_VENDOR_ID, NVIDIA_PCI_VENDOR_ID};
        vec![
            host::AdapterCandidate {
                luid: NV,
                vendor_id: NVIDIA_PCI_VENDOR_ID,
                software: false,
                ours: false,
                description: "dGPU".into(),
            },
            host::AdapterCandidate {
                luid: IGPU,
                vendor_id: INTEL_PCI_VENDOR_ID,
                software: false,
                ours: false,
                description: "iGPU".into(),
            },
        ]
    }

    fn encoder(vendor: crate::streamer::config::EncoderVendor) -> EncoderSource {
        EncoderSource {
            vendor: Arc::new(Mutex::new(vendor)),
            software: Arc::new(AtomicBool::new(false)),
        }
    }

    fn requests(mock: &crate::driver_ipc::mock::V2Mock, name: &str) -> usize {
        mock.received().iter().filter(|m| m.name() == name).count()
    }

    fn wait_for_names(
        mock: &crate::driver_ipc::mock::V2Mock,
        what: &str,
        mut ok: impl FnMut() -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok() {
            let names: Vec<String> = mock.received().iter().map(|m| m.name()).collect();
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {names:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ok() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    const OWNED_ONLY: LinkOptions = LinkOptions {
        owned_monitors: true,
        render_follows_encoder: false,
    };
    const RENDER_ONLY: LinkOptions = LinkOptions {
        owned_monitors: false,
        render_follows_encoder: true,
    };

    /// Both switches off, against a driver that offers everything: not one versioned request
    /// reaches it, through a whole create / remove / stop cycle.
    #[test]
    fn with_both_switches_off_not_one_new_byte_is_sent() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-off-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let mut display = WindowsVirtualDisplay::connect_with(
            &name,
            false,
            LinkOptions::OFF,
            Some(encoder(crate::streamer::config::EncoderVendor::Auto)),
        )
        .expect("connect");
        display.candidates = fake_adapters;
        let a = display
            .create_display("a".into(), 1280, 720, 60)
            .expect("create");
        display.remove_display(a);
        display.remove_all_displays();
        // notifies are not answered; wait until the mock has read the create's and the removal's
        wait_for("the notifies", || requests(&mock, "Notify") >= 3);
        let received = mock.received();
        assert!(received.iter().any(|m| m.name() == "Notify"));
        assert!(
            received.iter().all(|m| !m.is_v2_request()),
            "{:?}",
            received.iter().map(|m| m.name()).collect::<Vec<_>>()
        );
        assert!(received
            .iter()
            .all(|m| serde_json::from_str::<crate::driver_ipc::ServerCommand>(&m.text).is_ok()));
    }

    /// The connection dies under a streaming host. The link watcher reconnects at once with
    /// the same owner token, so the driver keeps the displays, and the token is never logged.
    #[test]
    fn the_owner_token_is_presented_again_after_a_reconnect_and_never_logged() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-owner-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        assert!(display.owned_by_driver.load(Ordering::Acquire));
        WindowsVirtualDisplay::watch_link(&display);
        display
            .create_display("a".into(), 1280, 720, 60)
            .expect("create");
        wait_for("the create", || mock.monitors().len() == 1);

        mock.drop_connections();
        wait_for("a second Hello", || requests(&mock, "Hello") >= 2);
        // the create's Notify and the re-sent list (an empty list needs no start-up clear)
        wait_for_names(&mock, "the list re-sent", || {
            mock.received()
                .iter()
                .filter(|m| m.name() == "Notify")
                .count()
                >= 2
        });
        assert_eq!(mock.monitors().len(), 1, "the display is still there");

        let token = host::owner_token().expect("a token").reveal().to_string();
        let hellos: Vec<_> = mock
            .received()
            .into_iter()
            .filter(|m| m.name() == "Hello")
            .collect();
        assert_ne!(hellos[0].connection, hellos[1].connection);
        for h in &hellos {
            let v: serde_json::Value = serde_json::from_str(&h.text).unwrap();
            assert_eq!(v["Hello"]["owner"]["token"], token.as_str());
            assert_eq!(v["Hello"]["owner"]["grace_ms"], host::OWNER_GRACE_MS);
        }
        std::thread::sleep(Duration::from_millis(300));
        let lines = crate::logbus::backlog_snapshot();
        assert!(
            lines.iter().any(|l| l.contains("owned monitors")),
            "the owner line was logged"
        );
        assert!(
            lines.iter().all(|l| !l.contains(&token)),
            "the full token reached the log"
        );
    }

    /// On an old driver the owner switch changes nothing: no watcher, no versioned request.
    #[test]
    fn an_old_driver_takes_todays_path_with_the_owner_switch_on() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-old-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    serve_v2: false,
                    ..Default::default()
                },
            )
        });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        assert!(!display.owned_by_driver.load(Ordering::Acquire));
        WindowsVirtualDisplay::watch_link(&display);
        display
            .create_display("a".into(), 1280, 720, 60)
            .expect("create");
        wait_for("the notifies", || requests(&mock, "Notify") >= 2);
        let received = mock.received();
        assert!(received.iter().all(|m| !m.on_v2_pipe && !m.is_v2_request()));
        // the startup clear is still there for a driver that cannot remove a dead host's
        // displays itself
        assert_eq!(received[1].name(), "Notify");
    }

    /// The preference follows the encoder choice at each create, is sent only when it changes
    /// something, and is cleared when the host stops.
    #[test]
    fn the_render_adapter_follows_the_encoder_and_is_cleared_on_stop() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        use crate::streamer::config::EncoderVendor;
        let rt = rt();
        let name = format!("screenextend-test-vd-render-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let source = encoder(EncoderVendor::Auto);
        let mut display =
            WindowsVirtualDisplay::connect_with(&name, false, RENDER_ONLY, Some(source.clone()))
                .expect("connect");
        display.candidates = fake_adapters;

        display.create_display("a".into(), 1280, 720, 60).unwrap();
        assert_eq!(mock.preferred(), Some(NV));
        display.create_display("b".into(), 1280, 720, 60).unwrap();
        assert_eq!(
            requests(&mock, "SetRenderAdapter"),
            1,
            "unchanged: not re-sent"
        );

        *source.vendor.lock_recover() = EncoderVendor::Intel;
        display.create_display("c".into(), 1280, 720, 60).unwrap();
        assert_eq!(mock.preferred(), Some(IGPU), "the newest session wins");

        source.software.store(true, Ordering::Relaxed);
        display.create_display("d".into(), 1280, 720, 60).unwrap();
        assert_eq!(mock.preferred(), None, "x264 asks for nothing");
        assert_eq!(requests(&mock, "SetRenderAdapter"), 3);

        source.software.store(false, Ordering::Relaxed);
        display.create_display("e".into(), 1280, 720, 60).unwrap();
        assert_eq!(mock.preferred(), Some(IGPU));
        display.remove_all_displays();
        assert_eq!(mock.preferred(), None, "cleared as the host stops");
        assert_eq!(requests(&mock, "SetRenderAdapter"), 5);
        // every SetRenderAdapter went out before the Notify that created its display
        let names: Vec<String> = mock.received().iter().map(|m| m.name()).collect();
        let first_set = names.iter().position(|n| n == "SetRenderAdapter").unwrap();
        let notifies_before = names[..first_set].iter().filter(|n| *n == "Notify").count();
        assert_eq!(
            notifies_before, 0,
            "nothing to clear at start-up, so the create's Notify is the first: {names:?}"
        );
    }

    /// While duplication owns the render preference the encoder that started does not move it,
    /// so the adapter duplication asked for stays; otherwise the encoder is followed.
    #[test]
    fn the_encoder_follow_stands_down_while_duplication_owns_the_preference() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        use crate::streamer::config::EncoderVendor;
        let rt = rt();
        let name = format!("screenextend-test-vd-dupowns-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let mut display = WindowsVirtualDisplay::connect_with(
            &name,
            false,
            RENDER_ONLY,
            Some(encoder(EncoderVendor::Auto)),
        )
        .expect("connect");
        display.candidates = fake_adapters;
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        assert_eq!(mock.preferred(), Some(NV));
        let sent = requests(&mock, "SetRenderAdapter");

        display.duplication_owns = || true;
        display.encoder_started(Some(IGPU));
        assert_eq!(mock.preferred(), Some(NV), "duplication's adapter stays");
        assert_eq!(requests(&mock, "SetRenderAdapter"), sent, "nothing sent");

        display.duplication_owns = || false;
        display.encoder_started(Some(IGPU));
        assert_eq!(
            mock.preferred(),
            Some(IGPU),
            "followed once duplication does not own it"
        );
    }

    /// A driver whose Hello does not list `render_adapter` (a build that withdrew it): with the
    /// follow switch on, creates, the encoder starting on either adapter, and the host's stop
    /// send no `SetRenderAdapter` at all, and nothing fails.
    #[test]
    fn a_driver_without_render_adapter_is_never_asked() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        use crate::streamer::config::EncoderVendor;
        let rt = rt();
        let name = format!("screenextend-test-vd-norender-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    capabilities: vec![v2::CAP_HELLO, v2::CAP_DIAGNOSTICS, v2::CAP_OWNED_MONITORS],
                    ..Default::default()
                },
            )
        });
        let mut display = WindowsVirtualDisplay::connect_with(
            &name,
            false,
            RENDER_ONLY,
            Some(encoder(EncoderVendor::Auto)),
        )
        .expect("connect");
        display.candidates = fake_adapters;
        assert!(!display
            .inner
            .lock_recover()
            .client
            .negotiated()
            .has(v2::CAP_RENDER_ADAPTER));
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        display.create_display("b".into(), 1280, 720, 60).unwrap();
        display.encoder_started(Some(IGPU));
        display.encoder_started(Some(NV));
        display.duplication_owns = || true;
        display.create_display("c".into(), 1280, 720, 60).unwrap();
        display.remove_all_displays();
        assert_eq!(requests(&mock, "SetRenderAdapter"), 0);
        assert_eq!(requests(&mock, "Hello"), 1, "no retry");
        assert_eq!(mock.preferred(), None);
    }

    /// The capture path's request to a driver that is alive and never answers costs its budget
    /// and no more, once: no second try, and nothing recorded as the driver's preference.
    #[test]
    fn a_silent_driver_costs_the_compose_request_its_budget_and_no_more() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-silent-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    render_silent: true,
                    ..Default::default()
                },
            )
        });
        let display =
            WindowsVirtualDisplay::connect_with(&name, false, RENDER_ONLY, None).expect("connect");
        let client = display.inner.lock_recover().client.duplicate();
        let started = Instant::now();
        let took = request_composition(&client, &display.render, NV, Duration::from_millis(300));
        let spent = started.elapsed();
        assert!(!took);
        assert!(spent >= Duration::from_millis(300), "{spent:?}");
        assert!(spent < Duration::from_millis(1500), "{spent:?}");
        assert_eq!(requests(&mock, "SetRenderAdapter"), 1, "one try only");
        assert_ne!(display.render.lock_recover().reported, Some(Some(NV)));
        // the connection is still in step for the next request, once the unanswered one is
        // given up on as lost (`LOST_REPLY_AFTER`, 300 ms under test)
        mock.set_behaviour(|b| b.render_silent = false);
        std::thread::sleep(Duration::from_millis(400));
        assert!(request_composition(
            &client,
            &display.render,
            NV,
            Duration::from_secs(2)
        ));
        assert_eq!(mock.preferred(), Some(NV));
    }

    /// Display 1 is removed while display 0 is created on the device path display 1 has just
    /// released and claims it; display 1's forget, which arrives after that claim, must not
    /// take display 0's claim with it.
    #[test]
    fn a_late_forget_of_a_removed_display_keeps_the_new_claim_on_its_path() {
        let path = format!(r"\?\DISPLAY#VDD#UID256-test-{}", std::process::id());
        remember_owned_path(&path, 1);
        remember_owned_path(&path, 0);
        forget_display_identity(1, &path);
        assert!(is_owned_path(&path), "display 0 still owns it");
        forget_display_identity(0, &path);
        assert!(!is_owned_path(&path));

        // the other order is the easy one: forget first, then the new claim
        remember_owned_path(&path, 1);
        forget_display_identity(1, &path);
        remember_owned_path(&path, 0);
        assert!(is_owned_path(&path));
        forget_display_identity(0, &path);

        // a claim with no pending id goes with its display's forget
        remember_owned_path(&path, UNKNOWN_ID);
        forget_display_identity(3, &path);
        assert!(!is_owned_path(&path));
    }

    fn mon(id: u32) -> Monitor {
        Monitor {
            id,
            name: None,
            enabled: true,
            modes: vec![Mode {
                width: 1280,
                height: 720,
                refresh_rates: vec![60],
            }],
        }
    }

    /// The property owned monitors must never break: a healthy host whose connection is lost
    /// while the driver stays up keeps its displays. The mock removes an owner's monitors when
    /// its connections stay gone for the grace, as the driver does.
    #[test]
    fn a_healthy_host_that_loses_its_connection_never_loses_a_display() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-healthy-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    expire_after_ms: Some(1500),
                    ..Default::default()
                },
            )
        });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        WindowsVirtualDisplay::watch_link(&display);
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        wait_for("the create", || mock.monitors().len() == 1);
        for _ in 0..3 {
            mock.drop_connections();
            std::thread::sleep(Duration::from_millis(2500));
            assert!(
                mock.expired().is_empty(),
                "the driver removed {:?}",
                mock.expired()
            );
            assert_eq!(mock.monitors().len(), 1);
            assert!(display.owned_by_driver.load(Ordering::Acquire));
        }
    }

    /// A reconnect whose Hello is not answered in time lands on the unversioned pipe without
    /// the token. The host must neither give up the versioned pipe nor stop there: it tries
    /// again inside the grace, and the display survives.
    #[test]
    fn a_slow_reconnect_hello_does_not_lose_the_display() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-slowhello-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    expire_after_ms: Some(3000),
                    // the start-up Hello at once, the first reconnect's well past its deadline
                    hello_delays: vec![0, 1500],
                    ..Default::default()
                },
            )
        });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        WindowsVirtualDisplay::watch_link(&display);
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        wait_for("the create", || mock.monitors().len() == 1);
        mock.drop_connections();
        wait_for("a third Hello", || requests(&mock, "Hello") >= 3);
        std::thread::sleep(Duration::from_millis(4000));
        assert!(
            mock.expired().is_empty(),
            "the driver removed {:?}",
            mock.expired()
        );
        assert_eq!(mock.monitors().len(), 1);
        assert!(display.owned_by_driver.load(Ordering::Acquire));
        // the versioned pipe is still in use for the next connection
        let last = mock
            .received()
            .into_iter()
            .rev()
            .find(|m| m.name() == "Hello");
        assert!(last.is_some_and(|m| m.on_v2_pipe));
    }

    /// A host that started before the driver's versioned pipe existed, and negotiated ownership
    /// only on a later reconnect, is watched from then on.
    #[test]
    fn ownership_negotiated_on_a_reconnect_is_watched() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-latev2-{}", std::process::id());
        let mut mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    serve_v2: false,
                    expire_after_ms: Some(1500),
                    ..Default::default()
                },
            )
        });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        WindowsVirtualDisplay::watch_link(&display);
        assert!(!display.watching.load(Ordering::Acquire));
        {
            let _rt = rt.enter();
            mock.serve_v2_now();
        }
        mock.drop_connections();
        std::thread::sleep(Duration::from_millis(300));
        // the commit finds the pipe dead and reconnects, onto the versioned pipe now
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        wait_for("the create", || mock.monitors().len() == 1);
        assert!(display.owned_by_driver.load(Ordering::Acquire));
        assert!(display.watching.load(Ordering::Acquire), "no watcher");
        mock.drop_connections();
        std::thread::sleep(Duration::from_millis(2500));
        assert!(
            mock.expired().is_empty(),
            "the driver removed {:?}",
            mock.expired()
        );
        assert_eq!(mock.monitors().len(), 1);
    }

    /// The driver under a running host is replaced by one without ownership: the host stops
    /// trying to regain it instead of reconnecting every 2 s for ever.
    #[test]
    fn a_driver_without_ownership_ends_the_repair_loop() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-noown-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        WindowsVirtualDisplay::watch_link(&display);
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        wait_for("the create", || mock.monitors().len() == 1);
        mock.set_behaviour(|b| b.capabilities = vec![v2::CAP_HELLO, v2::CAP_DIAGNOSTICS]);
        mock.drop_connections();
        wait_for("the repair to stop", || {
            !display.ownership_wanted.load(Ordering::Acquire)
        });
        let hellos = requests(&mock, "Hello");
        std::thread::sleep(Duration::from_millis(3000));
        assert_eq!(requests(&mock, "Hello"), hellos, "still reconnecting");
        assert_eq!(mock.monitors().len(), 1);
    }

    /// A shared-driver host on the versioned pipe never reuses a released id, even with no
    /// `Hello` sent (both switches off): another host may own it.
    #[test]
    fn released_ids_are_not_reused_on_the_versioned_pipe_whatever_the_switches() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-reuse-{}", std::process::id());
        let _v2 = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let plain = DriverClient::new_host(&name, None, Handshake::FIRST).expect("connect");
        assert_eq!(plain.negotiated().protocol, 2);
        assert!(!shared::reuse_released_for(&plain));
        let name1 = format!("screenextend-test-vd-reuse1-{}", std::process::id());
        let _v1 = rt.block_on(async {
            V2Mock::new(
                &name1,
                V2Behaviour {
                    serve_v2: false,
                    ..Default::default()
                },
            )
        });
        let old = DriverClient::new_host(&name1, None, Handshake::FIRST).expect("connect");
        assert!(shared::reuse_released_for(&old));
    }

    /// A dropped controller leaves nothing holding its connection: the driver sees the token go
    /// and removes the displays after the grace, and the watcher ends.
    #[test]
    fn a_dropped_controller_releases_its_connection() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-drop-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    expire_after_ms: Some(300),
                    ..Default::default()
                },
            )
        });
        let display = Arc::new(
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect"),
        );
        WindowsVirtualDisplay::watch_link(&display);
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        wait_for("the create", || mock.monitors().len() == 1);
        let weak = Arc::downgrade(&display);
        drop(display);
        wait_for("the driver to remove the display", || {
            mock.expired() == vec![0]
        });
        assert!(weak.upgrade().is_none());
    }

    /// A preference a crashed host left is cleared at start-up with the render switch off.
    #[test]
    fn a_stale_preference_is_cleared_at_startup_with_the_switch_off() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-stale-{}", std::process::id());
        let mock = rt.block_on(async {
            V2Mock::new(
                &name,
                V2Behaviour {
                    preferred: Some(NV),
                    ..Default::default()
                },
            )
        });
        let _display =
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect");
        assert_eq!(mock.preferred(), None);
        // in shared mode, with another host's display listed, it stays
        let name2 = format!("screenextend-test-vd-stale2-{}", std::process::id());
        let mock2 = rt.block_on(async {
            V2Mock::new(
                &name2,
                V2Behaviour {
                    preferred: Some(NV),
                    ..Default::default()
                },
            )
        });
        mock2.seed(mon(0), Some(("otherhost", 1)));
        let _shared =
            WindowsVirtualDisplay::connect_with(&name2, true, OWNED_ONLY, None).expect("connect");
        assert_eq!(mock2.preferred(), Some(NV));
    }

    /// The default-mode start-up clear spares a live host's display and takes the rest.
    #[test]
    fn the_startup_clear_spares_another_live_hosts_display() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        let rt = rt();
        let name = format!("screenextend-test-vd-clear-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        mock.seed(mon(0), Some(("livehost", 1)));
        mock.seed(mon(1), Some(("deadhost", 0)));
        mock.seed(mon(2), None);
        let _display =
            WindowsVirtualDisplay::connect_with(&name, false, OWNED_ONLY, None).expect("connect");
        wait_for("the clear", || mock.monitors().len() == 1);
        assert_eq!(mock.monitors()[0].id, 0);
        // on a driver without owners it clears everything
        let name1 = format!("screenextend-test-vd-clear1-{}", std::process::id());
        let mock1 = rt.block_on(async {
            V2Mock::new(
                &name1,
                V2Behaviour {
                    serve_v2: false,
                    ..Default::default()
                },
            )
        });
        mock1.seed(mon(0), None);
        let _old =
            WindowsVirtualDisplay::connect_with(&name1, false, OWNED_ONLY, None).expect("connect");
        wait_for("the full clear", || mock1.monitors().is_empty());
    }

    /// Whatever the driver refuses is "no preference", logged once and never sent again.
    #[test]
    fn a_refused_render_adapter_is_no_preference_and_is_not_resent() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        use crate::streamer::config::EncoderVendor;
        let rt = rt();
        for code in [
            "unknown_adapter",
            "remote_adapter",
            "device_failed",
            "software_adapter",
            "not_supported",
            "driver_error",
            "anything_else",
        ] {
            let name = format!("screenextend-test-vd-refused-{}-{code}", std::process::id());
            let mock = rt.block_on(async {
                V2Mock::new(
                    &name,
                    V2Behaviour {
                        render_error: Some(code),
                        ..Default::default()
                    },
                )
            });
            let mut display = WindowsVirtualDisplay::connect_with(
                &name,
                false,
                RENDER_ONLY,
                Some(encoder(EncoderVendor::Nvidia)),
            )
            .expect("connect");
            display.candidates = fake_adapters;
            display.create_display("a".into(), 1280, 720, 60).unwrap();
            display.create_display("b".into(), 1280, 720, 60).unwrap();
            assert_eq!(requests(&mock, "SetRenderAdapter"), 1, "{code}");
            wait_for("both displays", || mock.monitors().len() == 2);
            display.remove_all_displays();
            assert_eq!(
                requests(&mock, "SetRenderAdapter"),
                1,
                "{code}: nothing to clear"
            );
        }
    }

    /// The driver's host process restarts: the preference died with it, so the host sets it
    /// again when it reconnects, before re-sending its displays.
    #[test]
    fn the_preference_is_set_again_after_a_driver_restart() {
        use crate::driver_ipc::mock::{V2Behaviour, V2Mock};
        use crate::streamer::config::EncoderVendor;
        let rt = rt();
        let name = format!("screenextend-test-vd-restart-{}", std::process::id());
        let mock = rt.block_on(async { V2Mock::new(&name, V2Behaviour::default()) });
        let mut display = WindowsVirtualDisplay::connect_with(
            &name,
            false,
            LinkOptions {
                owned_monitors: true,
                render_follows_encoder: true,
            },
            Some(encoder(EncoderVendor::Auto)),
        )
        .expect("connect");
        display.candidates = fake_adapters;
        let display = Arc::new(display);
        WindowsVirtualDisplay::watch_link(&display);
        display.create_display("a".into(), 1280, 720, 60).unwrap();
        assert_eq!(mock.preferred(), Some(NV));
        wait_for("the create", || mock.monitors().len() == 1);

        mock.restart();
        wait_for("a second Hello", || requests(&mock, "Hello") >= 2);
        wait_for("the display re-sent", || mock.monitors().len() == 1);
        assert_eq!(mock.preferred(), Some(NV));
        assert_eq!(requests(&mock, "SetRenderAdapter"), 2);
        // set before the list went back: the new swap chain starts on the adapter
        let names: Vec<String> = mock.received().iter().map(|m| m.name()).collect();
        let second_hello = names.iter().rposition(|n| n == "Hello").unwrap();
        let after: Vec<&String> = names[second_hello..]
            .iter()
            .filter(|n| *n == "SetRenderAdapter" || *n == "Notify")
            .collect();
        assert_eq!(after, ["SetRenderAdapter", "Notify"], "{names:?}");
    }

    fn rows_touched(plan: &TopologyPlan, rows: &[TopologyRow]) -> Vec<TopologyRow> {
        plan.activate
            .iter()
            .chain(&plan.deactivate)
            .map(|&i| rows[i])
            .collect()
    }

    /// The user's own monitors are never named by a topology change, whatever state they are
    /// in, and with nothing of ours to fix there is no change at all.
    #[test]
    fn a_monitor_the_user_switched_off_stays_off() {
        let rows = vec![
            row(0, 100, true, true, false),  // the panel, on
            row(0, 101, false, true, false), // an external monitor the user disconnected
            row(1, 101, false, true, false),
            row(1, 102, true, true, false), // a duplicated pair, by choice
            row(1, 103, true, true, false),
        ];
        assert_eq!(plan_driver_paths(&rows), None);
    }

    #[test]
    fn our_inactive_display_gets_a_free_source_on_our_adapter_and_nothing_else_moves() {
        let rows = vec![
            row(0, 100, true, true, false),
            row(1, 101, false, true, false), // the user's disconnected monitor
            row(0, 256, true, true, true),   // an earlier display of ours, on source 0
            row(0, 257, false, true, true),  // the new one: source 0 is taken
            row(1, 257, false, true, true),
            row(2, 257, false, true, true),
            row(3, 258, false, false, true), // a target with no monitor behind it
        ];
        let plan = plan_driver_paths(&rows).expect("the new display needs a source");
        assert_eq!(plan.activate, vec![4], "source 1 is the first free one");
        assert!(plan.deactivate.is_empty());
        assert!(rows_touched(&plan, &rows).iter().all(|r| r.ours));
    }

    /// One of ours cloned onto a source it shares (a duplicated desktop extended to the new
    /// monitor) moves to a source of its own; whatever it shared with is left exactly as it is.
    #[test]
    fn our_cloned_display_moves_off_the_shared_source() {
        let rows = vec![
            TopologyRow {
                source: (2, 0, 0),
                target: (2, 0, 100),
                active: true,
                available: true,
                ours: false,
                parked: false,
                primary: true,
            },
            // ours, active on the GPU's source 0: cloned with the panel
            TopologyRow {
                source: (2, 0, 0),
                target: (1, 0, 256),
                active: true,
                available: true,
                ours: true,
                parked: false,
                primary: true,
            },
            row(0, 256, false, true, true),
        ];
        let plan = plan_driver_paths(&rows).expect("the clone is undone for ours");
        assert_eq!(plan.deactivate, vec![1]);
        assert_eq!(plan.activate, vec![2]);
        assert!(rows_touched(&plan, &rows).iter().all(|r| r.ours));

        // with no free source on our adapter it stays cloned rather than being detached
        let stuck = vec![rows[0], rows[1], row(0, 257, true, true, true)];
        let plan = plan_driver_paths(&stuck);
        assert_eq!(plan, None, "{plan:?}");
    }

    /// In the default mode a departed id in the middle of the run stays, idle, until the ones
    /// above it go: `[0, 2]` would detach every display.
    #[test]
    fn a_middle_display_leaving_does_not_break_the_run() {
        let (keep, held) = plan_release(&[0, 1, 2], &[], 1);
        assert_eq!(keep, vec![0, 1, 2]);
        assert_eq!(held, vec![1]);
        assert!(attachable(&keep));
        // a new join extends the run; it never reuses the idle slot
        assert_eq!(allocate(&keep, &[]), 3);
        // the top leaves: both go, and what is left is still one run
        let (keep, held) = plan_release(&[0, 1, 2], &[1], 2);
        assert_eq!(keep, vec![0]);
        assert!(held.is_empty());
        // the bottom can always go when a run from 1 remains
        let (keep, held) = plan_release(&[0, 1, 2], &[], 0);
        assert_eq!(keep, vec![1, 2]);
        assert!(held.is_empty());
        // the next join takes 0 back, which keeps the run
        assert_eq!(allocate(&keep, &[]), 0);
        // an id this host does not hold changes nothing
        let (keep, held) = plan_release(&[0, 1], &[], 7);
        assert_eq!((keep, held), (vec![0, 1], vec![]));
        // a plain removal would leave a list the driver detaches
        assert!(!attachable(&[0, 2]));
    }

    // hand-made display configurations, shaped the way QDC_ALL_PATHS returns them: one row per
    // (source, target) pair a target could take, active rows pointing at their modes
    mod desk {
        use super::super::*;
        use windows::Win32::Devices::Display::{
            DISPLAYCONFIG_2DREGION, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_0,
            DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
            DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_PIXELFORMAT_32BPP, DISPLAYCONFIG_RATIONAL,
            DISPLAYCONFIG_ROTATION_IDENTITY, DISPLAYCONFIG_SCALING_IDENTITY,
            DISPLAYCONFIG_SOURCE_MODE, DISPLAYCONFIG_TARGET_MODE, DISPLAYCONFIG_VIDEO_SIGNAL_INFO,
        };
        use windows::Win32::Foundation::{LUID, POINTL};

        /// The user's GPU, our driver's adapter, and another vendor's indirect display adapter.
        pub const GPU: (u32, i32) = (0x1000, 0);
        pub const GPU2: (u32, i32) = (0x1100, 0);
        pub const OURS: (u32, i32) = (0x2000, 0);
        pub const VENDOR: (u32, i32) = (0x3000, 0);

        pub fn luid(a: (u32, i32)) -> LUID {
            LUID {
                LowPart: a.0,
                HighPart: a.1,
            }
        }

        pub fn ours(a: LUID) -> bool {
            (a.LowPart, a.HighPart) == OURS
        }

        pub fn signal(w: u32, h: u32, hz: u32) -> DISPLAYCONFIG_TARGET_MODE {
            let mut v = DISPLAYCONFIG_VIDEO_SIGNAL_INFO {
                pixelRate: u64::from(w) * u64::from(h) * u64::from(hz),
                ..Default::default()
            };
            v.hSyncFreq = DISPLAYCONFIG_RATIONAL {
                Numerator: h * hz,
                Denominator: 1,
            };
            v.vSyncFreq = DISPLAYCONFIG_RATIONAL {
                Numerator: hz,
                Denominator: 1,
            };
            v.activeSize = DISPLAYCONFIG_2DREGION { cx: w, cy: h };
            v.totalSize = DISPLAYCONFIG_2DREGION { cx: w, cy: h };
            DISPLAYCONFIG_TARGET_MODE {
                targetVideoSignalInfo: v,
            }
        }

        #[derive(Default, Clone)]
        pub struct Desk {
            pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
            pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
        }

        impl Desk {
            fn source_mode(
                &mut self,
                adapter: (u32, i32),
                source: u32,
                size: (u32, u32),
                at: (i32, i32),
            ) -> u32 {
                let found = self.modes.iter().position(|m| {
                    m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
                        && m.id == source
                        && (m.adapterId.LowPart, m.adapterId.HighPart) == adapter
                });
                if let Some(i) = found {
                    return i as u32;
                }
                self.modes.push(DISPLAYCONFIG_MODE_INFO {
                    infoType: DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE,
                    id: source,
                    adapterId: luid(adapter),
                    Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                        sourceMode: DISPLAYCONFIG_SOURCE_MODE {
                            width: size.0,
                            height: size.1,
                            pixelFormat: DISPLAYCONFIG_PIXELFORMAT_32BPP,
                            position: POINTL { x: at.0, y: at.1 },
                        },
                    },
                });
                (self.modes.len() - 1) as u32
            }

            /// An active display. A second one on the same source is a clone of the first.
            pub fn screen(
                mut self,
                adapter: (u32, i32),
                source: u32,
                target: u32,
                size: (u32, u32),
                at: (i32, i32),
                hz: u32,
            ) -> Self {
                let src = self.source_mode(adapter, source, size, at);
                self.modes.push(DISPLAYCONFIG_MODE_INFO {
                    infoType: DISPLAYCONFIG_MODE_INFO_TYPE_TARGET,
                    id: target,
                    adapterId: luid(adapter),
                    Anonymous: DISPLAYCONFIG_MODE_INFO_0 {
                        targetMode: signal(size.0, size.1, hz),
                    },
                });
                let tgt = (self.modes.len() - 1) as u32;
                let mut p = DISPLAYCONFIG_PATH_INFO::default();
                p.sourceInfo.adapterId = luid(adapter);
                p.sourceInfo.id = source;
                p.sourceInfo.Anonymous.modeInfoIdx = src;
                p.sourceInfo.statusFlags = 1;
                p.targetInfo.adapterId = luid(adapter);
                p.targetInfo.id = target;
                p.targetInfo.Anonymous.modeInfoIdx = tgt;
                p.targetInfo.rotation = DISPLAYCONFIG_ROTATION_IDENTITY;
                p.targetInfo.scaling = DISPLAYCONFIG_SCALING_IDENTITY;
                p.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
                    Numerator: hz,
                    Denominator: 1,
                };
                p.targetInfo.targetAvailable = true.into();
                p.targetInfo.statusFlags = 1;
                p.flags = PATH_ACTIVE;
                self.paths.push(p);
                self
            }

            /// A target that is off, listed once per source it could take.
            pub fn idle(
                mut self,
                adapter: (u32, i32),
                sources: &[u32],
                target: u32,
                connected: bool,
            ) -> Self {
                for &source in sources {
                    let mut p = DISPLAYCONFIG_PATH_INFO::default();
                    p.sourceInfo.adapterId = luid(adapter);
                    p.sourceInfo.id = source;
                    p.sourceInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
                    p.targetInfo.adapterId = luid(adapter);
                    p.targetInfo.id = target;
                    p.targetInfo.Anonymous.modeInfoIdx = MODE_IDX_INVALID;
                    p.targetInfo.targetAvailable = connected.into();
                    self.paths.push(p);
                }
                self
            }

            pub fn config(&self) -> QueriedConfig {
                (self.paths.clone(), self.modes.clone())
            }

            pub fn rows(&self, parked: &[u32]) -> Vec<TopologyRow> {
                topology_rows(&self.paths, &self.modes, &mut |a| ours(a), &mut |_, id| {
                    parked.contains(&id)
                })
            }

            pub fn states(&self) -> Vec<PathState> {
                path_states(&self.paths, &self.modes, &ours)
            }

            pub fn row(&self, adapter: (u32, i32), source: u32, target: u32) -> usize {
                self.paths
                    .iter()
                    .position(|p| {
                        (
                            p.sourceInfo.adapterId.LowPart,
                            p.sourceInfo.adapterId.HighPart,
                        ) == adapter
                            && p.sourceInfo.id == source
                            && p.targetInfo.id == target
                    })
                    .expect("no such row")
            }
        }

        pub fn preferred(w: u32, h: u32, hz: u32) -> PreferredMode {
            PreferredMode {
                width: w,
                height: h,
                target_mode: signal(w, h, hz),
            }
        }

        /// A display on `target` of the written states, by target id.
        pub fn on(states: &[PathState], adapter: (u32, i32), target: u32) -> PathState {
            *states
                .iter()
                .find(|s| (s.target.0, s.target.1) == adapter && s.target.2 == target)
                .expect("not on")
        }
    }

    use desk::{Desk, GPU, GPU2, OURS, VENDOR};

    /// The laptop's own 2560x1600 panel at 360 Hz, primary.
    fn panel() -> Desk {
        Desk::default().screen(GPU, 0, 100, (2560, 1600), (0, 0), 360)
    }

    fn write_states(d: &Desk, plan: &TopologyPlan) -> Vec<PathState> {
        let pref = desk::preferred(1920, 1080, 60);
        let (paths, modes) =
            build_topology_write(&d.paths, &d.modes, plan, &|_| Some(pref)).expect("arrays");
        path_states(&paths, &modes, &desk::ours)
    }

    #[test]
    fn with_only_the_panel_there_is_nothing_to_write() {
        let d = panel();
        let rows = d.rows(&[]);
        assert_eq!(plan_driver_paths(&rows), None);
        assert!(plan_parking(&rows).is_empty());
        assert!(rows[0].primary && !rows[0].ours);
    }

    #[test]
    fn a_new_display_gets_an_explicit_mode_beside_the_panel_and_the_panel_is_written_as_it_is() {
        let d = panel().idle(OURS, &[0, 1], 7, true);
        let rows = d.rows(&[]);
        let plan = plan_driver_paths(&rows).expect("ours needs a source");
        assert_eq!(plan.activate, vec![d.row(OURS, 0, 7)]);
        let written = write_states(&d, &plan);
        assert_eq!(check_write(&d.states(), &written), Ok(()));
        assert_eq!(
            desk::on(&written, GPU, 100),
            d.states()[0],
            "the panel, untouched"
        );
        let mine = desk::on(&written, OURS, 7);
        let mode = mine.source_mode.expect("an explicit source mode");
        assert_eq!(
            (mode.x, mode.y, mode.width, mode.height),
            (2560, 0, 1920, 1080)
        );
        assert_eq!(mine.target_mode.map(|m| m.v_sync), Some((60, 1)));
        assert_eq!(mine.refresh, (60, 1));
    }

    #[test]
    fn a_second_physical_monitor_in_extend_is_written_exactly_as_it_is() {
        let d = panel()
            .screen(GPU, 1, 101, (1920, 1080), (2560, 200), 144)
            .idle(OURS, &[0], 7, true);
        let plan = plan_driver_paths(&d.rows(&[])).expect("plan");
        let written = write_states(&d, &plan);
        assert_eq!(check_write(&d.states(), &written), Ok(()));
        assert!(path_changes(&d.states(), &written, &|p| !p.ours).is_empty());
        let mode = desk::on(&written, OURS, 7).source_mode.unwrap();
        assert_eq!(
            (mode.x, mode.y),
            (4480, 200),
            "against the rightmost, top-aligned"
        );
    }

    #[test]
    fn a_cloned_pair_keeps_its_one_source_mode() {
        let d = panel()
            .screen(GPU, 0, 101, (2560, 1600), (0, 0), 60)
            .idle(OURS, &[0], 7, true);
        let plan = plan_driver_paths(&d.rows(&[])).expect("plan");
        let pref = desk::preferred(1920, 1080, 60);
        let (paths, modes) =
            build_topology_write(&d.paths, &d.modes, &plan, &|_| Some(pref)).unwrap();
        let theirs: Vec<u32> = paths
            .iter()
            .filter(|p| !desk::ours(p.targetInfo.adapterId))
            // SAFETY: built without virtual mode awareness; the union holds the index.
            .map(|p| unsafe { p.sourceInfo.Anonymous.modeInfoIdx })
            .collect();
        assert_eq!(theirs.len(), 2);
        assert_eq!(theirs[0], theirs[1], "still one shared source mode");
        let written = path_states(&paths, &modes, &desk::ours);
        assert_eq!(check_write(&d.states(), &written), Ok(()));
        assert_eq!(
            desk::on(&written, OURS, 7).source_mode.map(|m| m.x),
            Some(2560)
        );
    }

    #[test]
    fn a_monitor_at_a_negative_origin_keeps_it() {
        let d = panel()
            .screen(GPU2, 0, 200, (1920, 1080), (-1920, -300), 60)
            .idle(OURS, &[0], 7, true);
        let plan = plan_driver_paths(&d.rows(&[])).expect("plan");
        let written = write_states(&d, &plan);
        assert_eq!(check_write(&d.states(), &written), Ok(()));
        let left = desk::on(&written, GPU2, 200).source_mode.unwrap();
        assert_eq!((left.x, left.y), (-1920, -300));
        let mode = desk::on(&written, OURS, 7).source_mode.unwrap();
        assert_eq!((mode.x, mode.y), (2560, 0));
    }

    #[test]
    fn another_vendors_indirect_display_is_never_named() {
        // whatever the parked set says, a target off our adapter is neither ours nor parked
        let d = panel()
            .screen(VENDOR, 0, 300, (1920, 1080), (2560, 0), 60)
            .idle(VENDOR, &[1], 301, true)
            .screen(OURS, 0, 7, (1920, 1080), (4480, 0), 60);
        let everything: Vec<u32> = vec![300, 301, 7];
        let rows = d.rows(&everything);
        for r in rows.iter().filter(|r| (r.target.0, r.target.1) == VENDOR) {
            assert!(!r.ours && !r.parked, "{r:?}");
        }
        assert_eq!(plan_parking(&rows), vec![d.row(OURS, 0, 7)]);
        // its switched-off target is not ours to switch on
        assert_eq!(plan_driver_paths(&d.rows(&[])), None);
    }

    #[test]
    fn our_display_as_the_primary_is_never_taken_off() {
        let d = Desk::default()
            .screen(OURS, 0, 7, (1920, 1080), (0, 0), 60)
            .screen(GPU, 0, 100, (2560, 1600), (1920, 0), 360);
        let rows = d.rows(&[7]);
        assert!(rows[0].primary && rows[0].parked);
        assert!(plan_parking(&rows).is_empty());
        // a plan that tried anyway is refused before anything is written
        let plan = TopologyPlan {
            deactivate: vec![0],
            activate: Vec::new(),
        };
        let written = write_states(&d, &plan);
        assert!(check_write(&d.states(), &written).is_err());
        let mut io = FakeIo::new(vec![d.config()], 0);
        let outcome = write_topology_with(&mut io, &d.config(), &plan);
        assert!(matches!(outcome, WriteOutcome::Refused(_)), "{outcome:?}");
        assert_eq!((io.queries, io.applied), (0, 0));
    }

    #[test]
    fn a_write_that_leaves_nothing_on_or_moves_the_primary_is_refused() {
        let d = panel();
        assert!(check_write(&d.states(), &[]).is_err());
        let mut moved = d.states();
        if let Some(m) = moved[0].source_mode.as_mut() {
            m.x = 10;
        }
        assert!(check_write(&d.states(), &moved).is_err());
    }

    /// The machine, as a test sees it: a queue of configurations each query returns in turn,
    /// and a fixed answer from `SetDisplayConfig`.
    struct FakeIo {
        answers: Vec<QueriedConfig>,
        code: i32,
        may: bool,
        queries: usize,
        applied: usize,
    }

    impl FakeIo {
        fn new(answers: Vec<QueriedConfig>, code: i32) -> Self {
            Self {
                answers,
                code,
                may: true,
                queries: 0,
                applied: 0,
            }
        }
    }

    impl TopologyIo for FakeIo {
        fn query(&mut self) -> Result<QueriedConfig, String> {
            self.queries += 1;
            if self.answers.is_empty() {
                return Err("nothing to answer with".to_string());
            }
            Ok(self.answers.remove(0))
        }
        fn ours(&mut self, adapter: windows::Win32::Foundation::LUID) -> bool {
            desk::ours(adapter)
        }
        fn may_write(
            &mut self,
            path: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
        ) -> bool {
            self.may && desk::ours(path.targetInfo.adapterId)
        }
        fn preferred(
            &mut self,
            _: &windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO,
        ) -> Option<PreferredMode> {
            Some(desk::preferred(1920, 1080, 60))
        }
        fn apply(
            &mut self,
            _: &[windows::Win32::Devices::Display::DISPLAYCONFIG_PATH_INFO],
            _: &[windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO],
        ) -> i32 {
            self.applied += 1;
            self.code
        }
    }

    /// The panel and a released display of ours that is still on the desktop.
    fn panel_and_parked() -> Desk {
        panel().screen(OURS, 0, 7, (1920, 1080), (2560, 0), 60)
    }

    fn park_plan(d: &Desk) -> TopologyPlan {
        TopologyPlan {
            deactivate: plan_parking(&d.rows(&[7])),
            activate: Vec::new(),
        }
    }

    #[test]
    fn parking_writes_everything_else_as_it_is_and_checks_it_afterwards() {
        let d = panel_and_parked();
        let plan = park_plan(&d);
        assert_eq!(plan.deactivate, vec![1]);
        let mut io = FakeIo::new(vec![d.config(), panel().config()], 0);
        let outcome = write_topology_with(&mut io, &d.config(), &plan);
        assert_eq!(
            outcome,
            WriteOutcome::Applied {
                collateral: Ok(Vec::new())
            }
        );
        assert_eq!((io.queries, io.applied), (2, 1));
    }

    #[test]
    fn a_topology_that_moved_after_the_plan_is_never_written() {
        let d = panel_and_parked();
        let plan = park_plan(&d);
        // Windows settled the panel at 60 Hz between the plan and the write
        let mut settled = d.clone();
        settled.paths[0].targetInfo.refreshRate.Numerator = 60;
        let mut io = FakeIo::new(vec![settled.config()], 0);
        let outcome = write_topology_with(&mut io, &d.config(), &plan);
        let WriteOutcome::Stale(changes) = outcome else {
            panic!("{outcome:?}");
        };
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].target, (GPU.0, GPU.1, 100));
        assert!(changes[0].fields.contains(&"refresh rate"), "{changes:?}");
        assert_eq!(io.applied, 0, "nothing was written");
    }

    #[test]
    fn a_refused_write_changes_nothing_and_is_not_retried_inside_the_call() {
        let d = panel_and_parked();
        let mut io = FakeIo::new(vec![d.config(), d.config()], 87);
        let outcome = write_topology_with(&mut io, &d.config(), &park_plan(&d));
        assert_eq!(outcome, WriteOutcome::Rejected(87));
        assert_eq!((io.queries, io.applied), (1, 1));
    }

    #[test]
    fn a_display_that_is_not_ours_changing_under_a_write_is_reported() {
        let d = panel_and_parked();
        let mut after = panel();
        // SAFETY: the panel's first mode is its source mode.
        let mut moved = unsafe { after.modes[0].Anonymous.sourceMode };
        moved.position.x = -100;
        after.modes[0].Anonymous.sourceMode = moved;
        let mut io = FakeIo::new(vec![d.config(), after.config()], 0);
        let outcome = write_topology_with(&mut io, &d.config(), &park_plan(&d));
        let WriteOutcome::Applied {
            collateral: Ok(changes),
        } = outcome
        else {
            panic!("{outcome:?}");
        };
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].fields, vec!["position", "primary"]);
    }

    #[test]
    fn a_row_that_is_no_longer_ours_to_change_is_not_written() {
        let d = panel_and_parked();
        let mut io = FakeIo::new(vec![d.config()], 0);
        io.may = false;
        let outcome = write_topology_with(&mut io, &d.config(), &park_plan(&d));
        assert!(matches!(outcome, WriteOutcome::Refused(_)), "{outcome:?}");
        assert_eq!((io.queries, io.applied), (0, 0));
    }

    #[test]
    fn an_activation_is_dropped_when_windows_got_there_first() {
        let d = panel().idle(OURS, &[0], 7, true);
        let plan = plan_driver_paths(&d.rows(&[])).expect("plan");
        let extended = panel().screen(OURS, 0, 7, (1920, 1080), (2560, 0), 60);
        let mut io = FakeIo::new(vec![extended.config()], 0);
        let outcome = write_topology_with(&mut io, &d.config(), &plan);
        assert!(matches!(outcome, WriteOutcome::Stale(_)), "{outcome:?}");
        assert_eq!(io.applied, 0);
    }

    #[test]
    fn a_parked_target_must_be_on_our_adapter_when_it_is_parked() {
        let targets = vec![
            (r"\\?\DISPLAY#VDD#1".to_string(), IDD_ADAPTER.to_string()),
            (r"\\?\DISPLAY#PANEL#1".to_string(), GPU_ADAPTER.to_string()),
        ];
        assert_eq!(
            check_path_on_driver(&targets, r"\\?\DISPLAY#VDD#1", &driver()),
            Ok(())
        );
        assert!(check_path_on_driver(&targets, r"\\?\DISPLAY#PANEL#1", &driver()).is_err());
        assert!(check_path_on_driver(&targets, r"\\?\DISPLAY#GONE#1", &driver()).is_err());
        assert!(check_path_on_driver(&targets, "", &driver()).is_err());
        let twice = vec![targets[0].clone(), targets[0].clone()];
        assert!(check_path_on_driver(&twice, r"\\?\DISPLAY#VDD#1", &driver()).is_err());
    }

    #[test]
    fn only_a_root_enumerated_devnode_is_the_driver() {
        assert_eq!(
            root_instance_ids(vec![r"ROOT\DISPLAY\0000".into(), String::new()]),
            Ok(vec![r"ROOT\DISPLAY\0000".to_string()])
        );
        assert_eq!(
            root_instance_ids(vec![
                r"root\display\0001".into(),
                r"PCI\VEN_8086&DEV_46A3\3&11583659&1&10".into()
            ]),
            Ok(vec![r"root\display\0001".to_string()])
        );
        assert!(root_instance_ids(vec![r"SWD\IDD\0000".into()]).is_err());
        assert!(root_instance_ids(Vec::new()).is_err());
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

            // the lookup, polled the same way the diff is so the two are comparable
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

            // the diff, from the same start instant
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

    /// Drives every DPR step of the ladder at a real display, then proves the writes cannot
    /// reach anything else.
    ///
    /// A resolution in the display's registered mode list is a `ChangeDisplaySettingsEx` on a
    /// live monitor. One that is not needs the mode list to change, which the bundled driver's
    /// `notify()` does as `IddCxMonitorDeparture`, `IddCxMonitorCreate`, `IddCxMonitorArrival`:
    /// a full hot-unplug and re-plug, which is why `dpr_mode_ladder` pre-registers the whole
    /// ladder at create time. `IddCxMonitorUpdateModes` (IddCx 1.0; the bundled INF declares
    /// 1.2) would update in place; `report_mode_change_cost` times what it would replace.
    ///
    /// Every physical display's mode and DPI band is recorded before and checked at the end.
    /// After removing the virtual display the harness writes to its stale target again: that
    /// `\\.\DISPLAYn` is what Windows hands to whatever attaches next, and both writes must be
    /// refused. It also prints what each DPR step reached: a scale above the band the display
    /// advertises lands on the closest step it will take, and that, not the request, is what
    /// a session must record.
    #[test]
    #[ignore = "requires the virtual display driver installed; creates a real display and changes its mode and DPI; run with --ignored --nocapture"]
    fn report_display_change_targets() {
        use crate::windows_utils::streamer::capture::{
            dpi_percent_at, monitor_dimensions_owned, read_dpi_band, set_display_mode_owned,
            set_display_scale_owned,
        };

        let Ok(display) = WindowsVirtualDisplay::new_shared_reported() else {
            crate::test_support::skip_because(
                "windows display-change targets",
                "the virtual display driver is not installed",
            );
            return;
        };

        /// What has to be identical afterwards: a display's identity, mode and DPI band.
        struct Recorded {
            device_path: String,
            mode: Option<(u32, u32, u32)>,
            band: Option<(i32, i32, i32)>,
        }

        // a path Windows could not name is on its way in or out (most often a monitor the driver
        // is still departing): skipped, as there is nothing to compare it against later
        let physical_before: Vec<Recorded> = active_display_paths()
            .iter()
            .filter(|p| !p.device_path.is_empty())
            .map(|p| Recorded {
                device_path: p.device_path.clone(),
                mode: current_display_mode(&p.gdi_name),
                band: read_dpi_band(p.adapter_id, p.source_id),
            })
            .collect();
        log_display_inventory("before the harness");

        const CSS: (u32, u32) = (1920, 1080);
        let even = |v: f64| (v.round() as u32).clamp(2, 16384) & !1;
        let ratios = crate::streamer::platform::display_dpr_ladder();
        let ladder: Vec<(u32, u32)> = ratios
            .iter()
            .map(|r| (even(CSS.0 as f64 * r), even(CSS.1 as f64 * r)))
            .collect();

        let before_paths = attached_display_paths();
        let id = display
            .create_display_with_modes(
                "ScreenExtend dpr probe".to_string(),
                CSS.0,
                CSS.1,
                60,
                &ladder,
            )
            .expect("create display");

        // `claim_display_identity` enrols a path, so it is called once, on the display this
        // harness created, not on every candidate the poll looks at
        let mut found = None;
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
            let paths = active_display_paths();
            if let Some(hit) = paths
                .iter()
                .find(|p| !p.device_path.is_empty() && !before_paths.contains(&p.device_path))
            {
                found = Some((hit.gdi_name.clone(), hit.device_path.clone()));
                break;
            }
        }
        let Some((name, path)) = found else {
            display.remove_display(id);
            println!("the display never attached with an identity of its own");
            return;
        };
        claim_display_identity(&name, &before_paths).expect("the new display has an identity");
        let target = DisplayTarget {
            device_path: path.clone(),
            device_name: name.clone(),
        };
        println!("created {name} ({path})");

        println!("\n  DPR   requested         mode landed        scale asked  scale reached");
        for r in &ratios {
            let (w, h) = (even(CSS.0 as f64 * r), even(CSS.1 as f64 * r));
            let want_scale = (r * 100.0).round() as u32;
            let hz = set_display_mode_owned(&target, w, h, 60, false);
            std::thread::sleep(Duration::from_millis(250));
            let landed = monitor_dimensions_owned(&target);
            let scale = set_display_scale_owned(&target, want_scale);
            println!(
                "  {r:<5} {w}x{h}@60   {:?} @{:?}Hz   {want_scale}%   {:?}",
                landed, hz, scale
            );
        }

        // the band this display ended on: it moves with the resolution, which is why a DPR
        // reachable at one geometry is refused at another
        if let Ok(resolved) = resolve_owned_display(&target) {
            if let Some((cur, lo, hi)) = read_dpi_band(resolved.adapter_id, resolved.source_id) {
                println!(
                    "\n  final band: {}% in {}%..{}%",
                    dpi_percent_at(cur),
                    dpi_percent_at(lo),
                    dpi_percent_at(hi)
                );
            }
        }

        // take the display away and keep the target: `\\.\DISPLAYn` is now a name Windows is
        // free to hand to anything, and both writes must refuse it
        display.remove_display(id);
        forget_display_identity(id, &path);
        let gone = Instant::now() + Duration::from_secs(15);
        while Instant::now() < gone && active_display_paths().iter().any(|p| p.device_path == path)
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let mode_after_removal = set_display_mode_owned(&target, 1920, 1080, 60, false);
        let scale_after_removal = set_display_scale_owned(&target, 200);
        println!("\n  after removal, writing to the stale target:");
        println!("    set_display_mode_owned  -> {mode_after_removal:?}");
        println!("    set_display_scale_owned -> {scale_after_removal:?}");
        assert!(
            mode_after_removal.is_err(),
            "a mode write to a removed display must be refused, not land on whatever holds its name"
        );
        assert!(
            scale_after_removal.is_err(),
            "a DPI write to a removed display must be refused, not land on whatever holds its name"
        );

        log_display_inventory("after the harness");
        let now = active_display_paths();
        for was in &physical_before {
            let path = &was.device_path;
            let Some(p) = now.iter().find(|p| &p.device_path == path) else {
                panic!("{path} is no longer attached; the harness disturbed the desktop");
            };
            assert_eq!(
                current_display_mode(&p.gdi_name),
                was.mode,
                "{path} changed mode while the harness drove a virtual display"
            );
            assert_eq!(
                read_dpi_band(p.adapter_id, p.source_id),
                was.band,
                "{path} changed DPI while the harness drove a virtual display"
            );
        }
        println!("\n  every display that was not ours is exactly where it was.");
    }

    /// Times the two ways a connected device's geometry can change (see the note on
    /// `report_display_change_targets`): the before half of the `IddCxMonitorUpdateModes`
    /// comparison. Needs the driver installed; creates a real display for a few seconds. Run
    /// with `--ignored --nocapture`.
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

        // in the registered ladder: no mode-list change, so no departure
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

        // outside it: the only way to reach a new mode is to take the display away and build a
        // new one, which is what a mode-list change costs
        let t = Instant::now();
        display.remove_display(id);
        // wait for the old monitor to go, or the name search below would match the display just
        // removed and report a time that is not the reattach
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
    ///   - a new monitor comes up at 1280x720@60: Windows picks its own default, not the
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
