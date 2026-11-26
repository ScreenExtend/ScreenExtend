#[cfg(windows)]
use windows::Win32::Foundation::HANDLE;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    THREAD_PRIORITY, THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_TIME_CRITICAL,
};

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

/// `PROCESS_POWER_THROTTLING_EXECUTION_SPEED`, as the plain bit (pinned by a test).
const POWER_EXECUTION_SPEED: u32 = 0x1;
/// `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`, as the plain bit (pinned by a test).
const POWER_IGNORE_TIMER_RESOLUTION: u32 = 0x4;

/// Process-wide tuning, applied once per process: `Streamer::prepare` calls this on every adapter
/// start (each Wi-Fi roam, DHCP renewal or port change), and a repeat would stack another
/// unmatched `timeBeginPeriod(1)`.
pub fn apply_process_tuning() {
    #[cfg(windows)]
    apply_process_tuning_with(process_tuning_steps);
}

/// The once-per-process guard, with the steps injected so a test can drive the real static with a
/// counting stand-in. A concurrent second caller blocks until the first has finished, so no
/// adapter starts before the tuning is in place.
#[cfg(windows)]
fn apply_process_tuning_with(steps: fn()) {
    PROCESS_TUNING_APPLIED.call_once(steps);
}

#[cfg(windows)]
static PROCESS_TUNING_APPLIED: std::sync::Once = std::sync::Once::new();

#[cfg(windows)]
fn process_tuning_steps() {
    raise_gpu_scheduling_priority_class();
    raise_timer_resolution();
    configure_process_power_throttling();
    raise_process_priority();
}

/// Sets this process's GPU scheduler priority class (`D3DKMT_SCHEDULINGPRIORITYCLASS`: Idle 0,
/// Below Normal 1, Normal 2, Above Normal 3, High 4, Realtime 5).
///
/// Not Realtime: that puts capture, scale and encode above DWM's own packets, and a compositor
/// that loses the GPU is a slow desktop with a flickering pointer. `ABOVE_NORMAL` stops a
/// full-screen game pushing the capture out while staying below the compositor's `HIGH`.
/// `SE_GPU_SCHED_CLASS` overrides it with a raw class number; `2` (Normal) turns it off.
///
/// Resolved by `GetProcAddress`: the export is in gdi32 but in no import library. A non-zero
/// NTSTATUS is logged, not fatal.
#[cfg(windows)]
fn raise_gpu_scheduling_priority_class() {
    use windows::core::s;
    use windows::Win32::Foundation::HANDLE as WinHandle;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};
    use windows::Win32::System::Threading::GetCurrentProcess;

    /// `D3DKMT_SCHEDULINGPRIORITYCLASS_ABOVE_NORMAL`. Below the compositor's, above a game's.
    const ABOVE_NORMAL: u32 = 3;

    type SetPriorityClassFn = unsafe extern "system" fn(WinHandle, u32) -> i32;

    let class = crate::envcfg::var("SE_GPU_SCHED_CLASS")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v <= 5)
        .unwrap_or(ABOVE_NORMAL);

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
    let status = unsafe { set(GetCurrentProcess(), class) };
    if status == 0 {
        tprintln!("[gpu] GPU scheduling priority class set to {class} (0 idle .. 5 realtime)");
    } else {
        teprintln!(
            "[gpu] D3DKMTSetProcessSchedulingPriorityClass({class}) -> NTSTATUS 0x{status:08X}"
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
        let (tasks, critical) = mmcss_plan(MmcssRole::Realtime);
        let mmcss = register_mmcss(tasks, critical);
        raise_current_thread_priority(THREAD_PRIORITY_TIME_CRITICAL);
        exempt_thread_from_power_throttling();
        confine_to_performance_cores();
        ThreadTuning { mmcss }
    }
}

/// Tuning for a thread that runs on a timer rather than on the frame (the idle repeater).
///
/// MMCSS decides this thread's priority, not `SetThreadPriority`. By default it registers like
/// the capture and encode threads: "Pro Audio" at `AVRT_PRIORITY_CRITICAL`, which on a default
/// registry is Scheduling Category High, the realtime band (23 to 26). The `HIGHEST` below (14 in
/// `HIGH_PRIORITY_CLASS`) applies only to the slices MMCSS is not boosting. The tokio transport
/// workers are MMCSS "Playback" (Category Medium, 16 to 22 in the foreground), so they do not
/// outrank this thread.
///
/// The `timer_thread_playback` candidate (`SE_TUNE_ON=timer_thread_playback`) registers it as
/// "Playback" without `CRITICAL`, below capture and encode: a 200 ms keepalive can be a
/// millisecond late. Off by default. The P-core confinement applies either way.
pub fn tune_timer_thread() -> ThreadTuning {
    #[cfg(windows)]
    {
        let playback = super::tune::candidate("timer_thread_playback");
        let (tasks, critical) = mmcss_plan(MmcssRole::Timer { playback });
        let mmcss = register_mmcss(tasks, critical);
        raise_current_thread_priority(THREAD_PRIORITY_HIGHEST);
        exempt_thread_from_power_throttling();
        confine_to_performance_cores();
        ThreadTuning { mmcss }
    }
}

