//! The live stage's receiver: RTP packets off the in-process webrtc-rs client's video track,
//! reassembled into access units (RFC 6184 single NAL / STAP-A / FU-A), and decoded with
//! `VTDecompressionSession` on a thread of their own, the way a browser's hardware decoder would.
//!
//! Every frame records the host clock when its first and last packet came off the track (after
//! SRTP and the interceptor chain), the host capture time recovered from its RTP timestamp
//! (`host_ns_to_rtp90k` inverted — same process, same clock), when it was handed to the decoder,
//! when the decoder called back, and the frame stamp read out of the decoded luma.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use objc2_core_foundation::{CFNumber, CFRetained};
use objc2_core_media::CMTime;
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};

use super::cf;
use super::live_window::read_stamp;

/// 90 kHz ticks per 2^32 wrap, in ns.
const RTP_WRAP_NS: u128 = (1u128 << 32) * 1_000_000_000 / 90_000;

/// Host capture time (ns on the shared host clock) from a 90 kHz RTP timestamp, unwrapped
/// against the arrival time it cannot be later than.
pub fn rtp_to_host_ns(ts: u32, arrival_ns: u64) -> u64 {
    let base = ts as u128 * 1_000_000_000 / 90_000;
    let arrival = arrival_ns as u128;
    let mut wraps = 0u128;
    while base + (wraps + 1) * RTP_WRAP_NS <= arrival + RTP_WRAP_NS / 2 {
        wraps += 1;
    }
    (base + wraps * RTP_WRAP_NS) as u64
}

/// One access unit as it came off the wire.
pub struct RxFrame {
    pub seq: usize,
    pub rtp_ts: u32,
    pub capture_ns: u64,
    pub first_ns: u64,
    pub arrival_ns: u64,
    pub packets: usize,
    pub bytes: usize,
    pub keyframe: bool,
    /// Marker seen and no sequence gap since the previous frame's last packet, and every FU-A
    /// started where it should.
    pub complete: bool,
    pub nals: Vec<Vec<u8>>,
    /// Packets still missing when the frame was emitted.
    pub missing: usize,
    /// Packets that completed it out of order (retransmissions or reordering).
    pub recovered: usize,
    /// Emitted incomplete after waiting `MAX_HOLD_NS` for a repair.
    pub gave_up: bool,
}

/// What one frame's trip through the receiver came to.
#[derive(Clone, Debug, Default)]
pub struct FrameRec {
    pub seq: usize,
    pub rtp_ts: u32,
    pub capture_ns: u64,
    pub first_ns: u64,
    pub arrival_ns: u64,
    pub packets: usize,
    pub bytes: usize,
    pub keyframe: bool,
    pub complete: bool,
    /// Not decoded: incomplete, or waiting for a keyframe after one.
    pub skipped: bool,
    pub submit_ns: u64,
    pub done_ns: u64,
    pub submit_status: i32,
    pub decode_status: Option<i32>,
    pub stamp: Option<u32>,
    pub size: (usize, usize),
    pub missing: usize,
    pub recovered: usize,
    pub gave_up: bool,
}

/// Reassembles packets into access units, the way a real receiver's jitter buffer does: packets
/// are held by sequence number, so a retransmission (NACK repair) or a reordered packet still
/// completes its frame. A frame is emitted when every packet from the previous frame's marker to
/// its own has arrived; one with a hole is held up to `MAX_HOLD_NS` for the repair and then
/// emitted incomplete. Owned by the track-reader task.
///
/// The first version emitted a frame the moment its marker arrived and treated any sequence gap
/// as a loss, so NACK repair could never complete a frame — and every late retransmission broke
/// the frame it landed in as well. A 1,330-packet first keyframe with one loss was then never
/// decodable (r6, `live-hold-forced`).
pub struct Assembler {
    tx: Sender<RxFrame>,
    fu: Option<Vec<u8>>,
    broken: bool,
    /// Held packets by extended sequence number: (rtp ts, marker, payload, arrival ns, arrived
    /// after a higher sequence number had).
    pending: std::collections::BTreeMap<u64, Held>,
    next_start: Option<u64>,
    highest: Option<u64>,
    frames: usize,
    pub packets: u64,
    pub unsupported_nal: u64,
    /// Packets that arrived after a higher sequence number (retransmissions or reordering).
    pub out_of_order: u64,
    /// Packets that arrived for a frame already emitted (repair too late), and duplicates.
    pub too_late: u64,
    /// Sequence numbers first seen as a hole (each counted once, however it ended), and the ones
    /// still missing when their frame was emitted.
    pub holes: u64,
    pub never_arrived: u64,
    pub duplicates: u64,
}

