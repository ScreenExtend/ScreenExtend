//! Keeps the NVIDIA GPU out of its idle performance state while a live NVENC session streams.
//!
//! The capture, the ring copy, DWM's composition of the virtual display and NVENC all run on the
//! NVIDIA adapter on this kind of machine, and a 1080p60 stream is too light a load for the
//! driver's clock governor: measured on an RTX 3080 Ti Laptop, the GPU sat in P8 for the whole
//! session — 210 MHz graphics, 555 MHz video clock — and NVENC took 5.7 ms a frame against 1.7 ms
//! at full clocks. Everything on the GPU before the encode (composition, WGC delivery, the copy)
//! was slow the same way. Nothing an application can set for its own process moves the governor
//! (the per-application "prefer maximum performance" profile is a persistent change to the
//! driver's settings database, which is the owner's to make), so this does what the governor
//! responds to: it keeps the GPU busy with copies between two textures on a device of its own.
//!
//! Measured (`docs/overhaul-new/POST-REBOOT.md`, Part 4), 1080p60, stamp metric p50, four
//! interleaved windows per arm inside one join, GPU board power from nvidia-smi:
//!
//! | copies per 1 ms | P-state | graphics / video MHz | p50 ms | board W |
//! |---|---|---|---|---|
//! | none (before) | P8 | 210 / 555 | 26.5 | 18 |
//! | 4 | P5 | 860 / 740 | 21.7 | 27 |
//! | 8 (this) | P3 | 1243 / 1082 | 19.2 | 34 |
//! | 16 | P2 | 1498 / 1316 | 18.7 | 43 |
//!
//! Eight 16 MiB copies a millisecond is the knee: about nine tenths of the gain for about
//! +16 W. That is a laptop's battery, so the hold is on only while the machine is on AC power
//! when the session starts; `SE_GPU_CLOCK_HOLD=0` turns it off and `=1` forces it on. It runs
//! only while at least one live NVENC ring session holds a [`Guard`], and stops with the last.
//! Not measured on any other GPU; not used by the AMF, Quick Sync or x264 paths.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};

use super::encoder::{create_hold_textures, create_nvidia_d3d11_device};
use crate::streamer::lock::LockExt;

/// Copies per tick and the tick, from the dose measurement above.
const COPIES_PER_TICK: u32 = 8;
const TICK: Duration = Duration::from_millis(1);

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

/// Whether the hold should run for a session starting now, and why (for the log).
pub fn wanted() -> (bool, &'static str) {
    match std::env::var("SE_GPU_CLOCK_HOLD").as_deref() {
        Ok("0") => (false, "off: SE_GPU_CLOCK_HOLD=0"),
        Ok("1") => (true, "on: SE_GPU_CLOCK_HOLD=1"),
        _ if on_battery() => (false, "off: running on battery"),
        _ => (true, "on: AC power"),
    }
}

/// `ACLineStatus` 0 is battery; 1 is AC and 255 unknown (a desktop without a battery reports 1).
fn on_battery() -> bool {
    let mut s = SYSTEM_POWER_STATUS::default();
    unsafe { GetSystemPowerStatus(&mut s) }.is_ok() && s.ACLineStatus == 0
}

/// Take a hold for one live session, starting the thread if this is the first. `None` when the
/// policy says off.
pub fn acquire() -> Option<Guard> {
    let (on, why) = wanted();
    if !on {
        tprintln!("gpu clock hold: {why}");
        return None;
    }
    let mut h = HOLDER.lock_recover();
    h.users += 1;
    if h.stop.is_none() {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        match std::thread::Builder::new()
            .name("gpu-clock-hold".into())
            .spawn(move || run(s))
        {
            Ok(_) => {
                h.stop = Some(stop);
                tprintln!(
                    "gpu clock hold: {why}; {COPIES_PER_TICK} copies every {} ms",
                    TICK.as_millis()
                );
            }
            Err(e) => teprintln!("gpu clock hold: thread spawn failed: {e}"),
        }
    }
    Some(Guard(()))
}

fn run(stop: Arc<AtomicBool>) {
    let (device, context) = match create_nvidia_d3d11_device() {
        Ok(d) => d,
        Err(e) => {
            teprintln!("gpu clock hold: no NVIDIA device ({e:?}); not holding");
            return;
        }
    };
    let (a, b) = match create_hold_textures(&device) {
        Ok(t) => t,
        Err(e) => {
            teprintln!("gpu clock hold: textures ({e:?}); not holding");
            return;
        }
    };
    while !stop.load(Ordering::Relaxed) {
        unsafe {
            for i in 0..COPIES_PER_TICK {
                if i % 2 == 0 {
                    context.CopyResource(&b, &a);
                } else {
                    context.CopyResource(&a, &b);
                }
            }
            context.Flush();
        }
        std::thread::sleep(TICK);
    }
}
