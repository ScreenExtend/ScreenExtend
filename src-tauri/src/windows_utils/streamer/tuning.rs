#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;

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
        raise_current_thread_priority();
        ThreadTuning { mmcss }
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

#[cfg(windows)]
pub fn tune_transport_thread() {
    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL,
    };

    let mut task_index: u32 = 0;
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Playback"), &mut task_index) };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
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
fn raise_current_thread_priority() {
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, SetThreadPriorityBoost, THREAD_PRIORITY_TIME_CRITICAL,
    };
    unsafe {
        let t = GetCurrentThread();
        if let Err(e) = SetThreadPriority(t, THREAD_PRIORITY_TIME_CRITICAL) {
            teprintln!("SetThreadPriority failed: {e}");
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
