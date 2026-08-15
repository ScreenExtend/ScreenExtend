//! macOS injection backend — CoreGraphics event synthesis (`CGEventPost`).
//!
//! Mirrors the Windows backend's behavior wherever a public macOS API exists:
//! absolute + relative mouse, all five buttons, line/pixel/page wheel, keyboard
//! (scancode-equivalent virtual keys + Unicode fallback), text, clipboard set +
//! auto-paste, focus/visibility release, and per-display geometry targeting.
//!
//! Degraded (no public inject API on macOS): pen and touch collapse to a single
//! mouse pointer — pressure/tilt/twist/multi-touch are lost, exactly like the
//! Windows path when its synthetic pen/touch devices are unavailable. File drop
//! is unsupported (`WM_DROPFILES` has no macOS analog).
//!
//! Shortcut remap (per product decision): the client's Control key drives macOS
//! **Command**, and the client's Meta/Windows(/Super) key drives macOS
//! **Control** — so Ctrl+C on a Windows/Linux client becomes ⌘C on the Mac.
//!
//! Requires the Accessibility (TCC) grant, or `CGEventPost` silently no-ops
//! against other apps.

use std::collections::HashSet;
use std::ffi::c_void;
use std::time::{Duration, Instant};

use objc2_core_foundation::CGPoint;

use super::protocol::{btn, InputEvent, Lifecycle, MoveSample, Phase, SRC_MOUSE, SRC_PEN, SRC_TOUCH};
use super::DisplayRect;

pub const NAME: &str = "macos-cgevent";

// ─── CoreGraphics event FFI (no CGEvent feature in objc2-core-graphics) ──────
type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceCreate(state_id: i32) -> CGEventSourceRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        mouse_cursor_position: CGPoint,
        mouse_button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        keycode: u16,
        key_down: bool,
    ) -> CGEventRef;
    // Variadic in C: (source, units, wheelCount, wheel1[, wheel2[, wheel3]]).
    fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        ...
    ) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventKeyboardSetUnicodeString(event: CGEventRef, length: usize, unicode_string: *const u16);
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CFRelease(cf: *const c_void);
}

const HID_EVENT_TAP: u32 = 0; // kCGHIDEventTap
const SOURCE_STATE_HID: i32 = 1; // kCGEventSourceStateHIDSystemState

// CGEventType
const ET_LEFT_DOWN: u32 = 1;
const ET_LEFT_UP: u32 = 2;
const ET_RIGHT_DOWN: u32 = 3;
const ET_RIGHT_UP: u32 = 4;
const ET_MOUSE_MOVED: u32 = 5;
const ET_LEFT_DRAG: u32 = 6;
const ET_RIGHT_DRAG: u32 = 7;
const ET_OTHER_DOWN: u32 = 25;
const ET_OTHER_UP: u32 = 26;
const ET_OTHER_DRAG: u32 = 27;

// CGMouseButton (X1/X2 ride the "other" buttons 3/4)
const MB_LEFT: u32 = 0;
const MB_RIGHT: u32 = 1;
const MB_CENTER: u32 = 2;
const MB_X1: u32 = 3;
const MB_X2: u32 = 4;

// CGEventField
const FIELD_BUTTON_NUMBER: u32 = 3;
const FIELD_DELTA_X: u32 = 4;
const FIELD_DELTA_Y: u32 = 5;

// CGEventFlags
const FLAG_CAPSLOCK: u64 = 0x0001_0000;
const FLAG_SHIFT: u64 = 0x0002_0000;
const FLAG_CONTROL: u64 = 0x0004_0000;
const FLAG_OPTION: u64 = 0x0008_0000;
const FLAG_COMMAND: u64 = 0x0010_0000;

// CGScrollEventUnit
const SCROLL_UNIT_PIXEL: u32 = 0;
const SCROLL_UNIT_LINE: u32 = 1;

