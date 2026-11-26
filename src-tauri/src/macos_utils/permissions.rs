//! macOS permission reads (Screen Recording, Accessibility) and the prompt policy.
//!
//! Every read is tri-state ([`Silent`]) and `Unknown` is never a denial: only
//! `DefinitelyDenied` may refuse a join, kick a session or reset a TCC row.
//!
//! Periodic and automatic paths use silent reads only; a prompting call needs a
//! [`PromptPermit`]. Prompting: `CGRequestScreenCaptureAccess`, `AXIsProcessTrustedWithOptions`
//! with the prompt option, `SCShareableContent` while unauthorized (macOS 15+ raises the alert
//! on every call), and starting an `SCStream` or `CGDisplayStream` while unauthorized.
//!
//! `CGPreflightScreenCaptureAccess` is cached per process, so it counts as a positive only.
//! Whether `AXIsProcessTrusted` sees a grant made while running is unverified, so its negative
//! is definite only when the AX messaging probe agrees ([`silent_accessibility`]).
//! `CGDisplayStream` frame delivery is no authorization signal: an unauthorized process still
//! gets `FrameComplete` frames, they just contain wallpaper.

use std::collections::BTreeMap;
use std::ffi::{c_void, CString};
use std::path::PathBuf;
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFArray, CFBoolean,
    CFDictionary, CFNumber, CFString,
};

use crate::PermissionStatus;

type CFDictionaryRef = *const c_void;
type AXUIElementRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: *const c_void,
        value: *mut *const c_void,
    ) -> i32;
    fn AXUIElementSetMessagingTimeout(element: AXUIElementRef, timeout_seconds: f32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFStringGetLength(s: *const c_void) -> isize;
}

// `CGWindowListCopyWindowInfo` is 10.5+, so linking it is safe on the 10.15 floor
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *const CFArray;
}

// signatures must match the other declarations in this crate (`macos_utils/streamer/sck.rs`)
// or the linker sees a clash
unsafe extern "C-unwind" {
    fn CFDictionaryGetValue(the_dict: *const CFDictionary, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const CFNumber, the_type: isize, value_ptr: *mut c_void) -> u8;
}

const AX_ERROR_API_DISABLED: i32 = -25211;
const AX_ERROR_CANNOT_COMPLETE: i32 = -25204;
const AX_MESSAGING_TIMEOUT_SECS: f32 = 0.5;

const K_CF_NUMBER_S_INT32_TYPE: isize = 3;
const CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1 << 0;
const CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 1 << 4;
const CG_NULL_WINDOW_ID: u32 = 0;

/// How long after a prompt before concluding the grant is one macOS only hands to a fresh
/// process.
const RELAUNCH_GRACE: Duration = Duration::from_secs(3);
/// Background refresh interval for the probes too slow to run inline.
const PROBE_REFRESH: Duration = Duration::from_secs(2);

pub const STATE_GRANTED: &str = "granted";
pub const STATE_DENIED: &str = "denied";
pub const STATE_UNKNOWN: &str = "unknown";

pub const KEY_ACCESSIBILITY: &str = "accessibility";
pub const KEY_SCREEN_RECORDING: &str = "screen_recording";

// background-refreshed probe slot: `get` never blocks and never invents an answer, and a probe
// that panics loses its own result instead of wedging the slot

struct BgState<T> {
    running: bool,
    last_at: Option<Instant>,
    last: Option<T>,
}

struct BgProbe<T: 'static> {
    state: Mutex<BgState<T>>,
}

impl<T: Copy + Send + 'static> BgProbe<T> {
    const fn new() -> Self {
        Self {
            state: Mutex::new(BgState {
                running: false,
                last_at: None,
                last: None,
            }),
        }
    }

    /// Last known value, refreshing in the background when one is due.
    fn get(&'static self, label: &'static str, probe: fn() -> Option<T>) -> Option<T> {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let due = match st.last_at {
            Some(at) => at.elapsed() >= PROBE_REFRESH,
            None => true,
        };
        if !due || st.running {
            return st.last;
        }
        st.running = true;
        let last = st.last;
        drop(st);
        std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(probe).unwrap_or_else(|_| {
                teprintln!("[permissions] the {label} probe panicked; keeping the last answer");
                None
            });
            let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
            st.running = false;
            st.last_at = Some(Instant::now());
            if outcome.is_some() {
                st.last = outcome;
            }
        });
        last
    }
}

/// What the system-wide accessibility element had to say. A secondary signal: per Apple DTS a
/// failed AX call is not evidence of a missing grant, so a messaging failure downgrades the
/// answer to unknown rather than asserting a denial.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AxSecondary {
    Enabled,
    ApiDisabled,
    Unreadable,
}

impl AxSecondary {
    fn as_str(self) -> &'static str {
        match self {
            AxSecondary::Enabled => "enabled",
            AxSecondary::ApiDisabled => "api-disabled",
            AxSecondary::Unreadable => "unreadable",
        }
    }
}

static AX_SECONDARY: BgProbe<AxSecondary> = BgProbe::new();
static AX_REQUESTED_AT: Mutex<Option<Instant>> = Mutex::new(None);
static SCREEN_REQUESTED_AT: Mutex<Option<Instant>> = Mutex::new(None);

/// `AXUIElementSetMessagingTimeout` on the system-wide element sets the default for every
/// accessibility message this process sends, so keep this off the hot path. It is put back
/// to 0 (documented as "use the system default"; there is no getter) before returning, or
/// remote input and the AirPlay menu-extra walk would inherit the 0.5 s ceiling.
fn accessibility_api_probe() -> Option<AxSecondary> {
    unsafe {
        let el = AXUIElementCreateSystemWide();
        if el.is_null() {
            return Some(AxSecondary::Unreadable);
        }
        AXUIElementSetMessagingTimeout(el, AX_MESSAGING_TIMEOUT_SECS);
        let attr = CFString::from_str("AXFocusedApplication");
        let mut value: *const c_void = ptr::null();
        let err = AXUIElementCopyAttributeValue(el, (&*attr as *const CFString).cast(), &mut value);
        if !value.is_null() {
            CFRelease(value);
        }
        AXUIElementSetMessagingTimeout(el, 0.0);
        CFRelease(el);
        Some(match err {
            AX_ERROR_API_DISABLED => AxSecondary::ApiDisabled,
            AX_ERROR_CANNOT_COMPLETE => AxSecondary::Unreadable,
            _ => AxSecondary::Enabled,
        })
    }
}

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

/// PROMPTING. Reachable only with a [`PromptPermit`], i.e. through [`prompt_gate`].
fn prompt_accessibility(_permit: PromptPermit) -> bool {
    *AX_REQUESTED_AT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    let key = CFString::from_str("AXTrustedCheckOptionPrompt");
    let val = CFBoolean::new(true);
    let keys: [*const c_void; 1] = [(&*key as *const CFString).cast()];
    let values: [*const c_void; 1] = [(val as *const CFBoolean).cast()];
    let dict = unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    };
    match dict {
        Some(d) => unsafe { AXIsProcessTrustedWithOptions((&*d as *const CFDictionary).cast()) },
        None => accessibility_trusted(),
    }
}

fn requested_long_enough(slot: &Mutex<Option<Instant>>) -> bool {
    match *slot.lock().unwrap_or_else(|e| e.into_inner()) {
        Some(at) => at.elapsed() >= RELAUNCH_GRACE,
        None => false,
    }
}

