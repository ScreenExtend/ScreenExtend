#![allow(unused_unsafe)]

use std::collections::HashMap;
use std::collections::HashSet;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GHND};
use windows::Win32::System::Threading::{
    GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, DestroySyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::Input::Pointer::{
    InjectSyntheticPointerInput, POINTER_FLAGS, POINTER_FLAG_CANCELED, POINTER_FLAG_DOWN,
    POINTER_FLAG_INCONTACT, POINTER_FLAG_INRANGE, POINTER_FLAG_UP, POINTER_FLAG_UPDATE,
    POINTER_INFO, POINTER_PEN_INFO, POINTER_TOUCH_INFO,
};
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, PostMessageW, WindowFromPoint, PT_PEN, PT_TOUCH, SM_CXSCREEN,
    SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    WM_DROPFILES,
};

use super::protocol::{
    btn, positions_cursor, DropItem, InputEvent, Lifecycle, MoveSample, Phase, SRC_MOUSE, SRC_PEN,
    SRC_TOUCH,
};
use super::scancode::code_to_scancode;
use super::DisplayRect;

pub const NAME: &str = "windows-sendinput+syntheticpointer";

const WHEEL_DELTA: f32 = 120.0;
const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
const MAX_TOUCH_COUNT: u32 = 10;
const CF_UNICODETEXT: u32 = 13;
const SC_LCTRL: u16 = 0x1D;
const SC_KEY_V: u16 = 0x2F;
const PEN_MASK_PRESSURE: u32 = 0x0000_0001;
const PEN_MASK_ROTATION: u32 = 0x0000_0002;
const PEN_MASK_TILT_X: u32 = 0x0000_0004;
const PEN_MASK_TILT_Y: u32 = 0x0000_0008;
const PEN_FLAG_BARREL: u32 = 0x0000_0001;
const PEN_FLAG_ERASER: u32 = 0x0000_0004;
const TOUCH_MASK_CONTACTAREA: u32 = 0x0000_0001;
const TOUCH_MASK_ORIENTATION: u32 = 0x0000_0002;
const TOUCH_MASK_PRESSURE: u32 = 0x0000_0004;

pub fn boost_thread() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
    }
}