// ─── macOS virtual keycodes (post-remap for modifiers) ──────────────────────
const VK_COMMAND: u16 = 0x37;
const VK_RCOMMAND: u16 = 0x36;
const VK_SHIFT: u16 = 0x38;
const VK_RSHIFT: u16 = 0x3C;
const VK_OPTION: u16 = 0x3A;
const VK_ROPTION: u16 = 0x3D;
const VK_CONTROL: u16 = 0x3B;
const VK_RCONTROL: u16 = 0x3E;
const VK_CAPSLOCK: u16 = 0x39;
const VK_V: u16 = 0x09;

/// Map a browser `KeyboardEvent.code` to a macOS virtual keycode. Modifiers are
/// remapped: Control→Command, Meta/OS→Control (so client shortcuts land the way
/// a Windows/Linux user expects on a Mac).
fn code_to_keycode(code: &str) -> Option<u16> {
    let vk = match code {
        // Letters
        "KeyA" => 0x00,
        "KeyS" => 0x01,
        "KeyD" => 0x02,
        "KeyF" => 0x03,
        "KeyH" => 0x04,
        "KeyG" => 0x05,
        "KeyZ" => 0x06,
        "KeyX" => 0x07,
        "KeyC" => 0x08,
        "KeyV" => 0x09,
        "KeyB" => 0x0B,
        "KeyQ" => 0x0C,
        "KeyW" => 0x0D,
        "KeyE" => 0x0E,
        "KeyR" => 0x0F,
        "KeyY" => 0x10,
        "KeyT" => 0x11,
        "KeyO" => 0x1F,
        "KeyU" => 0x20,
        "KeyI" => 0x22,
        "KeyP" => 0x23,
        "KeyL" => 0x25,
        "KeyJ" => 0x26,
        "KeyK" => 0x28,
        "KeyN" => 0x2D,
        "KeyM" => 0x2E,
        // Digit row
        "Digit1" => 0x12,
        "Digit2" => 0x13,
        "Digit3" => 0x14,
        "Digit4" => 0x15,
        "Digit5" => 0x17,
        "Digit6" => 0x16,
        "Digit7" => 0x1A,
        "Digit8" => 0x1C,
        "Digit9" => 0x19,
        "Digit0" => 0x1D,
        // Punctuation
        "Minus" => 0x1B,
        "Equal" => 0x18,
        "BracketLeft" => 0x21,
        "BracketRight" => 0x1E,
        "Backslash" => 0x2A,
        "Semicolon" => 0x29,
        "Quote" => 0x27,
        "Backquote" => 0x32,
        "Comma" => 0x2B,
        "Period" => 0x2F,
        "Slash" => 0x2C,
        // Whitespace / control
        "Enter" => 0x24,
        "Tab" => 0x30,
        "Space" => 0x31,
        "Backspace" => 0x33,
        "Escape" => 0x35,
        "Delete" => 0x75, // ForwardDelete
        "Insert" => 0x72, // Help
        // Modifiers (REMAPPED)
        "ShiftLeft" => VK_SHIFT as _,
        "ShiftRight" => VK_RSHIFT as _,
        "ControlLeft" => VK_COMMAND as _, // Control → Command
        "ControlRight" => VK_RCOMMAND as _,
        "AltLeft" => VK_OPTION as _,
        "AltRight" => VK_ROPTION as _,
        "MetaLeft" | "OSLeft" => VK_CONTROL as _, // Windows/Super → Control
        "MetaRight" | "OSRight" => VK_RCONTROL as _,
        "CapsLock" => VK_CAPSLOCK as _,
        // Function keys
        "F1" => 0x7A,
        "F2" => 0x78,
        "F3" => 0x63,
        "F4" => 0x76,
        "F5" => 0x60,
        "F6" => 0x61,
        "F7" => 0x62,
        "F8" => 0x64,
        "F9" => 0x65,
        "F10" => 0x6D,
        "F11" => 0x67,
        "F12" => 0x6F,
        "F13" => 0x69,
        "F14" => 0x6B,
        "F15" => 0x71,
        "F16" => 0x6A,
        "F17" => 0x40,
        "F18" => 0x4F,
        "F19" => 0x50,
        "F20" => 0x5A,
        // Arrows / navigation
        "ArrowLeft" => 0x7B,
        "ArrowRight" => 0x7C,
        "ArrowDown" => 0x7D,
        "ArrowUp" => 0x7E,
        "Home" => 0x73,
        "End" => 0x77,
        "PageUp" => 0x74,
        "PageDown" => 0x79,
        // Numpad
        "Numpad0" => 0x52,
        "Numpad1" => 0x53,
        "Numpad2" => 0x54,
        "Numpad3" => 0x55,
        "Numpad4" => 0x56,
        "Numpad5" => 0x57,
        "Numpad6" => 0x58,
        "Numpad7" => 0x59,
        "Numpad8" => 0x5B,
        "Numpad9" => 0x5C,
        "NumpadDecimal" => 0x41,
        "NumpadMultiply" => 0x43,
        "NumpadAdd" => 0x45,
        "NumpadSubtract" => 0x4E,
        "NumpadDivide" => 0x4B,
        "NumpadEnter" => 0x4C,
        "NumpadEqual" => 0x51,
        "NumLock" => 0x47, // Clear
        _ => return None,
    };
    Some(vk as u16)
}