/// `(state, relaunch_required, detail)`
fn accessibility_status() -> (&'static str, bool, String) {
    if accessibility_trusted() {
        return (
            STATE_GRANTED,
            false,
            "Granted to this running copy of ScreenExtend.".to_string(),
        );
    }
    // unverified whether `AXIsProcessTrusted` sees a grant made while running, so once a
    // prompt has had time to be answered the restart hint is the fallback
    let relaunch = requested_long_enough(&AX_REQUESTED_AT);
    match AX_SECONDARY.get("accessibility", accessibility_api_probe) {
        Some(AxSecondary::Unreadable) => (
            STATE_UNKNOWN,
            relaunch,
            "The accessibility subsystem did not answer in time, so this could not be read. \
             Remote control stays available and will report its own error if it is blocked."
                .to_string(),
        ),
        _ if RESET_ACCESSIBILITY.load(Ordering::Relaxed) => (
            STATE_DENIED,
            relaunch,
            "ScreenExtend cleared its Accessibility entry and asked macOS again: the entry was \
             either switched off or no longer matched this copy of ScreenExtend. Turn \
             ScreenExtend on under System Settings → Privacy & Security → Accessibility; restart \
             ScreenExtend if it still says not granted."
                .to_string(),
        ),
        _ if relaunch => (
            STATE_DENIED,
            true,
            "Turn ScreenExtend on under System Settings → Privacy & Security → Accessibility. \
             If it is already on and this still says not granted, quit and reopen ScreenExtend."
                .to_string(),
        ),
        _ if first_requested_ms(KEY_ACCESSIBILITY).is_some() => (
            STATE_DENIED,
            false,
            "macOS has already asked once. Turn ScreenExtend on under System Settings → Privacy \
             & Security → Accessibility. If it is already on and this still says not granted, \
             press Retry: ScreenExtend will clear the entry and ask again."
                .to_string(),
        ),
        _ => (
            STATE_DENIED,
            false,
            "Not granted. Only remote keyboard and mouse control needs this; streaming works \
             without it."
                .to_string(),
        ),
    }
}

fn cg_bool_fn(name: &str) -> Option<unsafe extern "C" fn() -> bool> {
    let cname = CString::new(name).ok()?;
    unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr());
        if p.is_null() {
            None
        } else {
            Some(std::mem::transmute::<
                *mut c_void,
                unsafe extern "C" fn() -> bool,
            >(p))
        }
    }
}

/// `CGPreflightScreenCaptureAccess`, which is cached per process. Trustworthy as a positive,
/// stale as a negative: Apple DTS states it "will not reflect this change until the
/// application is restarted", so it never shows a grant made after the process started.
pub fn preflight_screen_capture() -> Option<bool> {
    cg_bool_fn("CGPreflightScreenCaptureAccess").map(|f| unsafe { f() })
}

fn dict_i32(dict: &CFDictionary, key: &CFString) -> Option<i32> {
    let v = unsafe { CFDictionaryGetValue(dict, (key as *const CFString).cast()) };
    if v.is_null() {
        return None;
    }
    let mut out: i32 = 0;
    let ok = unsafe {
        CFNumberGetValue(
            v as *const CFNumber,
            K_CF_NUMBER_S_INT32_TYPE,
            (&mut out as *mut i32).cast::<c_void>(),
        )
    };
    (ok != 0).then_some(out)
}

/// What one pass over the window list saw: foreign, normal (layer-0) windows only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WindowScan {
    /// Foreign layer-0 windows.
    pub candidates: usize,
    /// Distinct processes owning them.
    pub owners: usize,
    /// How many of them carried a non-empty `kCGWindowName`.
    pub named: usize,
}

/// A definite Screen Recording denial needs at least this many foreign normal windows on
/// screen, none titled...
pub const DENY_MIN_WINDOWS: usize = 2;
/// ...owned by at least this many different processes. Untitled windows are ordinary even
/// with the grant (borderless windows, games, helper panels: one of four on screen was
/// untitled on an authorized machine), but a whole screen of them across two or more apps
/// is not; and one app's untitled windows say nothing about the next app's.
pub const DENY_MIN_OWNERS: usize = 2;

/// Walk the window list. Without the Screen Recording grant macOS strips `kCGWindowName`
/// from every window belonging to another process, so this skips our own windows and
/// everything that is not a normal layer-0 window (the Dock, the menu bar, the wallpaper)
/// and counts what is left and how much of it is titled. The read needs no prompt and no
/// capture session, and it flips the instant the user ticks the toggle.
fn window_scan(on_screen_only: bool) -> Option<WindowScan> {
    let mut option = CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
    if on_screen_only {
        option |= CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY;
    }
    let list = unsafe { CGWindowListCopyWindowInfo(option, CG_NULL_WINDOW_ID) };
    let arr = unsafe { list.as_ref() }?;

    let our_pid = unsafe { libc::getpid() };
    let pid_key = CFString::from_str("kCGWindowOwnerPID");
    let layer_key = CFString::from_str("kCGWindowLayer");
    let name_key = CFString::from_str("kCGWindowName");

    let mut scan = WindowScan::default();
    let mut owners: Vec<i32> = Vec::new();
    for i in 0..arr.count() {
        let dict_ptr = unsafe { arr.value_at_index(i) } as *const CFDictionary;
        let Some(dict) = (unsafe { dict_ptr.as_ref() }) else {
            continue;
        };
        let pid = match dict_i32(dict, &pid_key) {
            Some(pid) if pid != our_pid => pid,
            _ => continue,
        };
        if dict_i32(dict, &layer_key) != Some(0) {
            continue;
        }
        scan.candidates += 1;
        if !owners.contains(&pid) {
            owners.push(pid);
        }
        let name = unsafe { CFDictionaryGetValue(dict, (&*name_key as *const CFString).cast()) };
        if !name.is_null() && unsafe { CFStringGetLength(name) } > 0 {
            scan.named += 1;
        }
    }
    unsafe { CFRelease(list.cast()) };
    scan.owners = owners.len();
    Some(scan)
}

/// The raw on-screen read, for diagnostics (`doctor`, the bench). Not a decision: use
/// [`screen_recording_silent`] for that.
///
/// - `Some(true)`: a foreign window's title is visible, so authorized right now.
/// - `Some(false)`: foreign windows exist and none is named. A definite denial only when
///   [`classify_screen_recording`] says so.
/// - `None`: nothing to look at. Unknown; conclude nothing.
pub fn window_name_probe() -> Option<bool> {
    let scan = window_scan(true)?;
    if scan.candidates == 0 {
        None
    } else {
        Some(scan.named > 0)
    }
}

/// Is any foreign window anywhere (minimised, hidden, on another Space) titled? Used when
/// nothing on screen can be read (Show Desktop, an empty Space). Positive evidence only:
/// off-screen windows are often untitled even with the grant (3 of 25 foreign layer-0 windows
/// were titled on an authorized macOS 26 session).
fn titled_window_anywhere() -> bool {
    window_scan(false).is_some_and(|s| s.named > 0)
}

/// `SCShareableContent`, 12.3+. PROMPTING while unauthorized on macOS 15+, so it takes
/// a [`PromptPermit`]: in practice the one from [`silently_granted_permit`], where it is
/// a confirmation that cannot raise anything. Never call it from a polled path: the system
/// alert comes up again on every call.
fn sck_probe(permit: PromptPermit) -> Option<bool> {
    if !crate::macos_utils::streamer::screencapturekit_available() {
        return None;
    }
    crate::macos_utils::streamer::sck::probe_screen_recording(permit)
}

static CONFIRMED: AtomicBool = AtomicBool::new(false);

/// A silent answer and where it came from. `source` is for the log and `doctor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SilentRead {
    pub state: Silent,
    pub source: &'static str,
}

pub const SRC_CONFIRMED: &str = "confirmed-earlier-in-this-process";
pub const SRC_PREFLIGHT: &str = "preflight";
pub const SRC_WINDOW_TITLE: &str = "on-screen-window-title";
pub const SRC_OFFSCREEN_TITLE: &str = "off-screen-window-title";
pub const SRC_UNTITLED_WINDOWS: &str = "on-screen-windows-untitled";
pub const SRC_TOO_FEW_WINDOWS: &str = "too-few-windows-to-conclude";
pub const SRC_NOTHING_TO_READ: &str = "no-foreign-window-to-read";
pub const SRC_AX_TRUSTED: &str = "ax-trusted";
pub const SRC_AX_API_DISABLED: &str = "ax-untrusted-and-api-disabled";
pub const SRC_AX_INCONCLUSIVE: &str = "ax-untrusted-but-messaging-inconclusive";

