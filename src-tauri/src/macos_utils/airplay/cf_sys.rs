//! One declaration each for the CoreFoundation symbols this module uses.
//!
//! Rust has no header, so every `extern "C"` block is an independent claim
//! about a symbol's type, and the compiler only compares two claims when they
//! are in the same crate *and* it happens to notice. `ax.rs` carries a comment
//! about exactly that hazard — its `CFBooleanGetValue` must keep matching the
//! one in `macos_utils/streamer/encoder.rs` or the linker rejects the crate —
//! and the two files in this module had drifted into declaring
//! `CFPreferencesCopyAppValue`, `CFRelease` and `CFGetTypeID` twice apiece.
//!
//! So they are declared here once and imported. `CFTypeRef` is deliberately a
//! single opaque pointer type rather than the family of `CFStringRef` /
//! `CFArrayRef` aliases: the aliases were all `*const c_void` anyway, and
//! having one of them made the duplicate declarations look different when they
//! were not.
//!
//! Everything here is 10.0-era. Nothing in this file can add a dyld symbol the
//! 10.15 floor does not have.

use std::ffi::c_void;

pub type CFTypeRef = *const c_void;
pub type CFTypeID = usize;

/// `kCFStringEncodingUTF8`.
pub const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    pub fn CFRelease(cf: CFTypeRef);
    pub fn CFRetain(cf: CFTypeRef) -> CFTypeRef;
    pub fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;

    pub fn CFStringGetTypeID() -> CFTypeID;
    pub fn CFArrayGetTypeID() -> CFTypeID;
    pub fn CFBooleanGetTypeID() -> CFTypeID;
    pub fn CFNumberGetTypeID() -> CFTypeID;

    pub fn CFStringCreateWithCString(alloc: CFTypeRef, cstr: *const i8, encoding: u32)
        -> CFTypeRef;
    pub fn CFStringGetCString(
        s: CFTypeRef,
        buffer: *mut i8,
        buffer_size: isize,
        encoding: u32,
    ) -> bool;

    pub fn CFArrayGetCount(array: CFTypeRef) -> isize;
    pub fn CFArrayGetValueAtIndex(array: CFTypeRef, idx: isize) -> CFTypeRef;

    pub fn CFURLCreateWithFileSystemPath(
        allocator: CFTypeRef,
        path: CFTypeRef,
        path_style: isize,
        is_directory: bool,
    ) -> CFTypeRef;
    pub fn CFBundleCreate(allocator: CFTypeRef, url: CFTypeRef) -> CFTypeRef;
    pub fn CFBundleCopyLocalizedString(
        bundle: CFTypeRef,
        key: CFTypeRef,
        value: CFTypeRef,
        table: CFTypeRef,
    ) -> CFTypeRef;

    pub fn CFPreferencesCopyAppValue(key: CFTypeRef, application_id: CFTypeRef) -> CFTypeRef;
    pub fn CFPreferencesSetAppValue(key: CFTypeRef, value: CFTypeRef, application_id: CFTypeRef);
    pub fn CFPreferencesAppSynchronize(application_id: CFTypeRef) -> bool;

    /// Returns a `Boolean`, i.e. `unsigned char`. Declaring it as Rust's `bool`
    /// is the mistake to avoid: the two agree in practice and the signature is
    /// what a second declaration elsewhere in the crate has to match.
    pub fn CFBooleanGetValue(boolean: CFTypeRef) -> u8;

    #[link_name = "kCFBooleanTrue"]
    pub static K_CF_BOOLEAN_TRUE: CFTypeRef;
    #[link_name = "kCFBooleanFalse"]
    pub static K_CF_BOOLEAN_FALSE: CFTypeRef;
}

/// An owned CFType, released on drop.
///
/// The one CF ownership rule that matters here: anything from a `Create` or
/// `Copy` call is owned and must be released; anything from a `Get` call is
/// borrowed and must not be.
pub struct CfOwned(pub CFTypeRef);

impl Drop for CfOwned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// Takes ownership of a borrowed reference so it can outlive its container.
pub fn retained(cf: CFTypeRef) -> CfOwned {
    unsafe { CFRetain(cf) };
    CfOwned(cf)
}

/// A UTF-8 Rust string as an owned `CFString`.
pub fn cf_string(s: &str) -> Option<CfOwned> {
    let c = std::ffi::CString::new(s).ok()?;
    let r = unsafe {
        CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8)
    };
    if r.is_null() {
        None
    } else {
        Some(CfOwned(r))
    }
}

/// A `CFString` back to a Rust `String`, type-checked.
pub fn cf_string_to_string(s: CFTypeRef) -> Option<String> {
    if s.is_null() || unsafe { CFGetTypeID(s) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let mut buf = vec![0i8; 1024];
    let ok = unsafe {
        CFStringGetCString(
            s,
            buf.as_mut_ptr(),
            buf.len() as isize,
            K_CF_STRING_ENCODING_UTF8,
        )
    };
    if !ok {
        return None;
    }
    unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_str()
        .ok()
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_round_trip() {
        let s = cf_string("AXChildren").expect("create");
        assert_eq!(cf_string_to_string(s.0).as_deref(), Some("AXChildren"));
    }

    #[test]
    fn a_nul_is_rejected_rather_than_truncating() {
        assert!(cf_string("bad\0name").is_none());
    }

    #[test]
    fn a_non_string_converts_to_none() {
        assert_eq!(cf_string_to_string(std::ptr::null()), None);
        let b = unsafe { K_CF_BOOLEAN_TRUE };
        assert_eq!(cf_string_to_string(b), None);
    }

    #[test]
    fn the_boolean_constants_are_distinguishable() {
        unsafe {
            assert_eq!(CFGetTypeID(K_CF_BOOLEAN_TRUE), CFBooleanGetTypeID());
            assert_ne!(CFBooleanGetValue(K_CF_BOOLEAN_TRUE), 0);
            assert_eq!(CFBooleanGetValue(K_CF_BOOLEAN_FALSE), 0);
        }
    }

    #[test]
    fn retaining_keeps_a_value_alive_past_its_container() {
        let s = cf_string("hold me").expect("create");
        let held = retained(s.0);
        drop(s);
        assert_eq!(cf_string_to_string(held.0).as_deref(), Some("hold me"));
    }
}