pub fn boost_thread() {
    crate::macos_utils::streamer::qos::pin_current_thread_user_interactive();
}

pub fn tune_process() {}

pub struct Injector {
    source: CGEventSourceRef,
    device_name: Option<String>,
    target: Option<DisplayRect>,
    desktop: DisplayRect,
    geom_checked: Option<Instant>,
    buttons: u16,
    down_keys: HashSet<u16>,
    mods: u64,
    pos: CGPoint,
    mouse_relative: bool,
    warned_drop: bool,
}

impl Injector {
    pub fn new(device_name: Option<String>) -> Self {
        let source = unsafe { CGEventSourceCreate(SOURCE_STATE_HID) };
        if source.is_null() {
            log::warn!("CGEventSourceCreate returned null; synthesized events may be degraded");
        }
        if !crate::macos_utils::permissions::accessibility_trusted() {
            log::warn!(
                "Accessibility (TCC) not granted — injected input is dropped until ScreenExtend \
                 is enabled in System Settings > Privacy & Security > Accessibility, then relaunched"
            );
        }
        let desktop = desktop_bounds();
        let center = CGPoint {
            x: desktop.left as f64 + desktop.width as f64 / 2.0,
            y: desktop.top as f64 + desktop.height as f64 / 2.0,
        };
        let pos = current_mouse_location().unwrap_or(center);
        Injector {
            source,
            device_name,
            target: None,
            desktop,
            geom_checked: None,
            buttons: 0,
            down_keys: HashSet::new(),
            mods: 0,
            pos,
            mouse_relative: true,
            warned_drop: false,
        }
    }

    fn refresh_geometry(&mut self) {
        let fresh =
            matches!(self.geom_checked, Some(t) if t.elapsed() < Duration::from_millis(500));
        if fresh {
            return;
        }
        self.desktop = desktop_bounds();
        if let Some(name) = &self.device_name {
            let resolved = crate::macos_utils::streamer::pipeline::monitor_rect(name).map(
                |(left, top, width, height)| DisplayRect {
                    left,
                    top,
                    width,
                    height,
                },
            );
            if resolved != self.target {
                if let Some(r) = resolved {
                    log::info!(
                        "remote-input geometry: display {name} at ({},{}) {}x{}",
                        r.left,
                        r.top,
                        r.width,
                        r.height
                    );
                }
                self.target = resolved;
            }
        }
        self.geom_checked = Some(Instant::now());
    }

