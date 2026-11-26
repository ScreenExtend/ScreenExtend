//! Thread tuning for libx264's own slice workers.
//!
//! With `b_sliced_threads = 1` the slices are encoded on pool threads `x264_encoder_open`
//! creates, which the calling thread's tuning (`tuning::tune_current_thread`) never reaches.
//! libx264 has no usable thread-start hook (enabling `nalu_process` makes `x264_encoder_encode`
//! stop returning NALs), so the workers are found from outside: threads that appear across the
//! open and whose Win32 start address is inside the libx264 module. The shipped MinGW build
//! links winpthreads statically, so its workers start inside the DLL.
//!
//! A worker gets `THREAD_PRIORITY_HIGHEST` (at TIME_CRITICAL, N sessions x 8 workers would
//! starve the ABOVE_NORMAL transport workers that push the RTP), the per-thread EcoQoS opt-out
//! and the creating thread's CPU-set masks. `Get/SetThreadSelectedCpuSetMasks` are Windows 11
//! exports, so they are resolved by `GetProcAddress`.

use std::collections::BTreeSet;
use std::ffi::c_void;

use windows::core::{s, w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GetProcAddress, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
    GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
};
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThread, OpenThread, SetThreadInformation, SetThreadPriority,
    ThreadPowerThrottling, THREAD_POWER_THROTTLING_CURRENT_VERSION,
    THREAD_POWER_THROTTLING_EXECUTION_SPEED, THREAD_POWER_THROTTLING_STATE,
    THREAD_PRIORITY_HIGHEST, THREAD_QUERY_INFORMATION, THREAD_SET_INFORMATION,
    THREAD_SET_LIMITED_INFORMATION,
};

/// What [`tune_workers`] did, for the encoder's init log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct WorkerReport {
    pub tuned: Vec<u32>,
    pub failed: usize,
    pub masks: usize,
}

/// A thread handle that closes itself.
struct ThreadHandle(HANDLE);

impl Drop for ThreadHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_thread(tid: u32) -> Option<ThreadHandle> {
    unsafe {
        OpenThread(
            THREAD_QUERY_INFORMATION | THREAD_SET_INFORMATION | THREAD_SET_LIMITED_INFORMATION,
            false,
            tid,
        )
    }
    .ok()
    .map(ThreadHandle)
}

// Toolhelp by GetProcAddress: the windows crate feature for it is not enabled

const TH32CS_SNAPTHREAD: u32 = 0x0000_0004;

#[repr(C)]
#[allow(non_snake_case)]
struct THREADENTRY32 {
    dwSize: u32,
    cntUsage: u32,
    th32ThreadID: u32,
    th32OwnerProcessID: u32,
    tpBasePri: i32,
    tpDeltaPri: i32,
    dwFlags: u32,
}

type FnSnapshot = unsafe extern "system" fn(u32, u32) -> HANDLE;
type FnThreadWalk = unsafe extern "system" fn(HANDLE, *mut THREADENTRY32) -> i32;
type FnNtQueryInformationThread =
    unsafe extern "system" fn(HANDLE, u32, *mut c_void, u32, *mut u32) -> i32;
type FnGetCpuSetMasks =
    unsafe extern "system" fn(HANDLE, *mut GROUP_AFFINITY, u16, *mut u16) -> i32;
type FnSetCpuSetMasks = unsafe extern "system" fn(HANDLE, *const GROUP_AFFINITY, u16) -> i32;

/// `GetProcAddress` from a module that is always loaded, typed. `None` when the export is
/// absent, which is the probe.
unsafe fn resolve_proc<T: Copy>(module: PCWSTR, name: windows::core::PCSTR) -> Option<T> {
    debug_assert_eq!(std::mem::size_of::<T>(), std::mem::size_of::<usize>());
    let module = unsafe { GetModuleHandleW(module) }.ok()?;
    let f = unsafe { GetProcAddress(module, name) }?;
    Some(unsafe { std::mem::transmute_copy::<_, T>(&f) })
}

/// Every thread id in this process right now, or `None` if Toolhelp is unavailable.
pub(crate) fn process_thread_ids() -> Option<BTreeSet<u32>> {
    let snapshot: FnSnapshot =
        unsafe { resolve_proc(w!("kernel32.dll"), s!("CreateToolhelp32Snapshot")) }?;
    let first: FnThreadWalk = unsafe { resolve_proc(w!("kernel32.dll"), s!("Thread32First")) }?;
    let next: FnThreadWalk = unsafe { resolve_proc(w!("kernel32.dll"), s!("Thread32Next")) }?;

    let pid = unsafe { GetCurrentProcessId() };
    let snap = unsafe { snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap.is_invalid() || snap.0.is_null() {
        return None;
    }
    let snap = ThreadHandle(snap);
    let mut entry = THREADENTRY32 {
        dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
        cntUsage: 0,
        th32ThreadID: 0,
        th32OwnerProcessID: 0,
        tpBasePri: 0,
        tpDeltaPri: 0,
        dwFlags: 0,
    };
    let mut out = BTreeSet::new();
    let mut ok = unsafe { first(snap.0, &mut entry) } != 0;
    while ok {
        if entry.th32OwnerProcessID == pid {
            out.insert(entry.th32ThreadID);
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        ok = unsafe { next(snap.0, &mut entry) } != 0;
    }
    Some(out)
}

/// The module an address belongs to, as a raw handle value.
pub(crate) fn module_of(addr: *const c_void) -> Option<isize> {
    let mut module = HMODULE::default();
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(addr as *const u16),
            &mut module,
        )
    }
    .ok()?;
    Some(module.0 as isize)
}