/// One held packet: rtp ts, marker, payload, arrival ns, arrived out of order.
type Held = (u32, bool, Vec<u8>, u64, bool);

/// How long a frame with a hole waits for its repair before it is emitted incomplete.
const MAX_HOLD_NS: u64 = 300_000_000;

impl Assembler {
    pub fn new(tx: Sender<RxFrame>) -> Assembler {
        Assembler {
            tx,
            fu: None,
            broken: false,
            pending: std::collections::BTreeMap::new(),
            next_start: None,
            highest: None,
            frames: 0,
            packets: 0,
            unsupported_nal: 0,
            out_of_order: 0,
            too_late: 0,
            holes: 0,
            never_arrived: 0,
            duplicates: 0,
        }
    }

    fn extend(&self, seq: u16) -> u64 {
        let Some(h) = self.highest else {
            return (1u64 << 32) + seq as u64;
        };
        let base = h & !0xffff;
        [base.wrapping_sub(0x10000), base, base + 0x10000]
            .into_iter()
            .map(|b| b + seq as u64)
            .min_by_key(|c| c.abs_diff(h))
            .unwrap()
    }

    pub fn push(&mut self, seq: u16, ts: u32, marker: bool, payload: &[u8], now_ns: u64) {
        self.packets += 1;
        let ext = self.extend(seq);
        if self.next_start.is_some_and(|n| ext < n) {
            self.too_late += 1;
            return;
        }
        if self.pending.contains_key(&ext) {
            self.duplicates += 1;
            return;
        }
        let ooo = self.highest.is_some_and(|h| ext < h);
        if ooo {
            self.out_of_order += 1;
        }
        if let Some(h) = self.highest {
            self.holes += ext.saturating_sub(h + 1);
        }
        self.highest = Some(self.highest.map_or(ext, |h| h.max(ext)));
        if self.next_start.is_none() {
            self.next_start = Some(ext);
        }
        self.pending
            .insert(ext, (ts, marker, payload.to_vec(), now_ns, ooo));
        self.drain(now_ns);
    }

    fn drain(&mut self, now_ns: u64) {
        loop {
            let Some(start) = self.next_start else { return };
            // Complete: start, start+1, ... up to a marker, one timestamp, no hole.
            let mut end = None;
            if let Some(first) = self.pending.get(&start) {
                let ts = first.0;
                let mut k = start;
                while let Some(p) = self.pending.get(&k) {
                    if p.0 != ts {
                        // The next frame began without this one's marker: its tail was lost
                        // for good only if nothing can fill it, so treat it as the end.
                        end = Some((k - 1, false));
                        break;
                    }
                    if p.1 {
                        end = Some((k, true));
                        break;
                    }
                    k += 1;
                }
            }
            if let Some((last, marker)) = end {
                self.emit_range(start, last, marker, false, now_ns);
                continue;
            }
            // Still arriving (no packet past a hole) is not a loss: wait for it.
            let mut c = start;
            while self.pending.contains_key(&c) {
                c += 1;
            }
            if self.pending.range(c..).next().is_none() {
                return;
            }
            // A hole. Give up on it once the oldest held packet has waited long enough.
            let Some((&k0, oldest)) = self.pending.iter().next() else {
                return;
            };
            if now_ns.saturating_sub(oldest.3) < MAX_HOLD_NS {
                return;
            }
            let ts = oldest.0;
            let last = self
                .pending
                .range(k0..)
                .take_while(|(_, p)| p.0 == ts)
                .map(|(&k, _)| k)
                .last()
                .unwrap_or(k0);
            let marker = self.pending.get(&last).is_some_and(|p| p.1);
            self.emit_range(k0, last, marker, true, now_ns);
        }
    }