    pub fn dispatch(&mut self, ev: &InputEvent) {
        self.refresh_geometry();
        match ev {
            InputEvent::Pointer {
                source,
                x,
                y,
                pressure,
                buttons,
                phase,
                ..
            } => match *source {
                SRC_MOUSE => self.mouse_pointer(*x, *y, *buttons, *phase),
                SRC_PEN => self.pen(*x, *y, *pressure, *buttons, *phase),
                SRC_TOUCH => self.touch(*x, *y, *phase),
                _ => {}
            },
            InputEvent::PointerBatch {
                source,
                buttons,
                samples,
                ..
            } => self.pointer_batch(*source, *buttons, samples),
            InputEvent::Wheel { dx, dy, mode, .. } => self.wheel(*dx, *dy, *mode),
            InputEvent::Zoom { delta } => self.zoom(*delta),
            InputEvent::MouseDelta { dx, dy, buttons } => self.mouse_delta(*dx, *dy, *buttons),
            InputEvent::Key {
                down, code, key, ..
            } => self.key(*down, code, key),
            InputEvent::Text { s, .. } => self.text(s),
            InputEvent::Clipboard { op, mime, data } => self.clipboard(*op, mime, data),
            InputEvent::Resize { .. } => {
                self.desktop = desktop_bounds();
            }
            InputEvent::Lifecycle(l) => self.lifecycle(*l),
            InputEvent::Drag { .. } => {}
            InputEvent::Drop { .. } => self.unsupported_drop(),
            InputEvent::Ping { .. } | InputEvent::Pong { .. } => {}
        }
    }

    pub fn release_all(&mut self) {
        let pos = self.pos;
        self.button_transitions(self.buttons, 0, pos);
        self.buttons = 0;
        let held: Vec<u16> = self.down_keys.drain().collect();
        for kc in held {
            self.post_key(kc, false, 0);
        }
        self.mods = 0;
    }

    // ── mouse ───────────────────────────────────────────────────────────────
    fn mouse_pointer(&mut self, x: f32, y: f32, buttons: u16, phase: Phase) {
        match phase {
            Phase::Leave | Phase::Out => return,
            Phase::Cancel => {
                self.button_transitions(self.buttons, 0, self.pos);
                self.buttons = 0;
                return;
            }
            // An absolute position arrived → leave relative/pointer-lock mode.
            Phase::Move | Phase::Enter | Phase::Over => self.mouse_relative = false,
            _ => {}
        }
        // In relative (pointer-lock) mode the event's absolute x/y are frozen and
        // meaningless, so act at the tracked cursor instead of warping to them —
        // this is what makes a click land where the cursor actually is.
        if !self.mouse_relative {
            let pos = self.point(x, y);
            self.pos = pos;
            self.motion(pos, self.buttons, 0, 0);
        }
        self.button_transitions(self.buttons, buttons, self.pos);
        self.buttons = buttons;
    }

    fn mouse_delta(&mut self, dx: i16, dy: i16, buttons: u16) {
        self.mouse_relative = true;
        let nx = (self.pos.x + dx as f64).clamp(
            self.desktop.left as f64,
            (self.desktop.left + self.desktop.width as i32) as f64,
        );
        let ny = (self.pos.y + dy as f64).clamp(
            self.desktop.top as f64,
            (self.desktop.top + self.desktop.height as i32) as f64,
        );
        let pos = CGPoint { x: nx, y: ny };
        self.motion(pos, self.buttons, dx as i64, dy as i64);
        self.button_transitions(self.buttons, buttons, pos);
        self.buttons = buttons;
        self.pos = pos;
    }

    /// Emit the move/drag event that carries the pointer to `pos`. Uses the
    /// dragged event type when a button is held, and threads relative deltas so
    /// pointer-lock consumers (games) see motion.
    fn motion(&self, pos: CGPoint, buttons: u16, dx: i64, dy: i64) {
        let (etype, button) = if buttons & btn::PRIMARY != 0 {
            (ET_LEFT_DRAG, MB_LEFT)
        } else if buttons & btn::SECONDARY != 0 {
            (ET_RIGHT_DRAG, MB_RIGHT)
        } else if buttons & btn::AUXILIARY != 0 {
            (ET_OTHER_DRAG, MB_CENTER)
        } else if buttons & btn::BACK != 0 {
            (ET_OTHER_DRAG, MB_X1)
        } else if buttons & btn::FORWARD != 0 {
            (ET_OTHER_DRAG, MB_X2)
        } else {
            (ET_MOUSE_MOVED, MB_LEFT)
        };
        self.post_mouse(etype, pos, button, dx, dy);
    }

