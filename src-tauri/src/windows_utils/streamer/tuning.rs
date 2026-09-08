#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    THREAD_PRIORITY, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_TIME_CRITICAL,
};

pub fn apply_process_tuning() {
    #[cfg(windows)]
    {
        raise_gpu_scheduling_priority_class();
        raise_timer_resolution();
        honor_timer_resolution_in_background();
        exempt_from_power_throttling();
        raise_process_priority();
    }
}

/// Asks the GPU scheduler to treat this process as real-time.
///
/// `IDXGIDevice::SetGPUThreadPriority` — which we also call, per device — turned out not to be
/// enough on its own: OBS added exactly this call (commit `ec769ef`, "libobs-d3d11: Set maximum
/// GPU priority") because capture was still being deprioritised underneath a full-screen game.
/// It is per-process, so one call at startup covers every device created later, which matters
/// because we create a capture device and an encoder device per client.
///
/// Resolved by `GetProcAddress`: it lives in gdi32 but is not in any import library, and a
/// non-zero NTSTATUS is a reason to carry on, not to fail.
#[cfg(windows)]
fn raise_gpu_scheduling_priority_class() {
    use windows::core::s;
    use windows::Win32::Foundation::HANDLE as WinHandle;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows::Win32::System::Threading::GetCurrentProcess;

    /// `D3DKMT_SCHEDULINGPRIORITYCLASS_REALTIME`, the top of the Idle..Realtime scale.
    const REALTIME: u32 = 5;

    type SetPriorityClassFn = unsafe extern "system" fn(WinHandle, u32) -> i32;

    let Ok(gdi32) = (unsafe { GetModuleHandleA(s!("gdi32.dll")) }) else {
        teprintln!("[gpu] gdi32 not loaded; skipping GPU scheduling priority");
        return;
    };
    let Some(proc) =
        (unsafe { GetProcAddress(gdi32, s!("D3DKMTSetProcessSchedulingPriorityClass")) })
    else {
        tprintln!("[gpu] D3DKMTSetProcessSchedulingPriorityClass unavailable; skipping");
        return;
    };
    let set: SetPriorityClassFn = unsafe { std::mem::transmute(proc) };
    let status = unsafe { set(GetCurrentProcess(), REALTIME) };
    if status == 0 {
        tprintln!("[gpu] GPU scheduling priority class raised to REALTIME");
    } else {
        teprintln!(
            "[gpu] D3DKMTSetProcessSchedulingPriorityClass(REALTIME) -> NTSTATUS 0x{status:08X}"
        );
    }
}

#[cfg(windows)]
pub struct ThreadTuning {
    mmcss: isize,
}

#[cfg(windows)]
unsafe impl Send for ThreadTuning {}

#[cfg(windows)]
impl Drop for ThreadTuning {
    fn drop(&mut self) {
        if self.mmcss != 0 {
            use windows::Win32::System::Threading::AvRevertMmThreadCharacteristics;
            unsafe {
                let _ =
                    AvRevertMmThreadCharacteristics(HANDLE(self.mmcss as *mut core::ffi::c_void));
            }
            self.mmcss = 0;
        }
    }
}

pub fn tune_current_thread() -> ThreadTuning {
    #[cfg(windows)]
    {
        let mmcss = register_mmcss();
        raise_current_thread_priority(THREAD_PRIORITY_TIME_CRITICAL);
        confine_to_performance_cores();
        ThreadTuning { mmcss }
    }
}

/// Tuning for a thread that runs on a timer rather than on the frame.
///
/// The process is already `HIGH_PRIORITY_CLASS` and every capture and encode thread is
/// `THREAD_PRIORITY_TIME_CRITICAL`, so N clients meant ~2N time-critical threads competing with
/// the tokio transport workers, which sit at `ABOVE_NORMAL` and are what actually push RTP. The
/// idle repeater is not one of the threads that needs to win those contests: it emits a keepalive
/// every 200 ms and being a millisecond late costs nothing. It keeps MMCSS and the P-core
/// confinement — it should still not be parked on an efficiency core — and drops to `HIGHEST`.
pub fn tune_timer_thread() -> ThreadTuning {
    #[cfg(windows)]
    {
        let mmcss = register_mmcss();
        raise_current_thread_priority(THREAD_PRIORITY_HIGHEST);
        confine_to_performance_cores();
        ThreadTuning { mmcss }
    }
}

