use std::ffi::c_void;

use objc2_core_foundation::CFString;

#[allow(non_camel_case_types)]
type IOPMAssertionID = u32;
type CFStringRef = *const c_void;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionCreateWithName(
        assertion_type: CFStringRef,
        level: u32, // IOPMAssertionLevel
        name: CFStringRef,
        out_id: *mut IOPMAssertionID,
    ) -> i32; // IOReturn; kIOReturnSuccess == 0
    fn IOPMAssertionRelease(id: IOPMAssertionID) -> i32;
}

/// `kIOPMAssertionLevelOn`.
const ASSERTION_LEVEL_ON: u32 = 255;
/// `kIOReturnSuccess`.
const IO_RETURN_SUCCESS: i32 = 0;

const ASSERTION_TYPES: [&str; 2] = [
    "PreventUserIdleSystemSleep",
    "PreventUserIdleDisplaySleep",
];

pub struct KeepAwake {
    ids: Vec<IOPMAssertionID>,
}

unsafe impl Send for KeepAwake {}

impl KeepAwake {
    pub fn begin() -> Self {
        let reason = CFString::from_str("ScreenExtend live capture");
        let mut ids = Vec::new();
        for ty in ASSERTION_TYPES {
            let ty = CFString::from_str(ty);
            let mut id: IOPMAssertionID = 0;
            let rc = unsafe {
                IOPMAssertionCreateWithName(
                    cfstr_ptr(&ty),
                    ASSERTION_LEVEL_ON,
                    cfstr_ptr(&reason),
                    &mut id,
                )
            };
            if rc == IO_RETURN_SUCCESS {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            teprintln!(
                "[power] keep-awake assertions failed; system/display may sleep mid-stream"
            );
        } else {
            tprintln!("[power] keep-awake asserted for session ({} assertion(s))", ids.len());
        }
        KeepAwake { ids }
    }
}

impl Drop for KeepAwake {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            unsafe {
                let _ = IOPMAssertionRelease(id);
            }
        }
    }
}

#[inline]
fn cfstr_ptr(s: &CFString) -> CFStringRef {
    (s as *const CFString).cast()
}
