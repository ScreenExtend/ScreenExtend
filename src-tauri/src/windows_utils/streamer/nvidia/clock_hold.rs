//! Keeps the NVIDIA GPU out of its idle performance state while a live NVENC session streams.
//!
//! A 1080p60 stream is too light for the driver's clock governor: on an RTX 3080 Ti Laptop the
//! GPU sat in P8 (210 MHz graphics) and NVENC took 5.7 ms a frame against 1.7 ms at full clocks.
//! Nothing a process can set for itself moves the governor (the per-application profile is a
//! persistent driver setting, the owner's to make), so a thread keeps the GPU busy with copies
//! between two textures on a device of its own. That costs about 16 W, so it copies only on AC
//! power. `SE_GPU_CLOCK_HOLD=0` turns it off and `=1` forces it on. Not measured on other GPUs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

use super::encoder::{create_hold_textures, create_nvidia_d3d11_device};
use crate::streamer::lock::LockExt;
use crate::windows_utils::streamer::tune;

/// Eight 16 MiB copies a millisecond is the knee: P8 to P3 and stamp p50 26.5 to 19.2 ms at
/// 1080p60 for +16 W of board power. Sixteen gain another 0.5 ms for another 9 W.
const COPIES_PER_TICK: u32 = 8;
const TICK: Duration = Duration::from_millis(1);

/// How often a running hold re-reads the power source (`clock_hold_follows_power`).
const POWER_CHECK: Duration = Duration::from_secs(1);

/// The sleep between checks while the hold is paused on battery: no copies and no 1 ms wakeups,
/// but still quick to notice the last session ending.
const PAUSED_TICK: Duration = Duration::from_millis(100);

/// The GPU thread priority the hold's device runs at under `clock_hold_low_gpu_priority`: the
/// lowest `IDXGIDevice::SetGPUThreadPriority` accepts, so its copies yield to the stage copy and
/// to DWM's composition while still being load the clock governor sees.
const LOW_GPU_PRIORITY: i32 = -7;

struct Holder {
    users: usize,
    stop: Option<Arc<AtomicBool>>,
}

static HOLDER: Mutex<Holder> = Mutex::new(Holder {
    users: 0,
    stop: None,
});

/// Released when the session that took it ends; the last release stops the thread.
pub struct Guard(());

impl Drop for Guard {
    fn drop(&mut self) {
        let mut h = HOLDER.lock_recover();
        h.users = h.users.saturating_sub(1);
        if h.users == 0 {
            if let Some(stop) = h.stop.take() {
                stop.store(true, Ordering::Relaxed);
                tprintln!("gpu clock hold: stopped (no live NVENC session)");
            }
        }
    }
}

/// What `SE_GPU_CLOCK_HOLD` asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    /// `=0`: never.
    ForcedOff,
    /// `=1`: always, battery included.
    ForcedOn,
    /// Unset (or anything else): on AC power only.
    Auto,
}

pub(crate) fn policy_from(var: Option<&str>) -> Policy {
    match var {
        Some("0") => Policy::ForcedOff,
        Some("1") => Policy::ForcedOn,
        _ => Policy::Auto,
    }
}

fn policy() -> Policy {
    policy_from(crate::envcfg::var("SE_GPU_CLOCK_HOLD").ok().as_deref())
}

/// Whether the copies should be running now, and why (for the log).
pub(crate) fn decide(policy: Policy, on_battery: bool) -> (bool, &'static str) {
    match policy {
        Policy::ForcedOff => (false, "off: SE_GPU_CLOCK_HOLD=0"),
        Policy::ForcedOn => (true, "on: SE_GPU_CLOCK_HOLD=1"),
        Policy::Auto if on_battery => (false, "off: running on battery"),
        Policy::Auto => (true, "on: AC power"),
    }
}

/// Whether the hold should run for a session starting now, and why (for the log).
pub fn wanted() -> (bool, &'static str) {
    let p = policy();
    // only `Auto` consults the power source
    decide(p, p == Policy::Auto && on_battery())
}

/// `ACLineStatus` 0 is battery; 1 is AC and 255 unknown (a desktop without a battery reports 1).
fn on_battery() -> bool {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.ACLineStatus == 0
}

