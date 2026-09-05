use std::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicPtr, Ordering};

use objc2::runtime::AnyObject;
use objc2::{msg_send, sel};

type Preprocessor = unsafe extern "C" fn(*mut AnyObject) -> *mut AnyObject;
type SetPreprocessor = unsafe extern "C" fn(Preprocessor) -> Option<Preprocessor>;

static PREVIOUS: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

unsafe fn describe(value: *mut AnyObject) -> String {
    if value.is_null() {
        return String::new();
    }
    let utf8: *const c_char = msg_send![value, UTF8String];
    if utf8.is_null() {
        return String::new();
    }
    CStr::from_ptr(utf8).to_string_lossy().into_owned()
}

unsafe extern "C" fn report(exception: *mut AnyObject) -> *mut AnyObject {
    if exception.is_null() {
        return exception;
    }

    let has_name: bool = msg_send![exception, respondsToSelector: sel!(name)];
    let has_reason: bool = msg_send![exception, respondsToSelector: sel!(reason)];
    let name: *mut AnyObject = if has_name {
        msg_send![exception, name]
    } else {
        std::ptr::null_mut()
    };
    let reason: *mut AnyObject = if has_reason {
        msg_send![exception, reason]
    } else {
        std::ptr::null_mut()
    };

    eprintln!(
        "[objc] exception raised: {} — {}",
        describe(name),
        describe(reason)
    );

    let has_stack: bool = msg_send![exception, respondsToSelector: sel!(callStackSymbols)];
    if has_stack {
        let symbols: *mut AnyObject = msg_send![exception, callStackSymbols];
        if !symbols.is_null() {
            let count: usize = msg_send![symbols, count];
            for i in 0..count.min(40) {
                let frame: *mut AnyObject = msg_send![symbols, objectAtIndex: i];
                eprintln!("[objc]   {}", describe(frame));
            }
        }
    }

    let previous = PREVIOUS.load(Ordering::Relaxed);
    if !previous.is_null() {
        let previous: Preprocessor = std::mem::transmute(previous);
        return previous(exception);
    }
    exception
}

pub fn install_logger() {
    let address = unsafe {
        libc::dlsym(
            libc::RTLD_DEFAULT,
            c"objc_setExceptionPreprocessor".as_ptr(),
        )
    };
    if address.is_null() {
        return;
    }
    let set: SetPreprocessor = unsafe { std::mem::transmute(address) };
    if let Some(previous) = unsafe { set(report) } {
        PREVIOUS.store(previous as *const () as *mut c_void, Ordering::Relaxed);
    }
}