/// Which thread is asking for an MMCSS registration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MmcssRole {
    /// Capture and encode: the frame path.
    Realtime,
    /// The idle repeater; `playback` is the `timer_thread_playback` candidate.
    Timer { playback: bool },
}

/// The MMCSS task names to try, in order, and whether to raise the registration to
/// `AVRT_PRIORITY_CRITICAL`.
fn mmcss_plan(role: MmcssRole) -> (&'static [&'static str], bool) {
    const REALTIME: &[&str] = &["Pro Audio", "Games", "Playback"];
    const PLAYBACK: &[&str] = &["Playback"];
    match role {
        MmcssRole::Realtime | MmcssRole::Timer { playback: false } => (REALTIME, true),
        MmcssRole::Timer { playback: true } => (PLAYBACK, false),
    }
}

/// One CPU set as `GetSystemCpuSetInformation` reports it, reduced to what the selection needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CpuSet {
    id: u32,
    group: u16,
    index: u8,
    class: u8,
}

/// The CPU sets of the highest `EfficiencyClass`, in both shapes the two thread APIs take.
#[derive(Debug, PartialEq, Eq)]
struct PerformanceSelection {
    class: u8,
    /// `(group, 64-bit mask)`, for `SetThreadSelectedCpuSetMasks`.
    masks: Vec<(u16, u64)>,
    /// CPU-set ids, for `SetThreadSelectedCpuSets`.
    ids: Vec<u32>,
}

/// The highest-`EfficiencyClass` sets on a hybrid CPU, or `None` on a homogeneous one. Hybrid
/// means more than one distinct `EfficiencyClass` reported, not a CPU model or Windows version.
fn select_performance_cores(sets: &[CpuSet]) -> Option<PerformanceSelection> {
    let best = sets.iter().map(|s| s.class).max()?;
    if sets.iter().all(|s| s.class == best) {
        return None;
    }
    let mut by_group: std::collections::BTreeMap<u16, u64> = std::collections::BTreeMap::new();
    let mut ids = Vec::new();
    for s in sets.iter().filter(|s| s.class == best) {
        ids.push(s.id);
        // `LogicalProcessorIndex` is within its group, and a group holds at most 64
        if s.index < 64 {
            *by_group.entry(s.group).or_default() |= 1u64 << s.index;
        }
    }
    Some(PerformanceSelection {
        class: best,
        masks: by_group.into_iter().collect(),
        ids,
    })
}

#[cfg(windows)]
struct PerformanceCores {
    masks: Vec<windows::Win32::System::SystemInformation::GROUP_AFFINITY>,
    ids: Vec<u32>,
}

/// The CPU sets of the highest `EfficiencyClass` on a hybrid CPU, or `None` on a homogeneous one.
///
/// Thread priority does not pick the core: an MMCSS-critical thread can still be parked on an
/// efficiency core, which costs tail latency. A selected CPU set is a soft restriction, unlike
/// `SetThreadAffinityMask`, so the scheduler can still migrate the thread.
#[cfg(windows)]
fn performance_cores() -> Option<&'static PerformanceCores> {
    use std::sync::OnceLock;

    use windows::Win32::System::SystemInformation::{
        GetSystemCpuSetInformation, GROUP_AFFINITY, SYSTEM_CPU_SET_INFORMATION,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    static CORES: OnceLock<Option<PerformanceCores>> = OnceLock::new();
    CORES
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

            // the buffer is a run of variable-size records; each one's `Size` is the stride
            let mut sets: Vec<CpuSet> = Vec::new();
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
                // `Type` 0 is `CpuSetInformation`, the only variant defined
                if record.Type.0 == 0 {
                    let cpu_set = unsafe { record.Anonymous.CpuSet };
                    sets.push(CpuSet {
                        id: cpu_set.Id,
                        group: cpu_set.Group,
                        index: cpu_set.LogicalProcessorIndex,
                        class: cpu_set.EfficiencyClass,
                    });
                }
                offset += size;
            }

            let selection = select_performance_cores(&sets)?;
            let masks: Vec<GROUP_AFFINITY> = selection
                .masks
                .iter()
                .map(|&(group, mask)| GROUP_AFFINITY {
                    Mask: mask as usize,
                    Group: group,
                    Reserved: [0; 3],
                })
                .collect();
            tprintln!(
                "[cpu] hybrid CPU detected: confining capture/encode threads to the {} \
                 logical processors of efficiency class {}",
                selection.ids.len(),
                selection.class
            );
            Some(PerformanceCores {
                masks,
                ids: selection.ids,
            })
        })
        .as_ref()
}