    fn emit_range(&mut self, start: u64, last: u64, marker: bool, gave_up: bool, now_ns: u64) {
        let expected_start = self.next_start.unwrap_or(start);
        let pkts: Vec<(u64, Held)> = {
            let keys: Vec<u64> = self.pending.range(start..=last).map(|(&k, _)| k).collect();
            keys.into_iter()
                .filter_map(|k| self.pending.remove(&k).map(|v| (k, v)))
                .collect()
        };
        // Anything older than this frame is gone for good.
        let stale: Vec<u64> = self.pending.range(..start).map(|(&k, _)| k).collect();
        for k in stale {
            self.pending.remove(&k);
        }
        self.next_start = Some(last + 1);
        let span = (last + 1 - start) as usize;
        let missing =
            span.saturating_sub(pkts.len()) + (start - expected_start.min(start)) as usize;
        self.never_arrived += missing as u64;
        let ts = pkts.first().map(|p| p.1 .0).unwrap_or(0);
        let first_ns = pkts.iter().map(|p| p.1 .3).min().unwrap_or(now_ns);
        let arrival_ns = pkts.iter().map(|p| p.1 .3).max().unwrap_or(now_ns);
        self.frames += 1;
        let mut f = RxFrame {
            seq: self.frames - 1,
            rtp_ts: ts,
            capture_ns: rtp_to_host_ns(ts, first_ns),
            first_ns,
            arrival_ns,
            packets: pkts.len(),
            bytes: pkts.iter().map(|p| p.1 .2.len()).sum(),
            keyframe: false,
            complete: false,
            nals: Vec::new(),
            missing,
            recovered: pkts.iter().filter(|p| p.1 .4).count(),
            gave_up,
        };
        self.broken = missing > 0;
        self.fu = None;
        for (_, p) in &pkts {
            self.depacketize(&mut f, &p.2);
        }
        if self.fu.take().is_some() {
            self.broken = true;
        }
        f.complete = marker && !self.broken;
        f.keyframe = f.nals.iter().any(|n| !n.is_empty() && n[0] & 0x1f == 5);
        self.broken = false;
        let _ = self.tx.send(f);
    }

    fn depacketize(&mut self, cur: &mut RxFrame, p: &[u8]) {
        if p.is_empty() {
            return;
        }
        match p[0] & 0x1f {
            1..=23 => cur.nals.push(p.to_vec()),
            24 => {
                let mut i = 1;
                while i + 2 <= p.len() {
                    let n = u16::from_be_bytes([p[i], p[i + 1]]) as usize;
                    i += 2;
                    if i + n > p.len() {
                        self.broken = true;
                        break;
                    }
                    cur.nals.push(p[i..i + n].to_vec());
                    i += n;
                }
            }
            28 if p.len() >= 2 => {
                let (start, end) = (p[1] & 0x80 != 0, p[1] & 0x40 != 0);
                if start {
                    if self.fu.is_some() {
                        self.broken = true;
                    }
                    let mut nal = Vec::with_capacity(p.len() * 8);
                    nal.push((p[0] & 0xe0) | (p[1] & 0x1f));
                    nal.extend_from_slice(&p[2..]);
                    self.fu = Some(nal);
                } else if let Some(nal) = self.fu.as_mut() {
                    nal.extend_from_slice(&p[2..]);
                } else {
                    self.broken = true;
                }
                if end {
                    if let Some(nal) = self.fu.take() {
                        cur.nals.push(nal);
                    }
                }
            }
            _ => self.unsupported_nal += 1,
        }
    }
}

// ---- VideoToolbox decode ---------------------------------------------------------------------

#[repr(C)]
struct CallbackRecord {
    cb: unsafe extern "C" fn(
        *mut c_void,
        *mut c_void,
        i32,
        u32,
        *mut CVPixelBuffer,
        CMTime,
        CMTime,
    ),
    refcon: *mut c_void,
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        alloc: *const c_void,
        fmt: *const c_void,
        spec: *const c_void,
        attrs: *const c_void,
        cb: *const CallbackRecord,
        out: *mut *mut c_void,
    ) -> i32;
    fn VTDecompressionSessionDecodeFrame(
        s: *mut c_void,
        sample: *const c_void,
        flags: u32,
        refcon: *mut c_void,
        info: *mut u32,
    ) -> i32;
    fn VTDecompressionSessionWaitForAsynchronousFrames(s: *mut c_void) -> i32;
    fn VTDecompressionSessionInvalidate(s: *mut c_void);
    fn VTDecompressionSessionCanAcceptFormatDescription(s: *mut c_void, d: *const c_void) -> u8;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        alloc: *const c_void,
        count: usize,
        ptrs: *const *const u8,
        sizes: *const usize,
        nal_header_len: i32,
        out: *mut *const c_void,
    ) -> i32;
    fn CMBlockBufferCreateWithMemoryBlock(
        alloc: *const c_void,
        mem: *mut c_void,
        len: usize,
        block_alloc: *const c_void,
        custom: *const c_void,
        offset: usize,
        data_len: usize,
        flags: u32,
        out: *mut *mut c_void,
    ) -> i32;
    fn CMBlockBufferReplaceDataBytes(
        src: *const c_void,
        bb: *mut c_void,
        off: usize,
        len: usize,
    ) -> i32;
    fn CMSampleBufferCreateReady(
        alloc: *const c_void,
        bb: *mut c_void,
        fmt: *const c_void,
        num_samples: isize,
        num_timing: isize,
        timing: *const c_void,
        num_sizes: isize,
        sizes: *const usize,
        out: *mut *mut c_void,
    ) -> i32;
}

unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

/// `kVTDecodeFrame_EnableAsynchronousDecompression | kVTDecodeFrame_1xRealTimePlayback`.
pub const DECODE_FLAGS: u32 = (1 << 0) | (1 << 3);

struct Done {
    seq: usize,
    done_ns: u64,
    status: i32,
    stamp: Option<u32>,
    size: (usize, usize),
}

struct CbCtx {
    tx: Sender<Done>,
    disp_w: usize,
    disp_h: usize,
}

unsafe extern "C" fn on_decoded(
    out_ref: *mut c_void,
    src_ref: *mut c_void,
    status: i32,
    _flags: u32,
    img: *mut CVPixelBuffer,
    _pts: CMTime,
    _dur: CMTime,
) {
    let done_ns = crate::streamer::audio::host_now_ns();
    let ctx = &*(out_ref as *const CbCtx);
    let seq = src_ref as usize;
    let (mut stamp, mut size) = (None, (0, 0));
    if status == 0 && !img.is_null() {
        let pb = &*img;
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
        let (w, h) = (CVPixelBufferGetWidth(pb), CVPixelBufferGetHeight(pb));
        let base = CVPixelBufferGetBaseAddressOfPlane(pb, 0) as *const u8;
        let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, 0);
        stamp = read_stamp(base, bpr, w, h, ctx.disp_w, ctx.disp_h);
        size = (w, h);
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags::ReadOnly);
    }
    let _ = ctx.tx.send(Done {
        seq,
        done_ns,
        status: if status == 0 && img.is_null() {
            -1
        } else {
            status
        },
        stamp,
        size,
    });
}

struct Decoder {
    session: *mut c_void,
    fmt: *const c_void,
    ctx: *mut CbCtx,
}

impl Decoder {
    fn set_format(&mut self, sps: &[u8], pps: &[u8]) -> Result<(), i32> {
        let ptrs = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        let mut fmt: *const c_void = ptr::null();
        let st = unsafe {
            CMVideoFormatDescriptionCreateFromH264ParameterSets(
                ptr::null(),
                2,
                ptrs.as_ptr(),
                sizes.as_ptr(),
                4,
                &mut fmt,
            )
        };
        if st != 0 || fmt.is_null() {
            return Err(st);
        }
        let keep = !self.session.is_null()
            && unsafe { VTDecompressionSessionCanAcceptFormatDescription(self.session, fmt) } != 0;
        if !self.fmt.is_null() {
            unsafe { CFRelease(self.fmt) };
        }
        self.fmt = fmt;
        if keep {
            return Ok(());
        }
        self.close_session();
        let cb = CallbackRecord {
            cb: on_decoded,
            refcon: self.ctx.cast(),
        };
        let num = CFNumber::new_i32(cf::PIXEL_420F as i32);
        let attrs = cf::dict(&[(unsafe { kCVPixelBufferPixelFormatTypeKey }, &num)]);
        let st = unsafe {
            VTDecompressionSessionCreate(
                ptr::null(),
                fmt,
                ptr::null(),
                CFRetained::as_ptr(&attrs).as_ptr().cast_const().cast(),
                &cb,
                &mut self.session,
            )
        };
        if st != 0 {
            self.session = ptr::null_mut();
            return Err(st);
        }
        Ok(())
    }

