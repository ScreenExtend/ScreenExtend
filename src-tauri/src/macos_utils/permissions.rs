use std::ffi::{c_void, CString};
use std::process::Command;

use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFString,
};

use crate::PermissionStatus;

type CFDictionaryRef = *const c_void;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
}

pub fn accessibility_trusted() -> bool {
    unsafe { AXIsProcessTrusted() }
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

fn screen_recording_granted() -> bool {
    match cg_bool_fn("CGPreflightScreenCaptureAccess") {
        Some(f) => unsafe { f() },
        None => true,
    }
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