/// Opts one thread out of EcoQoS, on top of the process-wide exemption.
///
/// Windows 11 also throttles the threads of a minimised or fully occluded window, which is the
/// usual state of this window while it streams; the per-thread opt-out is the documented
/// override. `ThreadPowerThrottling` needs Windows 10 1809: an older build refuses, so the
/// failure is logged once and ignored.
#[cfg(windows)]
pub(crate) fn exempt_thread_from_power_throttling() {
    use std::sync::atomic::AtomicBool;

    use windows::Win32::Foundation::HANDLE as WinHandle;
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadInformation, ThreadPowerThrottling,
        THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        THREAD_POWER_THROTTLING_STATE,
    };

    static WARNED: AtomicBool = AtomicBool::new(false);

    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0,
    };
    let rc = unsafe {
        SetThreadInformation(
            WinHandle(GetCurrentThread().0),
            ThreadPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        )
    };
    if rc.is_err() && !WARNED.swap(true, Ordering::Relaxed) {
        teprintln!(
            "[cpu] per-thread power-throttling exemption unavailable (pre-1809?): {:?}; \
             the process-wide exemption still applies",
            rc.err()
        );
    }
}

/// Which kernel32 export selects a thread's CPU sets on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CpuSetTier {
    /// `SetThreadSelectedCpuSetMasks`: Windows 11 / Server 2022 and later only.
    Masks,
    /// `SetThreadSelectedCpuSets`: Windows 10 1709 and later, by CPU-set id.
    Ids,
}

/// The best export present, preferring the mask form; `None` when neither exists.
fn pick_cpu_set_tier(has_masks: bool, has_ids: bool) -> Option<CpuSetTier> {
    if has_masks {
        Some(CpuSetTier::Masks)
    } else if has_ids {
        Some(CpuSetTier::Ids)
    } else {
        None
    }
}

#[cfg(windows)]
type SetThreadSelectedCpuSetMasksFn = unsafe extern "system" fn(
    HANDLE,
    *const windows::Win32::System::SystemInformation::GROUP_AFFINITY,
    u16,
) -> i32;
#[cfg(windows)]
type SetThreadSelectedCpuSetsFn = unsafe extern "system" fn(HANDLE, *const u32, u32) -> i32;

#[cfg(windows)]
#[derive(Clone, Copy)]
enum CpuSetApi {
    Masks(SetThreadSelectedCpuSetMasksFn),
    Ids(SetThreadSelectedCpuSetsFn),
}

/// Both CPU-set exports, resolved at run time.
///
/// `SetThreadSelectedCpuSetMasks` exists only from Windows 11. Called through the `windows`
/// crate it is a load-time import (raw-dylib), and a Windows 10 loader that cannot resolve one
/// import refuses the whole executable with `STATUS_ENTRYPOINT_NOT_FOUND` before `main`. The id
/// form covers Windows 10, and a machine with neither is a no-op.
/// `no_post_floor_import_is_bound_at_load_time` checks the import table.
#[cfg(windows)]
fn cpu_set_api() -> Option<CpuSetApi> {
    use std::sync::OnceLock;

    use windows::core::s;
    use windows::Win32::System::LibraryLoader::{GetModuleHandleA, GetProcAddress};

    static API: OnceLock<Option<CpuSetApi>> = OnceLock::new();
    *API.get_or_init(|| {
        let Ok(kernel32) = (unsafe { GetModuleHandleA(s!("kernel32.dll")) }) else {
            teprintln!("[cpu] kernel32 not loaded; threads keep the default CPU sets");
            return None;
        };
        let masks = unsafe { GetProcAddress(kernel32, s!("SetThreadSelectedCpuSetMasks")) };
        let ids = unsafe { GetProcAddress(kernel32, s!("SetThreadSelectedCpuSets")) };
        let api = match pick_cpu_set_tier(masks.is_some(), ids.is_some()) {
            Some(CpuSetTier::Masks) => masks.map(|f| {
                CpuSetApi::Masks(unsafe {
                    std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        SetThreadSelectedCpuSetMasksFn,
                    >(f)
                })
            }),
            Some(CpuSetTier::Ids) => ids.map(|f| {
                CpuSetApi::Ids(unsafe {
                    std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        SetThreadSelectedCpuSetsFn,
                    >(f)
                })
            }),
            None => None,
        };
        match api {
            Some(CpuSetApi::Masks(_)) => {
                tprintln!("[cpu] CPU-set tier: SetThreadSelectedCpuSetMasks")
            }
            Some(CpuSetApi::Ids(_)) => tprintln!(
                "[cpu] CPU-set tier: SetThreadSelectedCpuSets (no SetThreadSelectedCpuSetMasks \
                 on this Windows)"
            ),
            None => teprintln!(
                "[cpu] neither SetThreadSelectedCpuSetMasks nor SetThreadSelectedCpuSets is \
                 exported; threads keep the default CPU sets"
            ),
        }
        api
    })
}