    fn decode(&mut self, avcc: &[u8], seq: usize) -> i32 {
        if self.session.is_null() || self.fmt.is_null() {
            return -2;
        }
        unsafe {
            let mut bb: *mut c_void = ptr::null_mut();
            let st = CMBlockBufferCreateWithMemoryBlock(
                ptr::null(),
                ptr::null_mut(),
                avcc.len(),
                ptr::null(),
                ptr::null(),
                0,
                avcc.len(),
                1, // kCMBlockBufferAssureMemoryNowFlag
                &mut bb,
            );
            if st != 0 || bb.is_null() {
                return if st != 0 { st } else { -3 };
            }
            CMBlockBufferReplaceDataBytes(avcc.as_ptr().cast(), bb, 0, avcc.len());
            let size = avcc.len();
            let mut sb: *mut c_void = ptr::null_mut();
            let st = CMSampleBufferCreateReady(
                ptr::null(),
                bb,
                self.fmt,
                1,
                0,
                ptr::null(),
                1,
                &size,
                &mut sb,
            );
            CFRelease(bb);
            if st != 0 || sb.is_null() {
                return if st != 0 { st } else { -4 };
            }
            let mut info = 0u32;
            let st = VTDecompressionSessionDecodeFrame(
                self.session,
                sb,
                DECODE_FLAGS,
                seq as *mut c_void,
                &mut info,
            );
            CFRelease(sb);
            st
        }
    }

    fn close_session(&mut self) {
        if !self.session.is_null() {
            unsafe {
                VTDecompressionSessionWaitForAsynchronousFrames(self.session);
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
            }
            self.session = ptr::null_mut();
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.close_session();
        if !self.fmt.is_null() {
            unsafe { CFRelease(self.fmt) };
        }
        if !self.ctx.is_null() {
            drop(unsafe { Box::from_raw(self.ctx) });
        }
    }
}

/// What the decode thread hands back when the frame channel closes.
pub struct RxResult {
    pub frames: Vec<FrameRec>,
    pub decoder_create_status: Option<i32>,
    pub format_errors: u64,
    pub plis_requested: u64,
}

/// Spawn the decode thread. `request_pli` is called (rate-limited) when a frame is incomplete or
/// fails to decode; decoding then waits for the next keyframe, as a browser's decoder does.
/// What the live stage's client reports to the host as its page would: the last 512 decoded
/// frames' arrival→decoded time (the page's `arrival_to_paint`, over the same 512-frame window),
/// decode time, frames "painted" (decoded), decode errors and keyframe requests — sent as the real
/// `OP::STATS` frame on the `reliable` channel, so the host's own parser and telemetry path run.
#[derive(Default)]
pub struct ClientWindow {
    inner: std::sync::Mutex<ClientInner>,
}

#[derive(Default)]
struct ClientInner {
    a2p: std::collections::VecDeque<u64>,
    dec: std::collections::VecDeque<u64>,
    painted: u64,
    errors: u64,
    key_requests: u64,
}

const CLIENT_WINDOW: usize = 512;

impl ClientWindow {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn note(&self, arrival_ns: u64, submit_ns: u64, done_ns: u64, ok: bool) {
        let Ok(mut g) = self.inner.lock() else { return };
        if !ok {
            g.errors += 1;
            return;
        }
        g.painted += 1;
        for (w, v) in [
            (0, done_ns.saturating_sub(arrival_ns)),
            (1, done_ns.saturating_sub(submit_ns)),
        ] {
            let q = if w == 0 { &mut g.a2p } else { &mut g.dec };
            if q.len() >= CLIENT_WINDOW {
                q.pop_front();
            }
            q.push_back(v);
        }
    }

    pub fn note_key_request(&self) {
        if let Ok(mut g) = self.inner.lock() {
            g.key_requests += 1;
        }
    }