/// The CPU sets belonging to the highest `EfficiencyClass` on a hybrid CPU, or `None` on a
/// homogeneous one (and on any Windows without the API).
///
/// Thread priority orders threads *within* a core's queue; it says nothing about which core the
/// scheduler picks. On an Alder Lake or Core Ultra part an MMCSS-critical capture thread can
/// still be parked on an efficiency core, and the cost shows up as tail latency, not as a lower
/// average. `SetThreadSelectedCpuSetMasks` is a soft restriction, unlike `SetThreadAffinityMask`,
/// so the scheduler may still migrate the thread rather than deadlock behind it.
///
/// The probe is "does this machine report more than one distinct `EfficiencyClass`" — not a CPU
/// model, not a Windows version.
#[cfg(windows)]
fn performance_core_masks(
) -> Option<&'static Vec<windows::Win32::System::SystemInformation::GROUP_AFFINITY>> {
    use std::sync::OnceLock;

    use windows::Win32::System::SystemInformation::{
        GetSystemCpuSetInformation, GROUP_AFFINITY, SYSTEM_CPU_SET_INFORMATION,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    static MASKS: OnceLock<Option<Vec<GROUP_AFFINITY>>> = OnceLock::new();
    MASKS
        .get_or_init(|| {
            let mut needed: u32 = 0;
            unsafe {
                let _ = GetSystemCpuSetInformation(
                    None,
                    0,
                    &mut needed,
                    Some(GetCurrentProcess()),
                    None,
                );
            }
            if needed == 0 {
                return None;
            }
            let mut buf = vec![0u8; needed as usize];
            let mut written: u32 = 0;
            if !unsafe {
                GetSystemCpuSetInformation(
                    Some(buf.as_mut_ptr().cast::<SYSTEM_CPU_SET_INFORMATION>()),
                    needed,
                    &mut written,
                    Some(GetCurrentProcess()),
                    None,
                )
            }
            .as_bool()
            {
                return None;
            }

            // The buffer is a run of variable-size records; each one's `Size` is the stride.
            let mut sets: Vec<(u8, u16, u8)> = Vec::new();
            let mut offset = 0usize;
            while offset + std::mem::size_of::<SYSTEM_CPU_SET_INFORMATION>() <= written as usize {
                let record = unsafe {
                    &*buf
                        .as_ptr()
                        .add(offset)
                        .cast::<SYSTEM_CPU_SET_INFORMATION>()
                };
                let size = record.Size as usize;
                if size == 0 {
                    break;
                }
                // `Type` 0 is `CpuSetInformation`, the only variant defined.
                if record.Type.0 == 0 {
                    let cpu_set = unsafe { record.Anonymous.CpuSet };
                    sets.push((
                        cpu_set.EfficiencyClass,
                        cpu_set.Group,
                        cpu_set.LogicalProcessorIndex,
                    ));
                }
                offset += size;
            }

            let best = sets.iter().map(|(class, ..)| *class).max()?;
            if sets.iter().any(|(class, ..)| *class != best) {
                let mut by_group: std::collections::BTreeMap<u16, u64> =
                    std::collections::BTreeMap::new();
                for (class, group, index) in sets.iter().copied() {
                    if class == best && index < 64 {
                        *by_group.entry(group).or_default() |= 1u64 << index;
                    }
                }
                let masks: Vec<GROUP_AFFINITY> = by_group
                    .into_iter()
                    .map(|(group, mask)| GROUP_AFFINITY {
                        Mask: mask as usize,
                        Group: group,
                        Reserved: [0; 3],
                    })
                    .collect();
                let cores: u32 = masks.iter().map(|m| (m.Mask as u64).count_ones()).sum();
                tprintln!(
                    "[cpu] hybrid CPU detected: confining capture/encode threads to the {cores} \
                     logical processors of efficiency class {best}"
                );
                return Some(masks);
            }
            None
        })
        .as_ref()
}

#[cfg(windows)]
fn confine_to_performance_cores() {
    use windows::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSetMasks};

    let Some(masks) = performance_core_masks() else {
        return;
    };
    if !unsafe { SetThreadSelectedCpuSetMasks(GetCurrentThread(), Some(masks)) }.as_bool() {
        teprintln!(
            "[cpu] SetThreadSelectedCpuSetMasks failed: {}",
            windows::core::Error::from_thread()
        );
    }
}

#[cfg(windows)]
fn raise_timer_resolution() {
    use windows::Win32::Media::timeBeginPeriod;
    let rc = unsafe { timeBeginPeriod(1) };
    if rc == 0 {
        tprintln!("timer resolution raised to 1ms");
    } else {
        teprintln!("timeBeginPeriod(1) rejected (rc={rc})");
    }
}

#[cfg(windows)]
fn exempt_from_power_throttling() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_STATE,
    };

    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    let rc = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    match rc {
        Ok(()) => tprintln!("process exempted from power throttling"),
        Err(e) => teprintln!("power-throttling exemption failed: {e}"),
    }
}

#[cfg(windows)]
fn honor_timer_resolution_in_background() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        PROCESS_POWER_THROTTLING_STATE,
    };

    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        StateMask: 0,
    };
    let rc = unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    };
    match rc {
        Ok(()) => tprintln!("timer resolution honored in background (Win11)"),
        Err(e) => tprintln!("timer-resolution background opt-in unavailable (pre-Win11?): {e}"),
    }
}