#[cfg(windows)]
pub(crate) fn confine_to_performance_cores() {
    use windows::Win32::System::Threading::GetCurrentThread;

    let Some(cores) = performance_cores() else {
        return;
    };
    let Some(api) = cpu_set_api() else {
        return;
    };
    let (ok, name) = match api {
        CpuSetApi::Masks(set) => {
            let Ok(count) = u16::try_from(cores.masks.len()) else {
                return;
            };
            (
                unsafe { set(GetCurrentThread(), cores.masks.as_ptr(), count) } != 0,
                "SetThreadSelectedCpuSetMasks",
            )
        }
        CpuSetApi::Ids(set) => {
            let Ok(count) = u32::try_from(cores.ids.len()) else {
                return;
            };
            (
                unsafe { set(GetCurrentThread(), cores.ids.as_ptr(), count) } != 0,
                "SetThreadSelectedCpuSets",
            )
        }
    };
    if !ok {
        teprintln!(
            "[cpu] {name} failed: {}",
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PowerThrottlingOutcome {
    execution_speed: bool,
    timer_resolution: bool,
}

/// The `SetProcessInformation(ProcessPowerThrottling)` calls, with the setter injected.
///
/// `combined` (the `process_power_throttling_combined` switch, on by default) sends both control
/// bits in one call, because a call writes the process's throttling state as a whole (Microsoft's
/// "back to system managed" sample is `ControlMask = 0`, `StateMask = 0`): two one-bit calls
/// most likely leave only the second bit in force, and `timeBeginPeriod(1)` is then ignored
/// while the window is minimised. If the combined call is refused (`IGNORE_TIMER_RESOLUTION` is
/// the newer bit), the execution-speed bit is retried alone.
///
/// Off, it makes two one-bit calls, timer resolution first.
fn plan_power_throttling(
    combined: bool,
    mut set: impl FnMut(u32) -> bool,
) -> PowerThrottlingOutcome {
    if combined {
        if set(POWER_EXECUTION_SPEED | POWER_IGNORE_TIMER_RESOLUTION) {
            return PowerThrottlingOutcome {
                execution_speed: true,
                timer_resolution: true,
            };
        }
        PowerThrottlingOutcome {
            execution_speed: set(POWER_EXECUTION_SPEED),
            timer_resolution: false,
        }
    } else {
        let timer_resolution = set(POWER_IGNORE_TIMER_RESOLUTION);
        let execution_speed = set(POWER_EXECUTION_SPEED);
        PowerThrottlingOutcome {
            execution_speed,
            timer_resolution,
        }
    }
}

/// What a read-back of the process's throttling state says about what was asked for, or `None`
/// when it holds: every bit that was set must be in `ControlMask` and clear in `StateMask`.
fn power_readback_problem(expected: u32, control: u32, state: u32) -> Option<String> {
    let missing = expected & !control;
    let throttled = expected & state;
    if missing == 0 && throttled == 0 {
        return None;
    }
    Some(format!(
        "expected control 0x{expected:X} with state 0, read control 0x{control:X} state \
         0x{state:X} (missing 0x{missing:X}, throttled 0x{throttled:X})"
    ))
}

#[cfg(windows)]
fn set_process_power_throttling(control: u32) -> windows::core::Result<()> {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_STATE,
    };

    let state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: control,
        StateMask: 0,
    };
    unsafe {
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const core::ffi::c_void,
            core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )
    }
}

#[cfg(windows)]
fn read_process_power_throttling() -> windows::core::Result<(u32, u32)> {
    use windows::Win32::System::Threading::{
        GetCurrentProcess, GetProcessInformation, ProcessPowerThrottling,
        PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_STATE,
    };

    let mut state = PROCESS_POWER_THROTTLING_STATE {
        Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: 0,
        StateMask: 0,
    };
    unsafe {
        GetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &mut state as *mut _ as *mut core::ffi::c_void,
            core::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        )?;
    }
    Ok((state.ControlMask, state.StateMask))
}

