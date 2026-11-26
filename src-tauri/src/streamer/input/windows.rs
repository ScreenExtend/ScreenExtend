#![allow(unused_unsafe)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, CompareObjectHandles, DuplicateHandle, GlobalFree, DUPLICATE_SAME_ACCESS, HANDLE,
    HWND, LPARAM, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, PostMessageW, WindowFromPoint, PT_PEN, PT_TOUCH, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WM_DROPFILES,
};

use super::protocol::{
    btn, positions_cursor, DropItem, InputEvent, Lifecycle, MoveSample, Phase, SRC_MOUSE, SRC_PEN,
    SRC_TOUCH,
};
use super::scancode::code_to_scancode;
use super::{ClientProfile, DisplayRect, PointerScope, SharedLiveInput};
use crate::streamer::audio::host_now_ns;
use crate::streamer::lock::LockExt;
use crate::streamer::session::DisplayTarget;
use crate::windows_utils::streamer::tune;

pub const NAME: &str = "windows-sendinput+syntheticpointer";

const WHEEL_DELTA: f32 = 120.0;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
const MAX_TOUCH_COUNT: u32 = 10;
const CF_UNICODETEXT: u32 = 13;
const SC_LCTRL: u16 = 0x1D;
const SC_KEY_V: u16 = 0x2F;
const PEN_MASK_PRESSURE: u32 = 0x0000_0001;
const PEN_MASK_ROTATION: u32 = 0x0000_0002;
const PEN_MASK_TILT_X: u32 = 0x0000_0004;
const PEN_MASK_TILT_Y: u32 = 0x0000_0008;
const PEN_FLAG_BARREL: u32 = 0x0000_0001;
const PEN_FLAG_ERASER: u32 = 0x0000_0004;
const TOUCH_MASK_CONTACTAREA: u32 = 0x0000_0001;
const TOUCH_MASK_ORIENTATION: u32 = 0x0000_0002;
const TOUCH_MASK_PRESSURE: u32 = 0x0000_0004;

/// The five mouse buttons of the pointer button word. `btn::ERASER` is a pen flag, not a button.
const MOUSE_BUTTONS: u16 =
    btn::PRIMARY | btn::SECONDARY | btn::AUXILIARY | btn::BACK | btn::FORWARD;

/// How often the target display's geometry is re-read.
const GEOMETRY_REFRESH: Duration = Duration::from_millis(500);

/// How long after a `POINTER_DOWN`/`UP`/`CANCEL` a restated button word on a move-type frame may
/// not press the first button or release the last one.
///
/// Moves ride the unordered fast channel and Down/Up the reliable one, and the injector takes
/// whichever queue `select!` picks, so a move the client sent *before* its Up can be injected
/// *after* it. The two queues are drained within milliseconds of each other; 250 ms is far past
/// any reorder and far short of anything a person does on purpose.
const RESTATE_GUARD: Duration = Duration::from_millis(250);

/// At most one line per this interval for a failure that repeats per event.
const FAILURE_LOG_INTERVAL_NS: u64 = 1_000_000_000;

/// `OpenClipboard` fails at once while another process holds the clipboard. Off the injector
/// thread a short bounded retry is affordable; on it, it is not.
const CLIPBOARD_OPEN_ATTEMPTS: u32 = 5;
const CLIPBOARD_RETRY: Duration = Duration::from_millis(10);

/// Dropped files have to outlive the drop (the receiving window reads them after
/// `WM_DROPFILES` returns, sometimes much later), so they are swept on a later drop instead.
const DROP_SWEEP_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Unmapped key codes are logged once each, up to this many per session.
const MAX_REPORTED_KEYS: usize = 64;

/// Clipboard and drop jobs waiting for the bulk worker. A client cannot usefully queue more.
const BULK_QUEUE: usize = 16;
/// The longest a key, text or button event waits behind a paste the bulk worker has not
/// finished, so the Ctrl+V lands before it. Covers the worker's `OpenClipboard` retries
/// (`CLIPBOARD_OPEN_ATTEMPTS` x `CLIPBOARD_RETRY`) with room for the copy itself; past it the
/// event goes ahead and the reorder is logged.
const PASTE_ORDER_WAIT: Duration = Duration::from_millis(150);

/// Windows needs no permission to synthesize input into its own session, so the tier is
/// constant. The pen/touch degradation to mouse is reported by `Injector::new` instead: it is a
/// property of the session, not of the host.
pub fn probe() -> super::InputBackend {
    super::InputBackend::SendInput
}