/// The module holding a thread's Win32 start address (`ThreadQuerySetWin32StartAddress`).
pub(crate) fn thread_start_module(tid: u32) -> Option<isize> {
    const THREAD_QUERY_SET_WIN32_START_ADDRESS: u32 = 9;
    let query: FnNtQueryInformationThread =
        unsafe { resolve_proc(w!("ntdll.dll"), s!("NtQueryInformationThread")) }?;
    let handle = open_thread(tid)?;
    let mut start: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        query(
            handle.0,
            THREAD_QUERY_SET_WIN32_START_ADDRESS,
            &mut start as *mut _ as *mut c_void,
            std::mem::size_of::<*mut c_void>() as u32,
            std::ptr::null_mut(),
        )
    };
    if status < 0 || start.is_null() {
        return None;
    }
    module_of(start)
}

/// The threads that appeared between two snapshots and started inside `library`.
///
/// Pure, so the filter is testable without starting threads: a new thread that started
/// elsewhere (another subsystem's, created in the same window) is excluded, and so is an old
/// thread that happens to have started in the library (a previous encoder's workers).
pub(crate) fn select_new_workers(
    before: &BTreeSet<u32>,
    after: &BTreeSet<u32>,
    library: isize,
    start_module: impl Fn(u32) -> Option<isize>,
) -> Vec<u32> {
    after
        .difference(before)
        .copied()
        .filter(|&tid| start_module(tid) == Some(library))
        .collect()
}

/// The CPU-set masks the calling thread was given, empty when it has none (a homogeneous CPU,
/// an untuned thread, or a Windows without the call).
pub(crate) fn current_thread_cpu_set_masks() -> Vec<GROUP_AFFINITY> {
    let Some(get) = (unsafe {
        resolve_proc::<FnGetCpuSetMasks>(w!("kernel32.dll"), s!("GetThreadSelectedCpuSetMasks"))
    }) else {
        return Vec::new();
    };
    let thread = unsafe { GetCurrentThread() };
    let mut required: u16 = 0;
    unsafe { get(thread, std::ptr::null_mut(), 0, &mut required) };
    if required == 0 {
        return Vec::new();
    }
    let mut masks = vec![GROUP_AFFINITY::default(); required as usize];
    if unsafe { get(thread, masks.as_mut_ptr(), required, &mut required) } == 0 {
        return Vec::new();
    }
    masks.truncate(required as usize);
    masks.retain(|m| m.Mask != 0);
    masks
}

/// Logical processors named by a set of CPU-set masks.
pub(crate) fn logical_count(masks: &[GROUP_AFFINITY]) -> usize {
    masks.iter().map(|m| m.Mask.count_ones() as usize).sum()
}

/// Apply the worker tuning to each thread. A thread that refuses one step keeps the others; a
/// thread that cannot be opened at all (it exited) is counted and skipped.
pub(crate) fn tune_workers(tids: &[u32], masks: &[GROUP_AFFINITY]) -> WorkerReport {
    let set_masks = if masks.is_empty() {
        None
    } else {
        unsafe {
            resolve_proc::<FnSetCpuSetMasks>(w!("kernel32.dll"), s!("SetThreadSelectedCpuSetMasks"))
        }
    };
    let throttle = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    let mut report = WorkerReport {
        masks: if set_masks.is_some() {
            logical_count(masks)
        } else {
            0
        },
        ..Default::default()
    };
    for &tid in tids {
        let Some(handle) = open_thread(tid) else {
            report.failed += 1;
            continue;
        };
        let priority = unsafe { SetThreadPriority(handle.0, THREAD_PRIORITY_HIGHEST) };
        let _ = unsafe {
            SetThreadInformation(
                handle.0,
                ThreadPowerThrottling,
                &throttle as *const _ as *const c_void,
                std::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
            )
        };
        if let Some(set) = set_masks {
            unsafe { set(handle.0, masks.as_ptr(), masks.len() as u16) };
        }
        if priority.is_ok() {
            report.tuned.push(tid);
        } else {
            report.failed += 1;
        }
    }
    report
}

#[cfg(test)]
pub(crate) fn thread_priority(tid: u32) -> Option<i32> {
    let handle = open_thread(tid)?;
    Some(unsafe { windows::Win32::System::Threading::GetThreadPriority(handle.0) })
}

/// How many slice threads to ask libx264 for.
///
/// `available` is the machine's logical processor count and `cap` the fixed ceiling (8).
/// With `p_cores` the count is also held to the P-core logical processors the workers are
/// confined to, because a ninth worker confined to eight cores only queues. With
/// `min_mb_rows_per_slice` a small frame gets fewer, taller slices: libx264 already caps the
/// slice count at the frame's macroblock rows, but a 640x360 picture (23 rows) cut into 8
/// slices of ~3 rows loses intra and motion prediction across every boundary for parallelism
/// the frame is too small to need.
pub(crate) fn slice_thread_count(
    available: usize,
    cap: usize,
    p_cores: Option<usize>,
    height: usize,
    min_mb_rows_per_slice: Option<usize>,
) -> usize {
    let mut n = available.clamp(1, cap.max(1));
    if let Some(p) = p_cores.filter(|&p| p > 0) {
        n = n.min(p);
    }
    if let Some(rows) = min_mb_rows_per_slice.filter(|&r| r > 0) {
        let mb_rows = height.div_ceil(16);
        n = n.min((mb_rows / rows).max(1));
    }
    n.max(1)
}