/// Opts the process out of EcoQoS execution-speed throttling and asks Windows 11 to honour the
/// 1 ms timer request while the window is minimised or occluded, then reads the state back: a
/// call can succeed whatever the kernel kept.
#[cfg(windows)]
fn configure_process_power_throttling() {
    let combined = super::tune::on("process_power_throttling_combined");
    let outcome = plan_power_throttling(combined, |control| {
        match set_process_power_throttling(control) {
            Ok(()) => true,
            Err(e) => {
                tprintln!(
                    "[power] SetProcessInformation(ProcessPowerThrottling, control 0x{control:X}) \
                     refused: {e}"
                );
                false
            }
        }
    });
    if outcome.execution_speed {
        tprintln!("process exempted from power throttling");
    } else {
        teprintln!("power-throttling exemption failed");
    }
    if outcome.timer_resolution {
        tprintln!("timer resolution honored in background (Win11)");
    } else {
        tprintln!("timer-resolution background opt-in unavailable (pre-Win11?)");
    }

    let expected = if outcome.execution_speed {
        POWER_EXECUTION_SPEED
    } else {
        0
    } | if outcome.timer_resolution {
        POWER_IGNORE_TIMER_RESOLUTION
    } else {
        0
    };
    match read_process_power_throttling() {
        Ok((control, state)) => match power_readback_problem(expected, control, state) {
            None => tprintln!(
                "[power] throttling state read back: control 0x{control:X} state 0x{state:X} \
                 ({} call{})",
                if combined { "one" } else { "two" },
                if combined { "" } else { "s" }
            ),
            Some(problem) => teprintln!(
                "[power] throttling state does not hold what was set: {problem} ({} call{})",
                if combined { "one" } else { "two" },
                if combined { "" } else { "s" }
            ),
        },
        Err(e) => tprintln!("[power] throttling state read-back unavailable: {e}"),
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

/// Counts MMCSS registration failures of one kind and decides which ones are worth a log line.
struct MmcssFailures {
    count: AtomicU64,
    last_code: AtomicI32,
}

impl MmcssFailures {
    const fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            last_code: AtomicI32::new(0),
        }
    }

    /// Counts one failure with `code` (the `GetLastError` HRESULT). Returns the running count and
    /// whether to log it: the first, each power of two (so a quota being exhausted shows as a
    /// growing number, not one line), and any failure whose code differs from the previous one.
    fn note(&self, code: i32) -> (u64, bool) {
        let n = self.count.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = self.last_code.swap(code, Ordering::Relaxed);
        (n, n.is_power_of_two() || previous != code)
    }
}

/// Transport-thread (`tune_transport_thread`) registrations that failed.
static TRANSPORT_MMCSS_FAILURES: MmcssFailures = MmcssFailures::new();
/// Capture / encode / repeater registrations for which every task name failed.
static REALTIME_MMCSS_FAILURES: MmcssFailures = MmcssFailures::new();

/// The `transport_thread_ecoqos_optout` and `transport_thread_pcores` candidates, read once.
#[cfg(windows)]
fn transport_thread_candidates() -> (bool, bool) {
    use std::sync::OnceLock;
    static C: OnceLock<(bool, bool)> = OnceLock::new();
    *C.get_or_init(|| {
        (
            super::tune::candidate("transport_thread_ecoqos_optout"),
            super::tune::candidate("transport_thread_pcores"),
        )
    })
}

/// Tunes a tokio transport worker. Called from `on_thread_start`, so once per worker and, since
/// tokio runs that hook on its blocking-pool threads too, once per blocking thread.
///
/// The MMCSS handle is parked in a thread-local that reverts when the worker exits: dropped
/// without `AvRevertMmThreadCharacteristics`, a host that starts and stops many streams leaks
/// registrations until every MMCSS registration fails, the capture thread's included. A failure
/// (MMCSS service disabled, per-process task quota spent) is counted and logged with its code.
///
/// Two candidates, both off by default: `transport_thread_ecoqos_optout` adds the per-thread
/// EcoQoS opt-out the capture and encode threads have, and `transport_thread_pcores` confines the
/// thread to the performance cores (which it then shares with capture and encode).
#[cfg(windows)]
pub fn tune_transport_thread() {
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;

    use windows::core::w;
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, GetCurrentThread, SetThreadPriority,
        THREAD_PRIORITY_ABOVE_NORMAL,
    };

    thread_local! {
        static MMCSS: RefCell<Option<ThreadTuning>> = const { RefCell::new(None) };
    }
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
        result => {
            let error = match result {
                Err(e) => e,
                Ok(_) => windows::core::Error::from_thread(),
            };
            let code = error.code().0;
            let (count, log) = TRANSPORT_MMCSS_FAILURES.note(code);
            if log {
                teprintln!(
                    "MMCSS registration failed for a transport thread (0x{code:08X}: {error}); \
                     {count} transport thread(s) so far run unprioritised (is the MMCSS service \
                     running, or is the per-process task quota spent?)"
                );
            }
        }
    }

    if let Err(e) = unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) } {
        if !PRIORITY_WARNED.swap(true, Ordering::Relaxed) {
            teprintln!("SetThreadPriority(ABOVE_NORMAL) failed on a transport thread: {e}");
        }
    }

    let (ecoqos, pcores) = transport_thread_candidates();
    if ecoqos {
        exempt_thread_from_power_throttling();
    }
    if pcores {
        confine_to_performance_cores();
    }
}

