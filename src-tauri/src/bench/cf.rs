//! The small amount of CoreFoundation / CoreVideo / CoreMedia / mach FFI the bench needs.
//!
//! Every function declared here exists on macOS 10.15 (most since 10.4–10.9); anything newer is
//! `dlsym`'d. `scripts/build-bench-macos.sh` diffs the undefined symbols this module adds.

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::ptr::NonNull;

use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
    CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, CVPixelBuffer,
};

use crate::macos_utils::streamer::vt_recipe::{cf_to_json, cfstring_symbol, dlsym_ptr};

pub const PIXEL_420F: u32 = 0x3432_3066; // '420f'

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferPoolCreate(
        allocator: *const c_void,
        pool_attributes: *const c_void,
        pixel_buffer_attributes: *const c_void,
        pool_out: *mut *mut c_void,
    ) -> i32;
    fn CVPixelBufferPoolCreatePixelBuffer(
        allocator: *const c_void,
        pool: *mut c_void,
        out: *mut *mut CVPixelBuffer,
    ) -> i32;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSampleBufferGetTotalSampleSize(sb: *const c_void) -> usize;
    fn CMSampleBufferGetSampleAttachmentsArray(sb: *const c_void, create: u8) -> *const c_void;
    fn CMSampleBufferGetFormatDescription(sb: *const c_void) -> *const c_void;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        desc: *const c_void,
        index: usize,
        ptr_out: *mut *const u8,
        size_out: *mut usize,
        count_out: *mut usize,
        nal_header_len_out: *mut i32,
    ) -> i32;
    static kCMSampleAttachmentKey_NotSync: *const c_void;
}

unsafe extern "C-unwind" {
    // Same signature as the other declarations in the crate, or rustc warns about the clash.
    fn CFDictionaryGetValue(the_dict: *const CFDictionary, key: *const c_void) -> *const c_void;
}