    /// The 104-byte v3 STATS frame, laid out exactly as `input.js` writes it (little-endian):
    /// video path 1 (WebCodecs canvas), transport tier 1, render tier 1 — a client that paints.
    pub fn stats_frame(&self) -> Vec<u8> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let pct = |q: &std::collections::VecDeque<u64>| -> [u32; 3] {
            if q.is_empty() {
                return [0; 3];
            }
            let mut v: Vec<u64> = q.iter().copied().collect();
            v.sort_unstable();
            let at = |p: f64| (v[((p * v.len() as f64) as usize).min(v.len() - 1)] / 1_000) as u32;
            [at(0.5), at(0.9), at(0.99)]
        };
        let (a, d) = (pct(&g.a2p), pct(&g.dec));
        let mut b = vec![0u8; 104];
        b[0] = crate::streamer::input::protocol::op::STATS;
        b[1] = 3;
        b[2] = 1;
        b[3] = 0;
        let mut put = |o: usize, v: u32| b[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(8, a[0]);
        put(12, a[1]);
        put(16, a[2]);
        put(20, d[0]);
        put(24, d[1]);
        put(28, d[2]);
        put(44, g.painted as u32);
        put(52, g.errors as u32);
        put(56, g.key_requests as u32);
        b[72] = 1;
        b[73] = 1;
        b
    }
}

pub fn spawn_decoder(
    rx: Receiver<RxFrame>,
    disp: (usize, usize),
    decoded: Arc<AtomicU64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
    request_pli: Box<dyn Fn() + Send>,
    client: Option<Arc<ClientWindow>>,
) -> Receiver<RxResult> {
    let (res_tx, res_rx) = crossbeam_channel::bounded(1);
    std::thread::Builder::new()
        .name("bench-live-decode".into())
        .spawn(move || {
            crate::macos_utils::streamer::qos::pin_current_thread_user_initiated();
            let (dtx, drx) = unbounded::<Done>();
            let mut dec = Decoder {
                session: ptr::null_mut(),
                fmt: ptr::null(),
                ctx: Box::into_raw(Box::new(CbCtx {
                    tx: dtx,
                    disp_w: disp.0,
                    disp_h: disp.1,
                })),
            };
            let mut recs: Vec<FrameRec> = Vec::with_capacity(4096);
            let (mut sps, mut pps): (Vec<u8>, Vec<u8>) = (Vec::new(), Vec::new());
            let mut need_key = true;
            let mut create_status = None;
            let mut format_errors = 0u64;
            let mut plis = 0u64;
            let mut last_pli: Option<Instant> = None;
            let mut avcc: Vec<u8> = Vec::with_capacity(1 << 20);
            let pli = |plis: &mut u64, last: &mut Option<Instant>| {
                // A real receiver repeats it while it has nothing decodable (libwebrtc ~200 ms).
                if last.is_none_or(|t| t.elapsed() >= Duration::from_millis(250)) {
                    *last = Some(Instant::now());
                    *plis += 1;
                    request_pli();
                }
            };
            let mut index: HashMap<usize, usize> = HashMap::new();
            let drain = |recs: &mut Vec<FrameRec>, index: &HashMap<usize, usize>| {
                while let Ok(d) = drx.try_recv() {
                    if let Some(&i) = index.get(&d.seq) {
                        let r = &mut recs[i];
                        r.done_ns = d.done_ns;
                        r.decode_status = Some(d.status);
                        r.stamp = d.stamp;
                        r.size = d.size;
                        if let Some(c) = &client {
                            c.note(r.arrival_ns, r.submit_ns, d.done_ns, d.status == 0);
                        }
                        if d.status == 0 {
                            decoded.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            };
            loop {
                let f = match rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(f) => f,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        drain(&mut recs, &index);
                        if need_key {
                            pli(&mut plis, &mut last_pli);
                        }
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                let mut r = FrameRec {
                    seq: f.seq,
                    rtp_ts: f.rtp_ts,
                    capture_ns: f.capture_ns,
                    first_ns: f.first_ns,
                    arrival_ns: f.arrival_ns,
                    packets: f.packets,
                    bytes: f.bytes,
                    keyframe: f.keyframe,
                    complete: f.complete,
                    missing: f.missing,
                    recovered: f.recovered,
                    gave_up: f.gave_up,
                    ..Default::default()
                };
                let mut new_params = false;
                for n in &f.nals {
                    match n.first().map(|b| b & 0x1f) {
                        Some(7) if *n != sps => {
                            sps = n.clone();
                            new_params = true;
                        }
                        Some(8) if *n != pps => {
                            pps = n.clone();
                            new_params = true;
                        }
                        _ => {}
                    }
                }
                if new_params && !sps.is_empty() && !pps.is_empty() {
                    if let Err(st) = dec.set_format(&sps, &pps) {
                        create_status.get_or_insert(st);
                        format_errors += 1;
                    }
                }
                if !f.complete {
                    need_key = true;
                    r.skipped = true;
                    pli(&mut plis, &mut last_pli);
                } else if need_key && !f.keyframe {
                    r.skipped = true;
                    pli(&mut plis, &mut last_pli);
                } else {
                    need_key = false;
                    avcc.clear();
                    for n in &f.nals {
                        if matches!(n.first().map(|b| b & 0x1f), Some(7..=9) | None) {
                            continue;
                        }
                        avcc.extend_from_slice(&(n.len() as u32).to_be_bytes());
                        avcc.extend_from_slice(n);
                    }
                    r.submit_ns = crate::streamer::audio::host_now_ns();
                    index.insert(r.seq, recs.len());
                    r.submit_status = dec.decode(&avcc, r.seq);
                    if r.submit_status != 0 {
                        need_key = true;
                        pli(&mut plis, &mut last_pli);
                    }
                }
                recs.push(r);
                drain(&mut recs, &index);
            }
            // Frames still in the decoder come out on their own; the result is handed over BEFORE the
            // session is closed, because closing can wait on VideoToolbox for ever (seen at 1440p:
            // a FigSemaphore that never fires) and nothing may wait for that.
            let t = Instant::now();
            while recs
                .iter()
                .any(|r| r.submit_ns != 0 && r.submit_status == 0 && r.done_ns == 0)
                && t.elapsed() < Duration::from_millis(500)
            {
                std::thread::sleep(Duration::from_millis(5));
                drain(&mut recs, &index);
            }
            let _ = res_tx.send(RxResult {
                frames: recs,
                decoder_create_status: create_status,
                format_errors,
                plis_requested: plis,
            });
            dec.close_session();
        })
        .expect("spawn decode thread");
    res_rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_timestamps_invert_to_the_host_clock() {
        for ns in [0u64, 16_666_667, 5_000_000_000, 47_000_000_000_000] {
            let ts = crate::streamer::audio::host_ns_to_rtp90k(ns);
            let back = rtp_to_host_ns(ts, ns + 30_000_000);
            assert!(ns - back < 11_200, "{ns} -> {ts} -> {back}");
        }
        // Past one wrap (~13.25 h) the arrival time picks the right period.
        let ns = 50_000_000_000_000u64;
        let ts = crate::streamer::audio::host_ns_to_rtp90k(ns);
        let back = rtp_to_host_ns(ts, ns + 5_000_000);
        assert!(ns.abs_diff(back) < 11_200, "{back}");
    }

    #[test]
    fn stap_a_and_fu_a_reassemble() {
        let (tx, rx) = unbounded();
        let mut a = Assembler::new(tx);
        // STAP-A carrying SPS + PPS.
        let stap = [24u8, 0, 3, 0x67, 1, 2, 0, 2, 0x68, 9];
        a.push(10, 900, false, &stap, 1);
        // FU-A IDR in three pieces.
        a.push(11, 900, false, &[0x7c, 0x85, 0xaa], 2);
        a.push(12, 900, false, &[0x7c, 0x05, 0xbb], 3);
        a.push(13, 900, true, &[0x7c, 0x45, 0xcc], 4);
        let f = rx.try_recv().unwrap();
        assert!(f.complete && f.keyframe);
        assert_eq!(
            f.nals,
            vec![
                vec![0x67, 1, 2],
                vec![0x68, 9],
                vec![0x65, 0xaa, 0xbb, 0xcc]
            ]
        );
        assert_eq!((f.first_ns, f.arrival_ns, f.packets), (1, 4, 4));
        // A lost packet (14) holds the next frame for its repair...
        a.push(15, 1800, true, &[0x41, 1], 5);
        assert!(rx.try_recv().is_err());
        // ...and a retransmission completes it, counted as recovered.
        a.push(14, 1800, false, &[0x41, 0], 6);
        let g = rx.try_recv().unwrap();
        assert!(
            g.complete && g.recovered == 1 && g.missing == 0,
            "{:?}",
            (g.complete, g.recovered, g.missing)
        );
        // A packet lost for good: the frame waits MAX_HOLD_NS, then goes out incomplete.
        a.push(17, 2700, true, &[0x41, 2], 10);
        assert!(rx.try_recv().is_err());
        a.push(18, 3600, true, &[0x41, 3], 10 + MAX_HOLD_NS);
        let h = rx.try_recv().unwrap();
        assert!(!h.complete && h.gave_up && h.missing == 1);
        assert!(rx.try_recv().unwrap().complete);
        // The late repair for it is counted, not mis-assembled.
        a.push(16, 2700, false, &[0x41, 9], 20 + MAX_HOLD_NS);
        assert_eq!(a.too_late, 1);
    }
}
