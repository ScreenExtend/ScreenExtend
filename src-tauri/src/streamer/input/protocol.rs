#![allow(dead_code)]

use smol_str::SmolStr;

// ─── Opcodes (§3.1) ─────────────────────────────────────────────────────────
pub mod op {
    pub const POINTER_DOWN: u8 = 0x01;
    pub const POINTER_UP: u8 = 0x02;
    pub const POINTER_MOVE: u8 = 0x03;
    pub const POINTER_CANCEL: u8 = 0x04;
    pub const POINTER_ENTER: u8 = 0x05;
    pub const POINTER_LEAVE: u8 = 0x06;
    pub const POINTER_OVER: u8 = 0x07;
    pub const POINTER_OUT: u8 = 0x08;
    pub const POINTER_MOVE_BATCH: u8 = 0x09;
    pub const WHEEL: u8 = 0x10;
    pub const ZOOM: u8 = 0x11;
    pub const KEY: u8 = 0x20;
    pub const TEXT_INPUT: u8 = 0x21;
    pub const COMPOSITION_UPDATE: u8 = 0x22;
    pub const CLIPBOARD: u8 = 0x30;
    pub const DRAG: u8 = 0x40;
    pub const DROP: u8 = 0x41;
    pub const FOCUS_STATE: u8 = 0x50;
    pub const VISIBILITY: u8 = 0x51;
    pub const RESIZE: u8 = 0x52;
    pub const POINTERLOCK_STATE: u8 = 0x53;
    pub const MOUSE_DELTA: u8 = 0x54;
    pub const PING: u8 = 0x60;
    pub const PONG: u8 = 0x61;
    pub const STATS: u8 = 0x62;
}

// ─── Source codes (§3.2) ────────────────────────────────────────────────────
pub const SRC_MOUSE: u8 = 0x00;
pub const SRC_TOUCH: u8 = 0x01;
pub const SRC_PEN: u8 = 0x02;

// ─── Modifier bitmask (§3.4) ────────────────────────────────────────────────
pub mod modmask {
    pub const SHIFT: u16 = 1 << 0;
    pub const CONTROL: u16 = 1 << 1;
    pub const ALT: u16 = 1 << 2;
    pub const META: u16 = 1 << 3;
    pub const ALTGR: u16 = 1 << 4;
    pub const CAPSLOCK: u16 = 1 << 5;
    pub const NUMLOCK: u16 = 1 << 6;
}

// ─── Buttons bitmask (§3.3, mirrors MouseEvent.buttons) ─────────────────────
pub mod btn {
    pub const PRIMARY: u16 = 1 << 0; // left
    pub const SECONDARY: u16 = 1 << 1; // right
    pub const AUXILIARY: u16 = 1 << 2; // middle
    pub const BACK: u16 = 1 << 3; // X1
    pub const FORWARD: u16 = 1 << 4; // X2
    pub const ERASER: u16 = 1 << 5; // pen eraser
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Down,
    Up,
    Move,
    Cancel,
    Enter,
    Leave,
    Over,
    Out,
}

