#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;

pub fn apply_process_tuning() {
    #[cfg(windows)]
    {
        raise_timer_resolution();
        exempt_from_power_throttling();
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
        println!("timer resolution raised to 1ms");
    } else {
        eprintln!("timeBeginPeriod(1) rejected (rc={rc})");
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
        Ok(()) => println!("process exempted from power throttling"),
        Err(e) => eprintln!("power-throttling exemption failed: {e}"),
    }
}

#[cfg(windows)]
fn register_mmcss() -> isize {
    use windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW;
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
                println!("MMCSS registered ({name})");
                return h.0 as isize;
            }
            _ => continue,
        }
    }

    eprintln!("all MMCSS task names failed");
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
            eprintln!("SetThreadPriority failed: {e}");
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
        Ok(()) => println!("GPU thread priority raised (+5)"),
        Err(e) => eprintln!("SetGPUThreadPriority failed: {e}"),
    }
}