/// The whole Screen Recording decision, as a pure function of what the reads saw.
///
/// - `confirmed`: this process has already seen a positive (`CONFIRMED`).
/// - `preflight`: `CGPreflightScreenCaptureAccess` (`None` where it does not exist).
/// - `on_screen`: the on-screen window scan (`None` if the window list was unavailable).
/// - `titled_anywhere`: whether any foreign window, on screen or not, is titled.
///
/// The cached preflight negative never contributes to `DefinitelyDenied`: it is stale in
/// exactly the case that matters (granted while running). And once `confirmed` is latched,
/// no negative is definite at all.
pub fn classify_screen_recording(
    confirmed: bool,
    preflight: Option<bool>,
    on_screen: Option<WindowScan>,
    titled_anywhere: bool,
) -> SilentRead {
    let read = |state, source| SilentRead { state, source };
    if preflight == Some(true) {
        return read(Silent::Granted, SRC_PREFLIGHT);
    }
    if on_screen.is_some_and(|s| s.named > 0) {
        return read(Silent::Granted, SRC_WINDOW_TITLE);
    }
    if titled_anywhere {
        return read(Silent::Granted, SRC_OFFSCREEN_TITLE);
    }
    if confirmed {
        return read(Silent::Unknown, SRC_CONFIRMED);
    }
    match on_screen {
        Some(s) if s.candidates >= DENY_MIN_WINDOWS && s.owners >= DENY_MIN_OWNERS => {
            read(Silent::DefinitelyDenied, SRC_UNTITLED_WINDOWS)
        }
        Some(s) if s.candidates > 0 => read(Silent::Unknown, SRC_TOO_FEW_WINDOWS),
        _ => read(Silent::Unknown, SRC_NOTHING_TO_READ),
    }
}

/// The silent Screen Recording read. Nothing here can prompt. A `Granted` answer latches
/// `CONFIRMED` for the rest of the process.
pub fn screen_recording_silent() -> SilentRead {
    let preflight = preflight_screen_capture();
    // skip the window walks when the cheap answer is already yes
    let (on_screen, anywhere) = if preflight == Some(true) {
        (None, false)
    } else {
        let on_screen = window_scan(true);
        let anywhere = on_screen.is_none_or(|s| s.named == 0) && titled_window_anywhere();
        (on_screen, anywhere)
    };
    let read = classify_screen_recording(
        CONFIRMED.load(Ordering::Relaxed),
        preflight,
        on_screen,
        anywhere,
    );
    if read.state == Silent::Granted {
        CONFIRMED.store(true, Ordering::Relaxed);
    }
    read
}

fn silent_screen_recording() -> Silent {
    screen_recording_silent().state
}

/// The silent Accessibility read.
///
/// `AXIsProcessTrusted` true is a grant. False is definite only when the AX messaging probe,
/// run now (not the cached background slot), also fails with `kAXErrorAPIDisabled`, which is
/// what an untrusted process gets for every message. Both are read in the same moment, so a
/// fresh grant shows in at least one. A probe that timed out, or that worked while untrusted,
/// is `Unknown`, and `Unknown` never resets the TCC row.
fn silent_accessibility() -> SilentRead {
    if accessibility_trusted() {
        return SilentRead {
            state: Silent::Granted,
            source: SRC_AX_TRUSTED,
        };
    }
    let secondary = accessibility_api_probe();
    // re-read: a toggle flipped while the probe ran must not be reset
    if accessibility_trusted() {
        return SilentRead {
            state: Silent::Granted,
            source: SRC_AX_TRUSTED,
        };
    }
    classify_accessibility(false, secondary)
}

/// Pure half of [`silent_accessibility`].
pub fn classify_accessibility(trusted: bool, secondary: Option<AxSecondary>) -> SilentRead {
    if trusted {
        return SilentRead {
            state: Silent::Granted,
            source: SRC_AX_TRUSTED,
        };
    }
    match secondary {
        Some(AxSecondary::ApiDisabled) => SilentRead {
            state: Silent::DefinitelyDenied,
            source: SRC_AX_API_DISABLED,
        },
        _ => SilentRead {
            state: Silent::Unknown,
            source: SRC_AX_INCONCLUSIVE,
        },
    }
}

fn silent_state(key: &str) -> SilentRead {
    match key {
        KEY_ACCESSIBILITY => silent_accessibility(),
        KEY_SCREEN_RECORDING => screen_recording_silent(),
        _ => SilentRead {
            state: Silent::Unknown,
            source: "unknown-key",
        },
    }
}

/// The silent answer as an `Option<bool>`, for the callers that act on it: the session
/// watchdog and `ensure_screen_recording_access` at every pipeline start.
///
/// `Some(false)` means **`DefinitelyDenied`** and nothing weaker: never the cached
/// preflight negative, never a read with too few windows to judge, and never anything
/// once this process has confirmed a grant. `None` is `Unknown`: callers must not refuse,
/// kick or reset on it.
pub fn screen_recording_live() -> Option<bool> {
    match silent_screen_recording() {
        Silent::Granted => Some(true),
        Silent::DefinitelyDenied => Some(false),
        Silent::Unknown => None,
    }
}

/// `(state, relaunch_required, detail)`: what the startup gate and the lapse dialog show.
///
/// The one place the cached preflight negative counts, and only for display: with nothing in
/// the window list to read, `preflight == false` is shown as "not granted yet", so the gate
/// stays up (and the first request gets made) on a first run with an empty screen. Nothing
/// that refuses a join, kicks a session or resets a TCC row reads this: they read
/// [`screen_recording_silent`].
fn screen_recording_status() -> (&'static str, bool, String) {
    if CONFIRMED.load(Ordering::Relaxed) {
        return (
            STATE_GRANTED,
            false,
            "Confirmed for this running copy of ScreenExtend.".to_string(),
        );
    }

    // no ScreenCaptureKit here: `check_permissions` is polled, and on macOS 15+
    // `SCShareableContent` raises the system alert on every call while unauthorized
    let preflight = preflight_screen_capture();
    let read = screen_recording_silent();
    match read.state {
        Silent::Granted if read.source == SRC_PREFLIGHT => (
            STATE_GRANTED,
            false,
            "Granted to this running copy of ScreenExtend.".to_string(),
        ),
        Silent::Granted => {
            // the toggle is on and capture works now, but anything in this process that
            // consults the cached preflight (ScreenCaptureKit on macOS 15 included) may still
            // be looking at the old answer
            let stale_cache = preflight == Some(false);
            let detail = if stale_cache {
                "Granted. macOS still has the old answer cached for this process, so restart \
                 ScreenExtend if capture misbehaves."
                    .to_string()
            } else {
                "Granted, confirmed by reading the live window list.".to_string()
            };
            (STATE_GRANTED, stale_cache, detail)
        }
        Silent::DefinitelyDenied => (
            STATE_DENIED,
            requested_long_enough(&SCREEN_REQUESTED_AT),
            denied_detail(),
        ),
        Silent::Unknown if preflight == Some(false) => {
            let relaunch = requested_long_enough(&SCREEN_REQUESTED_AT);
            let detail = if first_requested_ms(KEY_SCREEN_RECORDING).is_none() {
                "Lets ScreenExtend capture this Mac's screen to stream it.".to_string()
            } else {
                "Not confirmed yet. macOS's own answer for this process is the one it had at \
                 launch, and there is no other app's window on screen to check it against. If \
                 you have just turned ScreenExtend on under System Settings → Privacy & \
                 Security → Screen Recording, open any window or restart ScreenExtend."
                    .to_string()
            };
            (STATE_DENIED, relaunch, detail)
        }
        Silent::Unknown => (
            STATE_UNKNOWN,
            false,
            "Could not read this permission. ScreenExtend will start anyway; if capture is \
             blocked, the device that tries to connect will say so."
                .to_string(),
        ),
    }
}