/// [`tune_transport_thread`] the first time it is called on this thread, and nothing after:
/// the `transport_mmcss_lazy` park hook, which runs on every park.
pub fn tune_transport_thread_once() {
    thread_local! {
        static TUNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if !TUNED.with(|t| t.replace(true)) {
        tune_transport_thread();
    }
}

/// The wide form of each task name `mmcss_plan` can return.
#[cfg(windows)]
fn mmcss_task_wide(name: &str) -> Option<windows::core::PCWSTR> {
    use windows::core::w;
    match name {
        "Pro Audio" => Some(w!("Pro Audio")),
        "Games" => Some(w!("Games")),
        "Playback" => Some(w!("Playback")),
        _ => None,
    }
}

/// Registers the calling thread with the first MMCSS task in `tasks` that accepts it. Every
/// refusal's error code is kept: they are logged with the task that did register, or all
/// together (and counted) when none did.
#[cfg(windows)]
fn register_mmcss(tasks: &[&'static str], critical: bool) -> isize {
    use windows::Win32::System::Threading::{
        AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority, AVRT_PRIORITY_CRITICAL,
    };

    let mut refused: Vec<String> = Vec::new();
    let mut last_code = 0i32;
    for &name in tasks {
        let Some(wide_name) = mmcss_task_wide(name) else {
            continue;
        };
        let mut task_index: u32 = 0;
        match unsafe { AvSetMmThreadCharacteristicsW(wide_name, &mut task_index) } {
            Ok(h) if !h.is_invalid() => {
                if refused.is_empty() {
                    tprintln!("MMCSS registered ({name})");
                } else {
                    teprintln!(
                        "MMCSS registered ({name}) after refusals: {}",
                        refused.join("; ")
                    );
                }
                if critical {
                    match unsafe { AvSetMmThreadPriority(h, AVRT_PRIORITY_CRITICAL) } {
                        Ok(()) => tprintln!("MMCSS priority raised to CRITICAL ({name})"),
                        Err(e) => teprintln!("AvSetMmThreadPriority(CRITICAL) failed: {e}"),
                    }
                }
                return h.0 as isize;
            }
            result => {
                let error = match result {
                    Err(e) => e,
                    Ok(_) => windows::core::Error::from_thread(),
                };
                last_code = error.code().0;
                refused.push(format!("{name}: 0x{last_code:08X} {error}"));
            }
        }
    }

    let (count, _) = REALTIME_MMCSS_FAILURES.note(last_code);
    teprintln!(
        "all MMCSS task names failed ({}); {count} capture/encode registration(s) refused so far",
        refused.join("; ")
    );
    0
}

/// Sets the calling thread's priority and **disables** its dynamic priority boost
/// (`SetThreadPriorityBoost(.., TRUE)` means "disable"). At `TIME_CRITICAL` the boost is inert
/// anyway, and while MMCSS holds the thread its priority comes from MMCSS, not from here.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_bits_match_the_sdk() {
        use windows::Win32::System::Threading::{
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION,
        };
        assert_eq!(
            POWER_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_EXECUTION_SPEED
        );
        assert_eq!(
            POWER_IGNORE_TIMER_RESOLUTION,
            PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION
        );
    }

    /// Runs the plan against a kernel that stores each call's control mask whole, optionally
    /// refusing the timer bit as a Windows without `IGNORE_TIMER_RESOLUTION` would. Returns the
    /// outcome, the stored mask and the number of calls.
    fn run_power_plan(
        combined: bool,
        supports_timer: bool,
    ) -> (PowerThrottlingOutcome, u32, usize) {
        let mut stored = 0u32;
        let mut calls = 0usize;
        let outcome = plan_power_throttling(combined, |mask| {
            calls += 1;
            if mask & POWER_IGNORE_TIMER_RESOLUTION != 0 && !supports_timer {
                return false;
            }
            stored = mask;
            true
        });
        (outcome, stored, calls)
    }

    #[test]
    fn combined_power_call_keeps_both_bits() {
        let (outcome, stored, calls) = run_power_plan(true, true);
        assert_eq!(calls, 1);
        assert_eq!(
            stored,
            POWER_EXECUTION_SPEED | POWER_IGNORE_TIMER_RESOLUTION
        );
        assert!(outcome.execution_speed && outcome.timer_resolution);
        assert_eq!(power_readback_problem(stored, stored, 0), None);
    }

    #[test]
    fn two_separate_power_calls_lose_the_timer_bit_on_a_whole_state_kernel() {
        // SE_TUNE_OFF=process_power_throttling_combined: both calls "succeed", and only the
        // read-back shows the timer bit is gone
        let (outcome, stored, calls) = run_power_plan(false, true);
        assert_eq!(calls, 2);
        assert!(outcome.execution_speed && outcome.timer_resolution);
        assert_eq!(stored, POWER_EXECUTION_SPEED);
        let expected = POWER_EXECUTION_SPEED | POWER_IGNORE_TIMER_RESOLUTION;
        assert!(power_readback_problem(expected, stored, 0).is_some());
    }

    #[test]
    fn combined_power_call_falls_back_to_execution_speed_alone() {
        let (outcome, stored, calls) = run_power_plan(true, false);
        assert_eq!(calls, 2);
        assert_eq!(stored, POWER_EXECUTION_SPEED);
        assert!(outcome.execution_speed);
        assert!(!outcome.timer_resolution);
    }

    #[test]
    fn readback_flags_a_throttled_bit() {
        assert!(power_readback_problem(POWER_EXECUTION_SPEED, POWER_EXECUTION_SPEED, 1).is_some());
        assert_eq!(power_readback_problem(0, 0, 0), None);
    }

    static TUNING_STEP_RUNS: AtomicU64 = AtomicU64::new(0);

    fn counting_tuning_steps() {
        TUNING_STEP_RUNS.fetch_add(1, Ordering::SeqCst);
    }

    /// Drives the same static `apply_process_tuning` uses. Another test in this binary may have
    /// reached the guard first through `Streamer::prepare`, in which case the stand-in never runs:
    /// hence `<= 1`, with the guard required to be complete either way.
    #[test]
    fn process_tuning_runs_once() {
        for _ in 0..3 {
            apply_process_tuning_with(counting_tuning_steps);
        }
        assert!(TUNING_STEP_RUNS.load(Ordering::SeqCst) <= 1);
        assert!(PROCESS_TUNING_APPLIED.is_completed());
    }

    #[test]
    fn mmcss_failures_are_counted_and_logged_sparingly() {
        let f = MmcssFailures::new();
        let quota = 0x8007_05AAu32 as i32;
        assert_eq!(f.note(quota), (1, true));
        assert_eq!(f.note(quota), (2, true));
        assert_eq!(f.note(quota), (3, false));
        assert_eq!(f.note(quota), (4, true));
        assert_eq!(f.note(quota), (5, false));
        // a different code always logs
        assert_eq!(f.note(0x8007_0005u32 as i32), (6, true));
    }

    #[test]
    fn mmcss_plan_keeps_the_frame_path_on_pro_audio() {
        assert_eq!(
            mmcss_plan(MmcssRole::Realtime),
            (&["Pro Audio", "Games", "Playback"][..], true)
        );
        // default: the repeater registers like the frame path
        assert_eq!(
            mmcss_plan(MmcssRole::Timer { playback: false }),
            mmcss_plan(MmcssRole::Realtime)
        );
        assert_eq!(
            mmcss_plan(MmcssRole::Timer { playback: true }),
            (&["Playback"][..], false)
        );
        for role in [
            MmcssRole::Realtime,
            MmcssRole::Timer { playback: false },
            MmcssRole::Timer { playback: true },
        ] {
            for name in mmcss_plan(role).0 {
                assert!(mmcss_task_wide(name).is_some(), "{name} has no wide form");
            }
        }
    }

    #[test]
    fn cpu_set_tier_prefers_masks_then_ids_then_nothing() {
        assert_eq!(pick_cpu_set_tier(true, true), Some(CpuSetTier::Masks));
        assert_eq!(pick_cpu_set_tier(false, true), Some(CpuSetTier::Ids));
        assert_eq!(pick_cpu_set_tier(false, false), None);
    }

    #[test]
    fn performance_selection_on_hybrid_and_homogeneous_cpus() {
        let set = |id, group, index, class| CpuSet {
            id,
            group,
            index,
            class,
        };
        let homogeneous = [set(256, 0, 0, 0), set(257, 0, 1, 0)];
        assert_eq!(select_performance_cores(&homogeneous), None);
        assert_eq!(select_performance_cores(&[]), None);

        let hybrid = [
            set(256, 0, 0, 1),
            set(257, 0, 1, 1),
            set(258, 0, 2, 0),
            set(259, 1, 3, 1),
        ];
        assert_eq!(
            select_performance_cores(&hybrid),
            Some(PerformanceSelection {
                class: 1,
                masks: vec![(0, 0b11), (1, 0b1000)],
                ids: vec![256, 257, 259],
            })
        );
    }

    /// The live machine's selection is consistent: one id per masked logical processor.
    #[test]
    fn live_performance_cores_ids_match_masks() {
        match performance_cores() {
            None => crate::test_support::skip_because(
                "live_performance_cores_ids_match_masks",
                "this CPU reports a single EfficiencyClass",
            ),
            Some(cores) => {
                let masked: u32 = cores
                    .masks
                    .iter()
                    .map(|m| (m.Mask as u64).count_ones())
                    .sum();
                assert_eq!(masked as usize, cores.ids.len());
            }
        }
    }

    /// Exports that do not exist on the Windows 10 2004 floor. Imported through the `windows`
    /// crate (raw-dylib) one becomes a load-time import, and an unresolved load-time import stops
    /// the whole executable before `main`. Resolve such a function with `GetProcAddress` instead
    /// and list its name here.
    const POST_FLOOR_EXPORTS: &[&str] = &["SetThreadSelectedCpuSetMasks"];

    #[test]
    fn no_post_floor_import_is_bound_at_load_time() {
        // keep the tuning entry points in this binary, so whatever they import is in the table
        std::hint::black_box(tune_current_thread as fn() -> ThreadTuning);
        std::hint::black_box(tune_timer_thread as fn() -> ThreadTuning);
        std::hint::black_box(tune_transport_thread as fn());
        std::hint::black_box(apply_process_tuning as fn());

        let exe = std::env::current_exe().expect("current_exe");
        let bytes = std::fs::read(&exe).expect("read the test binary");
        let imports = pe_imports(&bytes).expect("parse the import table");
        assert!(
            imports
                .iter()
                .any(|(dll, f)| dll.eq_ignore_ascii_case("kernel32.dll") && f == "GetCurrentThread"),
            "the parser found no kernel32!GetCurrentThread, so it is not reading the table"
        );
        let bad: Vec<&(String, String)> = imports
            .iter()
            .filter(|(_, f)| POST_FLOOR_EXPORTS.contains(&f.as_str()))
            .collect();
        assert!(
            bad.is_empty(),
            "load-time imports newer than Windows 10 2004: {bad:?}"
        );
    }

    /// `(dll, function)` for every by-name entry in a PE32+ image's import directory.
    fn pe_imports(b: &[u8]) -> Result<Vec<(String, String)>, String> {
        let u16_at = |o: usize| -> Result<u16, String> {
            b.get(o..o + 2)
                .map(|s| u16::from_le_bytes([s[0], s[1]]))
                .ok_or(format!("u16 at {o} out of range"))
        };
        let u32_at = |o: usize| -> Result<u32, String> {
            b.get(o..o + 4)
                .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
                .ok_or(format!("u32 at {o} out of range"))
        };
        let u64_at = |o: usize| -> Result<u64, String> {
            b.get(o..o + 8)
                .map(|s| {
                    let mut a = [0u8; 8];
                    a.copy_from_slice(s);
                    u64::from_le_bytes(a)
                })
                .ok_or(format!("u64 at {o} out of range"))
        };
        let cstr_at = |o: usize| -> Result<String, String> {
            let tail = b.get(o..).ok_or(format!("string at {o} out of range"))?;
            let end = tail
                .iter()
                .position(|&c| c == 0)
                .ok_or("unterminated string")?;
            Ok(String::from_utf8_lossy(&tail[..end]).into_owned())
        };

        let pe = u32_at(0x3C)? as usize;
        if b.get(pe..pe + 4) != Some(b"PE\0\0") {
            return Err("no PE signature".into());
        }
        let coff = pe + 4;
        let sections = u16_at(coff + 2)? as usize;
        let optional_size = u16_at(coff + 16)? as usize;
        let optional = coff + 20;
        if u16_at(optional)? != 0x20B {
            return Err("not PE32+".into());
        }
        // data directory 1 (imports) of a PE32+ optional header
        let import_rva = u32_at(optional + 112 + 8)? as usize;
        let section_table = optional + optional_size;
        let to_offset = |rva: usize| -> Result<usize, String> {
            for i in 0..sections {
                let s = section_table + i * 40;
                let size = u32_at(s + 8)?.max(u32_at(s + 16)?) as usize;
                let va = u32_at(s + 12)? as usize;
                let raw = u32_at(s + 20)? as usize;
                if rva >= va && rva < va + size {
                    return Ok(rva - va + raw);
                }
            }
            Err(format!("rva 0x{rva:X} is in no section"))
        };

        let mut out = Vec::new();
        if import_rva == 0 {
            return Ok(out);
        }
        let mut descriptor = to_offset(import_rva)?;
        loop {
            let original_thunk = u32_at(descriptor)? as usize;
            let name_rva = u32_at(descriptor + 12)? as usize;
            let first_thunk = u32_at(descriptor + 16)? as usize;
            if original_thunk == 0 && name_rva == 0 && first_thunk == 0 {
                break;
            }
            let dll = cstr_at(to_offset(name_rva)?)?;
            let thunks = if original_thunk != 0 {
                original_thunk
            } else {
                first_thunk
            };
            let mut thunk = to_offset(thunks)?;
            loop {
                let entry = u64_at(thunk)?;
                if entry == 0 {
                    break;
                }
                if entry & (1 << 63) == 0 {
                    // IMAGE_IMPORT_BY_NAME: a u16 hint, then the name
                    let name = cstr_at(to_offset(entry as usize)? + 2)?;
                    out.push((dll.clone(), name));
                }
                thunk += 8;
            }
            descriptor += 20;
        }
        Ok(out)
    }
}
