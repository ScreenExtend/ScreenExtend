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
/// A `u16` sequence number, appended past the stamp tail on the fast-channel frames.
pub const SEQ_LEN: usize = 2;

/// Read the trailing client stamp a v2 frame may carry at `at`, or 0 when it carries none.
///
/// Every tail is appended past the v1 length, so a v1 client reads as 0 and a client newer
/// than this build can append further fields without breaking the parse. The declared length
/// is a floor, never an equality — the same rule the client stats frame already follows.
#[inline]
fn rd_stamp(b: &[u8], at: usize) -> u64 {
    rd_u64(b, at).unwrap_or(0)
}

/// Read the fast-channel sequence number a frame may carry at `at`.
///
/// Optional for the same reason the stamp is: a client that does not send it reads as `None`
/// and the injector simply does not reorder-gate that session.
#[inline]
fn rd_seq(b: &[u8], at: usize) -> Option<u16> {
    rd_u16(b, at)
}

/// Whether `seq` is newer than `last` in a wrapping `u16` space.
///
/// The comparison is the signed difference, so it stays correct across the wrap at 65535 as
/// long as the two are less than half a space apart — which at one event per pointer sample is
/// several minutes of continuous motion.
#[inline]
pub fn seq_is_newer(seq: u16, last: u16) -> bool {
    (seq.wrapping_sub(last) as i16) > 0
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
    /// `None` for anything that is not one of the eight pointer opcodes.
    ///
    /// This used to fall through to `Phase::Move`, which turned a future protocol mistake into a
    /// silently injected cursor move rather than a rejected frame.
    #[inline]
    pub fn from_opcode(op: u8) -> Option<Phase> {
        Some(match op {
            op::POINTER_DOWN => Phase::Down,
            op::POINTER_UP => Phase::Up,
            op::POINTER_MOVE => Phase::Move,
            op::POINTER_CANCEL => Phase::Cancel,
            op::POINTER_ENTER => Phase::Enter,
            op::POINTER_LEAVE => Phase::Leave,
            op::POINTER_OVER => Phase::Over,
            op::POINTER_OUT => Phase::Out,
            _ => return None,
        })
    }
}

/// Why a frame did not decode.
///
/// Every one of these used to be `None`, so a client whose protocol had drifted from the host's
/// produced a session where input silently did nothing. The opcode is carried on every variant
/// because that is what the log line has to name to be actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParseError {
    /// A zero-length message.
    Empty,
    /// A first byte no version of this protocol has ever defined.
    UnknownOpcode(u8),
    /// The frame is shorter than its own opcode requires.
    Truncated { op: u8, need: usize, got: usize },
    /// A length prefix pointed past the end of the frame, or a count was implausible.
    BadLength(u8),
    /// A string field was not valid UTF-8.
    BadUtf8(u8),
}