fn denied_detail() -> String {
    if RESET_SCREEN_RECORDING.load(Ordering::Relaxed) {
        "ScreenExtend cleared its Screen Recording entry and asked macOS again: the entry was \
         either switched off or no longer matched this copy of ScreenExtend. Turn ScreenExtend \
         on under System Settings → Privacy & Security → Screen Recording, then restart \
         ScreenExtend."
            .to_string()
    } else if requested_long_enough(&SCREEN_REQUESTED_AT) {
        "Grant Screen Recording in System Settings, then restart ScreenExtend: macOS will not \
         hand a new grant to an app that is already running."
            .to_string()
    } else if first_requested_ms(KEY_SCREEN_RECORDING).is_some() {
        "macOS has already asked once. Turn ScreenExtend on under System Settings → Privacy & \
         Security → Screen Recording. If it is already on and this still says denied, press \
         Retry: ScreenExtend will clear the entry and ask again."
            .to_string()
    } else {
        "Lets ScreenExtend capture this Mac's screen to stream it.".to_string()
    }
}

/// PROMPTING. Reachable only with a [`PromptPermit`], i.e. through [`prompt_gate`].
fn request_screen_capture_access(_permit: PromptPermit) -> bool {
    *SCREEN_REQUESTED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    match cg_bool_fn("CGRequestScreenCaptureAccess") {
        Some(f) => unsafe { f() },
        None => false,
    }
}

// prompt policy: after the first request, poll with silent reads only. The user gets no second
// prompt unless they explicitly hit Retry

/// What the silent reads say. Never produced by a prompting call. Only `DefinitelyDenied` may
/// refuse a join or a capture start, kick a live session, or reset a TCC row on Retry;
/// `Granted` and `Unknown` do none of those.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Silent {
    Granted,
    /// Positive evidence of a denial, live, in this process. Never the cached preflight
    /// negative alone, and never after this process has confirmed a grant.
    DefinitelyDenied,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptDecision {
    /// A prompting API may be called.
    pub may_prompt: bool,
    /// The TCC row should be reset (`tccutil reset`) before prompting.
    pub may_reset: bool,
}

/// The whole policy, as a pure function.
///
/// - `first_requested_ms`: when this permission was first requested under this TCC identity
///   (the persisted marker for an `.app` that owns its identity, this process's memory
///   otherwise). `None` if it never was.
/// - `explicit_retry`: the user pressed a visible Retry button.
/// - `owns_identity`: [`owns_tcc_identity`]. A process that does not is attributed by TCC to
///   its parent (the terminal), so resetting "its" row would reset nothing useful, or the
///   wrong thing.
///
/// Granted never prompts. The first request prompts; a later automatic one does not, in this
/// process or after a relaunch. An explicit Retry prompts, and first resets the row only when
/// the silent read is `DefinitelyDenied`, whenever the first request happened: macOS will not
/// prompt again over an existing row, whether the user switched it off or it is stale and
/// pinned to an earlier build's cdhash. `Unknown` never resets, because it is exactly what a
/// grant made a moment ago looks like.
pub fn prompt_policy(
    first_requested_ms: Option<u64>,
    explicit_retry: bool,
    silent: Silent,
    owns_identity: bool,
) -> PromptDecision {
    const NO: PromptDecision = PromptDecision {
        may_prompt: false,
        may_reset: false,
    };
    if silent == Silent::Granted {
        return NO;
    }
    if first_requested_ms.is_some() && !explicit_retry {
        return NO;
    }
    PromptDecision {
        may_prompt: true,
        may_reset: explicit_retry && owns_identity && silent == Silent::DefinitelyDenied,
    }
}

/// The capability to call a prompting API. Only [`prompt_gate`],
/// [`silently_granted_permit`] and [`capture_start_permit`] construct one (pinned by a
/// test), so a periodic path that wants to reach `CGRequestScreenCaptureAccess`, the AX
/// prompt or `SCShareableContent` does not compile without going through one of them.
#[derive(Clone, Copy)]
pub struct PromptPermit(());

/// A permit for a confirmation that cannot prompt: only handed out when a silent read
/// has already returned granted, or this process has confirmed a grant before.
pub fn silently_granted_permit() -> Option<PromptPermit> {
    let granted = silent_screen_recording() == Silent::Granted || CONFIRMED.load(Ordering::Relaxed);
    granted.then_some(PromptPermit(()))
}

/// A permit for *starting capture* (ScreenCaptureKit system audio): refused only on a
/// definite denial, the same rule `ensure_screen_recording_access` applies to video.
///
/// On `Unknown` this can reach `SCShareableContent` while unauthorized, which raises the
/// system alert on macOS 15+. That is no wider than what already happens at the same
/// moment: the video pipeline starts its own `SCStream` (which fetches shareable content
/// too) on the same `Unknown`, because refusing there would refuse every join of a host
/// that was granted while running and has nothing else on screen.
pub fn capture_start_permit() -> Option<PromptPermit> {
    (silent_screen_recording() != Silent::DefinitelyDenied).then_some(PromptPermit(()))
}

/// The prefix the desktop UI puts on `request_permission`'s key for an explicit Retry.
/// A key, not a new argument or command, so the generated `bindings.ts` is unchanged.
pub const RETRY_PREFIX: &str = "retry:";

static RESET_SCREEN_RECORDING: AtomicBool = AtomicBool::new(false);
static RESET_ACCESSIBILITY: AtomicBool = AtomicBool::new(false);

/// The one door to a prompting API. Called by `request_permission` (the desktop UI: the
/// automatic first request and the Retry button) and by `serve` at startup (the first
/// request a headless host would otherwise never make). Blocks while macOS shows its
/// dialog, so call it off the main thread.
pub fn prompt_gate(key: &str, explicit_retry: bool) -> bool {
    let key: &'static str = match key {
        KEY_ACCESSIBILITY => KEY_ACCESSIBILITY,
        KEY_SCREEN_RECORDING => KEY_SCREEN_RECORDING,
        _ => return false,
    };
    let silent = silent_state(key);
    let first = first_requested_ms(key);
    let owns = owns_tcc_identity();
    let decision = prompt_policy(first, explicit_retry, silent.state, owns);
    if !decision.may_prompt {
        if silent.state != Silent::Granted {
            tprintln!(
                "[permissions] {key}: already requested once; not prompting again without an \
                 explicit Retry (silent read: {:?} from {})",
                silent.state,
                silent.source
            );
        }
        return silent.state == Silent::Granted;
    }
    if decision.may_reset {
        if let Some(id) = running_bundle_id() {
            reset_tcc_row(key, id, silent.source);
        }
    } else if explicit_retry && silent.state != Silent::DefinitelyDenied {
        tprintln!(
            "[permissions] {key}: Retry without a TCC reset. The silent read is {:?} ({}), \
             which could be a grant made a moment ago",
            silent.state,
            silent.source
        );
    }
    // recorded before the prompt, which blocks: a process that dies with the dialog up has
    // still asked once
    record_request(key);
    let permit = PromptPermit(());
    match key {
        KEY_ACCESSIBILITY => prompt_accessibility(permit),
        _ => request_screen_capture_access(permit),
    }
}

fn reset_tcc_row(key: &'static str, bundle_id: &str, source: &str) {
    let service = match key {
        KEY_ACCESSIBILITY => "Accessibility",
        _ => "ScreenCapture",
    };
    tprintln!(
        "[permissions] {key} reads as definitely denied ({source}) on an explicit Retry; the \
         row is either switched off or stale, and macOS will not ask over it, so running \
         `tccutil reset {service} {bundle_id}` before asking again"
    );
    match Command::new("/usr/bin/tccutil")
        .args(["reset", service, bundle_id])
        .output()
    {
        Ok(out) if out.status.success() => match key {
            KEY_ACCESSIBILITY => RESET_ACCESSIBILITY.store(true, Ordering::Relaxed),
            _ => RESET_SCREEN_RECORDING.store(true, Ordering::Relaxed),
        },
        Ok(out) => teprintln!(
            "[permissions] tccutil reset {service} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => teprintln!("[permissions] could not run tccutil: {e}"),
    }
}

/// The bundle id of the `.app` this process runs from, or `None` for a bare binary.
/// Cached: it cannot change for the life of the process.
fn running_bundle_id() -> Option<&'static str> {
    static ID: OnceLock<Option<String>> = OnceLock::new();
    ID.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let plist = bundle_path_of(&exe)?.join("Contents/Info.plist");
        let out = Command::new("/usr/libexec/PlistBuddy")
            .arg("-c")
            .arg("Print :CFBundleIdentifier")
            .arg(&plist)
            .output()
            .ok()?;
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let valid = out.status.success()
            && !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        valid.then_some(id)
    })
    .as_deref()
}

