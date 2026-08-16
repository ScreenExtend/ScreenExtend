use std::ffi::{c_void, CString};
use std::process::Command;
use std::ptr;

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFNumber, CFString,
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
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> *const c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(arr: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(arr: *const c_void, idx: isize) -> *const c_void;
    fn CFStringGetLength(s: *const c_void) -> isize;
    fn CFRelease(cf: *const c_void);
}

unsafe extern "C-unwind" {
    fn CFDictionaryGetValue(the_dict: *const CFDictionary, key: *const c_void) -> *const c_void;
    fn CFNumberGetValue(number: *const CFNumber, the_type: isize, value_ptr: *mut c_void) -> u8;
}

const AX_ERROR_API_DISABLED: i32 = -25211;

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() && accessibility_api_enabled() }
}

fn accessibility_api_enabled() -> bool {
    unsafe {
        let el = AXUIElementCreateSystemWide();
        if el.is_null() {
            return true;
        }
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
            Some(std::mem::transmute::<*mut c_void, unsafe extern "C" fn() -> bool>(p))
        }
    }
}

fn preflight_screen_capture() -> Option<bool> {
    cg_bool_fn("CGPreflightScreenCaptureAccess").map(|f| unsafe { f() })
}

fn screen_recording_granted() -> bool {
    match preflight_screen_capture() {
        None => true,
        Some(false) => false,
        Some(true) => screen_capture_content_visible().unwrap_or(true),
    }
}

fn screen_capture_content_visible() -> Option<bool> {
    const ON_SCREEN_ONLY: u32 = 1; // kCGWindowListOptionOnScreenOnly
    const EXCLUDE_DESKTOP: u32 = 16; // kCGWindowListExcludeDesktopElements
    const CF_NUMBER_SINT32: isize = 3; // kCFNumberSInt32Type

    let key_pid = CFString::from_str("kCGWindowOwnerPID");
    let key_layer = CFString::from_str("kCGWindowLayer");
    let key_name = CFString::from_str("kCGWindowName");
    let our_pid = std::process::id() as i32;

    unsafe {
        let list = CGWindowListCopyWindowInfo(ON_SCREEN_ONLY | EXCLUDE_DESKTOP, 0);
        if list.is_null() {
            return None;
        }
        let count = CFArrayGetCount(list);
        let mut saw_other_window = false;
        let mut visible = false;
        for i in 0..count {
            let dict = CFArrayGetValueAtIndex(list, i);
            if dict.is_null() {
                continue;
            }
            let (Some(pid), Some(layer)) = (
                cf_dict_i32(dict, &key_pid, CF_NUMBER_SINT32),
                cf_dict_i32(dict, &key_layer, CF_NUMBER_SINT32),
            ) else {
                continue;
            };
            if pid == our_pid || layer != 0 {
                continue;
            }
            saw_other_window = true;
            let name = CFDictionaryGetValue(dict.cast(), (&*key_name as *const CFString).cast());
            if !name.is_null() && CFStringGetLength(name) > 0 {
                visible = true;
                break;
            }
        }
        CFRelease(list);
        match (visible, saw_other_window) {
            (true, _) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        }
    }
}

unsafe fn cf_dict_i32(dict: *const c_void, key: &CFString, num_type: isize) -> Option<i32> {
    let v = CFDictionaryGetValue(dict.cast(), (key as *const CFString).cast());
    if v.is_null() {
        return None;
    }
    let mut out: i32 = 0;
    let ok = CFNumberGetValue(v.cast(), num_type, (&mut out as *mut i32).cast());
    (ok != 0).then_some(out)
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