    fn button_transitions(&self, old: u16, new: u16, pos: CGPoint) {
        let changed = old ^ new;
        let down = |bit: u16| new & bit != 0;
        if changed & btn::PRIMARY != 0 {
            let t = if down(btn::PRIMARY) { ET_LEFT_DOWN } else { ET_LEFT_UP };
            self.post_mouse(t, pos, MB_LEFT, 0, 0);
        }
        if changed & btn::SECONDARY != 0 {
            let t = if down(btn::SECONDARY) { ET_RIGHT_DOWN } else { ET_RIGHT_UP };
            self.post_mouse(t, pos, MB_RIGHT, 0, 0);
        }
        if changed & btn::AUXILIARY != 0 {
            let t = if down(btn::AUXILIARY) { ET_OTHER_DOWN } else { ET_OTHER_UP };
            self.post_mouse(t, pos, MB_CENTER, 0, 0);
        }
        if changed & btn::BACK != 0 {
            let t = if down(btn::BACK) { ET_OTHER_DOWN } else { ET_OTHER_UP };
            self.post_mouse(t, pos, MB_X1, 0, 0);
        }
        if changed & btn::FORWARD != 0 {
            let t = if down(btn::FORWARD) { ET_OTHER_DOWN } else { ET_OTHER_UP };
            self.post_mouse(t, pos, MB_X2, 0, 0);
        }
    }

    fn post_mouse(&self, etype: u32, pos: CGPoint, button: u32, dx: i64, dy: i64) {
        unsafe {
            let ev = CGEventCreateMouseEvent(self.source, etype, pos, button);
            if ev.is_null() {
                return;
            }
            if button > MB_CENTER {
                CGEventSetIntegerValueField(ev, FIELD_BUTTON_NUMBER, button as i64);
            }
            if dx != 0 || dy != 0 {
                CGEventSetIntegerValueField(ev, FIELD_DELTA_X, dx);
                CGEventSetIntegerValueField(ev, FIELD_DELTA_Y, dy);
            }
            if self.mods != 0 {
                CGEventSetFlags(ev, self.mods);
            }
            CGEventPost(HID_EVENT_TAP, ev);
            CFRelease(ev);
        }
    }

    fn wheel(&self, dx: f32, dy: f32, mode: u8) {
        let (unit, scale) = match mode {
            1 => (SCROLL_UNIT_LINE, 1.0f32),
            2 => (SCROLL_UNIT_LINE, 3.0f32),
            _ => (SCROLL_UNIT_PIXEL, 1.0f32),
        };
        // wheel1 = vertical (positive scrolls content up → invert browser dy);
        // wheel2 = horizontal.
        let v = (-dy * scale).round() as i32;
        let h = (-dx * scale).round() as i32;
        if v == 0 && h == 0 {
            return;
        }
        unsafe {
            let ev = CGEventCreateScrollWheelEvent(self.source, unit, 2, v, h);
            if ev.is_null() {
                return;
            }
            if self.mods != 0 {
                CGEventSetFlags(ev, self.mods);
            }
            CGEventPost(HID_EVENT_TAP, ev);
            CFRelease(ev);
        }
    }

    fn zoom(&self, delta: f32) {
        if delta == 0.0 {
            return;
        }
        let lines = (delta * 10.0).round() as i32;
        if lines == 0 {
            return;
        }
        unsafe {
            let ev = CGEventCreateScrollWheelEvent(self.source, SCROLL_UNIT_LINE, 1, lines);
            if ev.is_null() {
                return;
            }
            // Cmd+scroll = zoom in browsers/editors — the mac analog of the
            // Windows Ctrl+wheel pinch trick under the Control→Command remap.
            CGEventSetFlags(ev, self.mods | FLAG_COMMAND);
            CGEventPost(HID_EVENT_TAP, ev);
            CFRelease(ev);
        }
    }