/// Whether TCC attributes this process's requests to *this app*: an `.app` with a bundle id
/// that LaunchServices started (parent is launchd). A bare binary, or an `.app`'s executable
/// started from a shell (`.../Contents/MacOS/ScreenExtend serve` in Terminal), is attributed to
/// the responsible ancestor instead, so its request neither creates nor answers the app's
/// row. Only an identity-owning process persists the request marker or resets a row.
fn owns_tcc_identity() -> bool {
    static OWNS: OnceLock<bool> = OnceLock::new();
    *OWNS.get_or_init(|| identity_owned(running_bundle_id().is_some(), unsafe { libc::getppid() }))
}

/// Pure half of [`owns_tcc_identity`].
fn identity_owned(has_bundle_id: bool, parent_pid: i32) -> bool {
    has_bundle_id && parent_pid == 1
}

const REQUEST_MARKER_FILE: &str = "permission-requests.json";
/// Tauri's app-local-data directory on macOS is `~/Library/Application Support/<id>`;
/// used only until a command has handed us the resolved one.
const FALLBACK_IDENTIFIER: &str = "app.screenextend.desktop";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestRecord {
    first_requested_ms: u64,
    last_requested_ms: u64,
    count: u32,
}

static MARKER_DIR: OnceLock<PathBuf> = OnceLock::new();
static MARKER: Mutex<Option<BTreeMap<String, RequestRecord>>> = Mutex::new(None);

pub fn remember_marker_dir(app: &tauri::AppHandle) {
    if MARKER_DIR.get().is_none() {
        if let Ok(dir) = crate::data_dir::local_data_dir(app) {
            let _ = MARKER_DIR.set(dir);
        }
    }
}

fn marker_path() -> Option<PathBuf> {
    if let Some(dir) = crate::data_dir::override_dir() {
        return Some(dir.join(REQUEST_MARKER_FILE));
    }
    if let Some(dir) = MARKER_DIR.get() {
        return Some(dir.join(REQUEST_MARKER_FILE));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Application Support")
            .join(FALLBACK_IDENTIFIER)
            .join(REQUEST_MARKER_FILE),
    )
}

/// The request record. For a process that owns its TCC identity it is loaded from (and
/// written back to) `permission-requests.json`; for anything else it lives in memory only,
/// so a dev binary's request (attributed by TCC to the terminal) can never suppress the
/// `.app`'s one automatic prompt, and the dev binary may itself prompt once per process.
fn with_marker<R>(f: impl FnOnce(&mut BTreeMap<String, RequestRecord>) -> R) -> R {
    with_marker_for(owns_tcc_identity(), f)
}

fn with_marker_for<R>(
    persist: bool,
    f: impl FnOnce(&mut BTreeMap<String, RequestRecord>) -> R,
) -> R {
    let mut slot = MARKER.lock().unwrap_or_else(|e| e.into_inner());
    let map = slot.get_or_insert_with(|| {
        if !persist {
            return BTreeMap::new();
        }
        marker_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    });
    f(map)
}

fn first_requested_ms(key: &str) -> Option<u64> {
    with_marker(|m| m.get(key).map(|r| r.first_requested_ms))
}

fn record_request(key: &str) {
    let now = unix_ms_now();
    let persist = owns_tcc_identity();
    let snapshot = with_marker_for(persist, |m| {
        let r = m.entry(key.to_string()).or_insert(RequestRecord {
            first_requested_ms: now,
            last_requested_ms: now,
            count: 0,
        });
        r.last_requested_ms = now;
        r.count = r.count.saturating_add(1);
        m.clone()
    });
    if !persist {
        tprintln!(
            "[permissions] {key}: requested from a process that does not own the app's TCC \
             identity (no bundle id, or not started by LaunchServices); remembered for this \
             process only"
        );
        return;
    }
    let Some(path) = marker_path() else {
        teprintln!("[permissions] no directory for {REQUEST_MARKER_FILE}; the request is remembered for this run only");
        return;
    };
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&snapshot)?)?;
        std::fs::rename(&tmp, &path)
    };
    if let Err(e) = write() {
        teprintln!(
            "[permissions] could not write {}: {e}; the request is remembered for this run only",
            path.display()
        );
    }
}

fn unix_ms_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Every probe, separately and uncollapsed. A disagreement between these *is* the
/// diagnosis, which is why `doctor` prints them individually rather than the one
/// boolean the UI sees.
pub struct ProbeReport {
    pub preflight: Option<bool>,
    pub preflight_us: u128,
    pub window_name: Option<bool>,
    pub window_name_us: u128,
    pub ax_trusted: bool,
    pub ax_secondary: &'static str,
    pub ax_secondary_us: u128,
    pub sck_available: bool,
    pub sck: Option<bool>,
    pub sck_us: u128,
}

pub fn raw_probes() -> ProbeReport {
    let t = Instant::now();
    let preflight = preflight_screen_capture();
    let preflight_us = t.elapsed().as_micros();

    let t = Instant::now();
    let window_name = window_name_probe();
    let window_name_us = t.elapsed().as_micros();

    let ax_trusted = accessibility_trusted();

    let t = Instant::now();
    let ax_secondary = accessibility_api_probe().map_or("not-run", AxSecondary::as_str);
    let ax_secondary_us = t.elapsed().as_micros();

    let sck_available = crate::macos_utils::streamer::screencapturekit_available();
    let t = Instant::now();
    // only after a silent grant: unauthorized, it would raise the system alert from a
    // diagnostic command
    let sck = if sck_available {
        silently_granted_permit().and_then(sck_probe)
    } else {
        None
    };
    let sck_us = t.elapsed().as_micros();

    ProbeReport {
        preflight,
        preflight_us,
        window_name,
        window_name_us,
        ax_trusted,
        ax_secondary,
        ax_secondary_us,
        sck_available,
        sck,
        sck_us,
    }
}

/// The code identity TCC actually matches against, plus the two ways a bundle can
/// end up running from somewhere the user did not expect.
pub struct IdentityReport {
    pub exe_path: String,
    pub bundle_path: Option<String>,
    pub translocated: bool,
    pub designated_requirement: String,
    pub quarantine: String,
    pub sip: String,
    pub pid: i32,
    pub parent_pid: i32,
    pub parent_command: String,
    pub os_version: String,
}

/// The `.app` containing `exe`, if the executable sits in `.../Contents/MacOS/`.
pub fn bundle_path_of(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    let macos_dir = exe.parent()?;
    if macos_dir.file_name()? != "MacOS" {
        return None;
    }
    let contents = macos_dir.parent()?;
    if contents.file_name()? != "Contents" {
        return None;
    }
    let bundle = contents.parent()?;
    (bundle.extension()? == "app").then(|| bundle.to_path_buf())
}

fn shell_out(cmd: &str, args: &[&str]) -> String {
    match Command::new(cmd).args(args).output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let err = String::from_utf8_lossy(&out.stderr);
            let err = err.trim();
            if !err.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(err);
            }
            if s.is_empty() {
                format!("(no output, exit {})", out.status)
            } else {
                s
            }
        }
        Err(e) => format!("({cmd} failed: {e})"),
    }
}

