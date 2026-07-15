//! Linux injection backend — STUB (PRD §5.7).
//!
//! TODO: open /dev/uinput via `input-linux` or `uinput` crate.
//!   - Mouse/keyboard: EV_KEY / EV_REL / EV_ABS on a virtual device.
//!   - Multi-touch: create a MT type-B device (ABS_MT_SLOT, ABS_MT_TRACKING_ID,
//!     ABS_MT_POSITION_X/Y, ABS_MT_PRESSURE) and emit one SYN_REPORT per frame —
//!     one syscall per contact update, one SYN to commit, matching the touch model.
//!   - Pen: ABS_PRESSURE + ABS_TILT_X/Y on a tablet-style device.
//!   - Prefer direct uinput over shelling out to xdotool/ydotool (per-call process
//!     spawn is milliseconds of pure overhead).
//!   - Wayland vs X11: uinput is kernel-level and works under both.

use super::protocol::InputEvent;

pub const NAME: &str = "linux-stub";

pub fn boost_thread() {}
pub fn tune_process() {}

pub struct Injector {
    warned: bool,
}

impl Injector {
    pub fn new(_device_name: Option<String>) -> Self {
        Injector { warned: false }
    }

    pub fn dispatch(&mut self, _ev: &InputEvent) {
        if !self.warned {
            log::warn!("Linux injection backend is a stub — events are dropped (see PRD §5.7)");
            self.warned = true;
        }
    }

    pub fn release_all(&mut self) {}
}