    // ── keyboard / text ───────────────────────────────────────────────────────
    fn key(&mut self, down: bool, code: &str, key: &str) {
        if let Some(kc) = code_to_keycode(code) {
            if down {
                self.down_keys.insert(kc);
            } else {
                self.down_keys.remove(&kc);
            }
            self.recompute_mods();
            self.post_key(kc, down, self.mods);
            return;
        }
        // Unmapped printable key → inject as text on the down edge.
        if down {
            let mut chars = key.chars();
            if let (Some(c), None) = (chars.next(), chars.clone().next()) {
                if !c.is_control() {
                    self.text(&c.to_string());
                    return;
                }
            }
        }
        log::debug!("unmapped key code={code:?} key={key:?} down={down}");
    }

    fn recompute_mods(&mut self) {
        let has = |k: u16| self.down_keys.contains(&k);
        let mut m = 0u64;
        if has(VK_COMMAND) || has(VK_RCOMMAND) {
            m |= FLAG_COMMAND;
        }
        if has(VK_CONTROL) || has(VK_RCONTROL) {
            m |= FLAG_CONTROL;
        }
        if has(VK_OPTION) || has(VK_ROPTION) {
            m |= FLAG_OPTION;
        }
        if has(VK_SHIFT) || has(VK_RSHIFT) {
            m |= FLAG_SHIFT;
        }
        if has(VK_CAPSLOCK) {
            m |= FLAG_CAPSLOCK;
        }
        self.mods = m;
    }

    fn post_key(&self, keycode: u16, down: bool, flags: u64) {
        unsafe {
            let ev = CGEventCreateKeyboardEvent(self.source, keycode, down);
            if ev.is_null() {
                return;
            }
            if flags != 0 {
                CGEventSetFlags(ev, flags);
            }
            CGEventPost(HID_EVENT_TAP, ev);
            CFRelease(ev);
        }
    }

    fn text(&self, s: &str) {
        let utf16: Vec<u16> = s.encode_utf16().collect();
        if utf16.is_empty() {
            return;
        }
        unsafe {
            for down in [true, false] {
                let ev = CGEventCreateKeyboardEvent(self.source, 0, down);
                if ev.is_null() {
                    continue;
                }
                CGEventKeyboardSetUnicodeString(ev, utf16.len(), utf16.as_ptr());
                CGEventSetFlags(ev, 0); // typed text carries no stray modifiers
                CGEventPost(HID_EVENT_TAP, ev);
                CFRelease(ev);
            }
        }
    }

    // ── pen / touch (degrade to mouse) ────────────────────────────────────────
    fn pen(&mut self, x: f32, y: f32, pressure: f32, buttons: u16, phase: Phase) {
        let b = if pressure > 0.0 || buttons & btn::PRIMARY != 0 {
            btn::PRIMARY
        } else {
            0
        };
        self.mouse_pointer(x, y, b, phase);
    }

    fn touch(&mut self, x: f32, y: f32, phase: Phase) {
        let contacting = matches!(
            phase,
            Phase::Down | Phase::Move | Phase::Enter | Phase::Over
        );
        self.mouse_pointer(x, y, if contacting { btn::PRIMARY } else { 0 }, phase);
    }

    fn pointer_batch(&mut self, source: u8, buttons: u16, samples: &[MoveSample]) {
        for s in samples {
            match source {
                SRC_PEN => self.pen(s.x, s.y, s.pressure, buttons, Phase::Move),
                SRC_TOUCH => self.touch(s.x, s.y, Phase::Move),
                _ => self.mouse_pointer(s.x, s.y, buttons, Phase::Move),
            }
        }
    }

    // ── clipboard ────────────────────────────────────────────────────────────
    fn clipboard(&mut self, op: u8, mime: &str, data: &[u8]) {
        if set_pasteboard(mime, data) {
            log::info!("clipboard set ({mime}, {} bytes, op={op})", data.len());
            if op == 2 {
                self.paste_hotkey();
            }
        }
    }

    fn paste_hotkey(&self) {
        self.post_key(VK_V, true, FLAG_COMMAND);
        self.post_key(VK_V, false, FLAG_COMMAND);
    }

    // ── lifecycle / misc ──────────────────────────────────────────────────────
    fn lifecycle(&mut self, l: Lifecycle) {
        match l {
            Lifecycle::Focus(false) | Lifecycle::Visibility(false) => self.release_all(),
            Lifecycle::PointerLock(locked) => self.mouse_relative = locked,
            _ => {}
        }
    }