/// How a hold thread behaves, decided once when it starts.
#[derive(Clone, Copy, Debug)]
struct Plan {
    policy: Policy,
    /// `clock_hold_follows_power`: re-read the power source while running, pausing on battery
    /// and resuming on AC. Off, the decision taken at the first acquire stands for the life of
    /// the thread, so a laptop unplugged mid-session keeps drawing ~16 W.
    follow_power: bool,
    /// `clock_hold_low_gpu_priority`.
    low_priority: bool,
    /// Whether to copy from the first tick.
    copying: bool,
}

/// Take a hold for one live session, starting the thread if this is the first. `None` when the
/// policy says off.
pub fn acquire() -> Option<Guard> {
    let policy = policy();
    let follow_power = tune::on("clock_hold_follows_power");
    let (on, why) = decide(policy, policy == Policy::Auto && on_battery());
    // following the power source, a session that starts on battery still registers: the thread
    // starts paused and begins copying when the machine is plugged in; only `=0` refuses
    let register = on || (follow_power && policy == Policy::Auto);
    if !register {
        tprintln!("gpu clock hold: {why}");
        return None;
    }
    let mut h = HOLDER.lock_recover();
    h.users += 1;
    if h.stop.is_none() {
        let plan = Plan {
            policy,
            follow_power,
            low_priority: tune::candidate("clock_hold_low_gpu_priority"),
            copying: on,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        match std::thread::Builder::new()
            .name("gpu-clock-hold".into())
            .spawn(move || run(s, plan))
        {
            Ok(_) => {
                h.stop = Some(stop);
                if on {
                    tprintln!(
                        "gpu clock hold: {why}; {COPIES_PER_TICK} copies every {} ms",
                        TICK.as_millis()
                    );
                } else {
                    tprintln!("gpu clock hold: {why}; paused until the machine is on AC power");
                }
            }
            Err(e) => teprintln!("gpu clock hold: thread spawn failed: {e}"),
        }
    }
    Some(Guard(()))
}

/// The hold's own device and the two textures it copies between.
struct Hold {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    a: ID3D11Texture2D,
    b: ID3D11Texture2D,
}

fn open_hold(low_priority: bool) -> Result<Hold, String> {
    let (device, context) =
        create_nvidia_d3d11_device().map_err(|e| format!("no NVIDIA device ({e:?})"))?;
    let (a, b) = create_hold_textures(&device).map_err(|e| format!("textures ({e:?})"))?;
    if low_priority {
        match device
            .cast::<IDXGIDevice>()
            .and_then(|d| unsafe { d.SetGPUThreadPriority(LOW_GPU_PRIORITY) })
        {
            Ok(()) => tprintln!("gpu clock hold: device GPU thread priority {LOW_GPU_PRIORITY}"),
            Err(e) => teprintln!("gpu clock hold: SetGPUThreadPriority({LOW_GPU_PRIORITY}): {e}"),
        }
    }
    Ok(Hold {
        _device: device,
        context,
        a,
        b,
    })
}

/// Whether a hold that could not open its device waits and tries again rather than ending.
/// Only a hold that follows the power source drops and reopens its device mid-session (a
/// resume on AC, perhaps while a hybrid laptop's dGPU is still waking); every other hold opens
/// once, at start.
fn retries_open(plan: &Plan) -> bool {
    plan.follow_power && plan.policy == Policy::Auto
}

/// Clear the holder's stop flag if it is still the one this thread was started with, so the
/// next `acquire` starts a new thread. A flag a `Guard` has already taken, or one belonging to
/// a newer thread, is left alone.
fn forget_thread(h: &mut Holder, stop: &Arc<AtomicBool>) -> bool {
    match &h.stop {
        Some(s) if Arc::ptr_eq(s, stop) => {
            h.stop = None;
            true
        }
        _ => false,
    }
}

fn run(stop: Arc<AtomicBool>, plan: Plan) {
    run_hold(&stop, plan);
    // a thread that ended on its own (it could not open its device) must clear the holder:
    // `acquire` only starts a thread when there is none, so the hold would otherwise stay off
    // until every session had ended
    if !stop.load(Ordering::Relaxed) && forget_thread(&mut HOLDER.lock_recover(), &stop) {
        tprintln!("gpu clock hold: not holding; the next NVENC session will try again");
    }
}

fn run_hold(stop: &AtomicBool, plan: Plan) {
    let mut copying = plan.copying;
    let mut hold: Option<Hold> = None;
    let mut next_check = Instant::now() + POWER_CHECK;
    // while the device cannot be opened: when to try again, and whether that was logged
    let mut retry_at: Option<Instant> = None;
    let mut open_failing = false;
    while !stop.load(Ordering::Relaxed) {
        if plan.follow_power && plan.policy == Policy::Auto && Instant::now() >= next_check {
            next_check = Instant::now() + POWER_CHECK;
            let (now, why) = decide(plan.policy, on_battery());
            if now != copying {
                copying = now;
                retry_at = None;
                if copying {
                    tprintln!("gpu clock hold: resumed ({why})");
                } else {
                    // release the device and its 32 MiB while paused; resuming recreates them
                    hold = None;
                    tprintln!("gpu clock hold: paused ({why})");
                }
            }
        }
        if !copying || retry_at.is_some_and(|t| Instant::now() < t) {
            std::thread::sleep(PAUSED_TICK);
            continue;
        }
        if hold.is_none() {
            match open_hold(plan.low_priority) {
                Ok(h) => {
                    if open_failing {
                        tprintln!("gpu clock hold: device opened; holding");
                    }
                    open_failing = false;
                    retry_at = None;
                    hold = Some(h);
                }
                Err(e) if retries_open(&plan) => {
                    if !open_failing {
                        teprintln!(
                            "gpu clock hold: {e}; paused, trying again every {} s",
                            POWER_CHECK.as_secs()
                        );
                    }
                    open_failing = true;
                    retry_at = Some(Instant::now() + POWER_CHECK);
                    continue;
                }
                Err(e) => {
                    teprintln!("gpu clock hold: {e}; not holding");
                    return;
                }
            }
        }
        let Some(h) = &hold else {
            continue;
        };
        unsafe {
            for i in 0..COPIES_PER_TICK {
                if i % 2 == 0 {
                    h.context.CopyResource(&h.b, &h.a);
                } else {
                    h.context.CopyResource(&h.a, &h.b);
                }
            }
            h.context.Flush();
        }
        std::thread::sleep(TICK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_variable_maps_to_a_policy() {
        assert_eq!(policy_from(Some("0")), Policy::ForcedOff);
        assert_eq!(policy_from(Some("1")), Policy::ForcedOn);
        assert_eq!(policy_from(None), Policy::Auto);
        assert_eq!(policy_from(Some("yes")), Policy::Auto);
    }

    #[test]
    fn only_an_automatic_hold_follows_the_power_source() {
        assert!(decide(Policy::Auto, false).0, "AC power holds");
        assert!(!decide(Policy::Auto, true).0, "battery pauses");
        assert!(decide(Policy::ForcedOn, true).0, "=1 holds on battery");
        assert!(!decide(Policy::ForcedOff, false).0, "=0 never holds");
    }

    fn plan(policy: Policy, follow_power: bool) -> Plan {
        Plan {
            policy,
            follow_power,
            low_priority: false,
            copying: true,
        }
    }

    /// A hold that drops its device on battery must survive failing to reopen it on AC.
    #[test]
    fn only_a_hold_that_follows_the_power_source_retries_its_device() {
        assert!(retries_open(&plan(Policy::Auto, true)));
        assert!(!retries_open(&plan(Policy::Auto, false)));
        assert!(!retries_open(&plan(Policy::ForcedOn, true)));
        assert!(!retries_open(&plan(Policy::ForcedOn, false)));
    }

    /// A thread that ends on its own clears the holder so the next acquire starts another, and
    /// never clears a flag that is not its own.
    #[test]
    fn a_thread_that_ends_early_lets_the_next_acquire_restart_it() {
        let mine = Arc::new(AtomicBool::new(false));
        let newer = Arc::new(AtomicBool::new(false));
        let mut h = Holder {
            users: 2,
            stop: Some(Arc::clone(&mine)),
        };
        assert!(forget_thread(&mut h, &mine));
        assert!(
            h.stop.is_none(),
            "acquire starts a thread only when stop is None"
        );
        assert_eq!(h.users, 2, "the sessions still hold their guards");

        h.stop = Some(Arc::clone(&newer));
        assert!(!forget_thread(&mut h, &mine), "a newer thread's flag");
        assert!(h.stop.as_ref().is_some_and(|s| Arc::ptr_eq(s, &newer)));

        h.stop = None;
        assert!(
            !forget_thread(&mut h, &mine),
            "already taken by the last Guard"
        );
    }
}