pub fn identity_report() -> IdentityReport {
    // `current_exe` can return the path as invoked (`./` and all); the `/AppTranslocation/`
    // test and the codesign read need it resolved
    let exe = std::env::current_exe().unwrap_or_default();
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let bundle = bundle_path_of(&exe);
    let signed_path = bundle.clone().unwrap_or_else(|| exe.clone());
    let signed = signed_path.to_string_lossy().to_string();
    let exe_path = exe.to_string_lossy().to_string();

    let parent_pid = unsafe { libc::getppid() };
    let parent_command = shell_out("ps", &["-o", "command=", "-p", &parent_pid.to_string()]);

    IdentityReport {
        translocated: exe_path.contains("/AppTranslocation/"),
        designated_requirement: shell_out("codesign", &["-d", "-r-", "--verbose=4", &signed]),
        quarantine: shell_out("xattr", &["-p", "com.apple.quarantine", &signed]),
        sip: shell_out("csrutil", &["status"]),
        os_version: shell_out("sw_vers", &["-productVersion"]),
        pid: unsafe { libc::getpid() },
        parent_pid,
        parent_command,
        bundle_path: bundle.map(|b| b.to_string_lossy().to_string()),
        exe_path,
    }
}

/// The probe itself, for callers that are already off the main thread: `ScreenExtend
/// doctor`, and the async command below.
pub fn probe_permissions() -> Vec<PermissionStatus> {
    let (ax_state, ax_relaunch, ax_detail) = accessibility_status();
    let (sr_state, sr_relaunch, sr_detail) = screen_recording_status();
    vec![
        PermissionStatus {
            key: KEY_ACCESSIBILITY.to_string(),
            name: "Accessibility".to_string(),
            description: "Lets connected devices control this Mac's keyboard and mouse."
                .to_string(),
            state: ax_state.to_string(),
            detail: ax_detail,
            // only opt-in remote input needs this: a user who just wants a second monitor
            // must not be stopped here
            required: false,
            relaunch_required: ax_relaunch,
        },
        PermissionStatus {
            key: KEY_SCREEN_RECORDING.to_string(),
            name: "Screen Recording".to_string(),
            description: "Lets ScreenExtend capture this Mac's screen to stream it.".to_string(),
            state: sr_state.to_string(),
            detail: sr_detail,
            required: true,
            relaunch_required: sr_relaunch,
        },
    ]
}

/// Warms the window-list probe on a background thread, so its cold cost (37 ms on 10.15.8,
/// CoreGraphics connecting to WindowServer) is not paid inside the first `check_permissions`,
/// mid-launch. Silent only. It also loads the request marker.
pub fn prime() {
    std::thread::Builder::new()
        .name("se-perm-warm".into())
        .spawn(|| {
            let _ = window_name_probe();
            with_marker(|_| ());
        })
        .ok();
}

/// A plain `fn` command runs on the main thread. Nothing here needs to: the window-list
/// read is CoreGraphics IPC and `AXIsProcessTrusted` is a syscall, so both belong on a
/// blocking thread even at well under a millisecond apiece.
#[tauri::command]
#[specta::specta]
///
/// Silent only: this is what the bootstrap gate and the lapse dialog poll.
pub async fn check_permissions(app: tauri::AppHandle) -> Vec<PermissionStatus> {
    remember_marker_dir(&app);
    tauri::async_runtime::spawn_blocking(probe_permissions)
        .await
        .unwrap_or_else(|e| {
            teprintln!("[permissions] the permission probe thread failed: {e}");
            probe_permissions()
        })
}

/// `CGRequestScreenCaptureAccess` blocks until the user dismisses the OS prompt, which is
/// seconds of a frozen window if it runs on the main thread.
///
/// `key` is a permission key, or [`RETRY_PREFIX`] + a key for an explicit user Retry.
/// Everything goes through [`prompt_gate`]: the first request for a permission prompts,
/// a later automatic one returns the silent answer, and only a Retry prompts again.
#[tauri::command]
#[specta::specta]
pub async fn request_permission(app: tauri::AppHandle, key: String) -> bool {
    remember_marker_dir(&app);
    let (explicit_retry, key) = match key.strip_prefix(RETRY_PREFIX) {
        Some(k) => (true, k.to_string()),
        None => (false, key),
    };
    tauri::async_runtime::spawn_blocking(move || prompt_gate(&key, explicit_retry))
        .await
        .unwrap_or(false)
}

const SETTINGS_PANE_MODERN: &str =
    "x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension";
const SETTINGS_PANE_LEGACY: &str = "x-apple.systempreferences:com.apple.preference.security";

#[tauri::command]
#[specta::specta]
pub fn open_permission_settings(key: String) {
    let anchor = match key.as_str() {
        KEY_ACCESSIBILITY => "Privacy_Accessibility",
        KEY_SCREEN_RECORDING => "Privacy_ScreenCapture",
        _ => "Privacy",
    };
    // Ventura renamed the pane: try the modern identifier first and fall back, so the OS
    // decides rather than a version number
    for base in [SETTINGS_PANE_MODERN, SETTINGS_PANE_LEGACY] {
        let url = format!("{base}?{anchor}");
        match Command::new("open").arg(&url).status() {
            Ok(s) if s.success() => return,
            Ok(s) => teprintln!("[permissions] open {url} exited {s}"),
            Err(e) => teprintln!("[permissions] failed to run open for {url}: {e}"),
        }
    }
}