#[derive(Clone, Copy)]
struct VirtualScreen {
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl VirtualScreen {
    fn query() -> Self {
        unsafe {
            let left = GetSystemMetrics(SM_XVIRTUALSCREEN);
            let top = GetSystemMetrics(SM_YVIRTUALSCREEN);
            let mut width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
            let mut height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
            if width <= 0 {
                width = GetSystemMetrics(SM_CXSCREEN);
            }
            if height <= 0 {
                height = GetSystemMetrics(SM_CYSCREEN);
            }
            VirtualScreen {
                left,
                top,
                width: width.max(1),
                height: height.max(1),
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Contact {
    x: f32,
    y: f32,
    pressure: f32,
    w: f32,
    h: f32,
}

pub struct Injector {
    virt: VirtualScreen,
    target: Option<DisplayRect>,
    device_name: Option<String>,
    geom_checked: Option<std::time::Instant>,
    buttons: u16,
    down_keys: HashSet<(u16, bool)>,
    pen_dev: Option<HSYNTHETICPOINTERDEVICE>,
    touch_dev: Option<HSYNTHETICPOINTERDEVICE>,
    contacts: HashMap<u32, Contact>,
    pen_down: bool,
    /// The client holds pointer lock, so `MOUSE_DELTA` drives the cursor and the frozen
    /// coordinates an absolute frame still carries must be ignored.
    mouse_relative: bool,
    /// Where relative motion has put the cursor, in desktop pixels. Windows would happily
    /// accumulate `MOUSEEVENTF_MOVE` for us, but then it applies its own pointer ballistics on
    /// top of the curve the client already applied, and there is no position to clamp.
    rel_pos: Option<POINT>,
    scratch: Vec<INPUT>,
    touch_scratch: Vec<POINTER_TYPE_INFO>,
}

impl Injector {
    pub fn new(device_name: Option<String>) -> Self {
        let pen_dev = unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) }
            .map_err(|e| log::warn!("synthetic pen device unavailable, degrading pen→mouse: {e}"))
            .ok();
        let touch_dev = unsafe {
            CreateSyntheticPointerDevice(PT_TOUCH, MAX_TOUCH_COUNT, POINTER_FEEDBACK_DEFAULT)
        }
        .map_err(|e| log::warn!("synthetic touch device unavailable, degrading touch→mouse: {e}"))
        .ok();
        Injector {
            virt: VirtualScreen::query(),
            target: None,
            device_name,
            geom_checked: None,
            buttons: 0,
            down_keys: HashSet::new(),
            pen_dev,
            touch_dev,
            contacts: HashMap::new(),
            pen_down: false,
            // No lock until the client says so. Starting at `true` meant the first frame of a
            // session was treated as relative, so a touch client's opening tap — which arrives
            // as a bare `POINTER_DOWN` with no move before it — clicked wherever the host
            // cursor already was.
            mouse_relative: false,
            rel_pos: None,
            scratch: Vec::with_capacity(16),
            touch_scratch: Vec::with_capacity(MAX_TOUCH_COUNT as usize),
        }
    }

    fn refresh_geometry(&mut self) {
        use std::time::{Duration, Instant};
        let fresh =
            matches!(self.geom_checked, Some(t) if t.elapsed() < Duration::from_millis(500));
        if fresh {
            return;
        }
        self.virt = VirtualScreen::query();
        if let Some(name) = &self.device_name {
            let resolved = crate::windows_utils::streamer::capture::monitor_rect(name).map(
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
                        "remote-input geometry: display {name} at ({},{}) {}x{}; desktop origin ({},{})",
                        r.left, r.top, r.width, r.height, self.virt.left, self.virt.top
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
                id,
                x,
                y,
                pressure,
                tilt_x,
                tilt_y,
                twist,
                w,
                h,
                buttons,
                phase,
            } => match *source {
                SRC_MOUSE => self.mouse_pointer(*x, *y, *buttons, *phase),
                SRC_PEN => self.pen(
                    *x, *y, *pressure, *tilt_x, *tilt_y, *twist, *buttons, *phase,
                ),
                SRC_TOUCH => self.touch(*id, *x, *y, *pressure, *w, *h, *phase),
                _ => {}
            },
            InputEvent::PointerBatch {
                source,
                id,
                buttons,
                samples,
                predicted,
            } => self.pointer_batch(*source, *id, *buttons, samples, *predicted),
            InputEvent::Wheel { dx, dy, mode, .. } => self.wheel(*dx, *dy, *mode),
            InputEvent::Zoom { delta } => self.zoom(*delta),
            InputEvent::MouseDelta {
                dx, dy, buttons, ..
            } => self.mouse_delta(*dx, *dy, *buttons),
            InputEvent::Key {
                down, code, key, ..
            } => self.key(*down, code, key),
            InputEvent::Text { s, .. } => self.text(s),
            InputEvent::Resize { .. } => {
                self.virt = VirtualScreen::query();
            }
            InputEvent::Lifecycle(l) => self.lifecycle(*l),
            InputEvent::Clipboard { op, mime, data } => self.clipboard(*op, mime, data),
            InputEvent::Drag { .. } => {}
            InputEvent::Drop { x, y, items } => self.drop_files(*x, *y, items),
            // Handled by the transport before dispatch: none of these inject anything.
            InputEvent::Ping { .. }
            | InputEvent::Pong { .. }
            | InputEvent::ClientStats(_)
            | InputEvent::KeyframeRequest { .. } => {}
        }
    }

    pub fn release_all(&mut self) {
        self.scratch.clear();
        button_transitions(self.buttons, 0, &mut self.scratch);
        for (scan, ext) in self.down_keys.drain() {
            self.scratch.push(key_input(
                scan,
                KEYEVENTF_SCANCODE | ext_flag(ext) | KEYEVENTF_KEYUP,
            ));
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
        self.buttons = 0;

        if self.pen_down {
            if let Some(dev) = self.pen_dev {
                let info = self.pen_info(0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0, POINTER_FLAG_UP);
                inject_pointer(dev, &[info]);
            }
            self.pen_down = false;
        }

        if !self.contacts.is_empty() {
            if let Some(dev) = self.touch_dev {
                let mut frame = std::mem::take(&mut self.touch_scratch);
                frame.clear();
                for (id, c) in &self.contacts {
                    frame.push(self.touch_info(*id, c, POINTER_FLAG_UP));
                }
                inject_pointer(dev, &frame);
                self.touch_scratch = frame;
            }
            self.contacts.clear();
        }
    }

    fn mouse_pointer(&mut self, x: f32, y: f32, buttons: u16, phase: Phase) {
        match phase {
            Phase::Leave | Phase::Out => return,
            Phase::Cancel => {
                self.scratch.clear();
                button_transitions(self.buttons, 0, &mut self.scratch);
                send_inputs(&self.scratch);
                self.scratch.clear();
                self.buttons = 0;
                return;
            }
            // An absolute move is proof the client is not locked, for clients that never send
            // `POINTERLOCK_STATE`. A `Down`/`Up` is not: those still arrive while locked.
            Phase::Move | Phase::Enter | Phase::Over => self.mouse_relative = false,
            _ => {}
        }
        self.scratch.clear();
        self.push_mouse_pointer(x, y, buttons, phase);
        send_inputs(&self.scratch);
        self.scratch.clear();
        self.buttons = buttons;
    }

    /// The body of `mouse_pointer` without the flush, so a batch can build several samples into
    /// one `SendInput` array instead of paying a kernel transition per sample.
    fn push_mouse_pointer(&mut self, x: f32, y: f32, buttons: u16, phase: Phase) {
        if positions_cursor(self.mouse_relative, phase) {
            let (ax, ay) = self.abs(x, y);
            self.scratch.push(mouse_input(
                ax,
                ay,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
            self.rel_pos = None;
        }
        button_transitions(self.buttons, buttons, &mut self.scratch);
    }

    fn mouse_delta(&mut self, dx: i16, dy: i16, buttons: u16) {
        self.mouse_relative = true;
        self.scratch.clear();
        if dx != 0 || dy != 0 {
            // Absolute, from a position we track ourselves. A raw `MOUSEEVENTF_MOVE` goes
            // through the Windows pointer curve, and the client has already applied its own
            // (its OS's, unless `unadjustedMovement` was granted, plus its sensitivity
            // setting) — so the relative path used to accelerate twice while the absolute path
            // accelerated not at all. One curve, owned by the client, is the coherent answer,
            // and it matches what the macOS backend has always done.
            let pos = self.advance_rel(dx as i32, dy as i32);
            let vx = (pos.x - self.virt.left) as f32 / self.virt.width as f32;
            let vy = (pos.y - self.virt.top) as f32 / self.virt.height as f32;
            self.scratch.push(mouse_input(
                (vx.clamp(0.0, 1.0) * 65535.0).round() as i32,
                (vy.clamp(0.0, 1.0) * 65535.0).round() as i32,
                0,
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
            ));
        }
        button_transitions(self.buttons, buttons, &mut self.scratch);
        send_inputs(&self.scratch);
        self.scratch.clear();
        self.buttons = buttons;
    }

    /// Move the tracked relative position and clamp it to the virtual desktop.
    ///
    /// Clamped to the desktop rather than to the client's own display, which is what macOS has
    /// always done — deliberately, so the two backends agree. Whether a locked client should be
    /// confined to the display it is streaming is a product question, not a technical one; see
    /// `findings/minor-input-relative-mode-escapes-the-target-display.md`.
    fn advance_rel(&mut self, dx: i32, dy: i32) -> POINT {
        let base = match self.rel_pos {
            Some(p) => p,
            None => unsafe {
                let mut p = POINT::default();
                if GetCursorPos(&mut p).is_ok() {
                    p
                } else {
                    self.pixel(0.5, 0.5)
                }
            },
        };
        let p = POINT {
            x: (base.x + dx).clamp(self.virt.left, self.virt.left + self.virt.width - 1),
            y: (base.y + dy).clamp(self.virt.top, self.virt.top + self.virt.height - 1),
        };
        self.rel_pos = Some(p);
        p
    }

    fn wheel(&mut self, dx: f32, dy: f32, mode: u8) {
        let scale = match mode {
            1 => WHEEL_DELTA,
            2 => WHEEL_DELTA * 3.0,
            _ => WHEEL_DELTA / 100.0,
        };
        self.scratch.clear();
        if dy != 0.0 {
            let data = (-dy * scale).round() as i32 as u32;
            self.scratch
                .push(mouse_input(0, 0, data, MOUSEEVENTF_WHEEL));
        }
        if dx != 0.0 {
            let data = (dx * scale).round() as i32 as u32;
            self.scratch
                .push(mouse_input(0, 0, data, MOUSEEVENTF_HWHEEL));
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    fn pointer_batch(
        &mut self,
        source: u8,
        id: u32,
        buttons: u16,
        samples: &[MoveSample],
        predicted: u8,
    ) {
        if samples.is_empty() {
            return;
        }
        match source {
            // Pen and touch go one call per sample because the API says so:
            // `InjectSyntheticPointerInput` takes a frame of *simultaneous contacts*, capped at
            // the device's `maxCount` (1 for pen, 10 for touch) — it is not a sequence of
            // samples over time. Predicted samples are dropped here: a stroke is a shape, and a
            // guess that the next real sample corrects still leaves a kink in the ink.
            SRC_PEN | SRC_TOUCH => {
                let real = samples.len() - predicted as usize;
                for s in &samples[..real] {
                    if source == SRC_PEN {
                        self.pen(
                            s.x,
                            s.y,
                            s.pressure,
                            s.tilt_x,
                            s.tilt_y,
                            s.twist,
                            buttons,
                            Phase::Move,
                        );
                    } else {
                        self.touch(id, s.x, s.y, s.pressure, 0.0, 0.0, Phase::Move);
                    }
                }
            }
            // The cursor is one `SendInput` array for the whole batch: same path, one kernel
            // transition instead of one per sample. Predictions are kept, because the cursor is
            // last-value-wins and the next real sample lands a frame later.
            _ => {
                self.mouse_relative = false;
                self.scratch.clear();
                for s in &samples[..samples.len() - 1] {
                    let (ax, ay) = self.abs(s.x, s.y);
                    self.scratch.push(mouse_input(
                        ax,
                        ay,
                        0,
                        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    ));
                }
                let last = samples[samples.len() - 1];
                self.push_mouse_pointer(last.x, last.y, buttons, Phase::Move);
                send_inputs(&self.scratch);
                self.scratch.clear();
                self.buttons = buttons;
            }
        }
    }

    fn zoom(&mut self, delta: f32) {
        if delta == 0.0 {
            return;
        }
        let notches = (delta * WHEEL_DELTA).round() as i32;
        if notches == 0 {
            return;
        }
        self.scratch.clear();
        let ctrl_held = self.down_keys.contains(&(SC_LCTRL, false));
        if !ctrl_held {
            self.scratch.push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE));
        }
        self.scratch
            .push(mouse_input(0, 0, notches as u32, MOUSEEVENTF_WHEEL));
        if !ctrl_held {
            self.scratch
                .push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    fn key(&mut self, down: bool, code: &str, key: &str) {
        if let Some((scan, ext)) = code_to_scancode(code) {
            let mut flags = KEYEVENTF_SCANCODE | ext_flag(ext);
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            send_inputs(&[key_input(scan, flags)]);
            if down {
                self.down_keys.insert((scan, ext));
            } else {
                self.down_keys.remove(&(scan, ext));
            }
            return;
        }
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

    fn text(&mut self, s: &str) {
        self.scratch.clear();
        let mut buf = [0u16; 2];
        for ch in s.chars() {
            for unit in ch.encode_utf16(&mut buf) {
                self.scratch.push(key_input(*unit, KEYEVENTF_UNICODE));
                self.scratch
                    .push(key_input(*unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
            }
        }
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    #[allow(clippy::too_many_arguments)]
    fn pen(
        &mut self,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        twist: f32,
        buttons: u16,
        phase: Phase,
    ) {
        let Some(dev) = self.pen_dev else {
            let b = if pressure > 0.0 || buttons & btn::PRIMARY != 0 {
                btn::PRIMARY
            } else {
                0
            };
            self.mouse_pointer(x, y, b, phase);
            return;
        };
        let flags = match phase {
            Phase::Down => {
                self.pen_down = true;
                POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            }
            Phase::Move | Phase::Enter | Phase::Over => {
                if self.pen_down {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                } else {
                    POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE // hover
                }
            }
            Phase::Up => {
                self.pen_down = false;
                POINTER_FLAG_UP
            }
            Phase::Cancel => {
                self.pen_down = false;
                POINTER_FLAG_UP | POINTER_FLAG_CANCELED
            }
            Phase::Leave | Phase::Out => {
                self.pen_down = false;
                POINTER_FLAG_UP
            }
        };
        let info = self.pen_info(x, y, pressure, tilt_x, tilt_y, twist, buttons, flags);
        inject_pointer(dev, &[info]);
    }

    #[allow(clippy::too_many_arguments)]
    fn pen_info(
        &self,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        twist: f32,
        buttons: u16,
        flags: POINTER_FLAGS,
    ) -> POINTER_TYPE_INFO {
        let pt = self.inject_point(x, y);
        let mut pen_flags = 0u32;
        if buttons & btn::ERASER != 0 {
            pen_flags |= PEN_FLAG_ERASER;
        }
        if buttons & btn::SECONDARY != 0 {
            pen_flags |= PEN_FLAG_BARREL;
        }
        let pen = POINTER_PEN_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_PEN,
                pointerId: 1,
                pointerFlags: flags,
                ptPixelLocation: pt,
                ..Default::default()
            },
            penFlags: pen_flags,
            penMask: PEN_MASK_PRESSURE | PEN_MASK_ROTATION | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
            pressure: (pressure.clamp(0.0, 1.0) * 1024.0).round() as u32,
            rotation: (twist.rem_euclid(360.0)) as u32,
            tiltX: tilt_x.clamp(-90.0, 90.0) as i32,
            tiltY: tilt_y.clamp(-90.0, 90.0) as i32,
        };
        POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 { penInfo: pen },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn touch(&mut self, id: u32, x: f32, y: f32, pressure: f32, w: f32, h: f32, phase: Phase) {
        let Some(dev) = self.touch_dev else {
            let b = matches!(
                phase,
                Phase::Down | Phase::Move | Phase::Enter | Phase::Over
            );
            self.mouse_pointer(x, y, if b { btn::PRIMARY } else { 0 }, phase);
            return;
        };
        self.contacts.insert(
            id,
            Contact {
                x,
                y,
                pressure,
                w,
                h,
            },
        );

        let ending = matches!(phase, Phase::Up | Phase::Cancel | Phase::Leave | Phase::Out);
        let mut frame = std::mem::take(&mut self.touch_scratch);
        frame.clear();
        for (cid, c) in &self.contacts {
            let flags = if *cid == id {
                match phase {
                    Phase::Down | Phase::Enter | Phase::Over => {
                        POINTER_FLAG_DOWN | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                    }
                    Phase::Up | Phase::Leave | Phase::Out => POINTER_FLAG_UP,
                    Phase::Cancel => POINTER_FLAG_UP | POINTER_FLAG_CANCELED,
                    Phase::Move => {
                        POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
                    }
                }
            } else {
                POINTER_FLAG_UPDATE | POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT
            };
            frame.push(self.touch_info(*cid, c, flags));
        }
        inject_pointer(dev, &frame);
        self.touch_scratch = frame;

        if ending {
            self.contacts.remove(&id);
        }
    }

    fn touch_info(&self, id: u32, c: &Contact, flags: POINTER_FLAGS) -> POINTER_TYPE_INFO {
        let pt = self.inject_point(c.x, c.y);
        let (space_w, space_h) = self.space_dims();
        let cw = ((c.w * space_w as f32).round() as i32).max(2);
        let ch = ((c.h * space_h as f32).round() as i32).max(2);
        let in_contact = flags.contains(POINTER_FLAG_INCONTACT);
        let pressure = if in_contact && c.pressure == 0.0 {
            512
        } else {
            (c.pressure.clamp(0.0, 1.0) * 1024.0).round() as u32
        };
        let touch = POINTER_TOUCH_INFO {
            pointerInfo: POINTER_INFO {
                pointerType: PT_TOUCH,
                pointerId: id,
                pointerFlags: flags,
                ptPixelLocation: pt,
                ..Default::default()
            },
            touchFlags: 0,
            touchMask: TOUCH_MASK_CONTACTAREA | TOUCH_MASK_ORIENTATION | TOUCH_MASK_PRESSURE,
            rcContact: RECT {
                left: pt.x - cw / 2,
                top: pt.y - ch / 2,
                right: pt.x + cw / 2,
                bottom: pt.y + ch / 2,
            },
            rcContactRaw: RECT::default(),
            orientation: 0,
            pressure,
        };
        POINTER_TYPE_INFO {
            r#type: PT_TOUCH,
            Anonymous: POINTER_TYPE_INFO_0 { touchInfo: touch },
        }
    }

    fn clipboard(&mut self, op: u8, mime: &str, data: &[u8]) {
        if set_clipboard(mime, data) {
            log::info!("clipboard set ({mime}, {} bytes, op={op})", data.len());
            if op == 2 {
                self.paste_hotkey();
            }
        }
    }

    fn paste_hotkey(&mut self) {
        self.scratch.clear();
        self.scratch.push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE));
        self.scratch.push(key_input(SC_KEY_V, KEYEVENTF_SCANCODE));
        self.scratch
            .push(key_input(SC_KEY_V, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
        self.scratch
            .push(key_input(SC_LCTRL, KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP));
        send_inputs(&self.scratch);
        self.scratch.clear();
    }

    fn drop_files(&mut self, x: f32, y: f32, items: &[DropItem]) {
        if items.is_empty() {
            return;
        }
        let dir = std::env::temp_dir().join("input-bridge-drop");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            log::warn!("drop: cannot create temp dir {dir:?}: {e}");
            return;
        }
        let mut wide: Vec<u16> = Vec::new();
        let mut count = 0usize;
        for (i, it) in items.iter().enumerate() {
            let fname = std::path::Path::new(&it.name)
                .file_name()
                .map(|s| s.to_os_string())
                .unwrap_or_else(|| std::ffi::OsString::from(format!("drop-{i}.bin")));
            let path = dir.join(&fname);
            if let Err(e) = std::fs::write(&path, &it.data) {
                log::warn!("drop: write {path:?} failed: {e}");
                continue;
            }
            wide.extend(path.as_os_str().encode_wide());
            wide.push(0);
            count += 1;
        }
        if count == 0 {
            return;
        }
        wide.push(0);

        unsafe {
            let header = size_of::<DROPFILES>();
            let bytes = header + wide.len() * 2;
            let h = match GlobalAlloc(GHND, bytes) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("drop: GlobalAlloc failed: {e}");
                    return;
                }
            };
            let base = GlobalLock(h) as *mut u8;
            if base.is_null() {
                let _ = GlobalFree(Some(h));
                return;
            }
            let mut pt = self.pixel(x, y);
            let hwnd: HWND = WindowFromPoint(pt);
            let _ = ScreenToClient(hwnd, &mut pt);
            let df = DROPFILES {
                pFiles: header as u32,
                pt,
                fNC: windows::core::BOOL(0),
                fWide: windows::core::BOOL(1),
            };
            std::ptr::write_unaligned(base as *mut DROPFILES, df);
            std::ptr::copy_nonoverlapping(wide.as_ptr(), base.add(header) as *mut u16, wide.len());
            let _ = GlobalUnlock(h);

            match PostMessageW(Some(hwnd), WM_DROPFILES, WPARAM(h.0 as usize), LPARAM(0)) {
                Ok(_) => log::info!("posted WM_DROPFILES ({count} file(s)) to target window"),
                Err(e) => {
                    log::warn!("drop: PostMessage WM_DROPFILES failed: {e}");
                    let _ = GlobalFree(Some(h));
                }
            }
        }
    }

    fn lifecycle(&mut self, l: Lifecycle) {
        match l {
            Lifecycle::Focus(false) | Lifecycle::Visibility(false) => self.release_all(),
            Lifecycle::PointerLock(locked) => {
                self.mouse_relative = locked;
                if !locked {
                    self.rel_pos = None;
                }
            }
            _ => {}
        }
    }

    fn abs(&self, nx: f32, ny: f32) -> (i32, i32) {
        let pt = self.pixel(nx, ny);
        let vx = (pt.x - self.virt.left) as f32 / self.virt.width as f32;
        let vy = (pt.y - self.virt.top) as f32 / self.virt.height as f32;
        (
            (vx.clamp(0.0, 1.0) * 65535.0).round() as i32,
            (vy.clamp(0.0, 1.0) * 65535.0).round() as i32,
        )
    }

    fn pixel(&self, nx: f32, ny: f32) -> POINT {
        match self.target {
            Some(r) => POINT {
                x: r.left + (nx.clamp(0.0, 1.0) * r.width as f32).round() as i32,
                y: r.top + (ny.clamp(0.0, 1.0) * r.height as f32).round() as i32,
            },
            None => POINT {
                x: (nx.clamp(0.0, 1.0) * self.virt.width as f32).round() as i32 + self.virt.left,
                y: (ny.clamp(0.0, 1.0) * self.virt.height as f32).round() as i32 + self.virt.top,
            },
        }
    }

    fn inject_point(&self, nx: f32, ny: f32) -> POINT {
        let p = self.pixel(nx, ny);
        POINT {
            x: p.x - self.virt.left,
            y: p.y - self.virt.top,
        }
    }

    fn space_dims(&self) -> (i32, i32) {
        match self.target {
            Some(r) => (r.width as i32, r.height as i32),
            None => (self.virt.width, self.virt.height),
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // One pair of these per session leaked for the life of the host process. Windows caps
        // synthetic pointer devices per process, so the visible symptom was pen pressure and
        // multi-touch quietly degrading to mouse after enough reconnects.
        for dev in [self.pen_dev.take(), self.touch_dev.take()]
            .into_iter()
            .flatten()
        {
            unsafe { DestroySyntheticPointerDevice(dev) };
        }
    }
}

fn button_transitions(old: u16, new: u16, out: &mut Vec<INPUT>) {
    let changed = old ^ new;
    let is_down = |bit: u16| new & bit != 0;
    if changed & btn::PRIMARY != 0 {
        out.push(mouse_input(
            0,
            0,
            0,
            if is_down(btn::PRIMARY) {
                MOUSEEVENTF_LEFTDOWN
            } else {
                MOUSEEVENTF_LEFTUP
            },
        ));
    }
    if changed & btn::SECONDARY != 0 {
        out.push(mouse_input(
            0,
            0,
            0,
            if is_down(btn::SECONDARY) {
                MOUSEEVENTF_RIGHTDOWN
            } else {
                MOUSEEVENTF_RIGHTUP
            },
        ));
    }
    if changed & btn::AUXILIARY != 0 {
        out.push(mouse_input(
            0,
            0,
            0,
            if is_down(btn::AUXILIARY) {
                MOUSEEVENTF_MIDDLEDOWN
            } else {
                MOUSEEVENTF_MIDDLEUP
            },
        ));
    }
    if changed & btn::BACK != 0 {
        out.push(mouse_input(
            0,
            0,
            XBUTTON1,
            if is_down(btn::BACK) {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
        ));
    }
    if changed & btn::FORWARD != 0 {
        out.push(mouse_input(
            0,
            0,
            XBUTTON2,
            if is_down(btn::FORWARD) {
                MOUSEEVENTF_XDOWN
            } else {
                MOUSEEVENTF_XUP
            },
        ));
    }
}

#[inline]
fn ext_flag(ext: bool) -> KEYBD_EVENT_FLAGS {
    if ext {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    }
}

#[inline]
fn mouse_input(dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[inline]
fn key_input(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

#[inline]
fn send_inputs(inputs: &[INPUT]) {
    if inputs.is_empty() {
        return;
    }
    let sent = unsafe { SendInput(inputs, size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        log::warn!("SendInput inserted {sent}/{} events", inputs.len());
    }
}

#[inline]
fn inject_pointer(dev: HSYNTHETICPOINTERDEVICE, frame: &[POINTER_TYPE_INFO]) {
    if frame.is_empty() {
        return;
    }
    if let Err(e) = unsafe { InjectSyntheticPointerInput(dev, frame) } {
        log::warn!("InjectSyntheticPointerInput failed: {e}");
    }
}

fn set_clipboard(mime: &str, data: &[u8]) -> bool {
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let ok = if mime == "text/plain" {
            set_clip_text(data)
        } else {
            set_clip_raw(mime, data)
        };
        let _ = CloseClipboard();
        ok
    }
}

unsafe fn set_clip_text(text_utf8: &[u8]) -> bool {
    let Ok(s) = std::str::from_utf8(text_utf8) else {
        return false;
    };
    let mut wide: Vec<u16> = s.encode_utf16().collect();
    wide.push(0);
    unsafe { put_clipboard(CF_UNICODETEXT, bytemuck_u16(&wide)) }
}

unsafe fn set_clip_raw(mime: &str, data: &[u8]) -> bool {
    let mut w: Vec<u16> = mime.encode_utf16().collect();
    w.push(0);
    let fmt = unsafe { RegisterClipboardFormatW(PCWSTR(w.as_ptr())) };
    if fmt == 0 {
        return false;
    }
    unsafe { put_clipboard(fmt, data) }
}

unsafe fn put_clipboard(format: u32, bytes: &[u8]) -> bool {
    unsafe {
        let Ok(h) = GlobalAlloc(GHND, bytes.len().max(1)) else {
            return false;
        };
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            let _ = GlobalFree(Some(h));
            return false;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        let _ = GlobalUnlock(h);
        match SetClipboardData(format, Some(HANDLE(h.0))) {
            Ok(_) => true,
            Err(_) => {
                let _ = GlobalFree(Some(h));
                false
            }
        }
    }
}

fn bytemuck_u16(v: &[u16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn batch(n: usize) -> Vec<MoveSample> {
        (0..n)
            .map(|i| MoveSample {
                x: 0.40 + i as f32 * 0.0005,
                y: 0.40 + i as f32 * 0.0005,
                pressure: 0.0,
                tilt_x: 0.0,
                tilt_y: 0.0,
                twist: 0.0,
            })
            .collect()
    }

    /// Injects for real, so it moves the host cursor: run it deliberately, not in a suite.
    /// `cargo test -- --ignored --nocapture pointer_batch_costs`
    #[test]
    #[ignore]
    fn pointer_batch_costs_one_sendinput_not_one_per_sample() {
        const SAMPLES: usize = 40;
        const REPS: usize = 500;
        let samples = batch(SAMPLES);
        let mut inj = Injector::new(None);

        // The shape this replaces: one mouse_pointer -> one SendInput per sample.
        let t = Instant::now();
        for _ in 0..REPS {
            for s in &samples {
                inj.mouse_pointer(s.x, s.y, 0, Phase::Move);
            }
        }
        let per_sample = t.elapsed();

        let t = Instant::now();
        for _ in 0..REPS {
            inj.pointer_batch(SRC_MOUSE, 0, 0, &samples, 0);
        }
        let batched = t.elapsed();

        println!(
            "pointer batch of {SAMPLES}: per-sample {:.1} us/batch ({} SendInput),              batched {:.1} us/batch (1 SendInput)",
            per_sample.as_secs_f64() * 1e6 / REPS as f64,
            SAMPLES,
            batched.as_secs_f64() * 1e6 / REPS as f64,
        );
        assert!(batched < per_sample);
    }
}