#[cfg(windows)]
fn raise_process_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS,
    };
    match unsafe { SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS) } {
        Ok(()) => tprintln!("process priority class raised to HIGH"),
        Err(e) => teprintln!("SetPriorityClass(HIGH) failed: {e}"),
    }
}

#[cfg(windows)]
pub struct KeepAwake {
    active: bool,
}

#[cfg(windows)]
unsafe impl Send for KeepAwake {}

#[cfg(windows)]
impl KeepAwake {
    pub fn begin() -> Self {
        use windows::Win32::System::Power::{
            SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
        };
        let prev = unsafe {
            SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED)
        };
        if prev.0 == 0 {
            teprintln!("SetThreadExecutionState (keep-awake) failed");
            KeepAwake { active: false }
        } else {
            tprintln!("keep-awake asserted for session (no system/display sleep)");
            KeepAwake { active: true }
        }
    }
}

#[cfg(windows)]
impl Drop for KeepAwake {
    fn drop(&mut self) {
        if self.active {
            use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS};
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS);
            }
            self.active = false;
        }
    }
}

/// Tunes a tokio transport worker. Called from `on_thread_start`, so once per worker.
///
/// Both calls used to discard their result, which hid two things: a disabled MMCSS service or an
/// exhausted per-process task quota left every transport thread unprioritised with no trace, and
/// the returned MMCSS handle was dropped without `AvRevertMmThreadCharacteristics`, so a host that
/// starts and stops many streams leaks registrations until *all* MMCSS registrations start
/// failing -- including the capture thread's. The first failure of each kind is logged once per
/// process; the handle is parked in a thread-local that reverts when the worker exits.
#[cfg(windows)]
pub fn tune_transport_thread() {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL,
    };

    thread_local! {
        static MMCSS: RefCell<Option<ThreadTuning>> = const { RefCell::new(None) };
    }
    static MMCSS_WARNED: AtomicBool = AtomicBool::new(false);
    static PRIORITY_WARNED: AtomicBool = AtomicBool::new(false);

    let mut task_index: u32 = 0;
    match unsafe { AvSetMmThreadCharacteristicsW(w!("Playback"), &mut task_index) } {
        Ok(h) if !h.is_invalid() => {
            MMCSS.with(|slot| {
                *slot.borrow_mut() = Some(ThreadTuning {
                    mmcss: h.0 as isize,
                });
            });
        }
        Ok(_) | Err(_) if !MMCSS_WARNED.swap(true, Ordering::Relaxed) => {
            teprintln!(
                "MMCSS registration failed for a transport thread; transport threads will run \
                 unprioritised (is the MMCSS service running?)"
            );
        }
        _ => {}
    }

    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) } {
        if !PRIORITY_WARNED.swap(true, Ordering::Relaxed) {
            teprintln!("SetThreadPriority(ABOVE_NORMAL) failed on a transport thread: {e}");
        }
    }
}

#[cfg(windows)]
fn register_mmcss() -> isize {
    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, AVRT_PRIORITY_CRITICAL,
    };

    let tasks: &[(&str, windows::core::PCWSTR)] = &[
        ("Pro Audio", w!("Pro Audio")),
        ("Games", w!("Games")),
        ("Playback", w!("Playback")),
    ];

    for &(name, wide_name) in tasks {
        let mut task_index: u32 = 0;
        match unsafe { AvSetMmThreadCharacteristicsW(wide_name, &mut task_index) } {
            Ok(h) if !h.is_invalid() => {
                tprintln!("MMCSS registered ({name})");
                match unsafe { AvSetMmThreadPriority(h, AVRT_PRIORITY_CRITICAL) } {
                    Ok(()) => tprintln!("MMCSS priority raised to CRITICAL ({name})"),
                    Err(e) => teprintln!("AvSetMmThreadPriority(CRITICAL) failed: {e}"),
                }
                return h.0 as isize;
            }
            _ => continue,
        }
    }

    teprintln!("all MMCSS task names failed");
    0
}

#[cfg(windows)]
fn raise_current_thread_priority(level: THREAD_PRIORITY) {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, SetThreadPriorityBoost,
    };
    unsafe {
        let t = GetCurrentThread();
        if let Err(e) = SetThreadPriority(t, level) {
            teprintln!("SetThreadPriority({}) failed: {e}", level.0);
        }
        let _ = SetThreadPriorityBoost(t, true);
    }
}

#[cfg(windows)]
pub fn raise_d3d11_gpu_priority(device: &windows::Win32::Graphics::Direct3D11::ID3D11Device) {
    use windows::core::Interface;
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;

    let dxgi: IDXGIDevice = match device.cast() {
        Ok(d) => d,
        Err(_) => return,
    };
    match unsafe { dxgi.SetGPUThreadPriority(5) } {
        Ok(()) => tprintln!("GPU thread priority raised (+5)"),
        Err(e) => teprintln!("SetGPUThreadPriority failed: {e}"),
    }
}