/// Relaunch through LaunchServices rather than by spawning the executable.
///
/// TCC attributes a request to the *responsible* process, and a binary started by
/// another process inherits that process's attribution instead of establishing its
/// own: the documented reason an app launched from a terminal never appears in the
/// Screen Recording list. `open -n -a` hands the launch to LaunchServices, so the
/// new instance is its own responsible process and the grant the user just made
/// applies to it.
#[tauri::command]
#[specta::specta]
pub fn relaunch_app(app: tauri::AppHandle) {
    crate::exit_reason::record("relaunch_app: the desktop UI asked for a restart");
    let exe = std::env::current_exe().unwrap_or_default();
    match bundle_path_of(&exe) {
        Some(bundle) => {
            tprintln!(
                "[permissions] relaunching via LaunchServices: {}",
                bundle.display()
            );
            match Command::new("open")
                .arg("-n")
                .arg("-a")
                .arg(&bundle)
                .spawn()
            {
                Ok(_) => {
                    app.exit(0);
                    return;
                }
                Err(e) => {
                    teprintln!("[permissions] open -n -a failed: {e}; falling back to restart")
                }
            }
        }
        None => teprintln!(
            "[permissions] not running from an .app bundle ({}); falling back to restart",
            exe.display()
        ),
    }
    app.restart();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bundle_path_is_recognised_only_for_a_real_layout() {
        assert_eq!(
            bundle_path_of(&PathBuf::from(
                "/Applications/ScreenExtend.app/Contents/MacOS/ScreenExtend"
            )),
            Some(PathBuf::from("/Applications/ScreenExtend.app"))
        );
        assert_eq!(
            bundle_path_of(&PathBuf::from("/usr/local/bin/ScreenExtend")),
            None
        );
        assert_eq!(
            bundle_path_of(&PathBuf::from(
                "/tmp/ScreenExtend/Contents/MacOS/ScreenExtend"
            )),
            None
        );
    }

    /// The window-list probe has to be cheap enough to sit inline in a command the
    /// permission gate polls, and it must never claim a definite answer when there
    /// is nothing on screen to read.
    #[test]
    fn window_name_probe_is_cheap_and_tri_state() {
        let mut warm = Vec::new();
        for i in 0..6 {
            let t = Instant::now();
            let r = window_name_probe();
            let us = t.elapsed().as_micros();
            eprintln!("window_name_probe #{i} -> {r:?} in {us} us");
            if i > 0 {
                warm.push(us);
            }
        }
        warm.sort_unstable();
        let median = warm[warm.len() / 2];
        eprintln!("window_name_probe warm median: {median} us");
        // the gate polls this at 5 Hz, so the warm cost is the one that matters; the first
        // call also pays CoreGraphics' one-off setup
        assert!(median < 100_000, "warm window-list probe took {median} us");
    }

    /// A negative preflight must never be the whole answer, and an unknown read must
    /// never be reported as denied.
    #[test]
    fn status_is_tri_state_and_never_panics() {
        let (state, _relaunch, detail) = screen_recording_status();
        eprintln!("screen_recording_status -> {state} ({detail})");
        assert!(
            matches!(state, STATE_GRANTED | STATE_DENIED | STATE_UNKNOWN),
            "unexpected state {state}"
        );
        assert!(!detail.is_empty());
        let (state, _, detail) = accessibility_status();
        eprintln!("accessibility_status -> {state} ({detail})");
        assert!(matches!(
            state,
            STATE_GRANTED | STATE_DENIED | STATE_UNKNOWN
        ));
        assert!(!detail.is_empty());
    }

    /// A panicking probe loses its own result and nothing else: the slot must still
    /// accept the next refresh instead of reporting a permanent denial.
    #[test]
    fn a_panicking_probe_does_not_wedge_the_slot() {
        static SLOT: BgProbe<bool> = BgProbe::new();
        fn boom() -> Option<bool> {
            panic!("probe blew up");
        }
        fn fine() -> Option<bool> {
            Some(true)
        }
        assert_eq!(SLOT.get("test", boom), None);
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !SLOT.state.lock().unwrap_or_else(|e| e.into_inner()).running,
            "the slot stayed marked as running after a panic"
        );
        SLOT.state.lock().unwrap_or_else(|e| e.into_inner()).last_at =
            Some(Instant::now() - PROBE_REFRESH - Duration::from_secs(1));
        assert_eq!(SLOT.get("test", fine), None);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            SLOT.state.lock().unwrap_or_else(|e| e.into_inner()).last,
            Some(true)
        );
    }

    fn scan(candidates: usize, owners: usize, named: usize) -> Option<WindowScan> {
        Some(WindowScan {
            candidates,
            owners,
            named,
        })
    }

    /// What every caller does with a silent answer, mirrored from the call sites:
    /// `ensure_screen_recording_access` (refuses a join), the watchdog (kicks on
    /// `screen_recording_live() == Some(false)`), and `prompt_policy` on an explicit Retry
    /// from an identity-owning `.app` (resets the TCC row).
    fn consequences(state: Silent) -> (bool, bool, bool) {
        let refuses_join = state == Silent::DefinitelyDenied;
        let live = match state {
            Silent::Granted => Some(true),
            Silent::DefinitelyDenied => Some(false),
            Silent::Unknown => None,
        };
        let kicks = live == Some(false);
        let resets = prompt_policy(Some(1), true, state, true).may_reset;
        (refuses_join, kicks, resets)
    }

    /// A process that started unauthorized (preflight cached false), was granted while
    /// running, and then the user pressed Show Desktop, so there is no foreign window on
    /// screen or off it to read. That is `Unknown`, and `Unknown` refuses nothing, kicks
    /// nothing and resets nothing.
    #[test]
    fn granted_while_running_then_nothing_on_screen_is_unknown_and_harmless() {
        for confirmed in [false, true] {
            let r = classify_screen_recording(confirmed, Some(false), None, false);
            assert_eq!(r.state, Silent::Unknown, "confirmed={confirmed}: {r:?}");
            let r = classify_screen_recording(confirmed, Some(false), scan(0, 0, 0), false);
            assert_eq!(r.state, Silent::Unknown, "confirmed={confirmed}: {r:?}");
            assert_eq!(consequences(r.state), (false, false, false));
        }
        // and the moment any titled window exists, anywhere, it is a grant
        let r = classify_screen_recording(false, Some(false), scan(0, 0, 0), true);
        assert_eq!((r.state, r.source), (Silent::Granted, SRC_OFFSCREEN_TITLE));
        let r = classify_screen_recording(false, Some(false), scan(3, 2, 1), false);
        assert_eq!((r.state, r.source), (Silent::Granted, SRC_WINDOW_TITLE));
    }

    #[test]
    fn definite_denial_needs_enough_untitled_windows_from_enough_apps() {
        let r = classify_screen_recording(false, Some(false), scan(2, 2, 0), false);
        assert_eq!(
            (r.state, r.source),
            (Silent::DefinitelyDenied, SRC_UNTITLED_WINDOWS)
        );
        assert_eq!(consequences(r.state), (true, true, true));
        // preflight unavailable does not matter: the evidence is the window list
        let r = classify_screen_recording(false, None, scan(5, 3, 0), false);
        assert_eq!(r.state, Silent::DefinitelyDenied);

        // one untitled window, or several from one app: not enough
        for s in [scan(1, 1, 0), scan(4, 1, 0)] {
            let r = classify_screen_recording(false, Some(false), s, false);
            assert_eq!(
                (r.state, r.source),
                (Silent::Unknown, SRC_TOO_FEW_WINDOWS),
                "{s:?}"
            );
            assert_eq!(consequences(r.state), (false, false, false));
        }
        // a titled window off screen outweighs untitled ones on screen
        let r = classify_screen_recording(false, Some(false), scan(4, 3, 0), true);
        assert_eq!(r.state, Silent::Granted);
    }

    /// Once this process has proven a grant, nothing it reads later is a definite denial.
    #[test]
    fn a_confirmed_process_is_never_definitely_denied() {
        for preflight in [None, Some(false)] {
            for s in [
                None,
                scan(0, 0, 0),
                scan(1, 1, 0),
                scan(2, 2, 0),
                scan(9, 6, 0),
            ] {
                let r = classify_screen_recording(true, preflight, s, false);
                assert_ne!(
                    r.state,
                    Silent::DefinitelyDenied,
                    "{preflight:?} {s:?}: {r:?}"
                );
                assert_eq!(consequences(r.state), (false, false, false));
            }
        }
    }

    /// The cached preflight negative is never the whole answer.
    #[test]
    fn preflight_negative_alone_is_never_definite() {
        let r = classify_screen_recording(false, Some(false), None, false);
        assert_eq!((r.state, r.source), (Silent::Unknown, SRC_NOTHING_TO_READ));
        let r = classify_screen_recording(false, Some(true), scan(9, 6, 0), false);
        assert_eq!((r.state, r.source), (Silent::Granted, SRC_PREFLIGHT));
    }

    #[test]
    fn accessibility_is_definite_only_when_both_reads_agree() {
        assert_eq!(classify_accessibility(true, None).state, Silent::Granted);
        assert_eq!(
            classify_accessibility(true, Some(AxSecondary::ApiDisabled)).state,
            Silent::Granted
        );
        assert_eq!(
            classify_accessibility(false, Some(AxSecondary::ApiDisabled)).state,
            Silent::DefinitelyDenied
        );
        for secondary in [
            None,
            Some(AxSecondary::Unreadable),
            Some(AxSecondary::Enabled),
        ] {
            let r = classify_accessibility(false, secondary);
            assert_eq!(r.state, Silent::Unknown, "{secondary:?}");
            assert!(!prompt_policy(Some(1), true, r.state, true).may_reset);
        }
    }

    const EARLIER: u64 = 1_700_000_000_000;

    fn decide(first: Option<u64>, retry: bool, silent: Silent) -> PromptDecision {
        prompt_policy(first, retry, silent, true)
    }

    #[test]
    fn first_request_prompts() {
        for silent in [Silent::DefinitelyDenied, Silent::Unknown] {
            let d = decide(None, false, silent);
            assert!(d.may_prompt && !d.may_reset, "{silent:?}: {d:?}");
        }
    }

    /// A poll is an automatic request with a request already on record (the first one
    /// recorded it): it must never prompt, in this process or after a relaunch.
    #[test]
    fn poll_never_prompts() {
        for silent in [Silent::DefinitelyDenied, Silent::Unknown, Silent::Granted] {
            let d = decide(Some(EARLIER), false, silent);
            assert!(!d.may_prompt && !d.may_reset, "{silent:?}: {d:?}");
        }
    }

    /// A Retry on a definite denial resets, whenever the first request happened, including
    /// earlier in this same process.
    #[test]
    fn retry_on_a_definite_denial_resets_then_prompts() {
        for first in [None, Some(EARLIER)] {
            let d = decide(first, true, Silent::DefinitelyDenied);
            assert!(d.may_prompt && d.may_reset, "{first:?}: {d:?}");
        }
    }

    #[test]
    fn retry_never_resets_on_unknown_or_without_the_identity() {
        // `Unknown` is what a grant made a moment ago looks like
        let d = decide(Some(EARLIER), true, Silent::Unknown);
        assert!(d.may_prompt && !d.may_reset, "{d:?}");
        // a bare binary, or an .app started from a shell: TCC attributes it elsewhere
        let d = prompt_policy(Some(EARLIER), true, Silent::DefinitelyDenied, false);
        assert!(d.may_prompt && !d.may_reset, "{d:?}");
    }

    #[test]
    fn granted_never_prompts() {
        for first in [None, Some(EARLIER)] {
            for retry in [false, true] {
                let d = decide(first, retry, Silent::Granted);
                assert!(!d.may_prompt && !d.may_reset, "{first:?} {retry}: {d:?}");
            }
        }
    }

    #[test]
    fn only_a_launchservices_started_app_owns_its_identity() {
        assert!(identity_owned(true, 1));
        assert!(!identity_owned(false, 1), "a bare binary");
        assert!(
            !identity_owned(true, 4242),
            "an .app executable started from a shell"
        );
        assert!(!identity_owned(false, 4242));
        // `cargo test` is a bare binary started by cargo
        assert!(!owns_tcc_identity());
    }

    /// A bare binary keeps its request in memory and never reads or writes the marker file,
    /// so a dev session cannot suppress the `.app`'s one automatic prompt.
    #[test]
    fn a_bare_binary_never_persists_the_marker() {
        assert!(!owns_tcc_identity());
        let before =
            marker_path().and_then(|p| std::fs::metadata(p).ok().map(|m| m.modified().ok()));
        record_request("test-only-key");
        let after =
            marker_path().and_then(|p| std::fs::metadata(p).ok().map(|m| m.modified().ok()));
        assert_eq!(before, after, "a bare binary wrote the request marker");
        assert!(
            first_requested_ms("test-only-key").is_some(),
            "not remembered in memory"
        );
    }

    #[test]
    fn the_request_marker_round_trips() {
        let mut m = BTreeMap::new();
        m.insert(
            KEY_SCREEN_RECORDING.to_string(),
            RequestRecord {
                first_requested_ms: 1,
                last_requested_ms: 2,
                count: 3,
            },
        );
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"screen_recording":{"firstRequestedMs":1,"lastRequestedMs":2,"count":3}}"#
        );
        let back: BTreeMap<String, RequestRecord> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    /// The production half of this file, comment lines removed.
    fn code_lines(src: &str) -> Vec<&str> {
        let end = src.find("#[cfg(test)]\nmod tests").unwrap_or(src.len());
        src[..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect()
    }

    /// The name and signature of the `fn` enclosing line `idx`.
    fn enclosing_fn(lines: &[&str], idx: usize) -> (String, String) {
        for i in (0..=idx).rev() {
            let t = lines[i].trim_start();
            let t = t.strip_prefix("pub ").unwrap_or(t);
            let t = t.strip_prefix("async ").unwrap_or(t);
            if let Some(rest) = t.strip_prefix("fn ") {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                let mut sig = String::new();
                for l in &lines[i..] {
                    sig.push_str(l);
                    if l.contains('{') {
                        break;
                    }
                }
                return (name, sig);
            }
        }
        panic!("no enclosing fn for line {idx}: {}", lines[idx]);
    }

    fn sites(lines: &[&str], needle: &str) -> Vec<(String, String)> {
        lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.contains(needle))
            .map(|(i, _)| enclosing_fn(lines, i))
            .collect()
    }

    #[test]
    fn prompting_calls_sit_behind_the_permit() {
        let src = include_str!("permissions.rs");
        let lines = code_lines(src);

        // exactly three places mint a permit
        let no_decl: Vec<&str> = lines
            .iter()
            .map(|l| {
                if l.contains("struct PromptPermit") {
                    ""
                } else {
                    l
                }
            })
            .collect();
        let mut mints: Vec<String> = sites(&no_decl, "PromptPermit(())")
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        mints.dedup();
        assert_eq!(
            mints,
            [
                "silently_granted_permit",
                "capture_start_permit",
                "prompt_gate"
            ]
        );

        // every prompting call site is inside a fn that demands a permit
        for needle in [
            "\"CGRequestScreenCaptureAccess\"",
            "AXTrustedCheckOptionPrompt",
            "AXIsProcessTrustedWithOptions((",
            "sck::probe_screen_recording(",
        ] {
            let found = sites(&lines, needle);
            assert!(!found.is_empty(), "{needle} not found; update this test");
            for (name, sig) in found {
                assert!(
                    sig.contains("PromptPermit"),
                    "{needle} is reachable from `{name}`, which takes no PromptPermit: {sig}"
                );
            }
        }

        // the periodic entry points never mint one
        for (name, body_must_not) in [
            ("check_permissions", "prompt_gate"),
            ("probe_permissions", "prompt_gate"),
            ("screen_recording_status", "prompt_gate"),
            ("accessibility_status", "prompt_gate"),
            ("screen_recording_live", "prompt_gate"),
            ("screen_recording_silent", "prompt_gate"),
            ("classify_screen_recording", "prompt_gate"),
            ("silent_accessibility", "prompt_gate"),
            ("prime", "prompt_gate"),
        ] {
            let start = lines
                .iter()
                .position(|l| {
                    let t = l.trim_start();
                    t.starts_with(&format!("pub fn {name}("))
                        || t.starts_with(&format!("fn {name}("))
                        || t.starts_with(&format!("pub async fn {name}("))
                })
                .unwrap_or_else(|| panic!("fn {name} not found"));
            let body: String = lines[start..]
                .iter()
                .skip(1)
                .take_while(|l| !l.starts_with('}'))
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            for forbidden in [body_must_not, "PromptPermit(())", "sck_probe("] {
                assert!(
                    !body.contains(forbidden),
                    "periodic path `{name}` reaches `{forbidden}`"
                );
            }
        }
    }

    /// Outside this file, nothing may call a prompting API without a permit: the
    /// SCK probe and the capture-access helpers demand one in their signatures, and
    /// the raw request symbols do not appear at all.
    #[test]
    fn no_other_file_reaches_a_prompting_api() {
        let sck = include_str!("streamer/sck.rs");
        let sck_lines = code_lines(sck);
        for (name, sig) in sites(&sck_lines, "pub fn probe_screen_recording(") {
            assert!(sig.contains("PromptPermit"), "sck::{name}: {sig}");
        }

        let streamer = include_str!("streamer/mod.rs");
        let streamer_lines = code_lines(streamer);
        for (name, sig) in sites(&streamer_lines, "sck::probe_screen_recording(") {
            assert!(sig.contains("PromptPermit"), "streamer::{name}: {sig}");
        }
        for (name, sig) in sites(&streamer_lines, "cgds::probe_screen_recording(") {
            assert!(sig.contains("PromptPermit"), "streamer::{name}: {sig}");
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        let mut offenders = Vec::new();
        let mut gate_callers = Vec::new();
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs")
                    || path.ends_with("macos_utils/permissions.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let lines = code_lines(&text);
                for (name, _) in sites(&lines, "permissions::prompt_gate(") {
                    let file = path.file_name().unwrap().to_string_lossy().to_string();
                    gate_callers.push(format!("{file}::{name}"));
                }
                for line in lines {
                    if line.contains("CGRequestScreenCaptureAccess")
                        || line.contains("AXTrustedCheckOptionPrompt")
                        || line.contains("AXIsProcessTrustedWithOptions")
                    {
                        offenders.push(format!("{}: {}", path.display(), line.trim()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "prompting calls outside the gate:\n{}",
            offenders.join("\n")
        );
        // the deliberate allow-list of `prompt_gate` callers outside this file: `serve` makes
        // the one first Screen Recording request a headless host would otherwise never make
        // (without it the app never appears in the Screen Recording list). Any other outside
        // caller is a new prompting path
        gate_callers.sort();
        assert_eq!(
            gate_callers,
            ["cli.rs::run_serve"],
            "prompt_gate callers outside permissions.rs"
        );
    }
}
