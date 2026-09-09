//! Linux injection backend — a stub. It compiles and reports `Unsupported`; nothing is injected.
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
use super::ClientProfile;

pub const NAME: &str = "linux-stub";

pub fn probe() -> super::InputBackend {
    super::InputBackend::Unsupported
}

pub fn boost_thread() {}

pub struct Injector {
    warned: bool,
}

impl Injector {
    pub fn new(_client: ClientProfile) -> Self {
        Injector { warned: false }
    }

    pub fn dispatch(&mut self, _ev: &InputEvent) {
        if !self.warned {
            teprintln!(
                "remote control is not supported on Linux hosts yet; input from clients is ignored"
            );
            self.warned = true;
        }
    }

    pub fn release_all(&mut self) {}
}
