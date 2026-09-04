use std::ffi::{c_void, CString};
use std::process::Command;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFString,
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
}

const AX_ERROR_API_DISABLED: i32 = -25211;
const AX_MESSAGING_TIMEOUT_SECS: f32 = 0.5;
const PROBE_REFRESH: Duration = Duration::from_secs(2);

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() && accessibility_api_enabled() }
}

fn accessibility_api_enabled() -> bool {
    unsafe {
        let el = AXUIElementCreateSystemWide();
        if el.is_null() {
            return true;
        }
        AXUIElementSetMessagingTimeout(el, AX_MESSAGING_TIMEOUT_SECS);
        let attr = CFString::from_str("AXFocusedApplication");
        let mut value: *const c_void = ptr::null();
        let err = AXUIElementCopyAttributeValue(el, (&*attr as *const CFString).cast(), &mut value);
        if !value.is_null() {
            CFRelease(value);
        }
        CFRelease(el);
        err != AX_ERROR_API_DISABLED
    }
}

fn prompt_accessibility() -> bool {
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

fn preflight_screen_capture() -> Option<bool> {
    cg_bool_fn("CGPreflightScreenCaptureAccess").map(|f| unsafe { f() })
}

struct ProbeState {
    started: bool,
    running: bool,
    last: Option<Instant>,
}

static CONFIRMED: AtomicBool = AtomicBool::new(false);
static PROBE: Mutex<ProbeState> = Mutex::new(ProbeState {
    started: false,
    running: false,
    last: None,
});

fn probe_and_latch() -> bool {
    let granted = crate::macos_utils::streamer::probe_screen_recording();
    if granted {
        CONFIRMED.store(true, Ordering::Relaxed);
    }
    granted
}

fn screen_recording_granted() -> bool {
    if CONFIRMED.load(Ordering::Relaxed) {
        return true;
    }
    if preflight_screen_capture() == Some(false) {
        return false;
    }
    let first = {
        let mut st = PROBE.lock().unwrap();
        if st.started {
            let due = match st.last {
                Some(at) => at.elapsed() >= PROBE_REFRESH,
                None => true,
            };
            if due && !st.running {
                st.running = true;
                std::thread::spawn(|| {
                    probe_and_latch();
                    let mut st = PROBE.lock().unwrap();
                    st.running = false;
                    st.last = Some(Instant::now());
                });
            }
            false
        } else {
            st.started = true;
            true
        }
    };
    if !first {
        return false;
    }
    let granted = probe_and_latch();
    PROBE.lock().unwrap().last = Some(Instant::now());
    granted
}

fn request_screen_recording() -> bool {
    if screen_recording_granted() {
        return true;
    }
    match cg_bool_fn("CGRequestScreenCaptureAccess") {
        Some(f) => unsafe { f() },
        None => false,
    }
}

#[tauri::command]
#[specta::specta]
pub fn check_permissions() -> Vec<PermissionStatus> {
    vec![
        PermissionStatus {
            key: "accessibility".to_string(),
            name: "Accessibility".to_string(),
            description: "Lets connected devices control this Mac's keyboard and mouse."
                .to_string(),
            granted: accessibility_trusted(),
            required: true,
        },
        PermissionStatus {
            key: "screen_recording".to_string(),
            name: "Screen Recording".to_string(),
            description: "Lets ScreenExtend capture this Mac's screen to stream it.".to_string(),
            granted: screen_recording_granted(),
            required: true,
        },
    ]
}

#[tauri::command]
#[specta::specta]
pub fn request_permission(key: String) -> bool {
    match key.as_str() {
        "accessibility" => prompt_accessibility(),
        "screen_recording" => request_screen_recording(),
        _ => false,
    }
}

#[tauri::command]
#[specta::specta]
pub fn open_permission_settings(key: String) {
    let url = match key.as_str() {
        "accessibility" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
        }
        "screen_recording" => {
            "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        }
        _ => "x-apple.systempreferences:com.apple.preference.security?Privacy",
    };
    if let Err(e) = Command::new("open").arg(url).spawn() {
        teprintln!("[permissions] failed to open System Settings ({key}): {e}");
    }
}
