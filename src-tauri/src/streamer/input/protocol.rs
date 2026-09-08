#![allow(dead_code)]

use smol_str::SmolStr;

// opcodes
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
    /// Client -> host only. The host-to-client twin this opcode once had was never sent; the
    /// injector's own counters go to `SessionTelemetry` instead, where `status --json` reads
    /// them alongside every other counter. See `docs/INPUT-PROTOCOL.md`.
    pub const STATS: u8 = 0x62;
    pub const BYE: u8 = 0x63;
    /// Client -> host, reliable channel, 2 bytes: opcode then a reason byte.
    ///
    /// The last resort for a receiver that cannot ask for a keyframe any other way. A client
    /// running the legacy `createEncodedStreams` transform (Chrome and Edge 86-140) takes the
    /// frames *instead of* libwebrtc's reference finder, so nothing downstream ever emits an
    /// RTCP PLI and the host would otherwise never learn the picture is broken.
    pub const KEYFRAME_REQUEST: u8 = 0x64;
}

/// Why a client asked for a keyframe. Only `Unspecified` is sent today.
pub mod keyframe_reason {
    pub const UNSPECIFIED: u8 = 0x00;
}

pub mod bye {
    pub const KICKED: u8 = 0x01;
    pub const HOST_EXIT: u8 = 0x02;
}

// source codes
pub const SRC_MOUSE: u8 = 0x00;
pub const SRC_TOUCH: u8 = 0x01;
pub const SRC_PEN: u8 = 0x02;

// modifier bitmask
pub mod modmask {
    pub const SHIFT: u16 = 1 << 0;
    pub const CONTROL: u16 = 1 << 1;
    pub const ALT: u16 = 1 << 2;
    pub const META: u16 = 1 << 3;
    pub const ALTGR: u16 = 1 << 4;
    pub const CAPSLOCK: u16 = 1 << 5;
    pub const NUMLOCK: u16 = 1 << 6;
}

// buttons bitmask
pub mod btn {
    pub const PRIMARY: u16 = 1 << 0; // left
    pub const SECONDARY: u16 = 1 << 1; // right
    pub const AUXILIARY: u16 = 1 << 2; // middle
    pub const BACK: u16 = 1 << 3; // X1
    pub const FORWARD: u16 = 1 << 4; // X2
    pub const ERASER: u16 = 1 << 5; // pen eraser
}

/// Per-event flag bits carried by the v2 tail of `MOUSE_DELTA` and `POINTER_MOVE_BATCH`.
pub mod evflag {
    /// The client holds pointer lock with `unadjustedMovement: true`, so its deltas are raw
    /// device counts with no OS pointer curve applied. Without it the client's own OS has
    /// already curved them and the host must not curve them a second time.
    pub const UNADJUSTED: u8 = 1 << 0;
}

/// v1 wire lengths, and the offset each optional v2 tail starts at.
pub const POINTER_LEN: usize = 40;
pub const WHEEL_LEN: usize = 15;
pub const MOUSE_DELTA_LEN: usize = 8;
/// Bytes of tail: a `u64` client stamp, and for a batch two more bytes of flags + predicted
/// count. Both tails are optional; see `docs/INPUT-PROTOCOL.md`.
pub const STAMP_LEN: usize = 8;
pub const BATCH_TAIL_LEN: usize = STAMP_LEN + 2;

/// Read the trailing client stamp a v2 frame may carry at `at`, or 0 when it carries none.
///
/// Every tail is appended past the v1 length, so a v1 client reads as 0 and a client newer
/// than this build can append further fields without breaking the parse. The declared length
/// is a floor, never an equality — the same rule the client stats frame already follows.
#[inline]
fn rd_stamp(b: &[u8], at: usize) -> u64 {
    rd_u64(b, at).unwrap_or(0)
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
        /// How many of the trailing samples came from `getPredictedEvents()` rather than the
        /// real input stream. They are speculative, so a stroke backend drops them; a cursor
        /// backend may use them, since the next real sample corrects the guess a frame later.
        predicted: u8,
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
        /// The client had `unadjustedMovement` granted: raw device counts, no OS curve.
        unadjusted: bool,
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
    /// The client's own view of its half of the path. See `docs/TELEMETRY.md` for the layout.
    ClientStats(Box<crate::streamer::telemetry::ClientStats>),
    /// The client is asking for an IDR out of band. Handled by the transport; it injects
    /// nothing, so it must never reach a backend's `dispatch`.
    KeyframeRequest {
        reason: u8,
    },
}