impl Phase {
    #[inline]
    pub fn from_opcode(op: u8) -> Phase {
        match op {
            op::POINTER_DOWN => Phase::Down,
            op::POINTER_UP => Phase::Up,
            op::POINTER_MOVE => Phase::Move,
            op::POINTER_CANCEL => Phase::Cancel,
            op::POINTER_ENTER => Phase::Enter,
            op::POINTER_LEAVE => Phase::Leave,
            op::POINTER_OVER => Phase::Over,
            op::POINTER_OUT => Phase::Out,
            _ => Phase::Move,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Lifecycle {
    Focus(bool),
    Visibility(bool),
    PointerLock(bool),
}

#[derive(Debug, Clone)]
pub struct DropItem {
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct MoveSample {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    pub twist: f32,
}

#[derive(Debug, Clone)]
pub enum InputEvent {
    Pointer {
        source: u8,
        id: u32,
        x: f32,
        y: f32,
        pressure: f32,
        tilt_x: f32,
        tilt_y: f32,
        twist: f32,
        w: f32,
        h: f32,
        buttons: u16,
        phase: Phase,
    },
    PointerBatch {
        source: u8,
        id: u32,
        buttons: u16,
        samples: Vec<MoveSample>,
    },
    Wheel {
        source: u8,
        dx: f32,
        dy: f32,
        dz: f32,
        mode: u8,
    },
    Zoom {
        delta: f32,
    },
    MouseDelta {
        dx: i16,
        dy: i16,
        buttons: u16,
    },
    Key {
        down: bool,
        repeat: bool,
        mods: u16,
        code: SmolStr,
        key: SmolStr,
    },
    Text {
        composing: bool,
        s: String,
    },
    Clipboard {
        op: u8,
        mime: String,
        data: Vec<u8>,
    },
    Drag {
        phase: u8,
        x: f32,
        y: f32,
    },
    Drop {
        x: f32,
        y: f32,
        items: Vec<DropItem>,
    },
    Resize {
        w: u16,
        h: u16,
        dpr: f32,
    },
    Lifecycle(Lifecycle),
    Ping {
        t_ns: u64,
    },
    Pong {
        t_ns: u64,
    },
}

#[inline]
pub fn is_fast(op: u8) -> bool {
    matches!(
        op,
        op::POINTER_MOVE
            | op::POINTER_MOVE_BATCH
            | op::POINTER_ENTER
            | op::POINTER_LEAVE
            | op::POINTER_OVER
            | op::POINTER_OUT
            | op::WHEEL
            | op::ZOOM
            | op::MOUSE_DELTA
    )
}

// ─── Little-endian fixed-offset readers ─────────────────────────────────────
#[inline]
fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
#[inline]
fn rd_i16(b: &[u8], o: usize) -> Option<i16> {
    Some(i16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
#[inline]
fn rd_u32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
#[inline]
fn rd_u64(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}
#[inline]
fn rd_f32(b: &[u8], o: usize) -> Option<f32> {
    Some(f32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

pub fn parse(b: &[u8]) -> Option<(InputEvent, bool)> {
    let opcode = *b.first()?;
    let ev = match opcode {
        op::POINTER_DOWN
        | op::POINTER_UP
        | op::POINTER_MOVE
        | op::POINTER_CANCEL
        | op::POINTER_ENTER
        | op::POINTER_LEAVE
        | op::POINTER_OVER
        | op::POINTER_OUT => parse_pointer(b)?,
        op::POINTER_MOVE_BATCH => parse_pointer_batch(b)?,
        op::WHEEL => parse_wheel(b)?,
        op::ZOOM => parse_zoom(b)?,
        op::MOUSE_DELTA => parse_mouse_delta(b)?,
        op::KEY => parse_key(b)?,
        op::TEXT_INPUT | op::COMPOSITION_UPDATE => parse_text(b, opcode)?,
        op::CLIPBOARD => parse_clipboard(b)?,
        op::DRAG => parse_drag(b)?,
        op::DROP => parse_drop(b)?,
        op::RESIZE => parse_resize(b)?,
        op::FOCUS_STATE => InputEvent::Lifecycle(Lifecycle::Focus(*b.get(1)? != 0)),
        op::VISIBILITY => InputEvent::Lifecycle(Lifecycle::Visibility(*b.get(1)? != 0)),
        op::POINTERLOCK_STATE => InputEvent::Lifecycle(Lifecycle::PointerLock(*b.get(1)? != 0)),
        op::PING => InputEvent::Ping { t_ns: rd_u64(b, 1)? },
        op::PONG => InputEvent::Pong { t_ns: rd_u64(b, 1)? },
        _ => return None,
    };
    Some((ev, is_fast(opcode)))
}

fn parse_pointer(b: &[u8]) -> Option<InputEvent> {
    if b.len() < 40 {
        return None;
    }
    Some(InputEvent::Pointer {
        source: b[1],
        id: rd_u32(b, 2)?,
        x: rd_f32(b, 6)?,
        y: rd_f32(b, 10)?,
        pressure: rd_f32(b, 14)?,
        tilt_x: rd_f32(b, 18)?,
        tilt_y: rd_f32(b, 22)?,
        twist: rd_f32(b, 26)?,
        w: rd_f32(b, 30)?,
        h: rd_f32(b, 34)?,
        buttons: rd_u16(b, 38)?,
        phase: Phase::from_opcode(b[0]),
    })
}

const BATCH_HEADER: usize = 10;
const SAMPLE_SIZE: usize = 24;
fn parse_pointer_batch(b: &[u8]) -> Option<InputEvent> {
    let source = *b.get(1)?;
    let id = rd_u32(b, 2)?;
    let buttons = rd_u16(b, 6)?;
    let count = rd_u16(b, 8)? as usize;
    let need = BATCH_HEADER + count * SAMPLE_SIZE;
    if count == 0 || b.len() < need {
        return None;
    }
    let mut samples = Vec::with_capacity(count);
    let mut o = BATCH_HEADER;
    for _ in 0..count {
        samples.push(MoveSample {
            x: rd_f32(b, o)?,
            y: rd_f32(b, o + 4)?,
            pressure: rd_f32(b, o + 8)?,
            tilt_x: rd_f32(b, o + 12)?,
            tilt_y: rd_f32(b, o + 16)?,
            twist: rd_f32(b, o + 20)?,
        });
        o += SAMPLE_SIZE;
    }
    Some(InputEvent::PointerBatch { source, id, buttons, samples })
}

fn parse_zoom(b: &[u8]) -> Option<InputEvent> {
    Some(InputEvent::Zoom { delta: rd_f32(b, 1)? })
}

fn parse_wheel(b: &[u8]) -> Option<InputEvent> {
    if b.len() < 15 {
        return None;
    }
    Some(InputEvent::Wheel {
        source: b[1],
        dx: rd_f32(b, 2)?,
        dy: rd_f32(b, 6)?,
        dz: rd_f32(b, 10)?,
        mode: b[14],
    })
}

fn parse_mouse_delta(b: &[u8]) -> Option<InputEvent> {
    if b.len() < 8 {
        return None;
    }
    Some(InputEvent::MouseDelta {
        dx: rd_i16(b, 2)?,
        dy: rd_i16(b, 4)?,
        buttons: rd_u16(b, 6)?,
    })
}

fn parse_key(b: &[u8]) -> Option<InputEvent> {
    let flags = *b.get(1)?;
    let mods = rd_u16(b, 2)?;
    let code_len = *b.get(4)? as usize;
    let code_start = 5;
    let code_end = code_start + code_len;
    let code = std::str::from_utf8(b.get(code_start..code_end)?).ok()?;
    let key_len = *b.get(code_end)? as usize;
    let key_start = code_end + 1;
    let key_end = key_start + key_len;
    let key = std::str::from_utf8(b.get(key_start..key_end)?).ok()?;
    Some(InputEvent::Key {
        down: flags & 0b01 != 0,
        repeat: flags & 0b10 != 0,
        mods,
        code: SmolStr::new(code),
        key: SmolStr::new(key),
    })
}

fn parse_text(b: &[u8], opcode: u8) -> Option<InputEvent> {
    let len = rd_u32(b, 1)? as usize;
    let text = b.get(5..5 + len)?;
    Some(InputEvent::Text {
        composing: opcode == op::COMPOSITION_UPDATE,
        s: String::from_utf8(text.to_vec()).ok()?,
    })
}

fn parse_clipboard(b: &[u8]) -> Option<InputEvent> {
    let op = *b.get(1)?;
    let mime_len = rd_u32(b, 2)? as usize;
    let mime_start = 6;
    let mime_end = mime_start + mime_len;
    let mime = std::str::from_utf8(b.get(mime_start..mime_end)?).ok()?.to_string();
    let data_len = rd_u32(b, mime_end)? as usize;
    let data_start = mime_end + 4;
    let data = b.get(data_start..data_start + data_len)?.to_vec();
    Some(InputEvent::Clipboard { op, mime, data })
}

fn parse_drag(b: &[u8]) -> Option<InputEvent> {
    if b.len() < 10 {
        return None;
    }
    Some(InputEvent::Drag {
        phase: b[1],
        x: rd_f32(b, 2)?,
        y: rd_f32(b, 6)?,
    })
}

fn parse_drop(b: &[u8]) -> Option<InputEvent> {
    let x = rd_f32(b, 2)?;
    let y = rd_f32(b, 6)?;
    let count = rd_u16(b, 10)? as usize;
    let mut items = Vec::with_capacity(count);
    let mut pos = 12usize;
    for _ in 0..count {
        let name_len = rd_u16(b, pos)? as usize;
        pos += 2;
        let name = std::str::from_utf8(b.get(pos..pos + name_len)?).ok()?.to_string();
        pos += name_len;
        let mime_len = rd_u32(b, pos)? as usize;
        pos += 4;
        let mime = std::str::from_utf8(b.get(pos..pos + mime_len)?).ok()?.to_string();
        pos += mime_len;
        let size = rd_u64(b, pos)?;
        pos += 8;
        let data_len = rd_u64(b, pos)? as usize;
        pos += 8;
        let data = b.get(pos..pos + data_len)?.to_vec();
        pos += data_len;
        items.push(DropItem { name, mime, size, data });
    }
    Some(InputEvent::Drop { x, y, items })
}

fn parse_resize(b: &[u8]) -> Option<InputEvent> {
    if b.len() < 9 {
        return None;
    }
    Some(InputEvent::Resize {
        w: rd_u16(b, 1)?,
        h: rd_u16(b, 3)?,
        dpr: rd_f32(b, 5)?,
    })
}

pub fn build_pong(t_ns: u64) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = op::PONG;
    out[1..9].copy_from_slice(&t_ns.to_le_bytes());
    out
}

pub fn build_stats(received: u64, dropped: u64, contacts: u32, queue_depth: u32) -> [u8; 25] {
    let mut out = [0u8; 25];
    out[0] = op::STATS;
    out[1..9].copy_from_slice(&received.to_le_bytes());
    out[9..17].copy_from_slice(&dropped.to_le_bytes());
    out[17..21].copy_from_slice(&contacts.to_le_bytes());
    out[21..25].copy_from_slice(&queue_depth.to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_roundtrip() {
        let mut b = [0u8; 40];
        b[0] = op::POINTER_MOVE;
        b[1] = SRC_PEN;
        b[2..6].copy_from_slice(&7u32.to_le_bytes());
        b[6..10].copy_from_slice(&0.5f32.to_le_bytes());
        b[10..14].copy_from_slice(&0.25f32.to_le_bytes());
        b[14..18].copy_from_slice(&0.8f32.to_le_bytes());
        b[38..40].copy_from_slice(&btn::PRIMARY.to_le_bytes());
        let (ev, hot) = parse(&b).unwrap();
        assert!(hot);
        match ev {
            InputEvent::Pointer { source, id, x, y, pressure, buttons, phase, .. } => {
                assert_eq!(source, SRC_PEN);
                assert_eq!(id, 7);
                assert!((x - 0.5).abs() < 1e-6);
                assert!((y - 0.25).abs() < 1e-6);
                assert!((pressure - 0.8).abs() < 1e-6);
                assert_eq!(buttons, btn::PRIMARY);
                assert_eq!(phase, Phase::Move);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn pointer_batch_curve() {
        let mut b = vec![op::POINTER_MOVE_BATCH, SRC_PEN];
        b.extend_from_slice(&3u32.to_le_bytes());
        b.extend_from_slice(&btn::PRIMARY.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        for (x, p) in [(0.1f32, 0.2f32), (0.2f32, 0.9f32)] {
            b.extend_from_slice(&x.to_le_bytes());
            b.extend_from_slice(&0.5f32.to_le_bytes());
            b.extend_from_slice(&p.to_le_bytes());
            b.extend_from_slice(&0f32.to_le_bytes());
            b.extend_from_slice(&0f32.to_le_bytes());
            b.extend_from_slice(&0f32.to_le_bytes());
        }
        let (ev, hot) = parse(&b).unwrap();
        assert!(hot);
        match ev {
            InputEvent::PointerBatch { source, id, buttons, samples } => {
                assert_eq!(source, SRC_PEN);
                assert_eq!(id, 3);
                assert_eq!(buttons, btn::PRIMARY);
                assert_eq!(samples.len(), 2);
                assert!((samples[0].pressure - 0.2).abs() < 1e-6);
                assert!((samples[1].pressure - 0.9).abs() < 1e-6);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn zoom_frame() {
        let mut b = vec![op::ZOOM];
        b.extend_from_slice(&0.15f32.to_le_bytes());
        match parse(&b).unwrap().0 {
            InputEvent::Zoom { delta } => assert!((delta - 0.15).abs() < 1e-6),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn key_variable_len() {
        let mut b = vec![op::KEY, 0b01, 0, 0, 4];
        b.extend_from_slice(b"KeyA");
        b.push(1);
        b.extend_from_slice(b"a");
        let (ev, hot) = parse(&b).unwrap();
        assert!(!hot);
        match ev {
            InputEvent::Key { down, code, key, .. } => {
                assert!(down);
                assert_eq!(code, "KeyA");
                assert_eq!(key, "a");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn text_utf8() {
        let s = "好";
        let mut b = vec![op::TEXT_INPUT];
        b.extend_from_slice(&(s.len() as u32).to_le_bytes());
        b.extend_from_slice(s.as_bytes());
        let (ev, _) = parse(&b).unwrap();
        match ev {
            InputEvent::Text { composing, s: got } => {
                assert!(!composing);
                assert_eq!(got, "好");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn truncated_is_none() {
        assert!(parse(&[op::POINTER_MOVE, 0, 0]).is_none());
        assert!(parse(&[]).is_none());
    }

    #[test]
    fn ping_pong() {
        let mut b = vec![op::PING];
        b.extend_from_slice(&123456789u64.to_le_bytes());
        let (ev, _) = parse(&b).unwrap();
        match ev {
            InputEvent::Ping { t_ns } => assert_eq!(t_ns, 123456789),
            _ => panic!(),
        }
        let pong = build_pong(123456789);
        assert_eq!(pong[0], op::PONG);
        assert_eq!(u64::from_le_bytes(pong[1..9].try_into().unwrap()), 123456789);
    }
}
