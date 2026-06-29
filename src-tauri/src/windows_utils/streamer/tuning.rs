#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;

pub fn apply_process_tuning() {
    #[cfg(windows)]
    {
        prefer_high_performance_gpu();
        raise_timer_resolution();
        honor_timer_resolution_in_background();
        exempt_from_power_throttling();
        raise_process_priority();
    }
}

#[cfg(windows)]
fn prefer_high_performance_gpu() {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            teprintln!("[gpu] could not resolve current exe for GPU preference: {e}");
            return;
        }
    };
    let exe_path = exe.to_string_lossy().into_owned();

    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let key = match hkcu.create_subkey(r"Software\Microsoft\DirectX\UserGpuPreferences") {
        Ok((k, _)) => k,
        Err(e) => {
            teprintln!("[gpu] opening UserGpuPreferences failed: {e}");
            return;
        }
    };

    const HIGH_PERFORMANCE: &str = "GpuPreference=2;";
    if let Ok(existing) = key.get_value::<String, _>(&exe_path) {
        if existing == HIGH_PERFORMANCE {
            tprintln!("[gpu] high-performance GPU preference already set for {exe_path}");
            return;
        }
    }
    match key.set_value(&exe_path, &HIGH_PERFORMANCE) {
        Ok(()) => tprintln!("[gpu] set high-performance GPU preference for {exe_path}"),
        Err(e) => teprintln!("[gpu] setting high-performance GPU preference failed: {e}"),
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
                let _ = AvRevertMmThreadCharacteristics(HANDLE(self.mmcss as *mut core::ffi::c_void));
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
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
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
        GetCurrentProcess, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE,
        ProcessPowerThrottling, SetProcessInformation,
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
        GetCurrentProcess, HIGH_PRIORITY_CLASS, SetPriorityClass,
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
            ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED, SetThreadExecutionState,
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
            use windows::Win32::System::Power::{ES_CONTINUOUS, SetThreadExecutionState};
            unsafe {
                let _ = SetThreadExecutionState(ES_CONTINUOUS);
            }
            self.active = false;
        }
    }
}

#[cfg(windows)]
pub fn tune_transport_thread() {
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL,
    };
    use windows::core::w;

    let mut task_index: u32 = 0;
    let _ = unsafe { AvSetMmThreadCharacteristicsW(w!("Playback"), &mut task_index) };
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }
}

#[cfg(windows)]
fn register_mmcss() -> isize {
    use windows::Win32::System::Threading::{
        AVRT_PRIORITY_CRITICAL, AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority,
    };
    use windows::core::w;

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
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::core::Interface;

    let dxgi: IDXGIDevice = match device.cast() {
        Ok(d) => d,
        Err(_) => return,
    };
    match unsafe { dxgi.SetGPUThreadPriority(5) } {
        Ok(()) => tprintln!("GPU thread priority raised (+5)"),
        Err(e) => teprintln!("SetGPUThreadPriority failed: {e}"),
    }
}