/// Whether a pointer frame's coordinates should move the cursor before its buttons are applied.
///
/// They should, for every phase that carries them — `Down` and `Up` included, which is the
/// whole point: a browser fires `pointerdown` with no preceding `pointermove` on touch, so a
/// backend that positions only on move-ish phases clicks wherever the host cursor already was.
///
/// The one exception is pointer lock. While the client holds it the browser freezes
/// `clientX`/`clientY` at the lock point, so the coordinates a locked `POINTER_DOWN` carries are
/// stale by construction and only `MOUSE_DELTA` may move the cursor.
#[inline]
pub fn positions_cursor(pointer_locked: bool, phase: Phase) -> bool {
    !pointer_locked && !matches!(phase, Phase::Leave | Phase::Out | Phase::Cancel)
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

/// One decoded frame, plus the two things the transport needs to know about it.
#[derive(Debug)]
pub struct Parsed {
    pub ev: InputEvent,
    /// Belongs on the lossy latency-critical queue rather than the reliable one.
    pub fast: bool,
    /// The client's own monotonic nanosecond stamp for this event, or 0 when the frame carries
    /// none. It rides the same client timebase as the `PING` stamp, so the host can measure how
    /// long the event spent in flight without the client having to report anything back.
    pub client_ns: u64,
}

pub fn parse(b: &[u8]) -> Option<Parsed> {
    let opcode = *b.first()?;
    let mut client_ns = 0u64;
    let ev = match opcode {
        op::POINTER_DOWN
        | op::POINTER_UP
        | op::POINTER_MOVE
        | op::POINTER_CANCEL
        | op::POINTER_ENTER
        | op::POINTER_LEAVE
        | op::POINTER_OVER
        | op::POINTER_OUT => {
            let ev = parse_pointer(b)?;
            client_ns = rd_stamp(b, POINTER_LEN);
            ev
        }
        op::POINTER_MOVE_BATCH => parse_pointer_batch(b, &mut client_ns)?,
        op::WHEEL => {
            let ev = parse_wheel(b)?;
            client_ns = rd_stamp(b, WHEEL_LEN);
            ev
        }
        op::ZOOM => parse_zoom(b)?,
        op::MOUSE_DELTA => parse_mouse_delta(b, &mut client_ns)?,
        op::KEY => parse_key(b, &mut client_ns)?,
        op::TEXT_INPUT | op::COMPOSITION_UPDATE => parse_text(b, opcode)?,
        op::CLIPBOARD => parse_clipboard(b)?,
        op::DRAG => parse_drag(b)?,
        op::DROP => parse_drop(b)?,
        op::RESIZE => parse_resize(b)?,
        op::FOCUS_STATE => InputEvent::Lifecycle(Lifecycle::Focus(*b.get(1)? != 0)),
        op::VISIBILITY => InputEvent::Lifecycle(Lifecycle::Visibility(*b.get(1)? != 0)),
        op::POINTERLOCK_STATE => InputEvent::Lifecycle(Lifecycle::PointerLock(*b.get(1)? != 0)),
        op::PING => InputEvent::Ping {
            t_ns: rd_u64(b, 1)?,
        },
        op::PONG => InputEvent::Pong {
            t_ns: rd_u64(b, 1)?,
        },
        op::STATS => parse_client_stats(b)?,
        op::KEYFRAME_REQUEST => InputEvent::KeyframeRequest {
            reason: b.get(1).copied().unwrap_or(keyframe_reason::UNSPECIFIED),
        },
        _ => return None,
    };
    Some(Parsed {
        ev,
        fast: is_fast(opcode),
        client_ns,
    })
}

fn parse_pointer(b: &[u8]) -> Option<InputEvent> {
    if b.len() < POINTER_LEN {
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
fn parse_pointer_batch(b: &[u8], client_ns: &mut u64) -> Option<InputEvent> {
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
    let predicted = if b.len() >= need + BATCH_TAIL_LEN {
        *client_ns = rd_stamp(b, need);
        // A client that claims more predictions than it sent samples is malformed; clamping
        // rather than rejecting keeps the real samples, which are still good.
        b[need + STAMP_LEN + 1].min(count as u8)
    } else {
        0
    };
    Some(InputEvent::PointerBatch {
        source,
        id,
        buttons,
        samples,
        predicted,
    })
}

fn parse_zoom(b: &[u8]) -> Option<InputEvent> {
    Some(InputEvent::Zoom {
        delta: rd_f32(b, 1)?,
    })
}

fn parse_wheel(b: &[u8]) -> Option<InputEvent> {
    if b.len() < WHEEL_LEN {
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

fn parse_mouse_delta(b: &[u8], client_ns: &mut u64) -> Option<InputEvent> {
    if b.len() < MOUSE_DELTA_LEN {
        return None;
    }
    // Byte 8 was a zero pad every client already sent, which is why it can become the flags
    // byte without a version bump: an old client reads as "no flags set".
    let flags = b.get(MOUSE_DELTA_LEN).copied().unwrap_or(0);
    *client_ns = rd_stamp(b, MOUSE_DELTA_LEN + 1);
    Some(InputEvent::MouseDelta {
        dx: rd_i16(b, 2)?,
        dy: rd_i16(b, 4)?,
        buttons: rd_u16(b, 6)?,
        unadjusted: flags & evflag::UNADJUSTED != 0,
    })
}

fn parse_key(b: &[u8], client_ns: &mut u64) -> Option<InputEvent> {
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
    *client_ns = rd_stamp(b, key_end);
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
    let mime = std::str::from_utf8(b.get(mime_start..mime_end)?)
        .ok()?
        .to_string();
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

/// Smallest a `DropItem` can be on the wire: two length prefixes, a size and a data length.
const DROP_ITEM_MIN: usize = 2 + 4 + 8 + 8;
/// No real drag-and-drop carries more than this, and the count sizes a heap allocation.
const MAX_DROP_ITEMS: usize = 256;

fn parse_drop(b: &[u8]) -> Option<InputEvent> {
    let x = rd_f32(b, 2)?;
    let y = rd_f32(b, 6)?;
    let count = rd_u16(b, 10)? as usize;
    // The count sizes a `Vec<DropItem>` (~72 B each), so it has to be bounded by what the
    // message could actually contain before it is believed: a bare 12-byte header claiming
    // 65535 items would otherwise allocate ~4.7 MB and then fail on the first read.
    if count > MAX_DROP_ITEMS || b.len() < 12usize.checked_add(count.checked_mul(DROP_ITEM_MIN)?)? {
        return None;
    }
    let mut items = Vec::with_capacity(count);
    let mut pos = 12usize;
    for _ in 0..count {
        let name_len = rd_u16(b, pos)? as usize;
        pos += 2;
        let name_end = pos.checked_add(name_len)?;
        let name = std::str::from_utf8(b.get(pos..name_end)?).ok()?.to_string();
        pos = name_end;
        let mime_len = rd_u32(b, pos)? as usize;
        pos += 4;
        let mime_end = pos.checked_add(mime_len)?;
        let mime = std::str::from_utf8(b.get(pos..mime_end)?).ok()?.to_string();
        pos = mime_end;
        let size = rd_u64(b, pos)?;
        pos += 8;
        let data_len = rd_u64(b, pos)? as usize;
        pos += 8;
        let data_end = pos.checked_add(data_len)?;
        let data = b.get(pos..data_end)?.to_vec();
        pos = data_end;
        items.push(DropItem {
            name,
            mime,
            size,
            data,
        });
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

/// `[PONG][echoed client stamp u64][host_now_ns u64]`.
///
/// The echoed half gives the client its RTT; the host half is what lets it place a host
/// timestamp on its own timeline at all. The host clock's epoch is this process's start, so
/// without a measured offset the client has nothing to compare an RTP-recovered capture time
/// against. Clients older than this field read only the first 9 bytes and ignore the rest.
/// Wire size of a v1 client stats frame. Anything shorter is rejected; anything longer is a
/// later version whose extra fields this build ignores.
pub const CLIENT_STATS_LEN: usize = 72;
pub const CLIENT_STATS_VERSION: u8 = 1;

fn parse_client_stats(b: &[u8]) -> Option<InputEvent> {
    if b.len() < CLIENT_STATS_LEN {
        return None;
    }
    let version = b[1];
    if version == 0 {
        return None;
    }
    Some(InputEvent::ClientStats(Box::new(
        crate::streamer::telemetry::ClientStats {
            received_host_ns: crate::streamer::audio::host_now_ns(),
            version,
            video_path: b[2],
            audio_path: b[3],
            flags: rd_u32(b, 4)?,
            arrival_to_paint_p50_us: rd_u32(b, 8)?,
            arrival_to_paint_p90_us: rd_u32(b, 12)?,
            arrival_to_paint_p99_us: rd_u32(b, 16)?,
            decode_p50_us: rd_u32(b, 20)?,
            decode_p90_us: rd_u32(b, 24)?,
            decode_p99_us: rd_u32(b, 28)?,
            host_to_paint_p50_us: rd_u32(b, 32)?,
            host_to_paint_p90_us: rd_u32(b, 36)?,
            host_to_paint_p99_us: rd_u32(b, 40)?,
            frames_painted: rd_u32(b, 44)?,
            frames_dropped: rd_u32(b, 48)?,
            decode_errors: rd_u32(b, 52)?,
            keyframe_requests: rd_u32(b, 56)?,
            audio_underruns: rd_u32(b, 60)?,
            audio_overruns: rd_u32(b, 64)?,
            clock_offset_valid: rd_u32(b, 4)?
                & crate::streamer::telemetry::client_flags::CLOCK_VALID
                != 0,
            rtt_us: rd_u32(b, 68)?,
        },
    )))
}

pub fn build_pong(t_ns: u64, host_ns: u64) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[0] = op::PONG;
    out[1..9].copy_from_slice(&t_ns.to_le_bytes());
    out[9..17].copy_from_slice(&host_ns.to_le_bytes());
    out
}

pub fn build_bye(reason: u8) -> [u8; 2] {
    [op::BYE, reason]
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
        let p = parse(&b).unwrap();
        assert!(p.fast);
        match p.ev {
            InputEvent::Pointer {
                source,
                id,
                x,
                y,
                pressure,
                buttons,
                phase,
                ..
            } => {
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
        let p = parse(&b).unwrap();
        assert!(p.fast);
        assert_eq!(p.client_ns, 0, "a v1 batch carries no stamp");
        match p.ev {
            InputEvent::PointerBatch {
                source,
                id,
                buttons,
                samples,
                predicted,
            } => {
                assert_eq!(predicted, 0);
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
        match parse(&b).unwrap().ev {
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
        let p = parse(&b).unwrap();
        assert!(!p.fast);
        match p.ev {
            InputEvent::Key {
                down, code, key, ..
            } => {
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
        let p = parse(&b).unwrap();
        match p.ev {
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
        let p = parse(&b).unwrap();
        match p.ev {
            InputEvent::Ping { t_ns } => assert_eq!(t_ns, 123456789),
            _ => panic!(),
        }
        let pong = build_pong(123456789, 987654321);
        assert_eq!(pong[0], op::PONG);
        assert_eq!(
            u64::from_le_bytes(pong[1..9].try_into().unwrap()),
            123456789
        );
        assert_eq!(
            u64::from_le_bytes(pong[9..17].try_into().unwrap()),
            987654321
        );
    }

    #[test]
    fn client_stats_round_trip() {
        use crate::streamer::telemetry::client_flags;
        let mut b = vec![0u8; CLIENT_STATS_LEN];
        b[0] = op::STATS;
        b[1] = CLIENT_STATS_VERSION;
        b[2] = 1; // webcodecs + canvas
        b[3] = 2; // worklet over postMessage
        b[4..8].copy_from_slice(
            &(client_flags::CROSS_ORIGIN_ISOLATED | client_flags::CLOCK_VALID).to_le_bytes(),
        );
        b[8..12].copy_from_slice(&3_500u32.to_le_bytes());
        b[32..36].copy_from_slice(&41_200u32.to_le_bytes());
        b[44..48].copy_from_slice(&1799u32.to_le_bytes());
        b[68..72].copy_from_slice(&4_100u32.to_le_bytes());

        let p = parse(&b).unwrap();
        assert!(!p.fast);
        match p.ev {
            InputEvent::ClientStats(c) => {
                assert_eq!(c.version, CLIENT_STATS_VERSION);
                assert_eq!(c.video_path, 1);
                assert_eq!(c.audio_path, 2);
                assert!(c.clock_offset_valid);
                assert!(c.flags & client_flags::SAB_AVAILABLE == 0);
                assert_eq!(c.arrival_to_paint_p50_us, 3_500);
                assert_eq!(c.host_to_paint_p50_us, 41_200);
                assert_eq!(c.frames_painted, 1799);
                assert_eq!(c.rtt_us, 4_100);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_truncated_or_versionless_stats_frame_is_rejected() {
        let mut short = vec![0u8; CLIENT_STATS_LEN - 1];
        short[0] = op::STATS;
        short[1] = CLIENT_STATS_VERSION;
        assert!(parse(&short).is_none());

        let mut v0 = vec![0u8; CLIENT_STATS_LEN];
        v0[0] = op::STATS;
        assert!(parse(&v0).is_none());
    }

    #[test]
    fn a_longer_stats_frame_from_a_newer_client_still_parses() {
        let mut b = vec![0u8; CLIENT_STATS_LEN + 16];
        b[0] = op::STATS;
        b[1] = 2;
        b[2] = 3;
        match parse(&b).unwrap().ev {
            InputEvent::ClientStats(c) => {
                assert_eq!(c.version, 2);
                assert_eq!(c.video_path, 3);
            }
            _ => panic!("wrong variant"),
        }
    }

    fn batch_frame(count: u16, tail: Option<(u64, u8, u8)>) -> Vec<u8> {
        let mut b = vec![op::POINTER_MOVE_BATCH, SRC_PEN];
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&count.to_le_bytes());
        for i in 0..count {
            b.extend_from_slice(&(i as f32 / 100.0).to_le_bytes());
            for _ in 0..5 {
                b.extend_from_slice(&0f32.to_le_bytes());
            }
        }
        if let Some((stamp, flags, predicted)) = tail {
            b.extend_from_slice(&stamp.to_le_bytes());
            b.push(flags);
            b.push(predicted);
        }
        b
    }

    #[test]
    fn a_v2_pointer_frame_carries_a_client_stamp_and_a_v1_one_does_not() {
        let mut v1 = [0u8; POINTER_LEN];
        v1[0] = op::POINTER_DOWN;
        assert_eq!(parse(&v1).unwrap().client_ns, 0);

        let mut v2 = [0u8; POINTER_LEN + STAMP_LEN];
        v2[0] = op::POINTER_DOWN;
        v2[POINTER_LEN..].copy_from_slice(&987_654_321u64.to_le_bytes());
        assert_eq!(parse(&v2).unwrap().client_ns, 987_654_321);
    }

    #[test]
    fn a_v2_batch_tail_carries_the_stamp_and_the_predicted_count() {
        let p = parse(&batch_frame(4, Some((42, 0, 2)))).unwrap();
        assert_eq!(p.client_ns, 42);
        match p.ev {
            InputEvent::PointerBatch {
                samples, predicted, ..
            } => {
                assert_eq!(samples.len(), 4);
                assert_eq!(predicted, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_predicted_count_larger_than_the_batch_is_clamped_not_trusted() {
        match parse(&batch_frame(2, Some((0, 0, 200)))).unwrap().ev {
            InputEvent::PointerBatch {
                samples, predicted, ..
            } => {
                assert_eq!(samples.len(), 2);
                assert_eq!(predicted, 2);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn mouse_delta_reads_the_unadjusted_flag_out_of_the_old_pad_byte() {
        let mut v1 = vec![op::MOUSE_DELTA, 0];
        v1.extend_from_slice(&3i16.to_le_bytes());
        v1.extend_from_slice(&(-4i16).to_le_bytes());
        v1.extend_from_slice(&0u16.to_le_bytes());
        v1.push(0); // the pad byte every shipped client already sends
        let p = parse(&v1).unwrap();
        assert_eq!(p.client_ns, 0);
        match p.ev {
            InputEvent::MouseDelta {
                dx, dy, unadjusted, ..
            } => {
                assert_eq!((dx, dy), (3, -4));
                assert!(!unadjusted);
            }
            _ => panic!("wrong variant"),
        }

        let mut v2 = v1[..MOUSE_DELTA_LEN].to_vec();
        v2.push(evflag::UNADJUSTED);
        v2.extend_from_slice(&7u64.to_le_bytes());
        let p = parse(&v2).unwrap();
        assert_eq!(p.client_ns, 7);
        match p.ev {
            InputEvent::MouseDelta { unadjusted, .. } => assert!(unadjusted),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_key_frame_stamp_sits_past_its_variable_tail() {
        let mut b = vec![op::KEY, 0b01, 0, 0, 4];
        b.extend_from_slice(b"KeyA");
        b.push(1);
        b.extend_from_slice(b"a");
        assert_eq!(parse(&b).unwrap().client_ns, 0);
        b.extend_from_slice(&555u64.to_le_bytes());
        assert_eq!(parse(&b).unwrap().client_ns, 555);
    }

    #[test]
    fn down_and_up_position_the_cursor_unless_the_pointer_is_locked() {
        for phase in [
            Phase::Down,
            Phase::Up,
            Phase::Move,
            Phase::Enter,
            Phase::Over,
        ] {
            assert!(positions_cursor(false, phase), "{phase:?} unlocked");
            assert!(!positions_cursor(true, phase), "{phase:?} locked");
        }
        for phase in [Phase::Leave, Phase::Out, Phase::Cancel] {
            assert!(
                !positions_cursor(false, phase),
                "{phase:?} carries no position"
            );
        }
    }

    #[test]
    fn a_drop_count_the_message_cannot_hold_is_rejected_before_it_allocates() {
        let mut b = vec![0u8; 12];
        b[0] = op::DROP;
        b[10..12].copy_from_slice(&u16::MAX.to_le_bytes());
        assert!(parse(&b).is_none());

        b[10..12].copy_from_slice(&((MAX_DROP_ITEMS + 1) as u16).to_le_bytes());
        assert!(parse(&b).is_none());
    }

    #[test]
    fn a_keyframe_request_parses_on_the_reliable_channel() {
        let p = parse(&[op::KEYFRAME_REQUEST, keyframe_reason::UNSPECIFIED]).unwrap();
        assert!(!p.fast, "an IDR request is not disposable");
        match p.ev {
            InputEvent::KeyframeRequest { reason } => {
                assert_eq!(reason, keyframe_reason::UNSPECIFIED)
            }
            _ => panic!("wrong variant"),
        }
        // A client that sends only the opcode still gets a request, not a dropped frame.
        assert!(matches!(
            parse(&[op::KEYFRAME_REQUEST]).unwrap().ev,
            InputEvent::KeyframeRequest { .. }
        ));
    }

    #[test]
    fn bye_carries_its_reason() {
        assert_eq!(build_bye(bye::KICKED), [op::BYE, bye::KICKED]);
        assert_eq!(build_bye(bye::HOST_EXIT), [op::BYE, bye::HOST_EXIT]);
        assert!(parse(&build_bye(bye::HOST_EXIT)).is_none());
    }
}
