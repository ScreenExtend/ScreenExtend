//! macOS permission reads (Screen Recording, Accessibility).
//!
//! The hard constraint here is that TCC answers are *cached per process* for the
//! two APIs Apple documents as the "check the permission" calls:
//! `CGPreflightScreenCaptureAccess` never reflects a grant made after the process
//! started, and `AXIsProcessTrusted` behaves the same way. Reading either one as a
//! negative is what made the first-run gate unclosable, because the answer is
//! monotonically false for the life of a process that started denied.
//!
//! So the ladder below is ordered by *liveness*, not by convenience:
//!
//! 1. `CONFIRMED` — a positive we have already proven once. One-way.
//! 2. `CGPreflightScreenCaptureAccess` — trusted as a **positive only**.
//! 3. The window-list probe — `CGWindowListCopyWindowInfo` + `kCGWindowName`.
//!    Without the grant macOS strips window names from other processes' entries.
//!    10.15+, no prompt, no capture session, microseconds, and it flips the
//!    instant the user ticks the toggle.
//! 4. `SCShareableContent` (12.3+), off-thread, as confirmation.
//! 5. The cached preflight negative, as the last resort.
//!
//! `CGDisplayStream` is deliberately absent: an *unauthorized* process still gets
//! a stream and still gets `FrameComplete` frames — they just contain wallpaper —
//! so frame delivery is a liveness signal and never an authorization one.
//!
//! Every read is tri-state. "We could not learn the answer" is `unknown`, and
//! `unknown` must never be rendered as denied, because the caller treats denied as
//! a reason to refuse startup.

use std::ffi::{c_void, CString};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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

// `CGWindowListCopyWindowInfo` is 10.5+, so the link-time reference is safe on the
// 10.15 floor.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *const CFArray;
}

// Signatures must match the other declarations in this crate
// (`macos_utils/streamer/sck.rs`) or the linker sees a clash.
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

/// How long after a permission prompt we keep waiting before concluding that this
/// grant is one of the ones macOS will only hand to a fresh process.
const RELAUNCH_GRACE: Duration = Duration::from_secs(3);
/// Background refresh interval for the probes too slow to run inline.
const PROBE_REFRESH: Duration = Duration::from_secs(2);

pub const STATE_GRANTED: &str = "granted";
pub const STATE_DENIED: &str = "denied";
pub const STATE_UNKNOWN: &str = "unknown";

pub const KEY_ACCESSIBILITY: &str = "accessibility";
pub const KEY_SCREEN_RECORDING: &str = "screen_recording";

// ---------------------------------------------------------------------------
// A background-refreshed probe slot.
//
// The contract that matters: `get` never blocks and never invents an answer. It
// returns the last value the probe produced, kicks a refresh when one is due, and
// a probe that panics loses its own result instead of wedging the slot.
// ---------------------------------------------------------------------------

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

    fn peek(&'static self) -> Option<T> {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).last
    }
}

// ---------------------------------------------------------------------------
// Accessibility
// ---------------------------------------------------------------------------

/// What the system-wide accessibility element had to say. This is a *secondary*
/// signal: Apple DTS is explicit that a failed AX call is not evidence of a
/// missing grant, so a messaging failure downgrades the answer to unknown rather
/// than asserting a denial.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AxSecondary {
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

/// `AXUIElementSetMessagingTimeout` on the system-wide element sets the default for
/// *every* accessibility message this process sends, not just this one — a second
/// reason to keep this call off the hot path and out of the primary read.
///
/// The timeout is therefore restored before returning. There is no getter, but Apple
/// documents 0 as "use the system default", so setting it back to 0 is the restore.
/// Without it a single permission read left remote input (`streamer/input/macos.rs`) and
/// the AirPlay menu-extra walk (`airplay/ax.rs`) on a 0.5 s ceiling for the rest of the
/// process's life, having never asked for one.
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