unsafe extern "C" {
    fn CFArrayGetCount(a: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(a: *const c_void, i: isize) -> *const c_void;
    fn CFBooleanGetValue(b: *const c_void) -> u8;
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFBooleanGetTypeID() -> usize;
}

pub fn dict(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary> {
    let mut k: Vec<*const c_void> = pairs
        .iter()
        .map(|(k, _)| (*k as *const CFString).cast())
        .collect();
    let mut v: Vec<*const c_void> = pairs
        .iter()
        .map(|(_, v)| (*v as *const CFType).cast())
        .collect();
    unsafe {
        CFDictionary::new(
            kCFAllocatorDefault,
            k.as_mut_ptr(),
            v.as_mut_ptr(),
            k.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .expect("CFDictionaryCreate")
}

/// An IOSurface-backed `420f` pool, the same kind of buffer production capture hands VideoToolbox.
pub struct Pool {
    pool: *mut c_void,
}

unsafe impl Send for Pool {}

impl Pool {
    pub fn new(width: usize, height: usize, min_buffers: usize) -> Result<Pool, i32> {
        let empty = dict(&[]);
        let fmt = CFNumber::new_i32(PIXEL_420F as i32);
        let w = CFNumber::new_i64(width as i64);
        let h = CFNumber::new_i64(height as i64);
        let pb_attrs = unsafe {
            dict(&[
                (kCVPixelBufferPixelFormatTypeKey, &fmt),
                (kCVPixelBufferWidthKey, &w),
                (kCVPixelBufferHeightKey, &h),
                (kCVPixelBufferIOSurfacePropertiesKey, &empty),
            ])
        };
        let count = CFNumber::new_i64(min_buffers as i64);
        let pool_attrs = cfstring_symbol("kCVPixelBufferPoolMinimumBufferCountKey")
            .map(|k| dict(&[(k, &count)]));
        let mut pool: *mut c_void = ptr::null_mut();
        let st = unsafe {
            CVPixelBufferPoolCreate(
                ptr::null(),
                pool_attrs.as_ref().map_or(ptr::null(), |d| {
                    CFRetained::as_ptr(d).as_ptr().cast_const().cast()
                }),
                CFRetained::as_ptr(&pb_attrs).as_ptr().cast_const().cast(),
                &mut pool,
            )
        };
        if st != 0 || pool.is_null() {
            return Err(st);
        }
        Ok(Pool { pool })
    }

    pub fn get(&self) -> Result<CFRetained<CVPixelBuffer>, i32> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let st = unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), self.pool, &mut out) };
        match NonNull::new(out) {
            Some(p) if st == 0 => Ok(unsafe { CFRetained::from_raw(p) }),
            _ => Err(st),
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        unsafe extern "C" {
            fn CFRelease(cf: *const c_void);
        }
        unsafe { CFRelease(self.pool) };
    }
}

pub fn sb_ptr(sb: &CMSampleBuffer) -> *const c_void {
    (sb as *const CMSampleBuffer).cast()
}

pub fn sample_size(sb: &CMSampleBuffer) -> usize {
    unsafe { CMSampleBufferGetTotalSampleSize(sb_ptr(sb)) }
}

/// A sync sample (IDR / IRAP): `kCMSampleAttachmentKey_NotSync` absent or false.
pub fn is_keyframe(sb: &CMSampleBuffer) -> bool {
    unsafe {
        let arr = CMSampleBufferGetSampleAttachmentsArray(sb_ptr(sb), 0);
        if arr.is_null() || CFArrayGetCount(arr) < 1 {
            return true;
        }
        let d = CFArrayGetValueAtIndex(arr, 0);
        if d.is_null() {
            return true;
        }
        let v = CFDictionaryGetValue(d.cast(), kCMSampleAttachmentKey_NotSync);
        if v.is_null() {
            return true;
        }
        if CFGetTypeID(v) == CFBooleanGetTypeID() {
            return CFBooleanGetValue(v) == 0;
        }
        false
    }
}

pub fn format_description(sb: &CMSampleBuffer) -> *const c_void {
    unsafe { CMSampleBufferGetFormatDescription(sb_ptr(sb)) }
}

/// Parameter sets from the format description: H.264 SPS/PPS, or HEVC VPS/SPS/PPS through the
/// `dlsym`'d HEVC getter.
pub fn parameter_sets(sb: &CMSampleBuffer, hevc: bool) -> Vec<Vec<u8>> {
    type Getter = unsafe extern "C" fn(
        *const c_void,
        usize,
        *mut *const u8,
        *mut usize,
        *mut usize,
        *mut i32,
    ) -> i32;
    let desc = format_description(sb);
    if desc.is_null() {
        return Vec::new();
    }
    let getter: Getter = if hevc {
        match dlsym_ptr("CMVideoFormatDescriptionGetHEVCParameterSetAtIndex") {
            Some(p) => unsafe { std::mem::transmute::<*mut c_void, Getter>(p) },
            None => return Vec::new(),
        }
    } else {
        CMVideoFormatDescriptionGetH264ParameterSetAtIndex
    };
    let mut count = 0usize;
    unsafe {
        getter(
            desc,
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut count,
            ptr::null_mut(),
        );
    }
    let mut out = Vec::new();
    for i in 0..count {
        let mut p: *const u8 = ptr::null();
        let mut len = 0usize;
        let st = unsafe { getter(desc, i, &mut p, &mut len, ptr::null_mut(), ptr::null_mut()) };
        if st == 0 && !p.is_null() {
            out.push(unsafe { std::slice::from_raw_parts(p, len) }.to_vec());
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Process / system facts.
// ---------------------------------------------------------------------------------------------

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let c = CString::new(name).ok()?;
    let mut len = 0usize;
    if unsafe { libc::sysctlbyname(c.as_ptr(), ptr::null_mut(), &mut len, ptr::null_mut(), 0) } != 0
    {
        return None;
    }
    let mut buf = vec![0u8; len];
    if unsafe {
        libc::sysctlbyname(
            c.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            ptr::null_mut(),
            0,
        )
    } != 0
    {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

pub fn sysctl_string(name: &str) -> Option<String> {
    let b = sysctl_raw(name)?;
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    Some(String::from_utf8_lossy(&b[..end]).trim().to_string())
}

pub fn sysctl_int(name: &str) -> Option<i64> {
    let b = sysctl_raw(name)?;
    match b.len() {
        4 => Some(i32::from_ne_bytes(b[..4].try_into().ok()?) as i64),
        8 => Some(i64::from_ne_bytes(b[..8].try_into().ok()?)),
        _ => None,
    }
}

#[repr(C, packed(4))]
#[derive(Default)]
struct MachTaskBasicInfo {
    virtual_size: u64,
    resident_size: u64,
    resident_size_max: u64,
    user_time: [i32; 2],
    system_time: [i32; 2],
    policy: i32,
    suspend_count: i32,
}

/// (resident, peak resident) in bytes, from `task_info(MACH_TASK_BASIC_INFO)`.
pub fn rss() -> (u64, u64) {
    let mut info = MachTaskBasicInfo::default();
    let mut count = (std::mem::size_of::<MachTaskBasicInfo>() / 4) as u32;
    let rc = unsafe {
        mach2::task::task_info(
            mach2::traps::mach_task_self(),
            mach2::task_info::MACH_TASK_BASIC_INFO,
            (&mut info as *mut MachTaskBasicInfo).cast(),
            &mut count,
        )
    };
    if rc != 0 {
        return (0, 0);
    }
    (info.resident_size, info.resident_size_max)
}

/// `NSProcessInfo.thermalState`: 0 nominal, 1 fair, 2 serious, 3 critical.
pub fn thermal_state() -> Option<i64> {
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{msg_send, sel};
    let cls = AnyClass::get(c"NSProcessInfo")?;
    unsafe {
        let pi: *mut AnyObject = msg_send![cls, processInfo];
        let pi = pi.as_ref()?;
        let responds: Bool = msg_send![pi, respondsToSelector: sel!(thermalState)];
        if !responds.as_bool() {
            return None;
        }
        let s: isize = msg_send![pi, thermalState];
        Some(s as i64)
    }
}

/// `NSProcessInfo.isLowPowerModeEnabled` where the selector exists (it does not on 10.15).
pub fn low_power_mode() -> Option<bool> {
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2::{msg_send, sel};
    let cls = AnyClass::get(c"NSProcessInfo")?;
    unsafe {
        let pi: *mut AnyObject = msg_send![cls, processInfo];
        let pi = pi.as_ref()?;
        let responds: Bool = msg_send![pi, respondsToSelector: sel!(isLowPowerModeEnabled)];
        if !responds.as_bool() {
            return None;
        }
        let b: Bool = msg_send![pi, isLowPowerModeEnabled];
        Some(b.as_bool())
    }
}

pub fn hostname() -> String {
    let mut buf = [0 as c_char; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } != 0 {
        return "unknown-host".into();
    }
    let s = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    let short = s.split('.').next().unwrap_or("host").to_string();
    short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Local time as `YYYYMMDD-HHMMSS`.
pub fn local_stamp() -> String {
    let t = unsafe { libc::time(ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub fn free_disk_bytes(path: &std::path::Path) -> Option<u64> {
    let c = CString::new(path.as_os_str().to_string_lossy().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// `VTCopyVideoEncoderList` as JSON (dlsym'd).
pub fn video_encoder_list() -> serde_json::Value {
    type F = unsafe extern "C" fn(*const c_void, *mut *const c_void) -> i32;
    let Some(p) = dlsym_ptr("VTCopyVideoEncoderList") else {
        return serde_json::json!({ "error": "VTCopyVideoEncoderList missing" });
    };
    let f: F = unsafe { std::mem::transmute::<*mut c_void, F>(p) };
    let mut out: *const c_void = ptr::null();
    let st = unsafe { f(ptr::null(), &mut out) };
    if st != 0 || out.is_null() {
        return serde_json::json!({ "error": format!("OSStatus {st}") });
    }
    let v = unsafe { CFRetained::from_raw(NonNull::new(out as *mut CFType).unwrap()) };
    cf_to_json(&v)
}

// ---------------------------------------------------------------------------------------------
// Resource counts for leak checks (all from 10.0-era mach / POSIX).
// ---------------------------------------------------------------------------------------------

unsafe extern "C" {
    fn mach_port_names(
        task: u32,
        names: *mut *mut u32,
        names_cnt: *mut u32,
        types: *mut *mut u32,
        types_cnt: *mut u32,
    ) -> i32;
    fn vm_deallocate(task: u32, address: usize, size: usize) -> i32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
}

/// Open file descriptors, by listing `/dev/fd` (the listing's own descriptor excluded).
pub fn fd_count() -> Option<usize> {
    let n = std::fs::read_dir("/dev/fd").ok()?.count();
    Some(n.saturating_sub(1))
}

/// Mach port names held by this task (`mach_port_names`).
pub fn mach_port_count() -> Option<usize> {
    let task = unsafe { mach2::traps::mach_task_self() };
    let (mut names, mut ncnt, mut types, mut tcnt) =
        (ptr::null_mut::<u32>(), 0u32, ptr::null_mut::<u32>(), 0u32);
    let rc = unsafe { mach_port_names(task, &mut names, &mut ncnt, &mut types, &mut tcnt) };
    if rc != 0 {
        return None;
    }
    unsafe {
        if !names.is_null() {
            vm_deallocate(task, names as usize, ncnt as usize * 4);
        }
        if !types.is_null() {
            vm_deallocate(task, types as usize, tcnt as usize * 4);
        }
    }
    Some(ncnt as usize)
}

/// Threads in this task (`task_threads`; the returned thread ports are released).
pub fn thread_count() -> Option<usize> {
    let task = unsafe { mach2::traps::mach_task_self() };
    let mut list: mach2::mach_types::thread_act_array_t = ptr::null_mut();
    let mut cnt = 0u32;
    if unsafe { mach2::task::task_threads(task, &mut list, &mut cnt) } != 0 {
        return None;
    }
    unsafe {
        for i in 0..cnt as usize {
            mach_port_deallocate(task, *list.add(i));
        }
        vm_deallocate(task, list as usize, cnt as usize * 4);
    }
    Some(cnt as usize)
}

/// `{rss_mb, fds, mach_ports, threads}` right now.
pub fn resources() -> serde_json::Value {
    serde_json::json!({
        "rss_mb": (rss().0 as f64 / 1e5).round() / 10.0,
        "fds": fd_count(),
        "mach_ports": mach_port_count(),
        "threads": thread_count(),
    })
}

/// The first sample-attachment dictionary of a sample (borrowed), or null.
fn first_attachments(sb: &CMSampleBuffer) -> *const c_void {
    unsafe {
        let arr = CMSampleBufferGetSampleAttachmentsArray(sb_ptr(sb), 0);
        if arr.is_null() || CFArrayGetCount(arr) < 1 {
            return ptr::null();
        }
        CFArrayGetValueAtIndex(arr, 0)
    }
}

/// `kVTSampleAttachmentKey_RequireLTRAcknowledgementToken` (macOS 12+, `dlsym`'d) on a sample.
pub fn ltr_token(sb: &CMSampleBuffer) -> Option<i64> {
    let key = cfstring_symbol("kVTSampleAttachmentKey_RequireLTRAcknowledgementToken")?;
    let d = first_attachments(sb);
    if d.is_null() {
        return None;
    }
    let v = unsafe { CFDictionaryGetValue(d.cast(), (key as *const CFString).cast()) };
    let v = unsafe { (v as *const CFType).as_ref() }?;
    v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64())
}

/// Every sample attachment of a sample, as JSON (`null` when it has none).
pub fn attachments_json(sb: &CMSampleBuffer) -> serde_json::Value {
    let d = first_attachments(sb);
    match unsafe { (d as *const CFType).as_ref() } {
        Some(t) => cf_to_json(t),
        None => serde_json::Value::Null,
    }
}