impl ParseError {
    pub fn opcode(self) -> Option<u8> {
        match self {
            Self::Empty => None,
            Self::UnknownOpcode(op)
            | Self::Truncated { op, .. }
            | Self::BadLength(op)
            | Self::BadUtf8(op) => Some(op),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "empty frame"),
            Self::UnknownOpcode(op) => write!(f, "unknown opcode 0x{op:02x}"),
            Self::Truncated { op, need, got } => {
                write!(
                    f,
                    "opcode 0x{op:02x} truncated: need {need} bytes, got {got}"
                )
            }
            Self::BadLength(op) => write!(f, "opcode 0x{op:02x} has a length prefix past the end"),
            Self::BadUtf8(op) => write!(f, "opcode 0x{op:02x} carries invalid UTF-8"),
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
    /// The fast channel's per-session sequence number, when the frame carried one.
    ///
    /// The fast channel is unordered and zero-retransmit, so two moves can be delivered out of
    /// order and the older one would walk the cursor backwards for a frame. `None` means the
    /// client is older than the field and this session simply is not reorder-gated.
    pub seq: Option<u16>,
}

/// Length a frame of `op` must reach before its fixed part can be read at all.
fn fixed_len(op: u8) -> Option<usize> {
    Some(match op {
        op::POINTER_DOWN
        | op::POINTER_UP
        | op::POINTER_MOVE
        | op::POINTER_CANCEL
        | op::POINTER_ENTER
        | op::POINTER_LEAVE
        | op::POINTER_OVER
        | op::POINTER_OUT => POINTER_LEN,
        op::POINTER_MOVE_BATCH => BATCH_HEADER,
        op::WHEEL => WHEEL_LEN,
        op::ZOOM => 5,
        op::MOUSE_DELTA => MOUSE_DELTA_LEN,
        op::KEY => 5,
        op::TEXT_INPUT | op::COMPOSITION_UPDATE => 5,
        op::CLIPBOARD => 6,
        op::DRAG => 10,
        op::DROP => 12,
        op::RESIZE => 9,
        op::FOCUS_STATE | op::VISIBILITY | op::POINTERLOCK_STATE => 2,
        op::PING | op::PONG => 9,
        op::STATS => CLIENT_STATS_LEN,
        op::KEYFRAME_REQUEST => 1,
        _ => return None,
    })
}

pub fn parse(b: &[u8]) -> Result<Parsed, ParseError> {
    let opcode = *b.first().ok_or(ParseError::Empty)?;
    let need = fixed_len(opcode).ok_or(ParseError::UnknownOpcode(opcode))?;
    if b.len() < need {
        return Err(ParseError::Truncated {
            op: opcode,
            need,
            got: b.len(),
        });
    }
    let short = || ParseError::BadLength(opcode);
    let mut client_ns = 0u64;
    let mut seq = None;
    let ev = match opcode {
        op::POINTER_DOWN
        | op::POINTER_UP
        | op::POINTER_MOVE
        | op::POINTER_CANCEL
        | op::POINTER_ENTER
        | op::POINTER_LEAVE
        | op::POINTER_OVER
        | op::POINTER_OUT => {
            let ev = parse_pointer(b).ok_or_else(short)?;
            client_ns = rd_stamp(b, POINTER_LEN);
            seq = rd_seq(b, POINTER_LEN + STAMP_LEN);
            ev
        }
        op::POINTER_MOVE_BATCH => parse_pointer_batch(b, &mut client_ns, &mut seq)?,
        op::WHEEL => {
            let ev = parse_wheel(b).ok_or_else(short)?;
            client_ns = rd_stamp(b, WHEEL_LEN);
            seq = rd_seq(b, WHEEL_LEN + STAMP_LEN);
            ev
        }
        op::ZOOM => parse_zoom(b).ok_or_else(short)?,
        op::MOUSE_DELTA => {
            let ev = parse_mouse_delta(b, &mut client_ns).ok_or_else(short)?;
            seq = rd_seq(b, MOUSE_DELTA_LEN + 1 + STAMP_LEN);
            ev
        }
        op::KEY => parse_key(b, &mut client_ns)?,
        op::TEXT_INPUT | op::COMPOSITION_UPDATE => parse_text(b, opcode)?,
        op::CLIPBOARD => parse_clipboard(b)?,
        op::DRAG => parse_drag(b).ok_or_else(short)?,
        op::DROP => parse_drop(b)?,
        op::RESIZE => parse_resize(b).ok_or_else(short)?,
        op::FOCUS_STATE => InputEvent::Lifecycle(Lifecycle::Focus(b[1] != 0)),
        op::VISIBILITY => InputEvent::Lifecycle(Lifecycle::Visibility(b[1] != 0)),
        op::POINTERLOCK_STATE => InputEvent::Lifecycle(Lifecycle::PointerLock(b[1] != 0)),
        op::PING => InputEvent::Ping {
            t_ns: rd_u64(b, 1).ok_or_else(short)?,
        },
        op::PONG => InputEvent::Pong {
            t_ns: rd_u64(b, 1).ok_or_else(short)?,
        },
        op::STATS => parse_client_stats(b).ok_or_else(short)?,
        op::KEYFRAME_REQUEST => InputEvent::KeyframeRequest {
            reason: b.get(1).copied().unwrap_or(keyframe_reason::UNSPECIFIED),
        },
        _ => return Err(ParseError::UnknownOpcode(opcode)),
    };
    Ok(Parsed {
        ev,
        fast: is_fast(opcode),
        client_ns,
        seq,
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
        phase: Phase::from_opcode(b[0])?,
    })
}

const BATCH_HEADER: usize = 10;
const SAMPLE_SIZE: usize = 24;
fn parse_pointer_batch(
    b: &[u8],
    client_ns: &mut u64,
    seq: &mut Option<u16>,
) -> Result<InputEvent, ParseError> {
    let bad = || ParseError::BadLength(op::POINTER_MOVE_BATCH);
    let source = b[1];
    let id = rd_u32(b, 2).ok_or_else(bad)?;
    let buttons = rd_u16(b, 6).ok_or_else(bad)?;
    let count = rd_u16(b, 8).ok_or_else(bad)? as usize;
    let need = BATCH_HEADER + count * SAMPLE_SIZE;
    if count == 0 {
        return Err(bad());
    }
    if b.len() < need {
        return Err(ParseError::Truncated {
            op: op::POINTER_MOVE_BATCH,
            need,
            got: b.len(),
        });
    }
    let mut samples = Vec::with_capacity(count);
    let mut o = BATCH_HEADER;
    for _ in 0..count {
        samples.push(MoveSample {
            x: rd_f32(b, o).ok_or_else(bad)?,
            y: rd_f32(b, o + 4).ok_or_else(bad)?,
            pressure: rd_f32(b, o + 8).ok_or_else(bad)?,
            tilt_x: rd_f32(b, o + 12).ok_or_else(bad)?,
            tilt_y: rd_f32(b, o + 16).ok_or_else(bad)?,
            twist: rd_f32(b, o + 20).ok_or_else(bad)?,
        });
        o += SAMPLE_SIZE;
    }
    let predicted = if b.len() >= need + BATCH_TAIL_LEN {
        *client_ns = rd_stamp(b, need);
        *seq = rd_seq(b, need + BATCH_TAIL_LEN);
        // A client that claims more predictions than it sent samples is malformed; clamping
        // rather than rejecting keeps the real samples, which are still good.
        b[need + STAMP_LEN + 1].min(count as u8)
    } else {
        0
    };
    Ok(InputEvent::PointerBatch {
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

fn parse_key(b: &[u8], client_ns: &mut u64) -> Result<InputEvent, ParseError> {
    let bad = || ParseError::BadLength(op::KEY);
    let utf8 = || ParseError::BadUtf8(op::KEY);
    let flags = b[1];
    let mods = rd_u16(b, 2).ok_or_else(bad)?;
    let code_len = b[4] as usize;
    let code_start = 5;
    let code_end = code_start + code_len;
    let code =
        std::str::from_utf8(b.get(code_start..code_end).ok_or_else(bad)?).map_err(|_| utf8())?;
    let key_len = *b.get(code_end).ok_or_else(bad)? as usize;
    let key_start = code_end + 1;
    let key_end = key_start + key_len;
    let key =
        std::str::from_utf8(b.get(key_start..key_end).ok_or_else(bad)?).map_err(|_| utf8())?;
    *client_ns = rd_stamp(b, key_end);
    Ok(InputEvent::Key {
        down: flags & 0b01 != 0,
        repeat: flags & 0b10 != 0,
        mods,
        code: SmolStr::new(code),
        key: SmolStr::new(key),
    })
}

fn parse_text(b: &[u8], opcode: u8) -> Result<InputEvent, ParseError> {
    let len = rd_u32(b, 1).ok_or(ParseError::BadLength(opcode))? as usize;
    let text = b
        .get(5..5 + len)
        .ok_or(ParseError::BadLength(opcode))?
        .to_vec();
    Ok(InputEvent::Text {
        composing: opcode == op::COMPOSITION_UPDATE,
        s: String::from_utf8(text).map_err(|_| ParseError::BadUtf8(opcode))?,
    })
}

fn parse_clipboard(b: &[u8]) -> Result<InputEvent, ParseError> {
    let bad = || ParseError::BadLength(op::CLIPBOARD);
    let utf8 = || ParseError::BadUtf8(op::CLIPBOARD);
    let op = b[1];
    let mime_len = rd_u32(b, 2).ok_or_else(bad)? as usize;
    let mime_start = 6;
    let mime_end = mime_start + mime_len;
    let mime = std::str::from_utf8(b.get(mime_start..mime_end).ok_or_else(bad)?)
        .map_err(|_| utf8())?
        .to_string();
    let data_len = rd_u32(b, mime_end).ok_or_else(bad)? as usize;
    let data_start = mime_end + 4;
    let data = b
        .get(data_start..data_start + data_len)
        .ok_or_else(bad)?
        .to_vec();
    Ok(InputEvent::Clipboard { op, mime, data })
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

fn parse_drop(b: &[u8]) -> Result<InputEvent, ParseError> {
    let bad = || ParseError::BadLength(op::DROP);
    let utf8 = || ParseError::BadUtf8(op::DROP);
    let x = rd_f32(b, 2).ok_or_else(bad)?;
    let y = rd_f32(b, 6).ok_or_else(bad)?;
    let count = rd_u16(b, 10).ok_or_else(bad)? as usize;
    // The count sizes a `Vec<DropItem>` (~72 B each), so it has to be bounded by what the
    // message could actually contain before it is believed: a bare 12-byte header claiming
    // 65535 items would otherwise allocate ~4.7 MB and then fail on the first read.
    let floor = count
        .checked_mul(DROP_ITEM_MIN)
        .and_then(|n| n.checked_add(12))
        .ok_or_else(bad)?;
    if count > MAX_DROP_ITEMS || b.len() < floor {
        return Err(bad());
    }
    let mut items = Vec::with_capacity(count);
    let mut pos = 12usize;
    for _ in 0..count {
        let name_len = rd_u16(b, pos).ok_or_else(bad)? as usize;
        pos += 2;
        let name_end = pos.checked_add(name_len).ok_or_else(bad)?;
        let name = std::str::from_utf8(b.get(pos..name_end).ok_or_else(bad)?)
            .map_err(|_| utf8())?
            .to_string();
        pos = name_end;
        let mime_len = rd_u32(b, pos).ok_or_else(bad)? as usize;
        pos += 4;
        let mime_end = pos.checked_add(mime_len).ok_or_else(bad)?;
        let mime = std::str::from_utf8(b.get(pos..mime_end).ok_or_else(bad)?)
            .map_err(|_| utf8())?
            .to_string();
        pos = mime_end;
        let size = rd_u64(b, pos).ok_or_else(bad)?;
        pos += 8;
        let data_len = rd_u64(b, pos).ok_or_else(bad)? as usize;
        pos += 8;
        let data_end = pos.checked_add(data_len).ok_or_else(bad)?;
        let data = b.get(pos..data_end).ok_or_else(bad)?.to_vec();
        pos = data_end;
        items.push(DropItem {
            name,
            mime,
            size,
            data,
        });
    }
    Ok(InputEvent::Drop { x, y, items })
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
/// Wire size of a v2 frame: the v1 prefix plus the tier bytes, the degradation field and the
/// three audio numbers. Bytes 0..72 are byte-identical to v1.
pub const CLIENT_STATS_V2_LEN: usize = 96;
/// Wire size of a v3 frame: the v2 prefix plus the two audio-redundancy counters.
/// Bytes 0..96 are byte-identical to v2 and 0..72 to v1.
pub const CLIENT_STATS_V3_LEN: usize = 104;
pub const CLIENT_STATS_VERSION: u8 = 3;

fn parse_client_stats(b: &[u8]) -> Option<InputEvent> {
    if b.len() < CLIENT_STATS_LEN {
        return None;
    }
    let version = b[1];
    if version == 0 {
        return None;
    }
    // The declared length is a floor: a v1 client sends 72 bytes and a client newer than this
    // build may send more than 96. Take the tail only if all of it arrived.
    let tail: &[u8] = if b.len() >= CLIENT_STATS_V2_LEN {
        &b[CLIENT_STATS_LEN..CLIENT_STATS_V2_LEN]
    } else {
        &[]
    };
    // Each tail is read only when the whole of it arrived, and independently: a v3 client
    // against a host that stopped at v2 loses only these two fields, and a v2 client here
    // leaves them at 0, which is what "never sent one" and "not measured" both mean.
    let tail3: &[u8] = if b.len() >= CLIENT_STATS_V3_LEN {
        &b[CLIENT_STATS_V2_LEN..CLIENT_STATS_V3_LEN]
    } else {
        &[]
    };
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
            // The tail is read only when the whole of it is present. A v1 client leaves these
            // at 0, which is also what "not measured" means for every one of them.
            transport_tier: tail.first().copied().unwrap_or(0),
            render_tier: tail.get(1).copied().unwrap_or(0),
            key_tier: tail.get(2).copied().unwrap_or(0),
            sink_tier: tail.get(3).copied().unwrap_or(0),
            suppressed_errors: tail_u32(tail, 4),
            degraded_flags: tail_u32(tail, 8),
            audio_target_us: tail_u32(tail, 12),
            audio_concealed: tail_u32(tail, 16),
            audio_output_latency_us: tail_u32(tail, 20),
            audio_redundant_used: tail_u32(tail3, 0),
            audio_redundant_discarded: tail_u32(tail3, 4),
        },
    )))
}

#[inline]
fn tail_u32(tail: &[u8], at: usize) -> u32 {
    rd_u32(tail, at).unwrap_or(0)
}

/// What this host understands, so a client can feature-detect instead of assuming.
///
/// Announced in the `PONG` tail, which every client already receives once a second. A host older
/// than the tail sends 17 bytes and the client reads no bits, which is the correct answer for it.
pub mod hostcap {
    /// Parses `KEYFRAME_REQUEST` (`0x64`) and routes it to the encoder's IDR path.
    pub const KEYFRAME_REQUEST: u16 = 1 << 0;
    /// Reads the 96-byte v2 client stats frame, not only its v1 prefix.
    pub const STATS_V2: u16 = 1 << 1;
    /// Accepts a separate unordered/unreliable channel for pointer moves. Not implemented: the
    /// existing `fast` channel is already unordered with `maxRetransmits: 0`, so there is
    /// nothing for a second one to add. Reserved so the bit cannot be reused for anything else.
    pub const UNRELIABLE_MOVE_CHANNEL: u16 = 1 << 2;
    /// Reads the optional trailing client timestamp on the pointer, batch, wheel, key and
    /// mouse-delta frames, and reports the resulting one-way delay as `input_recv`. A client
    /// that does not see this bit can skip the eight bytes.
    pub const INPUT_TIMESTAMP: u16 = 1 << 3;
    /// Reads the `MOUSE_DELTA` flags byte, so reporting `unadjustedMovement` is worth doing.
    pub const INPUT_EVENT_FLAGS: u16 = 1 << 4;
    /// Reads the optional fast-channel `u16` sequence and rejects a positional event whose
    /// sequence is behind one already injected. A client that does not see this bit can skip
    /// the two bytes; sending them to a host that ignores them costs nothing either way.
    pub const INPUT_SEQUENCE: u16 = 1 << 5;
}

/// The bits this build sets. See `docs/INPUT-PROTOCOL.md`, "Host capabilities".
pub const HOST_CAPABILITIES: u16 = hostcap::KEYFRAME_REQUEST
    | hostcap::STATS_V2
    | hostcap::INPUT_TIMESTAMP
    | hostcap::INPUT_EVENT_FLAGS
    | hostcap::INPUT_SEQUENCE;

/// `[PONG][echoed client stamp u64][host_now_ns u64][host capabilities u16]`.
///
/// The tail is appended, never inserted: bytes 0..17 are byte-identical to the previous form, so
/// a client that reads only those keeps working, and one that reads past them learns what this
/// host can do without a handshake.
pub fn build_pong(t_ns: u64, host_ns: u64) -> [u8; 19] {
    let mut out = [0u8; 19];
    out[0] = op::PONG;
    out[1..9].copy_from_slice(&t_ns.to_le_bytes());
    out[9..17].copy_from_slice(&host_ns.to_le_bytes());
    out[17..19].copy_from_slice(&HOST_CAPABILITIES.to_le_bytes());
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
        assert!(parse(&[op::POINTER_MOVE, 0, 0]).is_err());
        assert!(parse(&[]).is_err());
    }

    /// A rejection has to say *what* was wrong, or a client whose page has drifted from this
    /// host build produces a session where input silently does nothing.
    #[test]
    fn every_rejection_names_its_reason() {
        assert_eq!(parse(&[]).unwrap_err(), ParseError::Empty);
        assert_eq!(parse(&[0xff]).unwrap_err(), ParseError::UnknownOpcode(0xff));
        assert_eq!(
            parse(&[op::POINTER_MOVE, 0, 0]).unwrap_err(),
            ParseError::Truncated {
                op: op::POINTER_MOVE,
                need: POINTER_LEN,
                got: 3,
            }
        );
        // A text frame whose length prefix runs past the end, then one whose bytes are not
        // UTF-8: the same opcode, two different answers.
        let mut over = vec![op::TEXT_INPUT];
        over.extend_from_slice(&99u32.to_le_bytes());
        over.extend_from_slice(b"hi");
        assert_eq!(
            parse(&over).unwrap_err(),
            ParseError::BadLength(op::TEXT_INPUT)
        );

        let mut bad = vec![op::TEXT_INPUT];
        bad.extend_from_slice(&2u32.to_le_bytes());
        bad.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(
            parse(&bad).unwrap_err(),
            ParseError::BadUtf8(op::TEXT_INPUT)
        );
    }

    /// The eight pointer opcodes are the only ones that name a phase. Anything else used to
    /// come back as `Move`, which would have turned a protocol mistake into an injected cursor
    /// move rather than a rejection.
    #[test]
    fn only_pointer_opcodes_have_a_phase() {
        assert_eq!(Phase::from_opcode(op::POINTER_DOWN), Some(Phase::Down));
        assert_eq!(Phase::from_opcode(op::POINTER_OUT), Some(Phase::Out));
        assert_eq!(Phase::from_opcode(op::WHEEL), None);
        assert_eq!(Phase::from_opcode(op::KEY), None);
        assert_eq!(Phase::from_opcode(0x00), None);
    }

    #[test]
    fn the_fast_channel_sequence_number_is_optional_on_every_frame_that_carries_one() {
        // Pointer: v1, then stamp, then stamp + seq.
        let mut b = vec![0u8; POINTER_LEN];
        b[0] = op::POINTER_MOVE;
        assert_eq!(parse(&b).unwrap().seq, None);
        b.extend_from_slice(&7u64.to_le_bytes());
        assert_eq!(parse(&b).unwrap().seq, None);
        b.extend_from_slice(&4242u16.to_le_bytes());
        assert_eq!(parse(&b).unwrap().seq, Some(4242));

        // Mouse delta: v1 8 bytes, flags byte, stamp, seq.
        let mut d = vec![0u8; MOUSE_DELTA_LEN];
        d[0] = op::MOUSE_DELTA;
        assert_eq!(parse(&d).unwrap().seq, None);
        d.push(0);
        d.extend_from_slice(&9u64.to_le_bytes());
        d.extend_from_slice(&5u16.to_le_bytes());
        assert_eq!(parse(&d).unwrap().seq, Some(5));

        // Batch: past the stamp + flags + predicted tail.
        let mut f = batch_frame(2, Some((42, 0, 0)));
        assert_eq!(parse(&f).unwrap().seq, None);
        f.extend_from_slice(&1234u16.to_le_bytes());
        assert_eq!(parse(&f).unwrap().seq, Some(1234));
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
    fn the_pong_tail_announces_what_this_host_understands() {
        let pong = build_pong(1, 2);
        // The v1 prefix has to stay byte-identical, or a client reading only it breaks.
        assert_eq!(pong[0], op::PONG);
        assert_eq!(u64::from_le_bytes(pong[1..9].try_into().unwrap()), 1);
        assert_eq!(u64::from_le_bytes(pong[9..17].try_into().unwrap()), 2);

        let caps = u16::from_le_bytes(pong[17..19].try_into().unwrap());
        assert_eq!(caps, HOST_CAPABILITIES);
        assert!(
            caps & hostcap::KEYFRAME_REQUEST != 0,
            "0x64 reaches request_idr"
        );
        assert!(caps & hostcap::STATS_V2 != 0, "the 96-byte tail is read");
        assert!(
            caps & hostcap::INPUT_TIMESTAMP != 0,
            "client stamps become input_recv"
        );
        assert!(
            caps & hostcap::INPUT_EVENT_FLAGS != 0,
            "the MOUSE_DELTA flags byte is read"
        );
        assert!(
            caps & hostcap::INPUT_SEQUENCE != 0,
            "the fast-channel sequence is read and reorder-gated"
        );
        assert!(
            caps & hostcap::UNRELIABLE_MOVE_CHANNEL == 0,
            "there is no second move channel; the fast one is already unordered"
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
        assert!(parse(&short).is_err());

        let mut v0 = vec![0u8; CLIENT_STATS_LEN];
        v0[0] = op::STATS;
        assert!(parse(&v0).is_err());
    }

    #[test]
    fn a_v2_stats_frame_carries_the_tier_bytes_and_the_degradation_field() {
        use crate::streamer::telemetry::client_degraded_list;
        let mut b = vec![0u8; CLIENT_STATS_V2_LEN];
        b[0] = op::STATS;
        b[1] = CLIENT_STATS_VERSION;
        b[72] = 2; // encoded-streams transport
        b[73] = 1; // offscreen canvas
        b[74] = 0; // cannot ask for a keyframe: the Chrome 94-140 case
        b[75] = 4; // selectAudioOutput
        b[76..80].copy_from_slice(&9u32.to_le_bytes());
        b[80..84].copy_from_slice(&((1u32 << 11) | (1u32 << 19)).to_le_bytes());
        b[84..88].copy_from_slice(&40_000u32.to_le_bytes());
        b[92..96].copy_from_slice(&12_000u32.to_le_bytes());

        match parse(&b).unwrap().ev {
            InputEvent::ClientStats(c) => {
                assert_eq!(
                    (c.transport_tier, c.render_tier, c.key_tier, c.sink_tier),
                    (2, 1, 0, 4)
                );
                assert_eq!(c.suppressed_errors, 9);
                assert_eq!(c.audio_target_us, 40_000);
                assert_eq!(c.audio_output_latency_us, 12_000);
                assert_eq!(
                    client_degraded_list(c.degraded_flags),
                    ["no_keyframe_request", "stepped_down_to_encoded_streams"]
                );
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_v1_stats_frame_still_parses_and_leaves_the_tail_zero() {
        let mut b = vec![0u8; CLIENT_STATS_LEN];
        b[0] = op::STATS;
        b[1] = 1;
        b[2] = 3;
        match parse(&b).unwrap().ev {
            InputEvent::ClientStats(c) => {
                assert_eq!(c.version, 1);
                assert_eq!(c.video_path, 3);
                assert_eq!(c.transport_tier, 0);
                assert_eq!(c.degraded_flags, 0);
                assert_eq!(c.audio_output_latency_us, 0);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_truncated_v2_tail_is_treated_as_absent_rather_than_half_read() {
        let mut b = vec![0u8; CLIENT_STATS_V2_LEN - 1];
        b[0] = op::STATS;
        b[1] = CLIENT_STATS_VERSION;
        b[72] = 2;
        match parse(&b).unwrap().ev {
            InputEvent::ClientStats(c) => assert_eq!(c.transport_tier, 0),
            _ => panic!("wrong variant"),
        }
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

    #[test]
    fn the_v3_tail_is_read_whole_or_not_at_all() {
        let mut b = vec![0u8; CLIENT_STATS_V3_LEN];
        b[0] = op::STATS;
        b[1] = 3;
        b[CLIENT_STATS_V2_LEN..CLIENT_STATS_V2_LEN + 4].copy_from_slice(&17u32.to_le_bytes());
        b[CLIENT_STATS_V2_LEN + 4..CLIENT_STATS_V3_LEN].copy_from_slice(&265u32.to_le_bytes());
        match parse(&b).unwrap().ev {
            InputEvent::ClientStats(c) => {
                assert_eq!(c.audio_redundant_used, 17);
                assert_eq!(c.audio_redundant_discarded, 265);
            }
            _ => panic!("wrong variant"),
        }

        // One byte short of the tail reads as absent, never as half-parsed — the same floor
        // rule the v2 tail follows, and the reason a v2 host can read a v3 client at all.
        let short = &b[..CLIENT_STATS_V3_LEN - 1];
        match parse(short).unwrap().ev {
            InputEvent::ClientStats(c) => {
                assert_eq!(c.audio_redundant_used, 0);
                assert_eq!(c.audio_redundant_discarded, 0);
                // Everything before it still parses.
                assert_eq!(c.version, 3);
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
        assert!(parse(&b).is_err());

        b[10..12].copy_from_slice(&((MAX_DROP_ITEMS + 1) as u16).to_le_bytes());
        assert!(parse(&b).is_err());
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
        assert!(parse(&build_bye(bye::HOST_EXIT)).is_err());
    }

    // ---- golden vectors: the same bytes the shipped client produced --------------------
    //
    // `client-frames.json` is written by `node scripts/test-client-input.mjs --write` from the
    // real `input.js` under Node. That harness asserts the bytes against
    // `docs/INPUT-PROTOCOL.md`; this asserts that the parser turns them into the events the
    // host acts on. Both sides check the same file, so neither can drift on its own — which is
    // the gap the audit named: the format was tested here and written there, with nothing
    // between them.
    //
    // Regenerate after any deliberate change to what the client emits, and read the diff: a
    // changed vector is a wire-format change and every shipped client is on the old one.

    const CLIENT_FRAMES: &str = include_str!("client-frames.json");

    /// One `{ "name", "channel", "hex" }` entry, without pulling serde_json into the test.
    fn golden_vectors() -> Vec<(String, String, Vec<u8>)> {
        fn field<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
            let at = obj.find(&format!("\"{key}\""))? + key.len() + 2;
            let rest = &obj[at..];
            let open = rest.find('"')? + 1;
            let close = open + rest[open..].find('"')?;
            Some(&rest[open..close])
        }

        let vectors_at = CLIENT_FRAMES
            .find("\"vectors\"")
            .expect("client-frames.json has no `vectors`");
        let mut out = Vec::new();
        for chunk in CLIENT_FRAMES[vectors_at..].split('{').skip(1) {
            let obj = &chunk[..chunk.find('}').unwrap_or(chunk.len())];
            let (Some(name), Some(channel), Some(hex)) =
                (field(obj, "name"), field(obj, "channel"), field(obj, "hex"))
            else {
                continue;
            };
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                .collect();
            out.push((name.to_string(), channel.to_string(), bytes));
        }
        assert!(
            !out.is_empty(),
            "no vectors parsed out of client-frames.json"
        );
        out
    }

    fn vector(name: &str) -> (String, Vec<u8>) {
        golden_vectors()
            .into_iter()
            .find(|(n, ..)| n == name)
            .map(|(_, ch, b)| (ch, b))
            .unwrap_or_else(|| panic!("no golden vector named {name}"))
    }

    #[test]
    fn every_golden_vector_parses() {
        // The blunt half: nothing the shipped client sends may come back as a parse error, and
        // the routing decision has to agree with the channel it went out on. A fast opcode on
        // the reliable channel is legal — the host routes on the opcode — but a *reliable*
        // opcode on the fast channel would mean the client is putting ordered input on a lossy
        // link, which no vector should show.
        for (name, channel, bytes) in golden_vectors() {
            let parsed = parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e:?}"));
            if channel == "fast" {
                assert!(
                    parsed.fast,
                    "{name} went on the fast channel but is not fast"
                );
            }
        }
    }

    #[test]
    fn a_pen_pointer_down_carries_its_tilt_and_pressure() {
        let (channel, bytes) = vector("pointer_down_pen");
        assert_eq!(channel, "reliable");
        let parsed = parse(&bytes).expect("pointer_down_pen");
        assert!(!parsed.fast, "a DOWN must not go on the lossy queue");
        match parsed.ev {
            InputEvent::Pointer {
                source,
                id,
                pressure,
                tilt_x,
                tilt_y,
                twist,
                buttons,
                phase,
                x,
                y,
                ..
            } => {
                assert_eq!(source, 2, "pen");
                assert_eq!(id, 7);
                assert_eq!(phase, Phase::Down);
                assert_eq!(buttons, 1);
                assert!((pressure - 0.5).abs() < 1e-4, "pressure {pressure}");
                assert!((tilt_x + 20.0).abs() < 1e-3, "tiltX {tilt_x}");
                assert!((tilt_y - 30.0).abs() < 1e-3, "tiltY {tilt_y}");
                assert!((twist - 90.0).abs() < 1e-3, "twist {twist}");
                // Normalised, not CSS pixels: the host scales them by the virtual display's own
                // size, which is not the client's.
                assert!(
                    (0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y),
                    "{x},{y}"
                );
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn the_pointer_opcode_is_the_phase() {
        for (name, want) in [
            ("pointer_down_pen", Phase::Down),
            ("pointer_up_pen", Phase::Up),
            ("pointer_cancel_touch", Phase::Cancel),
        ] {
            let (_, bytes) = vector(name);
            match parse(&bytes).expect(name).ev {
                InputEvent::Pointer { phase, .. } => assert_eq!(phase, want, "{name}"),
                other => panic!("{name} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn a_wheel_in_line_mode_keeps_its_mode_byte() {
        // The mode decides whether the host multiplies by a line height or takes the delta as
        // pixels. Losing it turns a three-line scroll into a three-pixel one.
        let (_, bytes) = vector("wheel_lines");
        let parsed = parse(&bytes).expect("wheel_lines");
        assert!(parsed.fast, "wheel is a fast opcode");
        match parsed.ev {
            InputEvent::Wheel {
                dx, dy, dz, mode, ..
            } => {
                assert!((dx + 12.5).abs() < 1e-4, "dx {dx}");
                assert!((dy - 120.0).abs() < 1e-3, "dy {dy}");
                assert!(dz.abs() < 1e-6, "dz {dz}");
                assert_eq!(mode, 1, "line mode");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_key_frame_carries_both_strings_and_the_modifier_state() {
        let (_, bytes) = vector("key_down_shift_a");
        match parse(&bytes).expect("key_down_shift_a").ev {
            InputEvent::Key {
                down,
                repeat,
                mods,
                code,
                key,
            } => {
                assert!(down);
                assert!(!repeat);
                assert_ne!(mods, 0, "shift was held and must reach the host");
                assert_eq!(code.as_str(), "KeyA");
                assert_eq!(key.as_str(), "a");
            }
            other => panic!("parsed as {other:?}"),
        }

        let (_, bytes) = vector("key_up_a");
        match parse(&bytes).expect("key_up_a").ev {
            InputEvent::Key { down, code, .. } => {
                assert!(!down);
                assert_eq!(code.as_str(), "KeyA");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_locked_mouse_delta_is_relative_and_signed() {
        // Signed: a leftward move is a negative dx, and reading it unsigned sends the cursor
        // 65 531 pixels the other way.
        let (_, bytes) = vector("mouse_delta_locked");
        let parsed = parse(&bytes).expect("mouse_delta_locked");
        assert!(parsed.fast);
        match parsed.ev {
            InputEvent::MouseDelta {
                dx,
                dy,
                buttons,
                unadjusted,
            } => {
                assert_eq!(dx, -5);
                assert_eq!(dy, 3);
                assert_eq!(buttons, 2);
                assert!(!unadjusted, "the client had no unadjustedMovement grant");
            }
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_ping_carries_the_clients_own_monotonic_stamp() {
        let (_, bytes) = vector("ping");
        match parse(&bytes).expect("ping").ev {
            InputEvent::Ping { t_ns } => assert_eq!(t_ns, 12_345_000_000),
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn losing_focus_is_a_lifecycle_event() {
        let (_, bytes) = vector("focus_lost");
        match parse(&bytes).expect("focus_lost").ev {
            InputEvent::Lifecycle(Lifecycle::Focus(false)) => {}
            other => panic!("parsed as {other:?}"),
        }
    }

    #[test]
    fn a_golden_vector_cut_below_its_fixed_length_is_refused() {
        // The fixture doubles as a corpus of the shapes that actually occur off the wire, and a
        // short SCTP message is something a peer can send. Anything below the frame's fixed part
        // has to be an error rather than a read past the end.
        //
        // At or above it is *not* an error, deliberately: the optional stamp, flags byte and
        // fast-channel sequence are all tails an older client simply does not send, so a
        // MOUSE_DELTA cut to its 8 fixed bytes is a valid frame from a client that predates the
        // flags byte. That is the compatibility rule, not a gap in the check.
        for (name, _, bytes) in golden_vectors() {
            let fixed = fixed_len(bytes[0]).unwrap_or_else(|| panic!("{name}: unknown opcode"));
            for cut in 0..fixed {
                if let Ok(p) = parse(&bytes[..cut]) {
                    panic!(
                        "{name} truncated to {cut} of its {fixed} fixed bytes parsed as {:?}",
                        p.ev
                    );
                }
            }
            // No claim about `fixed` itself: for the variable-length frames it is only the
            // header before the length-prefixed strings, so `KEY` cut to 5 bytes is correctly a
            // `BadLength`. What has to hold is that nothing *below* it is ever read.
        }
    }
}