pub fn boost_thread() {
    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) }
    {
        teprintln!("[input] SetThreadPriority(TIME_CRITICAL) failed on the injector: {e}");
    }
    if crate::windows_utils::streamer::tune::candidate("input_thread_tuning") {
        crate::windows_utils::streamer::tuning::exempt_thread_from_power_throttling();
        crate::windows_utils::streamer::tuning::confine_to_performance_cores();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VirtualScreen {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl VirtualScreen {
    fn query() -> Self {
        unsafe {
            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let mut width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let mut height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if width <= 0 {
                width = GetSystemMetrics(SM_CXSCREEN);
            }
            if height <= 0 {
                height = GetSystemMetrics(SM_CYSCREEN);
            }
            VirtualScreen {
                left,
                top,
                width: width.max(1),
                height: height.max(1),
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Contact {
    x: f32,
    y: f32,
    pressure: f32,
    w: f32,
    h: f32,
}

/// What one touch frame does to its own contact, decided from whether the contact is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContactStep {
    Begin,
    Update,
    End {
        canceled: bool,
    },
    /// A frame for a contact the synthetic device never saw go down: a Move, Up or Cancel
    /// processed after its own Up. Injecting it would put a contact in every later frame that
    /// the device rejects, and touch would stay dead until the next blur.
    Ignore,
    /// A hover phase (Over, Enter, Out, Leave) for a contact that is not down. Every tap sends
    /// `pointerover`/`pointerenter` before its `pointerdown` and `pointerout`/`pointerleave`
    /// after its `pointerup`, so this is ordinary traffic, not a late frame: not injected,
    /// not counted.
    Skip,
}

/// Only `Down` creates a contact. A second `Down` for a contact already down is an update, never
/// a second `POINTER_FLAG_DOWN` for one id.
fn contact_step(known: bool, phase: Phase) -> ContactStep {
    match (known, phase) {
        (false, Phase::Down) => ContactStep::Begin,
        (false, Phase::Over | Phase::Enter | Phase::Out | Phase::Leave) => ContactStep::Skip,
        (false, Phase::Move | Phase::Up | Phase::Cancel) => ContactStep::Ignore,
        (true, Phase::Down | Phase::Move | Phase::Enter | Phase::Over) => ContactStep::Update,
        (true, Phase::Up | Phase::Leave | Phase::Out) => ContactStep::End { canceled: false },
        (true, Phase::Cancel) => ContactStep::End { canceled: true },
    }
}

/// One snapshot of where input lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct GeometrySnapshot {
    virt: VirtualScreen,
    /// The session's own display, or `None` when it does not resolve (or there is none).
    target: Option<DisplayRect>,
}

/// How the session's display is found again on every refresh.
struct TargetIdent {
    name: Option<String>,
    /// The monitor's stable device-interface path, once it is known to be one this host created.
    /// With it, the refresh goes through the same ownership check every display write does, so a
    /// `\\.\DISPLAYn` slot handed to some other monitor is never mistaken for this session's.
    owned: Option<DisplayTarget>,
    /// The name addressed a monitor this host did not create while it does own attached
    /// displays: a stale slot. Never resolves, so the session is `lost` and positional input is
    /// withheld instead of landing on somebody else's monitor.
    refused: bool,
}

/// What the name a session's injector starts with turns out to address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IdentVerdict {
    /// One of ours: follow it by its device path from now on.
    Owned,
    /// No ownership information to judge by (no path behind the name, or a host that owns no
    /// attached display, as in a harness): followed by name.
    NameOnly,
    /// Somebody else's monitor while this host owns displays: never resolves.
    Refused,
}

/// `path_owned` is `None` when no attached monitor with a device path answers to the name.
fn ident_verdict(path_owned: Option<bool>, host_owns_attached: bool) -> IdentVerdict {
    match path_owned {
        Some(true) => IdentVerdict::Owned,
        Some(false) if host_owns_attached => IdentVerdict::Refused,
        Some(false) | None => IdentVerdict::NameOnly,
    }
}

impl TargetIdent {
    fn by_name(name: Option<String>) -> Self {
        TargetIdent {
            name,
            owned: None,
            refused: false,
        }
    }

    /// The display the host recorded when it created it, by its device path. Every refresh
    /// resolves that path through the ownership check, so nothing is guessed from the name.
    fn by_path(name: Option<String>, device_path: String) -> Self {
        TargetIdent {
            owned: Some(DisplayTarget {
                device_path,
                device_name: name.clone().unwrap_or_default(),
            }),
            name,
            refused: false,
        }
    }

    /// Read the stable identity of the display `name` addresses now. Called once, when the
    /// session's injector starts and the display has just been claimed as ours.
    fn owned(name: Option<String>) -> Self {
        use crate::windows_utils::virtual_display::{
            active_display_paths, resolve_owned_display, DisplayPath,
        };
        let Some(gdi) = name.as_deref() else {
            return Self::by_name(name);
        };
        let paths = active_display_paths();
        // the same ownership check every display write uses (the owned set itself is private
        // to the display module)
        let owns = |p: &DisplayPath| {
            !p.device_path.is_empty()
                && resolve_owned_display(&DisplayTarget {
                    device_path: p.device_path.clone(),
                    device_name: p.gdi_name.clone(),
                })
                .is_ok()
        };
        let hit = paths
            .iter()
            .find(|p| p.gdi_name == gdi && !p.device_path.is_empty());
        let path_owned = hit.map(owns);
        let host_owns_attached = path_owned == Some(true) || paths.iter().any(owns);
        match ident_verdict(path_owned, host_owns_attached) {
            IdentVerdict::Owned => TargetIdent {
                owned: hit.map(|p| DisplayTarget {
                    device_path: p.device_path.clone(),
                    device_name: gdi.to_string(),
                }),
                name,
                refused: false,
            },
            IdentVerdict::NameOnly => {
                if hit.is_some() {
                    teprintln!(
                        "remote-input: {gdi} is not a display this host created, and this host \
                         owns no attached display to judge by; resolving it by name only"
                    );
                }
                Self::by_name(name)
            }
            IdentVerdict::Refused => {
                teprintln!(
                    "remote-input: {gdi} addresses a monitor this host did not create; \
                     positional input for this session is withheld rather than sent there"
                );
                TargetIdent {
                    name,
                    owned: None,
                    refused: true,
                }
            }
        }
    }

    fn query(&mut self) -> GeometrySnapshot {
        let virt = VirtualScreen::query();
        let rect = match (&mut self.owned, &self.name) {
            _ if self.refused => None,
            (Some(t), _) => {
                match crate::windows_utils::virtual_display::resolve_owned_display(t) {
                    Ok(r) => {
                        // follow a rename, so the resolver's "now answers to" line is logged
                        // once and not on every refresh
                        if r.gdi_name != t.device_name {
                            t.device_name = r.gdi_name.clone();
                        }
                        crate::windows_utils::streamer::capture::monitor_rect(&r.gdi_name)
                    }
                    Err(_) => None,
                }
            }
            (None, Some(name)) => crate::windows_utils::streamer::capture::monitor_rect(name),
            (None, None) => None,
        };
        GeometrySnapshot {
            virt,
            target: rect.map(|(left, top, width, height)| DisplayRect {
                left,
                top,
                width,
                height,
            }),
        }
    }
}

struct SharedGeometry {
    version: AtomicU64,
    snap: Mutex<GeometrySnapshot>,
}

enum GeometrySource {
    /// Re-queried by name on the injector thread, before the event, every 500 ms.
    Inline {
        ident: TargetIdent,
        checked: Option<Instant>,
    },
    /// Re-queried on a worker thread; the injector only notices a new version.
    Worker {
        shared: Arc<SharedGeometry>,
        seen: u64,
        /// Dropping it ends the worker. Sending on it asks for a refresh now.
        poke: Sender<()>,
    },
}

fn spawn_geometry_worker(
    mut ident: TargetIdent,
    first: GeometrySnapshot,
) -> std::io::Result<GeometrySource> {
    let shared = Arc::new(SharedGeometry {
        version: AtomicU64::new(0),
        snap: Mutex::new(first),
    });
    let (poke, poked) = bounded::<()>(1);
    let worker_shared = Arc::clone(&shared);
    std::thread::Builder::new()
        .name("input-geometry".into())
        .spawn(move || {
            let mut last = first;
            // a poke and a timeout both mean "read it again"; a disconnect means the injector
            // has gone
            while let Ok(()) | Err(RecvTimeoutError::Timeout) = poked.recv_timeout(GEOMETRY_REFRESH)
            {
                let snap = ident.query();
                if snap != last {
                    last = snap;
                    *worker_shared.snap.lock_recover() = snap;
                    worker_shared.version.fetch_add(1, Ordering::Release);
                }
            }
        })?;
    Ok(GeometrySource::Worker {
        shared,
        seen: 0,
        poke,
    })
}

/// A failure that can repeat once per injected event: counted always, logged at most once a
/// second with the running count, so a locked workstation or a UAC prompt costs one line a
/// second (and a `serve -v` injector thread one console write) instead of one per event.
struct LogGate {
    count: AtomicU64,
    last_ns: AtomicU64,
}

impl LogGate {
    const fn new() -> Self {
        LogGate {
            count: AtomicU64::new(0),
            last_ns: AtomicU64::new(0),
        }
    }

    /// Count one occurrence. `Some(total)` when a line is due.
    fn note(&self, now_ns: u64) -> Option<u64> {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let last = self.last_ns.load(Ordering::Relaxed);
        if n != 1 && now_ns.saturating_sub(last) < FAILURE_LOG_INTERVAL_NS {
            return None;
        }
        self.last_ns
            .compare_exchange(last, now_ns, Ordering::Relaxed, Ordering::Relaxed)
            .ok()
            .map(|_| n)
    }
}

static SEND_INPUT_FAILURES: LogGate = LogGate::new();
static POINTER_INJECT_FAILURES: LogGate = LogGate::new();
static CLIPBOARD_REJECTED: LogGate = LogGate::new();

/// The two Ctrl keys the client holds, as bits, readable from the bulk worker.
const CTRL_LEFT: u8 = 1;
const CTRL_RIGHT: u8 = 2;

enum BulkJob {
    Clipboard { op: u8, mime: String, data: Vec<u8> },
    Drop { pt: POINT, items: Vec<DropItem> },
}

fn spawn_bulk_worker(
    ctrl: Arc<AtomicU8>,
    pastes_pending: Arc<AtomicU32>,
) -> std::io::Result<Sender<BulkJob>> {
    let (tx, rx): (Sender<BulkJob>, Receiver<BulkJob>) = bounded(BULK_QUEUE);
    std::thread::Builder::new()
        .name("input-bulk".into())
        .spawn(move || {
            while let Ok(job) = rx.recv() {
                match job {
                    BulkJob::Clipboard { op, mime, data } => {
                        apply_clipboard(op, &mime, &data, &ctrl, CLIPBOARD_OPEN_ATTEMPTS);
                        if op == PASTE_OP {
                            // released whether or not the paste worked: the events held
                            // behind it must not wait for a Ctrl+V that is not coming
                            pastes_pending.fetch_sub(1, Ordering::Release);
                        }
                    }
                    BulkJob::Drop { pt, items } => post_drop(pt, &items),
                }
            }
        })?;
    Ok(tx)
}

pub struct Injector {
    virt: VirtualScreen,
    /// The session's display as last resolved. Kept while `lost`, so releases still land.
    target: Option<DisplayRect>,
    device_name: Option<String>,
    geometry: GeometrySource,
    /// The session's display stopped resolving. Positional input is withheld rather than
    /// widened to the whole desktop; releases still go through.
    lost: bool,
    suppressed_positional: u64,
    buttons: u16,
    /// When the last `POINTER_DOWN`/`UP`/`CANCEL` was injected; see [`RESTATE_GUARD`].
    last_authoritative: Option<Instant>,
    restate_guard: bool,
    restates_ignored: u64,
    down_keys: HashSet<(u16, bool)>,
    ctrl: Arc<AtomicU8>,
    pen_dev: Option<PointerDevice>,
    touch_dev: Option<PointerDevice>,
    contacts: HashMap<u32, Contact>,
    stray_touch_frames: u64,
    pen_down: bool,
    /// Pointer id of the pen contact currently down, so the release on teardown carries the
    /// same id the down did.
    pen_id: u32,
    /// The client holds pointer lock, so `MOUSE_DELTA` drives the cursor and the frozen
    /// coordinates an absolute frame still carries must be ignored.
    mouse_relative: bool,
    /// The two settings the user can move under a live session: whether remote control is on
    /// at all, and what a pointer-locked cursor is clamped against. Shared with the command that
    /// writes them, so a change reaches the next event rather than the next join.
    live: SharedLiveInput,
    /// Sub-unit wheel motion carried to the next event, per axis (`WHEEL_DELTA` units).
    wheel_rest: (f32, f32),
    reported_keys: HashSet<String>,
    /// Clipboard and drop work, off this thread. `None` runs it inline.
    bulk: Option<Sender<BulkJob>>,
    /// Pastes queued to the bulk worker whose Ctrl+V has not been sent yet. Key, text and
    /// button events wait for it (bounded by [`PASTE_ORDER_WAIT`]).
    pastes_pending: Arc<AtomicU32>,
    scratch: Vec<INPUT>,
    touch_scratch: Vec<POINTER_TYPE_INFO>,
}

impl Injector {
    pub fn new(client: ClientProfile) -> Self {
        // the scancode table is layout-neutral, so nothing here depends on the client platform
        let ClientProfile {
            device_name,
            device_path,
            live,
            ..
        } = client;
        let pen_dev = PointerDevice::create(PT_PEN, 1)
            .map_err(|e| teprintln!("synthetic pen device unavailable, degrading pen→mouse: {e}"))
            .ok();
        let touch_dev = PointerDevice::create(PT_TOUCH, MAX_TOUCH_COUNT)
            .map_err(|e| {
                teprintln!("synthetic touch device unavailable, degrading touch→mouse: {e}")
            })
            .ok();

        let geometry_worker = tune::on("input_geometry_worker");
        let restate_guard = tune::on("input_button_restate_guard");
        let bulk_worker = tune::on("input_bulk_worker");

        // the first read is synchronous either way: the queue holds the session's first events
        // until this returns, and they must not be mapped against nothing
        let mut ident = if geometry_worker {
            match device_path {
                Some(path) => TargetIdent::by_path(device_name.clone(), path),
                None => TargetIdent::owned(device_name.clone()),
            }
        } else {
            TargetIdent::by_name(device_name.clone())
        };
        let first = ident.query();
        let geometry = if geometry_worker {
            match spawn_geometry_worker(ident, first) {
                Ok(g) => g,
                Err(e) => {
                    teprintln!(
                        "remote-input: geometry worker did not start ({e}); refreshing inline"
                    );
                    GeometrySource::Inline {
                        ident: TargetIdent::by_name(device_name.clone()),
                        checked: Some(Instant::now()),
                    }
                }
            }
        } else {
            GeometrySource::Inline {
                ident,
                checked: Some(Instant::now()),
            }
        };

        let ctrl = Arc::new(AtomicU8::new(0));
        let pastes_pending = Arc::new(AtomicU32::new(0));
        let bulk = if bulk_worker {
            spawn_bulk_worker(Arc::clone(&ctrl), Arc::clone(&pastes_pending))
                .map_err(|e| {
                    teprintln!("remote-input: bulk worker did not start ({e}); clipboard and drop run inline")
                })
                .ok()
        } else {
            None
        };

        let mut inj = Injector {
            virt: first.virt,
            target: None,
            device_name,
            geometry,
            lost: false,
            suppressed_positional: 0,
            buttons: 0,
            last_authoritative: None,
            restate_guard,
            restates_ignored: 0,
            down_keys: HashSet::new(),
            ctrl,
            pen_dev,
            touch_dev,
            contacts: HashMap::new(),
            stray_touch_frames: 0,
            pen_down: false,
            pen_id: 1,
            // no lock until the client says so: starting at `true`, a touch client's opening tap
            // (a bare `POINTER_DOWN` with no move before it) clicks wherever the host cursor is
            mouse_relative: false,
            live,
            wheel_rest: (0.0, 0.0),
            reported_keys: HashSet::new(),
            bulk,
            pastes_pending,
            scratch: Vec::with_capacity(16),
            touch_scratch: Vec::with_capacity(MAX_TOUCH_COUNT as usize),
        };
        inj.apply_geometry(first);
        inj
    }

    /// Pick up a new geometry, if there is one. With the worker this is one atomic load per
    /// event and nothing else unless the display actually changed.
    fn refresh_geometry(&mut self) {
        let snap = match &mut self.geometry {
            GeometrySource::Inline { ident, checked } => {
                if matches!(*checked, Some(t) if t.elapsed() < GEOMETRY_REFRESH) {
                    return;
                }
                *checked = Some(Instant::now());
                ident.query()
            }
            GeometrySource::Worker { shared, seen, .. } => {
                let version = shared.version.load(Ordering::Acquire);
                if version == *seen {
                    return;
                }
                // never wait: the worker holds this lock for one struct copy. An event that finds
                // it held is mapped against the previous geometry, and the next one looks again
                let snap = match shared.snap.try_lock() {
                    Ok(g) => *g,
                    Err(TryLockError::Poisoned(p)) => *p.into_inner(),
                    Err(TryLockError::WouldBlock) => return,
                };
                *seen = version;
                snap
            }
        };
        self.apply_geometry(snap);
    }

    fn apply_geometry(&mut self, snap: GeometrySnapshot) {
        self.virt = snap.virt;
        let Some(name) = self.device_name.as_deref() else {
            return;
        };
        match snap.target {
            Some(r) => {
                if self.lost {
                    tprintln!(
                        "remote-input: display {name} resolves again at ({},{}) {}x{}; positional \
                         input resumed ({} event(s) withheld while it did not)",
                        r.left,
                        r.top,
                        r.width,
                        r.height,
                        self.suppressed_positional
                    );
                } else if self.target != Some(r) {
                    tprintln!(
                        "remote-input geometry: display {name} at ({},{}) {}x{}; desktop origin ({},{})",
                        r.left, r.top, r.width, r.height, self.virt.left, self.virt.top
                    );
                }
                self.target = Some(r);
                self.lost = false;
            }
            None => {
                if !self.lost {
                    teprintln!(
                        "remote-input: display {name} does not resolve to this session's display; \
                         positional input is withheld until it does (never widened to the whole desktop)"
                    );
                    self.lost = true;
                }
            }
        }
    }

    /// Count a positional event withheld because the session's display does not resolve.
    #[inline]
    fn withhold(&mut self) {
        self.suppressed_positional += 1;
    }

    pub fn dispatch(&mut self, ev: &InputEvent) {
        self.refresh_geometry();
        if orders_after_paste(ev) {
            self.await_pastes();
        }
        match ev {
            InputEvent::Pointer {
                source,
                id,
                x,
                y,
                pressure,
                tilt_x,
                tilt_y,
                twist,
                w,
                h,
                buttons,
                phase,
            } => match *source {
                SRC_MOUSE => self.mouse_pointer(*x, *y, *buttons, *phase),
                SRC_PEN => self.pen(
                    *id, *x, *y, *pressure, *tilt_x, *tilt_y, *twist, *buttons, *phase,
                ),
                SRC_TOUCH => self.touch(*id, *x, *y, *pressure, *w, *h, *phase),
                _ => {}
            },
            InputEvent::PointerBatch {
                source,
                id,
                buttons,
                samples,
                predicted,
            } => self.pointer_batch(*source, *id, *buttons, samples, *predicted),
            InputEvent::Wheel { dx, dy, mode, .. } => self.wheel(*dx, *dy, *mode),
            InputEvent::Zoom { delta } => self.zoom(*delta),
            InputEvent::MouseDelta {
                dx,
                dy,
                buttons,
                unadjusted,
            } => self.mouse_delta(*dx, *dy, *buttons, *unadjusted),
            InputEvent::Key {
                down, code, key, ..
            } => self.key(*down, code, key),
            InputEvent::Text { s, .. } => self.text(s),
            InputEvent::Resize { .. } => match &self.geometry {
                // a resize asks for a new mode, so the worker re-reads now instead of up to
                // 500 ms later. A full slot is a refresh already asked for
                GeometrySource::Worker { poke, .. } => {
                    let _ = poke.try_send(());
                }
                GeometrySource::Inline { .. } => self.virt = VirtualScreen::query(),
            },
            InputEvent::Lifecycle(l) => self.lifecycle(*l),
            InputEvent::Clipboard { op, mime, data } => self.clipboard(*op, mime, data),
            InputEvent::Drag { .. } => {}
            InputEvent::Drop { x, y, items } => self.drop_files(*x, *y, items),
            // handled by the transport before dispatch: none of these inject anything
            InputEvent::Ping { .. }
            | InputEvent::Pong { .. }
            | InputEvent::ClientStats(_)
            | InputEvent::KeyframeRequest { .. } => {}
        }
    }

    /// The button word this injector has actually sent.
    pub fn buttons(&self) -> u16 {
        self.buttons
    }

    pub fn release_all(&mut self) {
        self.scratch.clear();
        button_transitions(self.buttons, 0, &mut self.scratch);
        for (scan, ext) in self.down_keys.drain() {
            self.scratch.push(key_input(
                scan,
                KEYEVENTF_SCANCODE | ext_flag(ext) | KEYEVENTF_KEYUP,
            ));
        }
        self.ctrl.store(0, Ordering::Relaxed);
        send_inputs(&self.scratch);
        self.scratch.clear();
        self.buttons = 0;
        // as authoritative as an Up: a fast frame sent before the blur that still says PRIMARY
        // must not press again, with the client now blurred and silent
        if self.restate_guard {
            self.last_authoritative = Some(Instant::now());
        }

        if self.pen_down {
            if let Some(dev) = self.pen_dev.as_ref().map(PointerDevice::raw) {
                let info = self.pen_info(
                    self.pen_id,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0.0,
                    0,
                    POINTER_FLAG_UP,
                );
                inject_pointer(dev, &[info]);
            }
            self.pen_down = false;
        }

        if !self.contacts.is_empty() {
            if let Some(dev) = self.touch_dev.as_ref().map(PointerDevice::raw) {
                let mut frame = std::mem::take(&mut self.touch_scratch);
                frame.clear();
                for (id, c) in &self.contacts {
                    frame.push(self.touch_info(*id, c, POINTER_FLAG_UP));
                }
                inject_pointer(dev, &frame);
                self.touch_scratch = frame;
            }
            self.contacts.clear();
        }
    }

    /// The button state a frame leaves behind.
    ///
    /// `authoritative` is a `POINTER_DOWN`/`UP`/`CANCEL`, which travel in order on the reliable
    /// channel. Everything else restates the word on the unordered fast channel, and a
    /// restatement may not cross zero inside [`RESTATE_GUARD`] of an authoritative frame. While
    /// a `positional` frame's display does not resolve, it may release but never press.
    fn next_buttons(&mut self, word: u16, authoritative: bool, positional: bool) -> u16 {
        let mut next = if authoritative {
            if self.restate_guard {
                self.last_authoritative = Some(Instant::now());
            }
            word
        } else if self.restate_guard {
            let kept = restated_buttons(
                self.buttons,
                word,
                self.last_authoritative.map(|t| t.elapsed()),
            );
            if kept != word {
                self.restates_ignored += 1;
                if self.restates_ignored == 1 {
                    tprintln!(
                        "remote-input: a late move restated buttons {word:#x} over {:#x} just after a \
                         press or release; kept the press/release (further ones are counted, not logged)",
                        self.buttons
                    );
                }
            }
            kept
        } else {
            word
        };
        if positional && self.lost {
            next &= self.buttons;
        }
        next
    }

    fn mouse_pointer(&mut self, x: f32, y: f32, buttons: u16, phase: Phase) {
        match phase {
            Phase::Leave | Phase::Out => return,
            Phase::Cancel => {
                self.next_buttons(0, true, false);
                self.scratch.clear();
                button_transitions(self.buttons, 0, &mut self.scratch);
                send_inputs(&self.scratch);
                self.scratch.clear();
                self.buttons = 0;
                return;
            }
            // an absolute move is proof the client is not locked, for clients that never send
            // `POINTERLOCK_STATE`. A `Down`/`Up` is not: those still arrive while locked
            Phase::Move | Phase::Enter | Phase::Over => self.mouse_relative = false,
            _ => {}
        }
        self.scratch.clear();
        self.push_mouse_pointer(x, y, buttons, phase);
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    /// The body of `mouse_pointer` without the flush, so a batch can build several samples into
    /// one `SendInput` array instead of paying a kernel transition per sample. Updates
    /// `self.buttons` to what it queued.
    fn push_mouse_pointer(&mut self, x: f32, y: f32, buttons: u16, phase: Phase) {
        if positions_cursor(self.mouse_relative, phase) {
            if self.lost {
                self.withhold();
            } else {
                let (ax, ay) = self.abs(x, y);
                self.scratch.push(mouse_input(
                    ax,
                    ay,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ));
            }
        }
        let authoritative = matches!(phase, Phase::Down | Phase::Up | Phase::Cancel);
        let next = self.next_buttons(buttons, authoritative, true);
        button_transitions(self.buttons, next, &mut self.scratch);
        self.buttons = next;
    }

    /// Inject one relative pointer movement.
    ///
    /// **Exactly one pointer curve, applied by whichever side holds raw counts.** Raw counts
    /// (`unadjustedMovement` granted) go relative, so Windows' pointer speed and "enhance
    /// pointer precision" apply; counts the client's OS already curved keep the
    /// tracked-absolute path. Curving on neither side is a two-to-six-times gain cut, which
    /// reads as lag.
    ///
    /// `PointerScope::Display` keeps the tracked path whatever the flag says: clamping to one
    /// monitor needs a position, and `MOUSEEVENTF_MOVE` has none.
    ///
    /// The tracked path starts from `GetCursorPos` every frame (a remembered position would
    /// snap back a cursor the host's own trackpad moved), so [`to_absolute`] has to land on
    /// exactly the pixel asked for: any error in it is the next frame's starting point.
    fn mouse_delta(&mut self, dx: i16, dy: i16, buttons: u16, unadjusted: bool) {
        self.scratch.clear();
        self.push_mouse_delta(dx, dy, buttons, unadjusted);
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    /// The body of `mouse_delta` without the flush, so the button-word handling is testable
    /// without a `SendInput`. Updates `self.buttons` to what it queued.
    fn push_mouse_delta(&mut self, dx: i16, dy: i16, buttons: u16, unadjusted: bool) {
        self.mouse_relative = true;
        let os_curve = self.lets_os_curve(unadjusted);
        // only the `Display` scope is about this session's display; `All` is the desktop
        let confined = matches!(self.live.pointer_scope(), PointerScope::Display);
        if dx != 0 || dy != 0 {
            if os_curve {
                self.scratch
                    .push(mouse_input(dx as i32, dy as i32, 0, MOUSEEVENTF_MOVE));
            } else if confined && self.lost {
                self.withhold();
            } else {
                let pos = self.advance_rel(dx as i32, dy as i32);
                self.scratch.push(mouse_input(
                    to_absolute(pos.x, self.virt.left, self.virt.width),
                    to_absolute(pos.y, self.virt.top, self.virt.height),
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ));
            }
        }
        let next = self.next_buttons(buttons, false, confined);
        button_transitions(self.buttons, next, &mut self.scratch);
        self.buttons = next;
    }

    /// Whether this frame's motion should be handed to Windows' pointer curve. Split out from
    /// `mouse_delta` so the decision is testable without a desktop.
    fn lets_os_curve(&self, unadjusted: bool) -> bool {
        unadjusted && matches!(self.live.pointer_scope(), PointerScope::All)
    }

    /// The rectangle a pointer-locked cursor may not leave.
    ///
    /// `PointerScope::Display` is this session's own monitor, which is what the client is
    /// looking at; `All` is the whole virtual desktop, including the host's real monitors. With
    /// no device name (a harness) there is no display to confine to, and the desktop is the
    /// answer; a session whose display has stopped resolving does not get here at all, because
    /// `mouse_delta` withholds the motion instead.
    fn pointer_bounds(&self) -> DisplayRect {
        match (self.live.pointer_scope(), self.target) {
            (PointerScope::Display, Some(r)) => r,
            _ => DisplayRect {
                left: self.virt.left,
                top: self.virt.top,
                width: self.virt.width as u32,
                height: self.virt.height as u32,
            },
        }
    }

    /// Apply a delta to the cursor's current position and clamp it to [`Self::pointer_bounds`].
    ///
    /// The base is always the real cursor (`GetCursorPos`), never a remembered one, so motion
    /// from the host's own mouse or trackpad between two client frames is where the next delta
    /// starts. Both backends clamp the same way and to the same choice of rectangle, so a
    /// client that crosses to the host's desktop on one OS crosses on the other.
    fn advance_rel(&mut self, dx: i32, dy: i32) -> POINT {
        let base = unsafe {
            let mut p = POINT::default();
            if GetCursorPos(&mut p).is_ok() {
                p
            } else {
                self.pixel(0.5, 0.5)
            }
        };
        let b = self.pointer_bounds();
        POINT {
            x: (base.x + dx).clamp(b.left, b.left + b.width as i32 - 1),
            y: (base.y + dy).clamp(b.top, b.top + b.height as i32 - 1),
        }
    }

    fn wheel(&mut self, dx: f32, dy: f32, mode: u8) {
        let scale = match mode {
            1 => WHEEL_DELTA,
            2 => WHEEL_DELTA * 3.0,
            _ => WHEEL_DELTA / 100.0,
        };
        self.scratch.clear();
        let v = wheel_units(&mut self.wheel_rest.1, -dy * scale);
        if v != 0 {
            self.scratch
                .push(mouse_input(0, 0, v as u32, MOUSEEVENTF_WHEEL));
        }
        let h = wheel_units(&mut self.wheel_rest.0, dx * scale);
        if h != 0 {
            self.scratch
                .push(mouse_input(0, 0, h as u32, MOUSEEVENTF_HWHEEL));
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    fn pointer_batch(
        &mut self,
        source: u8,
        id: u32,
        buttons: u16,
        samples: &[MoveSample],
        predicted: u8,
    ) {
        if samples.is_empty() {
            return;
        }
        match source {
            // one call per sample: `InjectSyntheticPointerInput` takes a frame of simultaneous
            // contacts (at most the device's `maxCount`), not samples over time. Predicted
            // samples are dropped: a corrected guess still leaves a kink in the ink
            SRC_PEN | SRC_TOUCH => {
                let real = samples.len() - predicted as usize;
                for s in &samples[..real] {
                    if source == SRC_PEN {
                        self.pen(
                            id,
                            s.x,
                            s.y,
                            s.pressure,
                            s.tilt_x,
                            s.tilt_y,
                            s.twist,
                            buttons,
                            Phase::Move,
                        );
                    } else {
                        self.touch(id, s.x, s.y, s.pressure, 0.0, 0.0, Phase::Move);
                    }
                }
            }
            // one `SendInput` array for the whole batch: one kernel transition, not one per
            // sample. Predictions are kept, because the cursor is last-value-wins
            _ => {
                self.mouse_relative = false;
                self.scratch.clear();
                if self.lost {
                    self.withhold();
                } else {
                    for s in &samples[..samples.len() - 1] {
                        let (ax, ay) = self.abs(s.x, s.y);
                        self.scratch.push(mouse_input(
                            ax,
                            ay,
                            0,
                            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        ));
                    }
                }
                let last = samples[samples.len() - 1];
                self.push_mouse_pointer(last.x, last.y, buttons, Phase::Move);
                send_inputs(&self.scratch);
                self.scratch.clear();
            }
        }
    }

    fn zoom(&mut self, delta: f32) {
        if delta == 0.0 {
            return;
        }
        let notches = (delta * WHEEL_DELTA).round() as i32;
        if notches == 0 {
            return;
        }
        self.scratch.clear();
        let ctrl_held = self.ctrl.load(Ordering::Relaxed) != 0;
        if !ctrl_held {
            self.scratch.push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE));
        }
        self.scratch
            .push(mouse_input(0, 0, notches as u32, MOUSEEVENTF_WHEEL));
        if !ctrl_held {
            self.scratch
                .push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    fn key(&mut self, down: bool, code: &str, key: &str) {
        if let Some((scan, ext)) = code_to_scancode(code) {
            let mut flags = KEYEVENTF_SCANCODE | ext_flag(ext);
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            send_inputs(&[key_input(scan, flags)]);
            if down {
                self.down_keys.insert((scan, ext));
            } else {
                self.down_keys.remove(&(scan, ext));
            }
            if scan == SC_LCTRL {
                let bit = if ext { CTRL_RIGHT } else { CTRL_LEFT };
                if down {
                    self.ctrl.fetch_or(bit, Ordering::Relaxed);
                } else {
                    self.ctrl.fetch_and(!bit, Ordering::Relaxed);
                }
            }
            return;
        }
        if down {
            let mut chars = key.chars();
            if let (Some(c), None) = (chars.next(), chars.clone().next()) {
                if !c.is_control() {
                    self.text(&c.to_string());
                    return;
                }
            }
        }
        // once per code: a held unmapped key repeats
        if down
            && self.reported_keys.len() < MAX_REPORTED_KEYS
            && self.reported_keys.insert(code.to_string())
        {
            teprintln!(
                "remote-input: no scancode for key code={code:?} (key={key:?}); not injected"
            );
        }
    }

    fn text(&mut self, s: &str) {
        self.scratch.clear();
        let mut buf = [0u16; 2];
        for ch in s.chars() {
            for unit in ch.encode_utf16(&mut buf) {
                self.scratch.push(key_input(*unit, KEYEVENTF_UNICODE));
                self.scratch
                    .push(key_input(*unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn pen(
        &mut self,
        id: u32,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        twist: f32,
        buttons: u16,
        phase: Phase,
    ) {
        let Some(dev) = self.pen_dev.as_ref().map(PointerDevice::raw) else {
            let b = if pressure > 0.0 || buttons & btn::PRIMARY != 0 {
                btn::PRIMARY
            } else {
                0
            };
            self.mouse_pointer(x, y, b, phase);
            return;
        };
        let ending = matches!(phase, Phase::Up | Phase::Cancel | Phase::Leave | Phase::Out);
        if self.lost && !(ending && self.pen_down) {
            self.withhold();
            return;
        }
        self.pen_id = pen_pointer_id(id);
        let flags = match phase {
            Phase::Down => {
                self.pen_down = true;
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            }
            Phase::Move | Phase::Enter | Phase::Over => {
                if self.pen_down {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                } else {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE // hover
                }
            }
            Phase::Up => {
                self.pen_down = false;
                POINTER_FLAG_UP
            }
            Phase::Cancel => {
                self.pen_down = false;
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            }
            Phase::Leave | Phase::Out => {
                self.pen_down = false;
                POINTER_FLAG_UP
            }
        };
        let info = self.pen_info(id, x, y, pressure, tilt_x, tilt_y, twist, buttons, flags);
        inject_pointer(dev, &[info]);
    }

    #[allow(clippy::too_many_arguments)]
    fn pen_info(
        &self,
        id: u32,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        twist: f32,
        buttons: u16,
        flags: POINTER_FLAGS,
    ) -> POINTER_TYPE_INFO {
        let pt = self.inject_point(x, y);
        let mut pen_flags = 0u32;
        if buttons & btn::ERASER != 0 {
            pen_flags |= PEN_FLAG_ERASER;
        }
        if buttons & btn::SECONDARY != 0 {
            pen_flags |= PEN_FLAG_BARREL;
        }
        let pen = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: pen_pointer_id(id),
                pointerFlags: flags,
                ptPixelLocation: pt,
                ..Default::default()
            },
            penFlags: pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: (pressure.clamp(0.0, 1.0) * 1024.0).round() as u32,
            rotation: (twist.rem_euclid(360.0)) as u32,
            tiltX: tilt_x.clamp(-90.0, 90.0) as i32,
            tiltY: tilt_y.clamp(-90.0, 90.0) as i32,
        };
        POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn touch(&mut self, id: u32, x: f32, y: f32, pressure: f32, w: f32, h: f32, phase: Phase) {
        let Some(dev) = self.touch_dev.as_ref().map(PointerDevice::raw) else {
            let b = matches!(
                phase,
                Phase::Down | Phase::Move | Phase::Enter | Phase::Over
            );
            self.mouse_pointer(x, y, if b { btn::PRIMARY } else { 0 }, phase);
            return;
        };
        let contact = Contact {
            x,
            y,
            pressure,
            w,
            h,
        };
        let step = track_contact(&mut self.contacts, id, contact, phase);
        let own_flags = match step {
            ContactStep::Skip => return,
            ContactStep::Ignore => {
                self.stray_touch_frames += 1;
                if self.stray_touch_frames == 1 {
                    tprintln!(
                        "remote-input: touch {id} {phase:?} arrived for a contact that is not down \
                         (a late frame); not injected (further ones are counted, not logged)"
                    );
                }
                return;
            }
            ContactStep::End { .. } if self.lost => {
                // released where it was last seen: the display it was on has gone, and the
                // contact must not outlive it
                POINTER_FLAG_UP
            }
            _ if self.lost => {
                self.withhold();
                // a contact begun while lost was never injected; forget it again
                if step == ContactStep::Begin {
                    self.contacts.remove(&id);
                }
                return;
            }
            ContactStep::Begin => POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT,
            ContactStep::Update => {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            }
            ContactStep::End { canceled: false } => POINTER_FLAG_UP,
            ContactStep::End { canceled: true } => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
        };

        let mut frame = std::mem::take(&mut self.touch_scratch);
        frame.clear();
        for (cid, c) in &self.contacts {
            let flags = if *cid == id {
                own_flags
            } else {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            };
            frame.push(self.touch_info(*cid, c, flags));
        }
        inject_pointer(dev, &frame);
        self.touch_scratch = frame;

        finish_contact(&mut self.contacts, id, step);
    }

    fn touch_info(&self, id: u32, c: &Contact, flags: POINTER_FLAGS) -> POINTER_TYPE_INFO {
        let pt = self.inject_point(c.x, c.y);
        let (space_w, space_h) = self.space_dims();
        let cw = ((c.w * space_w as f32).round() as i32).max(2);
        let ch = ((c.h * space_h as f32).round() as i32).max(2);
        let in_contact = flags.contains(POINTER_FLAG_INCONTACT);
        let pressure = if in_contact && c.pressure == 0.0 {
            512
        } else {
            (c.pressure.clamp(0.0, 1.0) * 1024.0).round() as u32
        };
        let touch = POINTER_TOUCH_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_TOUCH,
                pointerId: id,
                pointerFlags: flags,
                ptPixelLocation: pt,
                ..Default::default()
            },
            touchFlags: 0,
            touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
            rcContact: RECT {
                left: pt.x - cw / 2,
                top: pt.y - ch / 2,
                right: pt.x + cw / 2,
                bottom: pt.y + ch / 2,
            },
            rcContactRaw: RECT::default(),
            orientation: 0,
            pressure,
        };
        POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 { touchInfo: touch },
        }
    }

    /// Hold an event that must follow a queued paste's Ctrl+V until the worker has sent it.
    /// Costs one atomic load when no paste is outstanding, which is every event but the few
    /// after a paste.
    fn await_pastes(&mut self) {
        if self.pastes_pending.load(Ordering::Acquire) == 0 {
            return;
        }
        if !wait_for_pastes(&self.pastes_pending, PASTE_ORDER_WAIT) {
            teprintln!(
                "clipboard: a paste was still pending after {} ms; the next key or click goes \
                 ahead of its Ctrl+V",
                PASTE_ORDER_WAIT.as_millis()
            );
        }
    }

    fn clipboard(&mut self, op: u8, mime: &str, data: &[u8]) {
        match &self.bulk {
            Some(tx) => {
                let job = BulkJob::Clipboard {
                    op,
                    mime: mime.to_string(),
                    data: data.to_vec(),
                };
                if op == PASTE_OP {
                    self.pastes_pending.fetch_add(1, Ordering::Release);
                }
                if tx.try_send(job).is_err() {
                    if op == PASTE_OP {
                        self.pastes_pending.fetch_sub(1, Ordering::Release);
                    }
                    teprintln!(
                        "clipboard: {} queued jobs are still waiting; this {mime} ({} bytes) is dropped",
                        BULK_QUEUE,
                        data.len()
                    );
                }
            }
            // no worker: on this thread, one attempt at the clipboard
            None => apply_clipboard(op, mime, data, &self.ctrl, 1),
        }
    }

    fn drop_files(&mut self, x: f32, y: f32, items: &[DropItem]) {
        if items.is_empty() {
            return;
        }
        if self.lost {
            self.withhold();
            teprintln!("drop: this session's display does not resolve; the drop is not delivered");
            return;
        }
        let pt = self.pixel(x, y);
        match &self.bulk {
            Some(tx) => {
                let job = BulkJob::Drop {
                    pt,
                    items: items.to_vec(),
                };
                if tx.try_send(job).is_err() {
                    teprintln!(
                        "drop: {BULK_QUEUE} queued jobs are still waiting; this drop is dropped"
                    );
                }
            }
            None => post_drop(pt, items),
        }
    }

    fn lifecycle(&mut self, l: Lifecycle) {
        match l {
            Lifecycle::Focus(false) | Lifecycle::Visibility(false) => self.release_all(),
            Lifecycle::PointerLock(locked) => {
                self.mouse_relative = locked;
            }
            _ => {}
        }
    }

    fn abs(&self, nx: f32, ny: f32) -> (i32, i32) {
        let pt = self.pixel(nx, ny);
        (
            to_absolute(pt.x, self.virt.left, self.virt.width),
            to_absolute(pt.y, self.virt.top, self.virt.height),
        )
    }

    fn pixel(&self, nx: f32, ny: f32) -> POINT {
        let (left, top, w, h) = match self.target {
            Some(r) => (r.left, r.top, r.width, r.height),
            None => (
                self.virt.left,
                self.virt.top,
                self.virt.width as u32,
                self.virt.height as u32,
            ),
        };
        POINT {
            x: norm_to_pixel(nx, left, w),
            y: norm_to_pixel(ny, top, h),
        }
    }

    fn inject_point(&self, nx: f32, ny: f32) -> POINT {
        let p = self.pixel(nx, ny);
        POINT {
            x: p.x - self.virt.left,
            y: p.y - self.virt.top,
        }
    }

    fn space_dims(&self) -> (i32, i32) {
        match self.target {
            Some(r) => (r.width as i32, r.height as i32),
            None => (self.virt.width, self.virt.height),
        }
    }
}

/// One synthetic pointer device, given back when dropped. It is owned from the moment it exists,
/// so a constructor that panics or returns early part way still releases it.
struct PointerDevice(HSYNTHETICPOINTERDEVICE);

impl PointerDevice {
    fn create(
        kind: windows::Win32::UI::WindowsAndMessaging::POINTER_INPUT_TYPE,
        max_count: u32,
    ) -> windows::core::Result<Self> {
        // SAFETY: plain creation; the value returned owns the result.
        unsafe { CreateSyntheticPointerDevice(kind, max_count, POINTER_FEEDBACK_DEFAULT) }.map(Self)
    }

    fn raw(&self) -> HSYNTHETICPOINTERDEVICE {
        self.0
    }
}

/// Windows caps synthetic pointer devices per process, so a pair left behind per session ends
/// in pen pressure and multi-touch quietly degrading to mouse after enough reconnects.
impl Drop for PointerDevice {
    fn drop(&mut self) {
        release_pointer_device(self.0);
    }
}

/// Destroy a synthetic pointer device and close the handle it is.
///
/// The device is a kernel `RawInputManager` handle, and `DestroySyntheticPointerDevice` tears the
/// device down without closing it: measured, every destroyed device stayed in the handle table,
/// two per session. Whether some Windows build closes it in the destroy is not documented, and a
/// value the destroy freed can be handed straight to another session's new device. So the close
/// has to prove the value still names this device: a duplicate taken before the destroy keeps the
/// object alive, and only a handle that still compares equal to it is closed. Without a duplicate
/// nothing is closed, which costs a handle rather than another session's pen and touch.
fn release_pointer_device(dev: HSYNTHETICPOINTERDEVICE) {
    let h = HANDLE(dev.0);
    note_device_type(h);
    let me = unsafe { GetCurrentProcess() };
    let mut dup = HANDLE::default();
    // SAFETY: duplicating a handle this process owns, into this process.
    let held = unsafe { DuplicateHandle(me, h, me, &mut dup, 0, false, DUPLICATE_SAME_ACCESS) };
    // SAFETY: a device this injector created and nothing else destroys.
    unsafe { DestroySyntheticPointerDevice(dev) };
    if let Err(e) = held {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            teprintln!(
                "remote-input: a pointer device handle could not be duplicated ({e}); destroyed \
                 devices keep their handle open"
            )
        });
        return;
    }
    // SAFETY: `dup` is ours, and `h` is either still the device or a value that cannot compare
    // equal to an object `dup` keeps alive. That rests on every device handle naming an object of
    // its own (`each_pointer_device_is_an_object_of_its_own` checks it).
    unsafe {
        if CompareObjectHandles(h, dup).as_bool() {
            let _ = CloseHandle(h);
        }
        let _ = CloseHandle(dup);
    }
}

/// Logs once if a device handle is not the object type its release was measured against, which
/// would mean a Windows build where the handle is something else.
fn note_device_type(h: HANDLE) {
    static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if LOGGED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let kind = handle_type_name(h);
    if kind.as_deref() != Some("RawInputManager")
        && !LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed)
    {
        teprintln!("remote-input: a pointer device handle reads as {kind:?}, not RawInputManager");
    }
}

/// The leading `UNICODE_STRING` of `OBJECT_TYPE_INFORMATION`.
#[repr(C)]
struct UnicodeString {
    len: u16,
    max: u16,
    buf: *const u16,
}

/// The kernel object type of a live handle this process holds, from `NtQueryObject`.
fn handle_type_name(h: HANDLE) -> Option<String> {
    type NtQueryObject = unsafe extern "system" fn(HANDLE, u32, *mut u64, u32, *mut u32) -> i32;
    const OBJECT_TYPE_INFORMATION: u32 = 2;
    static QUERY: std::sync::OnceLock<Option<NtQueryObject>> = std::sync::OnceLock::new();
    let query = (*QUERY.get_or_init(|| {
        // SAFETY: ntdll is mapped into every process, and the export has this signature.
        unsafe {
            let m = GetModuleHandleW(windows::core::w!("ntdll.dll")).ok()?;
            let f = GetProcAddress(m, windows::core::s!("NtQueryObject"))?;
            Some(std::mem::transmute::<
                unsafe extern "system" fn() -> isize,
                NtQueryObject,
            >(f))
        }
    }))?;
    // u64s, so the record at the start is aligned for its pointer
    let mut buf = [0u64; 128];
    let mut len = 0u32;
    // SAFETY: a buffer of the stated size; the call writes at most that much.
    let st = unsafe {
        query(
            h,
            OBJECT_TYPE_INFORMATION,
            buf.as_mut_ptr(),
            std::mem::size_of_val(&buf) as u32,
            &mut len,
        )
    };
    if st < 0 {
        return None;
    }
    // SAFETY: the call wrote a UNICODE_STRING at the start whose buffer lies inside `buf`.
    let name = unsafe { &*(buf.as_ptr() as *const UnicodeString) };
    if name.buf.is_null() || name.len == 0 {
        return None;
    }
    // SAFETY: as above, `len` bytes long.
    Some(String::from_utf16_lossy(unsafe {
        std::slice::from_raw_parts(name.buf, name.len as usize / 2)
    }))
}

/// Record a touch frame's contact before the frame is built: a new contact is added, a known one
/// updated, and a frame for a contact that is not down changes nothing.
fn track_contact(
    contacts: &mut HashMap<u32, Contact>,
    id: u32,
    contact: Contact,
    phase: Phase,
) -> ContactStep {
    let step = contact_step(contacts.contains_key(&id), phase);
    if !matches!(step, ContactStep::Ignore | ContactStep::Skip) {
        contacts.insert(id, contact);
    }
    step
}

/// Forget a contact once the frame that ends it has been injected.
fn finish_contact(contacts: &mut HashMap<u32, Contact>, id: u32, step: ContactStep) {
    if matches!(step, ContactStep::End { .. }) {
        contacts.remove(&id);
    }
}

/// What a move-type frame's restated button word may change.
///
/// Pointer Events guarantee that the first button pressed fires `pointerdown` and the last one
/// released fires `pointerup`; only a *chorded* change (a second button while one is held) is a
/// `pointermove`. So a move-type frame whose word crosses zero is restating a state that a
/// `POINTER_DOWN`/`UP` owns. Just after one of those it is a late frame from the other side of
/// it, and honouring it re-presses a button the user released, or releases one mid-drag.
/// Outside the guard it is honoured, which is what recovers a press or a release the page
/// never saw (a drag entering the stage with a button already down).
fn restated_buttons(current: u16, restated: u16, since_authoritative: Option<Duration>) -> u16 {
    let crosses = (current & MOUSE_BUTTONS == 0) != (restated & MOUSE_BUTTONS == 0);
    if crosses && since_authoritative.is_some_and(|d| d < RESTATE_GUARD) {
        current
    } else {
        restated
    }
}

/// A pixel on the virtual desktop as a `MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`
/// coordinate along one axis.
///
/// Windows maps a normalised coordinate back with a truncating `dx * extent >> 16`. The obvious
/// `round(p / extent * 65535)` therefore lands one pixel short for most of the desktop (on a
/// 4480-px desktop pixel 3000 comes back as 2999), and the tracked-delta path, which re-bases
/// on `GetCursorPos` every frame, repeats the error on every event. The ceiling of
/// `p * 65536 / extent` is the smallest coordinate that truncates back to `p`, so it is exact
/// under truncation, and still exact if the mapping rounds instead, for any desktop under
/// 32768 px.
fn to_absolute(p: i32, origin: i32, extent: i32) -> i32 {
    let extent = i64::from(extent.max(1));
    let q = (i64::from(p) - i64::from(origin)).clamp(0, extent - 1);
    ((q * 65536 + extent - 1) / extent).min(65535) as i32
}

/// A normalised client coordinate as a pixel of a display `extent` pixels wide at `origin`.
///
/// The client sends 0..=1 across its picture, so pixel `k` covers `[k/extent, (k+1)/extent)`
/// and the last one has to take 1.0 too: `round(n * extent)` would put a click half a pixel off
/// and send `n = 1.0` to `origin + extent`, the first pixel of whatever is next to it.
fn norm_to_pixel(n: f32, origin: i32, extent: u32) -> i32 {
    let extent = extent.max(1);
    // NaN passes through `clamp` and saturates to 0 in the cast
    let k = (n.clamp(0.0, 1.0) * extent as f32) as u32;
    origin + k.min(extent - 1) as i32
}

/// Whole `WHEEL_DELTA` units to send now, carrying the fraction into `rest`.
///
/// A trackpad sends pixel deltas well under one unit (0.3 px is 0.36 units), and rounding each
/// one on its own would throw every one of them away. A change of direction starts afresh, so
/// a remainder never pushes the first unit of a reversal the wrong way.
fn wheel_units(rest: &mut f32, units: f32) -> i32 {
    if units == 0.0 || !units.is_finite() {
        return 0;
    }
    if *rest != 0.0 && rest.is_sign_negative() != units.is_sign_negative() {
        *rest = 0.0;
    }
    *rest += units;
    let whole = rest.trunc();
    *rest -= whole;
    whole as i32
}

fn button_transitions(old: u16, new: u16, out: &mut Vec<INPUT>) {
    let changed = old ^ new;
    let is_down = |bit: u16| new & bit != 0;
    if changed & btn::PRIMARY != 0 {
        out.push(mouse_input(
            0,
            0,
            0,
            if is_down(btn::PRIMARY) {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
        ));
    }
    if changed & btn::SECONDARY != 0 {
        out.push(mouse_input(
            0,
            0,
            0,
            if is_down(btn::SECONDARY) {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
        ));
    }
    if changed & btn::AUXILIARY != 0 {
        out.push(mouse_input(
            0,
            0,
            0,
            if is_down(btn::AUXILIARY) {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
        ));
    }
    if changed & btn::BACK != 0 {
        out.push(mouse_input(
            0,
            0,
            XBUTTON1,
            if is_down(btn::BACK) {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
        ));
    }
    if changed & btn::FORWARD != 0 {
        out.push(mouse_input(
            0,
            0,
            XBUTTON2,
            if is_down(btn::FORWARD) {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
        ));
    }
}

#[inline]
fn ext_flag(ext: bool) -> KEYBD_EVENT_FLAGS {
    if ext {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    }
}

#[inline]
fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// The pointer id to inject a pen contact under: the client's own, as on the touch path, so
/// two sessions drawing at once never share an id. Windows treats 0 as "no pointer".
#[inline]
fn pen_pointer_id(id: u32) -> u32 {
    if id == 0 {
        1
    } else {
        id
    }
}

#[inline]
fn key_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[inline]
fn send_inputs(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        // read before anything else can overwrite the thread's last error
        let err = windows::core::Error::from_thread();
        if let Some(total) = SEND_INPUT_FAILURES.note(host_now_ns()) {
            teprintln!(
                "SendInput inserted {sent}/{} events ({err}); {total} failed call(s) so far \
                 (a locked workstation, a UAC prompt or another secure desktop refuses all of them)",
                inputs.len()
            );
        }
    }
}

#[inline]
fn inject_pointer(dev: HSYNTHETICPOINTERDEVICE, frame: &[POINTER_TYPE_INFO]) {
    if frame.is_empty() {
        return;
    }
    if let Err(e) = unsafe { InjectSyntheticPointerInput(dev, frame) } {
        if let Some(total) = POINTER_INJECT_FAILURES.note(host_now_ns()) {
            teprintln!("InjectSyntheticPointerInput failed: {e}; {total} failed call(s) so far");
        }
    }
}

/// The clipboard op that also presses Ctrl+V.
const PASTE_OP: u8 = 2;

/// Events whose effect depends on a paste that came before them having landed: keys and text
/// (Enter after a paste submits what was pasted) and button presses and releases (a click on
/// "Send"). Motion does not wait.
fn orders_after_paste(ev: &InputEvent) -> bool {
    matches!(
        ev,
        InputEvent::Key { .. }
            | InputEvent::Text { .. }
            | InputEvent::Pointer {
                phase: Phase::Down | Phase::Up | Phase::Cancel,
                ..
            }
    )
}

/// Wait until no paste is pending, or `limit` passes. `true` when none is.
fn wait_for_pastes(pending: &AtomicU32, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if pending.load(Ordering::Acquire) == 0 {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Ctrl+V, leaving a Ctrl the client is already holding exactly as it is.
///
/// Pressing and releasing LCtrl unconditionally would release a Ctrl the client still holds:
/// every key the user then types with it reaches the host unmodified.
fn push_paste_hotkey(ctrl_held: bool, out: &mut Vec<INPUT>) {
    if !ctrl_held {
        out.push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE));
    }
    out.push(key_input(SC_KEY_V, KEYEVENTF_SCANCODE));
    out.push(key_input(SC_KEY_V, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
    if !ctrl_held {
        out.push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
    }
}

/// Set the clipboard and, for a paste (`op == 2`), press Ctrl+V. Runs on the bulk worker, or
/// inline on the injector thread when the worker is switched off.
fn apply_clipboard(op: u8, mime: &str, data: &[u8], ctrl: &AtomicU8, open_attempts: u32) {
    let Some(format) = ClipFormat::for_mime(mime) else {
        // never register a client's MIME string as a format of its own name: that is a
        // session-wide atom that is never freed, for a format no Windows application reads
        if let Some(total) = CLIPBOARD_REJECTED.note(host_now_ns()) {
            teprintln!(
                "clipboard: {mime:?} is not a type this host maps to a Windows format \
                 (text/plain, text/html, image/png); not set ({total} rejected so far)"
            );
        }
        return;
    };
    if set_clipboard(format, data, open_attempts) {
        tprintln!("clipboard set ({mime}, {} bytes, op={op})", data.len());
        if op == PASTE_OP {
            let mut keys = Vec::with_capacity(4);
            push_paste_hotkey(ctrl.load(Ordering::Relaxed) != 0, &mut keys);
            send_inputs(&keys);
        }
    } else {
        // say so: the clipboard can refuse (another process holding it, a format it will not
        // take), and silence reads to the user as "paste does nothing"
        teprintln!(
            "clipboard: could not set {mime} ({} bytes); the paste will not carry it",
            data.len()
        );
    }
}

/// The clipboard formats a client MIME type is allowed to become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClipFormat {
    /// `CF_UNICODETEXT`.
    Text,
    /// The registered `"HTML Format"`, with the CF_HTML header every reader expects.
    Html,
    /// The registered `"PNG"`, which is what applications that take PNG read.
    Png,
}

impl ClipFormat {
    fn for_mime(mime: &str) -> Option<Self> {
        let base = mime.split(';').next().unwrap_or("").trim();
        if base.eq_ignore_ascii_case("text/plain") {
            Some(Self::Text)
        } else if base.eq_ignore_ascii_case("text/html") {
            Some(Self::Html)
        } else if base.eq_ignore_ascii_case("image/png") {
            Some(Self::Png)
        } else {
            None
        }
    }
}

/// A CF_HTML payload around `fragment`: the header with byte offsets into the UTF-8 that follows,
/// then the document, NUL-terminated (the offsets do not count the NUL).
fn cf_html(fragment: &str) -> Vec<u8> {
    const PRE: &str = "<html><body>\r\n<!--StartFragment-->";
    const POST: &str = "<!--EndFragment-->\r\n</body>\r\n</html>";
    fn header(start_html: usize, end_html: usize, start_frag: usize, end_frag: usize) -> String {
        format!(
            "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
             StartFragment:{start_frag:010}\r\nEndFragment:{end_frag:010}\r\n"
        )
    }
    let start_html = header(0, 0, 0, 0).len();
    let start_frag = start_html + PRE.len();
    let end_frag = start_frag + fragment.len();
    let end_html = end_frag + POST.len();
    let mut out = header(start_html, end_html, start_frag, end_frag).into_bytes();
    out.reserve(end_html + 1 - out.len());
    out.extend_from_slice(PRE.as_bytes());
    out.extend_from_slice(fragment.as_bytes());
    out.extend_from_slice(POST.as_bytes());
    out.push(0);
    out
}

fn set_clipboard(format: ClipFormat, data: &[u8], open_attempts: u32) -> bool {
    unsafe {
        let mut opened = false;
        for attempt in 0..open_attempts.max(1) {
            if attempt > 0 {
                std::thread::sleep(CLIPBOARD_RETRY);
            }
            if OpenClipboard(None).is_ok() {
                opened = true;
                break;
            }
        }
        if !opened {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = match format {
            ClipFormat::Text => set_clip_text(data),
            ClipFormat::Html => match std::str::from_utf8(data) {
                Ok(s) => set_clip_registered("HTML Format", &cf_html(s)),
                Err(_) => false,
            },
            ClipFormat::Png => set_clip_registered("PNG", data),
        };
        let _ = CloseClipboard();
        ok
    }
}

unsafe fn set_clip_text(text_utf8: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(text_utf8) else {
        return false;
    };
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0);
    unsafe { put_clipboard(CF_UNICODETEXT, bytemuck_u16(&wide)) }
}

/// Put `data` under one of the fixed, well-known registered format names above. Never a name a
/// client chose.
unsafe fn set_clip_registered(name: &'static str, data: &[u8]) -> bool {
    let mut w: Vec<u16> = name.encode_utf16().collect();
    w.push(0);
    let fmt = unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) };
    if fmt == 0 {
        return false;
    }
    unsafe { put_clipboard(fmt, data) }
}

unsafe fn put_clipboard(format: u32, bytes: &[u8]) -> bool {
    unsafe {
        let Ok(h) = GlobalAlloc(GHND, bytes.len().max(1)) else {
            return false;
        };
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        match SetClipboardData(format, Some(HANDLE(h.0))) {
            Ok(_) => true,
            Err(_) => {
                let _ = GlobalFree(Some(h));
                false
            }
        }
    }
}

fn bytemuck_u16(v: &[u16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) }
}

fn drop_root() -> std::path::PathBuf {
    std::env::temp_dir().join("input-bridge-drop")
}

/// Whether a drop directory written at `modified` is old enough to remove at `now`. A clock
/// that went backwards makes nothing stale.
fn drop_is_stale(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age >= DROP_SWEEP_AGE)
}

/// Remove drops under `root` older than [`DROP_SWEEP_AGE`]. Loose files directly in `root`
/// (older builds wrote them there) go the same way.
fn sweep_drops(root: &std::path::Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        if !drop_is_stale(modified, now) {
            continue;
        }
        let path = entry.path();
        let _ = if meta.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
    }
}

/// Write the dropped files and post `WM_DROPFILES` to the window under `pt`. Each drop gets its
/// own directory, so two drops of same-named files never overwrite one another while the first
/// is still being read. Old drops are swept here.
fn post_drop(pt: POINT, items: &[DropItem]) {
    static DROP_SEQ: AtomicU64 = AtomicU64::new(0);
    let root = drop_root();
    let now = SystemTime::now();
    sweep_drops(&root, now);
    let stamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = root.join(format!(
        "{stamp}-{}-{}",
        std::process::id(),
        DROP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        teprintln!("drop: cannot create temp dir {dir:?}: {e}");
        return;
    }
    let mut wide: Vec<u16> = Vec::new();
    let mut count = 0usize;
    for (i, it) in items.iter().enumerate() {
        let fname = std::path::Path::new(&it.name)
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from(format!("drop-{i}.bin")));
        let path = dir.join(&fname);
        if let Err(e) = std::fs::write(&path, &it.data) {
            teprintln!("drop: write {path:?} failed: {e}");
            continue;
        }
        wide.extend(path.as_os_str().encode_wide());
        wide.push(0);
        count += 1;
    }
    if count == 0 {
        return;
    }
    wide.push(0);

    unsafe {
        let header = size_of::<DROPFILES>();
        let bytes = header + wide.len() * 2;
        let h = match GlobalAlloc(GHND, bytes) {
            Ok(h) => h,
            Err(e) => {
                teprintln!("drop: GlobalAlloc failed: {e}");
                return;
            }
        };
        let base = GlobalLock(h) as *mut u8;
        if base.is_null() {
            let _ = GlobalFree(Some(h));
            return;
        }
        let mut pt = pt;
        let hwnd: HWND = WindowFromPoint(pt);
        let _ = ScreenToClient(hwnd, &mut pt);
        let df = DROPFILES {
            pFiles: header as u32,
            pt,
            fNC: windows::core::BOOL(0),
            fWide: windows::core::BOOL(1),
        };
        std::ptr::write_unaligned(base as *mut DROPFILES, df);
        std::ptr::copy_nonoverlapping(wide.as_ptr(), base.add(header) as *mut u16, wide.len());
        let _ = GlobalUnlock(h);

        match PostMessageW(Some(hwnd), WM_DROPFILES, WPARAM(h.0 as usize), LPARAM(0)) {
            Ok(_) => tprintln!("posted WM_DROPFILES ({count} file(s)) to target window"),
            Err(e) => {
                teprintln!("drop: PostMessage WM_DROPFILES failed: {e}");
                let _ = GlobalFree(Some(h));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn batch(n: usize) -> Vec<MoveSample> {
        (0..n)
            .map(|i| MoveSample {
                x: 0.40 + i as f32 * 0.0005,
                y: 0.40 + i as f32 * 0.0005,
                pressure: 0.0,
                tilt_x: 0.0,
                tilt_y: 0.0,
                twist: 0.0,
            })
            .collect()
    }

    /// Injects for real, so it moves the host cursor: run it deliberately, not in a suite.
    /// `cargo test -- --ignored --nocapture pointer_batch_costs`
    #[test]
    #[ignore = "injects real input and moves the host cursor; run with --ignored --nocapture"]
    fn pointer_batch_costs_one_sendinput_not_one_per_sample() {
        const SAMPLES: usize = 40;
        const REPS: usize = 500;
        let samples = batch(SAMPLES);
        let mut inj = Injector::new(ClientProfile::default());

        // the unbatched shape: one mouse_pointer, so one SendInput, per sample
        let t = Instant::now();
        for _ in 0..REPS {
            for s in &samples {
                inj.mouse_pointer(s.x, s.y, 0, Phase::Move);
            }
        }
        let per_sample = t.elapsed();

        let t = Instant::now();
        for _ in 0..REPS {
            inj.pointer_batch(SRC_MOUSE, 0, 0, &samples, 0);
        }
        let batched = t.elapsed();

        println!(
            "pointer batch of {SAMPLES}: per-sample {:.1} us/batch ({} SendInput),              batched {:.1} us/batch (1 SendInput)",
            per_sample.as_secs_f64() * 1e6 / REPS as f64,
            SAMPLES,
            batched.as_secs_f64() * 1e6 / REPS as f64,
        );
        assert!(batched < per_sample);
    }

    /// The pointer-lock clamp, without injecting anything. A locked client accumulates
    /// `MOUSE_DELTA` into a position we track; the scope is what that position may not leave:
    /// `Display` is the session's own monitor, `All` the virtual desktop.
    #[test]
    fn a_locked_pointer_is_clamped_to_the_scope_it_was_given() {
        let target = DisplayRect {
            left: 1920,
            top: 0,
            width: 1280,
            height: 720,
        };

        let mut inj = Injector::new(ClientProfile::default());
        inj.target = Some(target);

        inj.live.set_pointer_scope(PointerScope::Display);
        assert_eq!(inj.pointer_bounds(), target);

        inj.live.set_pointer_scope(PointerScope::All);
        let desktop = inj.pointer_bounds();
        assert_eq!(desktop.left, inj.virt.left);
        assert_eq!(desktop.width, inj.virt.width as u32);
        assert_ne!(
            desktop, target,
            "the desktop must be wider than one monitor"
        );

        // no display to confine to (a harness with no device name): the desktop is the answer
        inj.target = None;
        inj.live.set_pointer_scope(PointerScope::Display);
        assert_eq!(inj.pointer_bounds(), desktop);
    }

    /// Exactly one pointer curve, and which side applies it follows the client's flag.
    /// `PointerScope::Display` overrides the flag: clamping to one monitor needs a position.
    #[test]
    fn raw_counts_get_the_os_curve_and_already_curved_counts_do_not() {
        let inj = Injector::new(ClientProfile::default());

        inj.live.set_pointer_scope(PointerScope::All);
        assert!(
            inj.lets_os_curve(true),
            "raw counts under the unrestricted scope are Windows' to accelerate"
        );
        assert!(
            !inj.lets_os_curve(false),
            "counts the client's OS already curved must not be curved again"
        );

        inj.live.set_pointer_scope(PointerScope::Display);
        assert!(
            !inj.lets_os_curve(true),
            "confining the cursor to one display needs a tracked position to clamp"
        );
        assert!(!inj.lets_os_curve(false));
    }

    /// Only the explicit restriction restricts. A device saved before the setting existed
    /// carries no `pointerScope` at all, and reading that as `Display` would silently fence in
    /// every device the user already has.
    #[test]
    fn only_an_explicit_display_scope_restricts_the_pointer() {
        assert_eq!(PointerScope::from_str("display"), PointerScope::Display);
        assert_eq!(PointerScope::from_str("Display"), PointerScope::Display);
        assert_eq!(PointerScope::from_str("all"), PointerScope::All);
        assert_eq!(PointerScope::from_str("everything"), PointerScope::All);
        assert_eq!(PointerScope::from_str(""), PointerScope::All);
        assert_eq!(PointerScope::default(), PointerScope::All);
        assert_eq!(PointerScope::All.as_str(), "all");
        assert_eq!(PointerScope::Display.as_str(), "display");
    }

    /// Display widths and heights real sessions use, plus the degenerate ones.
    const EXTENTS: &[i32] = &[
        1, 2, 3, 720, 768, 1080, 1280, 1366, 1440, 1600, 1920, 2160, 2560, 2880, 3000, 3200, 3440,
        3840, 4480, 5120, 5760, 6400, 7680, 10240, 15360,
    ];

    /// Every pixel of every extent survives the round trip Windows applies, at any desktop
    /// origin, including one left of or above the primary monitor.
    #[test]
    fn every_pixel_normalises_to_a_coordinate_that_lands_on_it() {
        for &extent in EXTENTS {
            for origin in [0, -1920, 1920, -2160] {
                for q in 0..extent {
                    let dx = to_absolute(origin + q, origin, extent);
                    assert!((0..=65535).contains(&dx), "{dx} out of range");
                    // Windows: truncating `dx * extent >> 16`
                    let back = (i64::from(dx) * i64::from(extent)) >> 16;
                    assert_eq!(
                        back,
                        i64::from(q),
                        "extent {extent} origin {origin}: pixel {q} -> {dx} -> {back}"
                    );
                    // a rounding mapping would land on it too
                    let rounded = (i64::from(dx) * i64::from(extent) + 32768) >> 16;
                    assert_eq!(rounded, i64::from(q), "extent {extent}: rounding model");
                }
            }
        }
    }

    /// A worked example against the naive `round(p / extent * 65535)`.
    #[test]
    fn the_old_normalisation_landed_one_pixel_short() {
        let old = |p: i32, w: i32| ((p as f32 / w as f32).clamp(0.0, 1.0) * 65535.0).round() as i64;
        assert_eq!((old(3000, 4480) * 4480) >> 16, 2999, "the defect");
        assert_eq!((i64::from(to_absolute(3000, 0, 4480)) * 4480) >> 16, 3000);
        // off the desktop clamps to its edge pixels, never past them
        assert_eq!(to_absolute(-50, 0, 1920), 0);
        assert_eq!((i64::from(to_absolute(5000, 0, 1920)) * 1920) >> 16, 1919);
    }

    /// A normalised coordinate stays on its display, and 1.0 is the last pixel, not the first
    /// pixel of the next monitor.
    #[test]
    fn a_normalised_coordinate_never_leaves_its_display() {
        for &extent in EXTENTS {
            let w = extent as u32;
            for origin in [0, 1920, -1280] {
                assert_eq!(norm_to_pixel(0.0, origin, w), origin);
                assert_eq!(norm_to_pixel(1.0, origin, w), origin + extent - 1);
                assert_eq!(norm_to_pixel(-0.5, origin, w), origin);
                assert_eq!(norm_to_pixel(7.0, origin, w), origin + extent - 1);
                assert_eq!(norm_to_pixel(f32::NAN, origin, w), origin);
                // the centre of pixel k is pixel k
                for k in [0, extent / 3, extent / 2, extent - 1] {
                    let n = (k as f32 + 0.5) / extent as f32;
                    assert_eq!(
                        norm_to_pixel(n, origin, w),
                        origin + k,
                        "extent {extent} k {k}"
                    );
                }
            }
        }
    }

    /// A frame for a contact that is not down never creates one: Down(5), Up(5), Move(5) must
    /// not leave contact 5 behind as `UPDATE|INCONTACT` in every later frame.
    #[test]
    fn a_late_touch_move_after_its_up_leaves_no_phantom_contact() {
        let c = Contact {
            x: 0.5,
            y: 0.5,
            pressure: 0.5,
            w: 0.01,
            h: 0.01,
        };
        let mut contacts = HashMap::new();
        let run = |id: u32, phase: Phase, contacts: &mut HashMap<u32, Contact>| {
            let step = track_contact(contacts, id, c, phase);
            finish_contact(contacts, id, step);
            step
        };
        assert_eq!(run(5, Phase::Down, &mut contacts), ContactStep::Begin);
        assert_eq!(run(5, Phase::Move, &mut contacts), ContactStep::Update);
        assert_eq!(
            run(5, Phase::Up, &mut contacts),
            ContactStep::End { canceled: false }
        );
        assert_eq!(run(5, Phase::Move, &mut contacts), ContactStep::Ignore);
        assert!(
            contacts.is_empty(),
            "a late move must not resurrect the contact"
        );

        // a Move, Up or Cancel for a contact the device does not hold is a late frame: ignored
        // and counted
        for phase in [Phase::Move, Phase::Up, Phase::Cancel] {
            assert_eq!(run(7, phase, &mut contacts), ContactStep::Ignore);
        }
        // Over/Enter before every tap's Down and Out/Leave after its Up are ordinary traffic:
        // skipped without being counted as late, and never a contact
        for phase in [Phase::Over, Phase::Enter, Phase::Out, Phase::Leave] {
            assert_eq!(run(7, phase, &mut contacts), ContactStep::Skip);
        }
        assert!(contacts.is_empty());

        // a second Down for a contact already down is an update, never a second DOWN
        assert_eq!(run(8, Phase::Down, &mut contacts), ContactStep::Begin);
        assert_eq!(run(8, Phase::Down, &mut contacts), ContactStep::Update);
        assert_eq!(run(8, Phase::Over, &mut contacts), ContactStep::Update);
        assert_eq!(
            run(8, Phase::Cancel, &mut contacts),
            ContactStep::End { canceled: true }
        );
        assert!(contacts.is_empty());
    }

    #[test]
    fn a_late_restatement_cannot_undo_a_press_or_a_release() {
        let just = Some(Duration::from_millis(2));
        let long_ago = Some(Duration::from_secs(5));
        let p = btn::PRIMARY;
        let s = btn::SECONDARY;

        // end of a drag: Up injected, then the stroke's last batch (still PRIMARY) arrives
        assert_eq!(restated_buttons(0, p, just), 0);
        // start of a drag: Down injected, then the hover move sent before it arrives
        assert_eq!(restated_buttons(p, 0, just), p);
        // a chord is a move by definition, and it is honoured
        assert_eq!(restated_buttons(p, p | s, just), p | s);
        assert_eq!(restated_buttons(p | s, p, just), p);
        // nothing changes: nothing to guard
        assert_eq!(restated_buttons(p, p, just), p);
        // long after the last Down/Up the word is honoured, which is what recovers a release
        // the page never saw
        assert_eq!(restated_buttons(p, 0, long_ago), 0);
        assert_eq!(restated_buttons(0, p, long_ago), p);
        // no Down/Up yet this session: honoured
        assert_eq!(restated_buttons(0, p, None), p);
        // the eraser bit is not a mouse button and does not count as one held
        assert_eq!(restated_buttons(0, btn::ERASER, just), btn::ERASER);
    }

    #[test]
    fn a_repeating_failure_is_counted_every_time_and_logged_once_a_second() {
        let gate = LogGate::new();
        let t0 = 5_000_000_000u64;
        assert_eq!(gate.note(t0), Some(1), "the first failure is always logged");
        for i in 1..1000u64 {
            assert_eq!(gate.note(t0 + i * 500_000), None, "within the second");
        }
        assert_eq!(gate.note(t0 + FAILURE_LOG_INTERVAL_NS), Some(1001));
        assert_eq!(gate.note(t0 + FAILURE_LOG_INTERVAL_NS + 1), None);
        assert_eq!(gate.count.load(Ordering::Relaxed), 1002);
    }

    /// Only three client types become clipboard formats, and none by the client's own string.
    #[test]
    fn only_allowlisted_mime_types_reach_the_clipboard() {
        assert_eq!(ClipFormat::for_mime("text/plain"), Some(ClipFormat::Text));
        assert_eq!(
            ClipFormat::for_mime("text/plain;charset=utf-8"),
            Some(ClipFormat::Text)
        );
        assert_eq!(ClipFormat::for_mime("TEXT/HTML"), Some(ClipFormat::Html));
        assert_eq!(ClipFormat::for_mime(" image/png "), Some(ClipFormat::Png));
        for hostile in [
            "",
            "image/jpeg",
            "application/x-unique-0001",
            "text/plainx",
            "HTML Format",
            "PNG",
        ] {
            assert_eq!(ClipFormat::for_mime(hostile), None, "{hostile:?}");
        }
    }

    #[test]
    fn the_html_clipboard_header_offsets_are_exact() {
        let fragment = "<b>héllo</b> wörld";
        let out = cf_html(fragment);
        assert_eq!(out.last(), Some(&0), "NUL-terminated");
        let text = std::str::from_utf8(&out[..out.len() - 1]).unwrap();
        let field = |name: &str| -> usize {
            let at = text.find(name).unwrap() + name.len();
            text[at..at + 10].parse().unwrap()
        };
        let (sh, eh) = (field("StartHTML:"), field("EndHTML:"));
        let (sf, ef) = (field("StartFragment:"), field("EndFragment:"));
        assert!(text[sh..].starts_with("<html>"));
        assert_eq!(eh, out.len() - 1);
        assert_eq!(&out[sf..ef], fragment.as_bytes());
        assert!(text[..sf].ends_with("<!--StartFragment-->"));
        assert!(text[ef..].starts_with("<!--EndFragment-->"));
    }

    #[test]
    fn the_paste_hotkey_does_not_release_a_held_ctrl() {
        let scans = |held: bool| -> Vec<(u16, bool)> {
            let mut v = Vec::new();
            push_paste_hotkey(held, &mut v);
            v.iter()
                .map(|i| unsafe {
                    (
                        i.Anonymous.ki.wScan,
                        i.Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP),
                    )
                })
                .collect()
        };
        assert_eq!(
            scans(false),
            vec![
                (SC_LCTRL, false),
                (SC_KEY_V, false),
                (SC_KEY_V, true),
                (SC_LCTRL, true)
            ]
        );
        assert_eq!(scans(true), vec![(SC_KEY_V, false), (SC_KEY_V, true)]);
    }

    #[test]
    fn sub_unit_wheel_motion_is_carried_not_lost() {
        let scale = WHEEL_DELTA / 100.0;
        let mut rest = 0.0f32;
        // 0.3 px is 0.36 units: rounded on its own, every one of these is 0
        let total: i32 = (0..100).map(|_| wheel_units(&mut rest, 0.3 * scale)).sum();
        assert!((35..=36).contains(&total), "{total}");
        // a reversal starts from nothing rather than from the leftover
        let mut rest = 0.9f32;
        assert_eq!(wheel_units(&mut rest, -0.5), 0);
        assert!((rest + 0.5).abs() < 1e-6);
        // whole units pass straight through
        let mut rest = 0.0f32;
        assert_eq!(wheel_units(&mut rest, -360.0), -360);
        assert_eq!(wheel_units(&mut rest, f32::NAN), 0);
    }

    #[test]
    fn old_drops_are_swept_and_new_ones_kept() {
        let now = SystemTime::now();
        assert!(!drop_is_stale(now, now));
        assert!(drop_is_stale(now - DROP_SWEEP_AGE, now));
        assert!(
            !drop_is_stale(now + Duration::from_secs(60), now),
            "clock skew"
        );

        let root = std::env::temp_dir().join(format!(
            "se-drop-sweep-test-{}-{}",
            std::process::id(),
            now.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let sub = root.join("123-1-0");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("a.txt"), b"x").unwrap();
        std::fs::write(root.join("legacy.bin"), b"y").unwrap();
        sweep_drops(&root, now);
        assert!(sub.exists(), "a fresh drop is still being read");
        sweep_drops(&root, now + DROP_SWEEP_AGE + Duration::from_secs(1));
        assert!(!sub.exists());
        assert!(!root.join("legacy.bin").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A session whose display stops resolving withholds positional input instead of mapping
    /// it across the whole desktop, and says so once.
    #[test]
    fn an_unresolved_display_is_never_widened_to_the_desktop() {
        let mut inj = Injector::new(ClientProfile::default());
        inj.device_name = Some(r"\\.\DISPLAY99".to_string());
        let virt = inj.virt;
        let rect = DisplayRect {
            left: virt.left + 10,
            top: virt.top,
            width: 100,
            height: 100,
        };
        inj.apply_geometry(GeometrySnapshot {
            virt,
            target: Some(rect),
        });
        assert!(!inj.lost);
        inj.apply_geometry(GeometrySnapshot { virt, target: None });
        assert!(inj.lost, "an unresolved name is lost, not the desktop");
        assert_eq!(
            inj.target,
            Some(rect),
            "the last good rect is kept for releases"
        );
        // a press while lost is withheld; a release still goes through
        inj.buttons = 0;
        assert_eq!(inj.next_buttons(btn::PRIMARY, true, true), 0);
        inj.buttons = btn::PRIMARY;
        assert_eq!(inj.next_buttons(0, true, true), 0);
        inj.apply_geometry(GeometrySnapshot {
            virt,
            target: Some(rect),
        });
        assert!(!inj.lost, "and it recovers when the display resolves again");

        // with no device name there is no display to lose: the desktop is the target
        inj.device_name = None;
        inj.target = None;
        inj.lost = false;
        inj.apply_geometry(GeometrySnapshot { virt, target: None });
        assert!(!inj.lost);
    }

    /// The worker publishes a changed geometry, the injector picks it up without blocking, and
    /// an unchanged version costs nothing.
    #[test]
    fn the_injector_picks_up_a_geometry_the_worker_published() {
        let mut inj = Injector::new(ClientProfile::default());
        let virt = inj.virt;
        let moved = VirtualScreen {
            left: virt.left - 7,
            ..virt
        };
        let shared = Arc::new(SharedGeometry {
            version: AtomicU64::new(0),
            snap: Mutex::new(GeometrySnapshot { virt, target: None }),
        });
        let (poke, _poked) = bounded::<()>(1);
        inj.geometry = GeometrySource::Worker {
            shared: Arc::clone(&shared),
            seen: 0,
            poke,
        };
        inj.refresh_geometry();
        assert_eq!(inj.virt, virt, "no new version, nothing read");
        *shared.snap.lock().unwrap() = GeometrySnapshot {
            virt: moved,
            target: None,
        };
        shared.version.fetch_add(1, Ordering::Release);
        // held by the worker: the event goes ahead on the old geometry
        {
            let _held = shared.snap.lock().unwrap();
            inj.refresh_geometry();
            assert_eq!(inj.virt, virt);
        }
        inj.refresh_geometry();
        assert_eq!(inj.virt, moved);
    }
    fn presses_left(inputs: &[INPUT]) -> bool {
        inputs.iter().any(|i| {
            i.r#type == INPUT_MOUSE
                && unsafe { i.Anonymous.mi.dwFlags }.contains(MOUSEEVENTF_LEFTDOWN)
        })
    }

    /// The restate guard through the injector, not only the pure rule: the mouse paths route
    /// the word through `next_buttons` with the right `authoritative` flag. Nothing is sent: the
    /// `push_*` bodies only fill `scratch`.
    #[test]
    fn the_mouse_paths_keep_a_release_against_a_late_restatement() {
        let mut inj = Injector::new(ClientProfile::default());
        inj.restate_guard = true;

        inj.scratch.clear();
        inj.push_mouse_pointer(0.5, 0.5, btn::PRIMARY, Phase::Down);
        assert_eq!(inj.buttons, btn::PRIMARY);
        assert!(presses_left(&inj.scratch));
        inj.scratch.clear();
        inj.push_mouse_pointer(0.5, 0.5, 0, Phase::Up);
        assert_eq!(inj.buttons, 0);
        inj.scratch.clear();
        // the stroke's last fast-channel move, still saying PRIMARY, lands after the Up
        inj.push_mouse_pointer(0.5, 0.5, btn::PRIMARY, Phase::Move);
        assert_eq!(inj.buttons, 0, "a late move re-pressed after the Up");
        assert!(!presses_left(&inj.scratch));
        inj.scratch.clear();

        // the same word on a pointer-locked delta (no motion, so nothing reads the cursor)
        inj.push_mouse_delta(0, 0, btn::PRIMARY, false);
        assert_eq!(inj.buttons, 0, "a late MOUSE_DELTA re-pressed after the Up");
        assert!(!presses_left(&inj.scratch));
        inj.scratch.clear();
        // Down mid-drag, then a hover move sent before it: the press is kept
        inj.push_mouse_pointer(0.5, 0.5, btn::PRIMARY, Phase::Down);
        inj.scratch.clear();
        inj.push_mouse_delta(0, 0, 0, false);
        assert_eq!(
            inj.buttons,
            btn::PRIMARY,
            "a late MOUSE_DELTA released mid-drag"
        );
        // leave nothing for Drop/teardown to release for real
        inj.buttons = 0;
        inj.scratch.clear();
    }

    /// A blur or a hidden page releases everything, and that release is as authoritative as an
    /// Up. With nothing held, the release sends nothing.
    #[test]
    fn a_focus_loss_release_is_guarded_like_an_up() {
        let mut inj = Injector::new(ClientProfile::default());
        inj.restate_guard = true;
        assert_eq!(inj.buttons, 0);
        inj.lifecycle(Lifecycle::Focus(false));
        assert!(inj.last_authoritative.is_some());
        assert_eq!(
            inj.next_buttons(btn::PRIMARY, false, true),
            0,
            "a fast frame sent before the blur pressed again after the release"
        );
        inj.lifecycle(Lifecycle::Visibility(false));
        assert_eq!(inj.next_buttons(btn::PRIMARY, false, true), 0);
    }

    /// Through the injector: a tap's hover phases are not counted as late frames, a real late
    /// Move is. Neither injects (both return before any frame is built).
    #[test]
    fn only_real_late_touch_frames_are_counted() {
        let mut inj = Injector::new(ClientProfile::default());
        if inj.touch_dev.is_none() {
            crate::test_support::skip_because(
                "only_real_late_touch_frames_are_counted",
                "no synthetic touch device on this machine",
            );
            return;
        }
        for phase in [Phase::Over, Phase::Enter, Phase::Out, Phase::Leave] {
            inj.touch(9, 0.5, 0.5, 0.0, 0.01, 0.01, phase);
        }
        assert_eq!(inj.stray_touch_frames, 0);
        inj.touch(9, 0.5, 0.5, 0.0, 0.01, 0.01, Phase::Move);
        assert_eq!(inj.stray_touch_frames, 1);
        assert!(inj.contacts.is_empty());
    }

    /// A name that addresses somebody else's monitor while this host owns displays is refused,
    /// not followed by name.
    #[test]
    fn a_stale_slot_is_refused_not_resolved_by_name() {
        assert_eq!(ident_verdict(Some(true), true), IdentVerdict::Owned);
        assert_eq!(ident_verdict(Some(false), true), IdentVerdict::Refused);
        // no ownership information at all: a harness naming a display
        assert_eq!(ident_verdict(Some(false), false), IdentVerdict::NameOnly);
        assert_eq!(ident_verdict(None, true), IdentVerdict::NameOnly);
        assert_eq!(ident_verdict(None, false), IdentVerdict::NameOnly);

        let mut refused = TargetIdent {
            name: Some(String::from("DISPLAY-THAT-IS-NOT-OURS")),
            owned: None,
            refused: true,
        };
        assert_eq!(refused.query().target, None);
    }

    /// Given the path the host recorded, a name that now addresses another monitor (here the
    /// machine's own) is never resolved by name instead.
    #[test]
    fn a_session_given_its_display_path_never_falls_back_to_the_name() {
        let paths = crate::windows_utils::virtual_display::active_display_paths();
        let Some(someone_else) = paths.first() else {
            crate::test_support::skip_because(
                "a_session_given_its_display_path_never_falls_back_to_the_name",
                "no active display to borrow a name from",
            );
            return;
        };
        let mut ident = TargetIdent::by_path(
            Some(someone_else.gdi_name.clone()),
            String::from(r"\\?\DISPLAY#NOT-CREATED-BY-THIS-HOST#0"),
        );
        assert_eq!(ident.query().target, None);
    }

    /// With the bulk worker, a key after a paste waits for its Ctrl+V; motion does not; and the
    /// wait is bounded.
    #[test]
    fn a_key_after_a_paste_waits_for_its_ctrl_v() {
        let key = InputEvent::Key {
            down: true,
            repeat: false,
            mods: 0,
            code: "Enter".into(),
            key: "Enter".into(),
        };
        assert!(orders_after_paste(&key));
        assert!(!orders_after_paste(&InputEvent::MouseDelta {
            dx: 1,
            dy: 0,
            buttons: 0,
            unadjusted: false,
        }));

        let pending = Arc::new(AtomicU32::new(1));
        let worker = Arc::clone(&pending);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker.fetch_sub(1, Ordering::Release);
        });
        let t0 = Instant::now();
        assert!(wait_for_pastes(&pending, Duration::from_secs(5)));
        assert!(t0.elapsed() >= Duration::from_millis(15));
        t.join().unwrap();

        pending.store(1, Ordering::Release);
        let t0 = Instant::now();
        assert!(!wait_for_pastes(&pending, Duration::from_millis(30)));
        assert!(t0.elapsed() < Duration::from_secs(2));
    }

    /// A session's injector owns two synthetic pointer devices; thirty sessions must hand all
    /// of them back. Creating and destroying a device injects nothing. Counted in a process of
    /// its own (this test binary running [`injector_teardown_in_isolation`] alone), because
    /// other tests in this one create injectors while it counts.
    #[test]
    fn thirty_injectors_leave_no_pointer_devices_behind() {
        let module = module_path!()
            .split_once("::")
            .map_or(module_path!(), |(_, m)| m);
        let name = format!("{module}::injector_teardown_in_isolation");
        let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                name.as_str(),
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .expect("run the injector teardown test in its own process");
        let stderr = String::from_utf8_lossy(&out.stderr);
        for line in stderr
            .lines()
            .filter(|l| l.starts_with("SKIP") || l.contains("injector teardown"))
        {
            eprintln!("{line}");
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the isolated injector teardown run failed:\n{stdout}\n{stderr}"
        );
    }

    #[test]
    #[ignore = "run in its own process by thirty_injectors_leave_no_pointer_devices_behind"]
    fn injector_teardown_in_isolation() {
        use crate::windows_utils::process_handles;
        let session = || {
            std::thread::spawn(|| {
                let inj = Injector::new(ClientProfile::default());
                let had = (inj.pen_dev.is_some(), inj.touch_dev.is_some());
                drop(inj);
                had
            })
            .join()
            .unwrap()
        };
        let first = session();
        if first == (false, false) {
            crate::test_support::skip_because(
                "injector teardown",
                "this session cannot create synthetic pointer devices",
            );
            return;
        }
        let Some(before) = process_handles::snapshot() else {
            crate::test_support::skip_because("injector teardown", "handle table unreadable");
            return;
        };
        // every session must still get its devices: a release that stopped freeing them would
        // show here as a creation refused at the per-process cap
        for i in 0..30 {
            assert_eq!(
                session(),
                first,
                "session {i} could not create its pointer devices"
            );
        }
        // run alone nothing else creates devices; run beside other tests, one of theirs can be
        // alive at any one sample while a leaked device stays in every one. The lowest of up to 20
        // samples, 100 ms apart
        let mut raw_input = i64::MAX;
        for _ in 0..20 {
            let after = process_handles::snapshot().expect("handle table");
            let grown = process_handles::growth(
                &process_handles::by_type(&before),
                &process_handles::by_type(&after),
            );
            raw_input = raw_input.min(grown.get("RawInputManager").copied().unwrap_or(0));
            if raw_input <= 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        eprintln!("injector teardown, 30 sessions: {raw_input} RawInputManager handles grown");
        assert!(
            raw_input <= 0,
            "{raw_input} RawInputManager handles grew over 30 injectors"
        );
    }

    /// The release closes a handle only while it compares equal to a duplicate of the same device,
    /// which is safe only if each device is an object of its own: were they handles to one shared
    /// object, another session's device holding a recycled value would compare equal too.
    #[test]
    fn each_pointer_device_is_an_object_of_its_own() {
        let created = (
            PointerDevice::create(PT_PEN, 1),
            PointerDevice::create(PT_TOUCH, 1),
            PointerDevice::create(PT_PEN, 1),
        );
        let (Ok(pen_a), Ok(touch_a), Ok(pen_b)) = created else {
            crate::test_support::skip_because(
                "pointer device identity",
                "this session cannot create synthetic pointer devices",
            );
            return;
        };
        let same = |a: &PointerDevice, b: &PointerDevice| {
            // SAFETY: both handles are live devices this test owns.
            unsafe { CompareObjectHandles(HANDLE(a.raw().0), HANDLE(b.raw().0)) }.as_bool()
        };
        assert!(same(&pen_a, &pen_a), "a device compares equal to itself");
        assert!(
            !same(&pen_a, &touch_a),
            "a pen and a touch device are one object"
        );
        assert!(!same(&pen_a, &pen_b), "two pens are one object");
    }
}
