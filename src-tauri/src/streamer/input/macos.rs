//! macOS injection backend — STUB (PRD §5.6).
//!
//! TODO: CGEventCreateMouseEvent + CGEventPost(kCGHIDEventTap, ...)
//!       CGEventCreateKeyboardEvent for keys; CGEventKeyboardSetUnicodeString for text.
//!       Multi-touch: no public synthetic multi-touch API — pen/touch likely degrade to
//!       mouse, or require a private/virtual-HID driver. Flag as a known gap.
//!       Requires Accessibility permission (TCC).

use super::protocol::InputEvent;

pub const NAME: &str = "macos-stub";

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
            log::warn!("macOS injection backend is a stub — events are dropped (see PRD §5.6)");
            self.warned = true;
        }
    }

    pub fn release_all(&mut self) {}
}