    fn unsupported_drop(&mut self) {
        if !self.warned_drop {
            log::warn!("file drop injection is unsupported on macOS (no public API); ignoring");
            self.warned_drop = true;
        }
    }

    // ── coordinate mapping ────────────────────────────────────────────────────
    /// Normalized [0,1] → global display point on the target display (or the
    /// whole desktop when no specific display is bound).
    fn point(&self, nx: f32, ny: f32) -> CGPoint {
        let r = self.target.unwrap_or(self.desktop);
        CGPoint {
            x: r.left as f64 + nx.clamp(0.0, 1.0) as f64 * r.width as f64,
            y: r.top as f64 + ny.clamp(0.0, 1.0) as f64 * r.height as f64,
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        if !self.source.is_null() {
            unsafe { CFRelease(self.source) };
        }
    }
}

/// Current global cursor location, so relative/pointer-lock mode starts from the
/// real cursor instead of a default position.
fn current_mouse_location() -> Option<CGPoint> {
    unsafe {
        let ev = CGEventCreate(std::ptr::null_mut());
        if ev.is_null() {
            return None;
        }
        let p = CGEventGetLocation(ev);
        CFRelease(ev);
        Some(p)
    }
}

/// Warm up the CoreGraphics keyboard-event/TSM subsystem. MUST be called once on
/// the **main thread** at startup: `CGEventCreateKeyboardEvent`'s lazy
/// `key_translate` init (SkyLight/HIToolbox) is not thread-safe and SIGILLs if
/// first triggered on the injector thread (Catalina). Priming here runs its
/// dispatch_once on the main thread so later off-thread calls are safe.
pub fn prime_keyboard() {
    unsafe {
        let src = CGEventSourceCreate(SOURCE_STATE_HID);
        let ev = CGEventCreateKeyboardEvent(src, 0, true);
        if !ev.is_null() {
            CFRelease(ev);
        }
        if !src.is_null() {
            CFRelease(src);
        }
    }
}

/// Bounding rect of every active display, in the global point space.
fn desktop_bounds() -> DisplayRect {
    use crate::macos_utils::streamer::pipeline::{monitor_device_names, monitor_rect};
    let mut acc: Option<(i32, i32, i32, i32)> = None; // (left, top, right, bottom)
    for name in monitor_device_names() {
        if let Some((l, t, w, h)) = monitor_rect(&name) {
            let (r, b) = (l + w as i32, t + h as i32);
            acc = Some(match acc {
                Some((al, at, ar, ab)) => (al.min(l), at.min(t), ar.max(r), ab.max(b)),
                None => (l, t, r, b),
            });
        }
    }
    match acc {
        Some((l, t, r, b)) => DisplayRect {
            left: l,
            top: t,
            width: (r - l).max(1) as u32,
            height: (b - t).max(1) as u32,
        },
        None => DisplayRect {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
        },
    }
}

fn set_pasteboard(mime: &str, data: &[u8]) -> bool {
    use objc2::runtime::AnyObject;
    use objc2::{class, msg_send};
    use objc2_foundation::NSString;

    unsafe {
        let pb: *mut AnyObject = msg_send![class!(NSPasteboard), generalPasteboard];
        if pb.is_null() {
            return false;
        }
        let _: isize = msg_send![pb, clearContents];
        if mime == "text/plain" {
            let Ok(s) = std::str::from_utf8(data) else {
                return false;
            };
            let ns = NSString::from_str(s);
            let ty = NSString::from_str("public.utf8-plain-text");
            let ok: bool = msg_send![pb, setString: &*ns, forType: &*ty];
            ok
        } else {
            let ty = NSString::from_str(mime);
            let nsdata: *mut AnyObject = msg_send![
                class!(NSData),
                dataWithBytes: data.as_ptr() as *const c_void,
                length: data.len(),
            ];
            if nsdata.is_null() {
                return false;
            }
            let ok: bool = msg_send![pb, setData: nsdata, forType: &*ty];
            ok
        }
    }
}