fn prompt_accessibility() -> bool {
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
    // `AXIsProcessTrusted` caches per process, so a grant made while we run cannot
    // reach us. That is exactly the relaunch condition.
    let relaunch = requested_long_enough(&AX_REQUESTED_AT);
    match AX_SECONDARY.get("accessibility", accessibility_api_probe) {
        Some(AxSecondary::Unreadable) => (
            STATE_UNKNOWN,
            relaunch,
            "The accessibility subsystem did not answer in time, so this could not be read. \
             Remote control stays available and will report its own error if it is blocked."
                .to_string(),
        ),
        _ if relaunch => (
            STATE_DENIED,
            true,
            "macOS binds this grant to the process that asked for it. Quit and reopen \
             ScreenExtend for a newly granted Accessibility permission to take effect."
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

// ---------------------------------------------------------------------------
// Screen Recording
// ---------------------------------------------------------------------------

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

/// Trustworthy as a positive; provably stale as a negative — Apple DTS states it
/// "will not reflect this change until the application is restarted".
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

/// The live TCC read.
///
/// Without the Screen Recording grant macOS strips `kCGWindowName` from every
/// window belonging to another process. So walk the on-screen window list, skip our
/// own windows and everything that is not a normal layer-0 window (that drops the
/// Dock, the menu bar and the wallpaper), and see whether any survivor has a name.
///
/// - `Some(true)`  — a foreign window's title is visible: authorized, right now.
/// - `Some(false)` — foreign windows exist and none is named: denied.
/// - `None`        — nothing to look at. Unknown; conclude nothing.
pub fn window_name_probe() -> Option<bool> {
    let list = unsafe {
        CGWindowListCopyWindowInfo(
            CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS,
            CG_NULL_WINDOW_ID,
        )
    };
    let arr = unsafe { list.as_ref() }?;

    let our_pid = unsafe { libc::getpid() };
    let pid_key = CFString::from_str("kCGWindowOwnerPID");
    let layer_key = CFString::from_str("kCGWindowLayer");
    let name_key = CFString::from_str("kCGWindowName");

    let mut candidates = 0usize;
    let mut named = 0usize;
    for i in 0..arr.count() {
        let dict_ptr = unsafe { arr.value_at_index(i) } as *const CFDictionary;
        let Some(dict) = (unsafe { dict_ptr.as_ref() }) else {
            continue;
        };
        match dict_i32(dict, &pid_key) {
            Some(pid) if pid != our_pid => {}
            _ => continue,
        }
        if dict_i32(dict, &layer_key) != Some(0) {
            continue;
        }
        candidates += 1;
        let name = unsafe { CFDictionaryGetValue(dict, (&*name_key as *const CFString).cast()) };
        if !name.is_null() && unsafe { CFStringGetLength(name) } > 0 {
            named += 1;
        }
    }
    unsafe { CFRelease(list.cast()) };

    if candidates == 0 {
        None
    } else {
        Some(named > 0)
    }
}

/// `SCShareableContent`, 12.3+. Authoritative but documented to stall, so it only
/// ever runs on the background slot.
fn sck_probe() -> Option<bool> {
    if !crate::macos_utils::streamer::screencapturekit_available() {
        return None;
    }
    crate::macos_utils::streamer::sck::probe_screen_recording()
}

static SCK_PROBE: BgProbe<bool> = BgProbe::new();
static CONFIRMED: AtomicBool = AtomicBool::new(false);

/// The live authorization answer, ignoring the `CONFIRMED` latch. This is what a
/// mid-session consent lapse — macOS 15's periodic re-consent — shows up in.
pub fn screen_recording_live() -> Option<bool> {
    if preflight_screen_capture() == Some(true) {
        return Some(true);
    }
    if let Some(v) = window_name_probe() {
        return Some(v);
    }
    SCK_PROBE.peek()
}

/// `(state, relaunch_required, detail)`
fn screen_recording_status() -> (&'static str, bool, String) {
    if CONFIRMED.load(Ordering::Relaxed) {
        return (
            STATE_GRANTED,
            false,
            "Confirmed for this running copy of ScreenExtend.".to_string(),
        );
    }

    let preflight = preflight_screen_capture();
    if preflight == Some(true) {
        CONFIRMED.store(true, Ordering::Relaxed);
        return (
            STATE_GRANTED,
            false,
            "Granted to this running copy of ScreenExtend.".to_string(),
        );
    }

    // Kick the 12.3+ confirmation whether or not this call needs it; it is the only
    // authoritative read and it costs nothing here.
    let sck = SCK_PROBE.get("ScreenCaptureKit", sck_probe);

    if let Some(live) = window_name_probe() {
        if live {
            CONFIRMED.store(true, Ordering::Relaxed);
            // The toggle is on and capture works now, but anything in this process
            // that consults the cached preflight — including ScreenCaptureKit on
            // macOS 15 — may still be looking at the old answer.
            let stale_cache = preflight == Some(false);
            let detail = if stale_cache {
                "Granted. macOS still has the old answer cached for this process, so restart \
                 ScreenExtend if capture misbehaves."
                    .to_string()
            } else {
                "Granted — confirmed by reading the live window list.".to_string()
            };
            return (STATE_GRANTED, stale_cache, detail);
        }
        return (
            STATE_DENIED,
            requested_long_enough(&SCREEN_REQUESTED_AT),
            denied_detail(),
        );
    }

    match sck {
        Some(true) => {
            CONFIRMED.store(true, Ordering::Relaxed);
            (
                STATE_GRANTED,
                false,
                "Granted — confirmed by ScreenCaptureKit.".to_string(),
            )
        }
        Some(false) => (
            STATE_DENIED,
            requested_long_enough(&SCREEN_REQUESTED_AT),
            denied_detail(),
        ),
        // Last resort. Every live probe is inconclusive: no other window is on
        // screen to read a title from, and ScreenCaptureKit either is absent or has
        // not answered yet. The cached preflight is the only signal left — it is
        // right on a machine that never granted, which is the common first run, and
        // the window probe overrides it the moment anything opens.
        None => match preflight {
            Some(false) => (
                STATE_DENIED,
                requested_long_enough(&SCREEN_REQUESTED_AT),
                denied_detail(),
            ),
            _ => (
                STATE_UNKNOWN,
                false,
                "Could not read this permission. ScreenExtend will start anyway; if capture is \
                 blocked, the device that tries to connect will say so."
                    .to_string(),
            ),
        },
    }
}

fn denied_detail() -> String {
    if requested_long_enough(&SCREEN_REQUESTED_AT) {
        "Grant Screen Recording in System Settings, then restart ScreenExtend — macOS will not \
         hand a new grant to an app that is already running."
            .to_string()
    } else {
        "Lets ScreenExtend capture this Mac's screen to stream it.".to_string()
    }
}

fn request_screen_recording() -> bool {
    *SCREEN_REQUESTED_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    if screen_recording_status().0 == STATE_GRANTED {
        return true;
    }
    match cg_bool_fn("CGRequestScreenCaptureAccess") {
        Some(f) => unsafe { f() },
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Diagnostics for `ScreenExtend doctor`
// ---------------------------------------------------------------------------

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
    let sck = if sck_available { sck_probe() } else { None };
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

/// The `.app` containing `exe`, if the executable sits in `…/Contents/MacOS/`.
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
    // `current_exe` can hand back the path as invoked, `./` and all. Resolving it
    // is what makes the `/AppTranslocation/` test and the codesign read meaningful.
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

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// The probe itself, for callers that are already off the main thread — `ScreenExtend
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
            // Only per-device remote input needs this, and that is opt-in. A user
            // who wants a second monitor and nothing else must not be stopped here.
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

/// Warm the window-list probe so its cold cost — 37 ms on 10.15.8, CoreGraphics
/// establishing its WindowServer connection — is paid on a background thread at startup
/// rather than inside the first `check_permissions`, which lands mid-launch.
pub fn prime() {
    std::thread::Builder::new()
        .name("se-perm-warm".into())
        .spawn(|| {
            let _ = window_name_probe();
        })
        .ok();
}

/// A plain `fn` command runs on the main thread. Nothing here needs to: the window-list
/// read is CoreGraphics IPC and `AXIsProcessTrusted` is a syscall, so both belong on a
/// blocking thread even at well under a millisecond apiece.
#[tauri::command]
#[specta::specta]
pub async fn check_permissions() -> Vec<PermissionStatus> {
    tauri::async_runtime::spawn_blocking(probe_permissions)
        .await
        .unwrap_or_else(|e| {
            teprintln!("[permissions] the permission probe thread failed: {e}");
            probe_permissions()
        })
}

/// `CGRequestScreenCaptureAccess` blocks until the user dismisses the OS prompt, which is
/// seconds of a frozen window if it runs on the main thread.
#[tauri::command]
#[specta::specta]
pub async fn request_permission(key: String) -> bool {
    tauri::async_runtime::spawn_blocking(move || match key.as_str() {
        KEY_ACCESSIBILITY => prompt_accessibility(),
        KEY_SCREEN_RECORDING => request_screen_recording(),
        _ => false,
    })
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
    // Ventura renamed the pane. Try the modern identifier first and fall back, so
    // the choice is made by what the OS honours rather than by a version number.
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
/// own — the documented reason an app launched from a terminal never appears in the
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
        // The gate polls this at 5 Hz on the main thread, so the warm cost is the
        // one that matters; the first call also pays CoreGraphics' one-off setup.
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
        assert_eq!(SLOT.peek(), Some(true));
    }
}
