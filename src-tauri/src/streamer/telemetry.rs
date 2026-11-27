//! Per-stage frame timing and degradation counters.
//!
//! Every timestamp is nanoseconds on the shared host clock,
//! [`crate::streamer::audio::host_now_ns`]: the epoch the video RTP timestamps and the audio
//! `capture_ns` ride, so a number from this module compares directly with one the client reports
//! over `OP::STATS`.
//!
//! Nothing on the record path allocates, locks or logs: the capture callbacks and the audio
//! capture thread are real-time contexts. Histograms are fixed log-scale buckets, so percentiles
//! need no sample storage.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Sub-buckets per power of two, as a shift: 8 gives at most ~9% bucket width for 512 counters.
const SUB_BITS: u32 = 3;
const SUBS: usize = 1 << SUB_BITS;
const OCTAVES: usize = 64;
const BUCKETS: usize = OCTAVES * SUBS;

/// Bucket index for a value. Values below `SUBS` get a bucket each.
#[inline]
fn bucket_of(ns: u64) -> usize {
    if ns < SUBS as u64 {
        return ns as usize;
    }
    let octave = 63 - ns.leading_zeros() as usize;
    let shift = octave - SUB_BITS as usize;
    let sub = ((ns >> shift) as usize) & (SUBS - 1);
    (octave << SUB_BITS) | sub
}

#[inline]
fn bucket_lower_bound(bucket: usize) -> u64 {
    if bucket < SUBS {
        return bucket as u64;
    }
    if bucket >= BUCKETS {
        return u64::MAX;
    }
    let octave = bucket >> SUB_BITS;
    // indices between `SUBS` and the first real octave are unreachable from `bucket_of`: pin
    // them to the first reachable value so a walk over indices stays monotonic
    if octave < SUB_BITS as usize {
        return SUBS as u64;
    }
    let sub = (bucket & (SUBS - 1)) as u64;
    let shift = octave - SUB_BITS as usize;
    (SUBS as u64 + sub) << shift
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HistSnapshot {
    pub count: u64,
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
    pub mean_ns: u64,
}

impl HistSnapshot {
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// `p50/p90/p99/max` in milliseconds.
    pub fn brief(&self) -> String {
        format!(
            "{:.2}/{:.2}/{:.2}/{:.2}",
            self.p50_ns as f64 / 1.0e6,
            self.p90_ns as f64 / 1.0e6,
            self.p99_ns as f64 / 1.0e6,
            self.max_ns as f64 / 1.0e6,
        )
    }
}

pub struct Histogram {
    buckets: Box<[AtomicU64]>,
    count: AtomicU64,
    sum_ns: AtomicU64,
    max_ns: AtomicU64,
}

/// The derived form would print 512 buckets.
impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.snapshot();
        f.debug_struct("Histogram")
            .field("count", &s.count)
            .field("p50_ns", &s.p50_ns)
            .field("p99_ns", &s.p99_ns)
            .field("max_ns", &s.max_ns)
            .finish()
    }
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Self {
        let mut buckets = Vec::with_capacity(BUCKETS);
        buckets.resize_with(BUCKETS, AtomicU64::default);
        Self {
            buckets: buckets.into_boxed_slice(),
            count: AtomicU64::new(0),
            sum_ns: AtomicU64::new(0),
            max_ns: AtomicU64::new(0),
        }
    }

    /// Hot path. Relaxed ordering throughout: the counters are statistics, not synchronization,
    /// and a reader that sees a bucket one increment stale reports a percentile one sample stale.
    #[inline]
    pub fn record(&self, ns: u64) {
        self.buckets[bucket_of(ns)].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_ns.fetch_add(ns, Ordering::Relaxed);
        self.max_ns.fetch_max(ns, Ordering::Relaxed);
    }

    /// Record `end - start`, skipping the sample when either stamp is missing (0) or the pair is
    /// out of order. A stage that never fires reports `count == 0`, which is itself the signal.
    #[inline]
    pub fn record_span(&self, start_ns: u64, end_ns: u64) {
        if start_ns == 0 || end_ns == 0 || end_ns < start_ns {
            return;
        }
        self.record(end_ns - start_ns);
    }

    pub fn snapshot(&self) -> HistSnapshot {
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            return HistSnapshot::default();
        }
        let mut targets = [
            (count as f64 * 0.50).ceil() as u64,
            (count as f64 * 0.90).ceil() as u64,
            (count as f64 * 0.99).ceil() as u64,
        ];
        for t in targets.iter_mut() {
            *t = (*t).max(1);
        }
        let mut out = [0u64; 3];
        let mut cumulative = 0u64;
        let mut next = 0usize;
        for (i, b) in self.buckets.iter().enumerate() {
            cumulative += b.load(Ordering::Relaxed);
            while next < 3 && cumulative >= targets[next] {
                out[next] = bucket_lower_bound(i);
                next += 1;
            }
            if next == 3 {
                break;
            }
        }
        HistSnapshot {
            count,
            p50_ns: out[0],
            p90_ns: out[1],
            p99_ns: out[2],
            max_ns: self.max_ns.load(Ordering::Relaxed),
            mean_ns: self.sum_ns.load(Ordering::Relaxed) / count,
        }
    }

    pub fn reset(&self) {
        for b in self.buckets.iter() {
            b.store(0, Ordering::Relaxed);
        }
        self.count.store(0, Ordering::Relaxed);
        self.sum_ns.store(0, Ordering::Relaxed);
        self.max_ns.store(0, Ordering::Relaxed);
    }
}

/// The stamps one frame collects on its way from the compositor to the socket.
///
/// `capture_available_ns` is the OS's own stamp on the frame (WGC `SystemRelativeTime`, DXGI
/// `LastPresentTime`, CGDisplayStream `displayTime`, ScreenCaptureKit PTS): when the pixels
/// existed, not when the callback ran, so `capture_available_ns -> capture_cb_entry_ns` is the
/// host's own delivery latency. Zero means the backend did not supply one.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    /// True for a frame the encoder re-emitted because the picture did not change (the idle
    /// repeater and the ring keepalive). It was never captured, so its capture-relative spans
    /// are not recorded: on a static desktop repeats are most frames (145 of 235 over 30 s) and
    /// would put the p50 on the repeater. Its encode and transport stages are still real.
    pub repeated: bool,
    pub capture_available_ns: u64,
    pub capture_cb_entry_ns: u64,
    pub scale_done_ns: u64,
    pub encode_submit_ns: u64,
    pub encode_done_ns: u64,
    pub packetize_done_ns: u64,
    pub socket_write_done_ns: u64,
    /// Encoded size and IDR flag, filled by the RTP writer only under `SE_FRAME_TRACE=1`.
    pub trace_bytes: u32,
    pub trace_idr: bool,
}

impl FrameTiming {
    /// Stamp callback entry. Everything downstream is relative to this.
    pub fn entering_capture() -> Self {
        Self {
            capture_cb_entry_ns: crate::streamer::audio::host_now_ns(),
            ..Self::default()
        }
    }

    /// Stamps for a frame the encoder is repeating rather than one it captured.
    pub fn repeating() -> Self {
        Self {
            repeated: true,
            ..Self::entering_capture()
        }
    }

    #[inline]
    pub fn mark_scale_done(&mut self) {
        self.scale_done_ns = crate::streamer::audio::host_now_ns();
    }

    #[inline]
    pub fn mark_encode_submit(&mut self) {
        self.encode_submit_ns = crate::streamer::audio::host_now_ns();
    }

    #[inline]
    pub fn mark_encode_done(&mut self) {
        self.encode_done_ns = crate::streamer::audio::host_now_ns();
    }

    #[inline]
    pub fn mark_packetize_done(&mut self) {
        self.packetize_done_ns = crate::streamer::audio::host_now_ns();
    }

    #[inline]
    pub fn mark_socket_write_done(&mut self) {
        self.socket_write_done_ns = crate::streamer::audio::host_now_ns();
    }

    /// The stamp to measure end-to-end host latency from: the OS frame time when the backend
    /// supplied one, else callback entry.
    #[inline]
    pub fn origin_ns(&self) -> u64 {
        if self.capture_available_ns != 0 {
            self.capture_available_ns
        } else {
            self.capture_cb_entry_ns
        }
    }
}

/// `SE_FRAME_TRACE=1`: one log line per frame with every stamp, for slicing a time window out of
/// a live session (the histograms are cumulative from join). Read once. The line is written
/// after the frame's last packet, so formatting cannot delay the frame it describes.
pub fn frame_trace_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::var("SE_FRAME_TRACE").is_ok_and(|v| v == "1"))
}

fn trace_frame(t: &FrameTiming) {
    // spans in microseconds from the previous stamp that was set; 0 = not stamped
    let us = |a: u64, b: u64| {
        if a == 0 || b == 0 || b < a {
            0
        } else {
            (b - a) / 1000
        }
    };
    let pre_submit = if t.scale_done_ns != 0 {
        t.scale_done_ns
    } else {
        t.capture_cb_entry_ns
    };
    tprintln!(
        "[frame-trace] host_ns={} repeated={} origin_os={} delivery_us={} scale_us={} queue_us={} encode_us={} packetize_us={} socket_us={} total_us={} bytes={} idr={}",
        t.socket_write_done_ns,
        t.repeated as u8,
        (t.capture_available_ns != 0) as u8,
        us(t.capture_available_ns, t.capture_cb_entry_ns),
        us(t.capture_cb_entry_ns, t.scale_done_ns),
        us(pre_submit, t.encode_submit_ns),
        us(t.encode_submit_ns, t.encode_done_ns),
        us(t.encode_done_ns, t.packetize_done_ns),
        us(t.packetize_done_ns, t.socket_write_done_ns),
        us(t.origin_ns(), t.socket_write_done_ns),
        t.trace_bytes,
        t.trace_idr as u8,
    );
}

macro_rules! stages {
    ($($field:ident => $label:literal),+ $(,)?) => {
        #[derive(Default)]
        pub struct Stages {
            $(pub $field: Histogram,)+
        }

        impl Stages {
            pub fn each(&self) -> Vec<(&'static str, &Histogram)> {
                vec![$(($label, &self.$field),)+]
            }

            pub fn reset(&self) {
                $(self.$field.reset();)+
            }
        }
    };
}

stages! {
    capture_delivery => "capture_delivery",
    scale => "scale",
    encode_queue => "encode_queue",
    encode => "encode",
    // A re-submitted picture's time inside the encoder (keepalive repeat, forced-keyframe repeat,
    // macOS hold push). Kept out of `encode`: on a one-frame-hold encoder it is waiting for the
    // next submit, not work (folded in, `encode` p50 read 201 ms on a still screen).
    encode_repeat => "encode_repeat",
    packetize => "packetize",
    socket_write => "socket_write",
    pace_wait => "pace_wait",
    capture_to_encode_done => "capture_to_encode",
    capture_to_socket => "capture_to_socket",
    // input: recorded per event by `streamer::input`, not per frame, so these three are the
    // only stages `record_frame` does not feed
    input_recv => "input_recv",
    input_queue => "input_queue",
    input_inject => "input_inject",
}

macro_rules! counters {
    ($($field:ident => $label:literal),+ $(,)?) => {
        /// Every way the pipeline can quietly do less than it claims. A counter that never moves
        /// is as informative as one that does; both are reported.
        #[derive(Default)]
        pub struct Degradation {
            $(pub $field: AtomicU64,)+
        }

        impl Degradation {
            pub fn each(&self) -> Vec<(&'static str, u64)> {
                vec![$(($label, self.$field.load(Ordering::Relaxed)),)+]
            }
        }
    };
}

counters! {
    capture_fallback => "capture_fallback",
    capture_no_first_frame => "capture_no_first_frame",
    frames_repeated => "frames_repeated",
    // `frames_hold_pushed`: of `frames_repeated`, those submitted to push a held frame out of a
    // one-frame-hold encoder (macOS `hold_push_mode`), not for the keepalive; 0 on other encoders
    frames_hold_pushed => "frames_hold_pushed",
    // `hold_push_suspensions`: times the macOS hold push left immediate mode because pushing was
    // making real frames slower (the efficacy guard, or captured frames waiting for the cap).
    hold_push_suspensions => "hold_push_suspensions",
    encoder_output_drops => "encoder_output_drops",
    encoder_rate_drops => "encoder_rate_drops",
    frames_dropped_ring => "frames_dropped_ring",
    frames_dropped_broadcast => "frames_dropped_broadcast",
    frames_dropped_stale => "frames_dropped_stale",
    // `frames_dropped_capture`: presents the OS coalesced away while the previous frame was still
    // in hand (DXGI's `AccumulatedFrames` past the first)
    frames_dropped_capture => "frames_dropped_capture",
    // `frames_skipped_unchanged`: frames the compositor reported no dirty region for and the
    // capture path therefore did not encode. A pointer moving over a still desktop is one such
    // scene: the dirty-region report is empty and the picture has still changed.
    frames_skipped_unchanged => "frames_skipped_unchanged",
    // `frames_skipped_cadence`: frames dropped because the display composes faster than the rate
    // this device asked for. Not a degradation (it is the cap doing its job), but it must be
    // distinguishable from `frames_skipped_unchanged` and from a frame nobody dropped on purpose.
    frames_skipped_cadence => "frames_skipped_cadence",
    // `cursor_frames`: frames emitted because the *pointer* moved and nothing else did. On an
    // indirect display the compositor produces no frame for pointer-only motion.
    // `cursor_composites` / `cursor_composite_us`: sprite draws and the total CPU time spent
    // submitting them. GPU time is not in it; that shows up in `capture_to_socket`.
    // `cursor_shape_errors`: cursors whose bitmap could not be converted. A host with a moving
    // count is drawing no pointer at all for that shape.
    cursor_frames => "cursor_frames",
    cursor_composites => "cursor_composites",
    cursor_composite_us => "cursor_composite_us",
    cursor_shape_errors => "cursor_shape_errors",
    nacks_received => "nacks_received",
    keyframe_requests => "keyframe_requests",
    keyframe_requests_suppressed => "keyframe_requests_suppressed",
    client_keyframe_requests => "client_keyframe_requests",
    delay_overuse_events => "delay_overuse_events",
    pacer_deadline_misses => "pacer_deadline_misses",
    idrs_sent => "idrs_sent",
    encoder_resets => "encoder_resets",
    // `encoder_tier_fallbacks`: the macOS drop guard rebuilt a low-latency constant-quality
    // session on the standard encoder because its rate controller was dropping frames. At most
    // one per session; it never flaps back.
    encoder_tier_fallbacks => "encoder_tier_fallbacks",
    encoder_transient_errors => "encoder_transient_errors",
    bitrate_cuts => "bitrate_cuts",
    audio_broadcast_lag_drops => "audio_broadcast_lag_drops",
    client_jitter_underruns => "client_jitter_underruns",
    client_jitter_overruns => "client_jitter_overruns",
    input_queue_drops => "input_queue_drops",
    input_stale_drops => "input_stale_drops",
    // `input_reorder_drops`: fast-channel frames whose sequence number was behind one already
    // injected (the channel is unordered and zero-retransmit, so this is reorder, not loss).
    // `input_parse_errors`: frames the input parser rejected; a drifted client page shows up
    // here and nowhere else.
    input_reorder_drops => "input_reorder_drops",
    input_parse_errors => "input_parse_errors",
    // `capture_access_denied_waits`: times the capture found the desktop refused to it (the
    // secure desktop, the lock screen, a session switch) and waited for it to come back.
    // `capture_format_skips`: captured surfaces skipped because they could not be converted.
    capture_access_denied_waits => "capture_access_denied_waits",
    capture_format_skips => "capture_format_skips",
    // `capture_lost`: the captured display went away under a running capture.
    // `capture_stopped`: the capture ended for good while its session was still open.
    capture_lost => "capture_lost",
    // `ring_slot_quarantines`: ring slots taken out of service after their keyed mutex could not
    // be acquired or handed on. Kept apart from `encoder_resets`, which counts a ring that gave
    // up.
    ring_slot_quarantines => "ring_slot_quarantines",
    capture_stopped => "capture_stopped",
    // `capture_first_frame_reopens`: captures that opened, delivered no frame within their bound
    // and were opened again before the session was handed over.
    capture_first_frame_reopens => "capture_first_frame_reopens",
    // `input_contact_stale_drops`: pen and touch positions dropped because the stroke had moved
    // on (older than a second when reached), or refused while a stalled injector's queue was
    // full. Downs, ups and cancels are never dropped.
    input_contact_stale_drops => "input_contact_stale_drops",
    // `capture_dark_rebuilds`: renegotiations because a capture answered without a first frame
    // stayed dark (Windows, `capture_late_first_frame`).
    capture_dark_rebuilds => "capture_dark_rebuilds",
    // `capture_never_ready`: sessions answered before their capture opened whose monitor never
    // handed out a capture item within the wait's budget, and were ended (Windows).
    capture_never_ready => "capture_never_ready",
    // `keyframes_for_packet_distance`: IDRs a viewer was sent because a delta was about to get
    // `PACKET_DISTANCE_LIMIT` sequence numbers from its keyframe (`keyframe::PacketDistance`).
    keyframes_for_packet_distance => "keyframes_for_packet_distance",
    // `packet_distance_asks_refused`: those asks the gate's window refused; each is retried.
    packet_distance_asks_refused => "packet_distance_asks_refused",
}

/// What a client last reported about its own half of the path. Written by the `OP::STATS`
/// parser, read by `status --json` and the log summary.
#[derive(Clone, Debug, Default)]
pub struct ClientStats {
    pub received_host_ns: u64,
    pub version: u8,
    pub video_path: u8,
    pub audio_path: u8,
    pub flags: u32,
    pub arrival_to_paint_p50_us: u32,
    pub arrival_to_paint_p90_us: u32,
    pub arrival_to_paint_p99_us: u32,
    pub decode_p50_us: u32,
    pub decode_p90_us: u32,
    pub decode_p99_us: u32,
    pub host_to_paint_p50_us: u32,
    pub host_to_paint_p90_us: u32,
    pub host_to_paint_p99_us: u32,
    pub frames_painted: u32,
    pub frames_dropped: u32,
    pub decode_errors: u32,
    pub keyframe_requests: u32,
    pub audio_underruns: u32,
    pub audio_overruns: u32,
    pub clock_offset_valid: bool,
    pub rtt_us: u32,
    // v2 tail: all zero when the client sent only the 72-byte v1 prefix, which is also the
    // "not measured" value for every one of them
    pub transport_tier: u8,
    pub render_tier: u8,
    pub key_tier: u8,
    pub sink_tier: u8,
    pub suppressed_errors: u32,
    pub degraded_flags: u32,
    pub audio_target_us: u32,
    pub audio_concealed: u32,
    pub audio_output_latency_us: u32,
    /// v3 tail. A redundant Opus copy that filled a real hole, and one that arrived for a
    /// packet the client already had: the benefit and the cost of `FLAG_REDUNDANT`. A
    /// client older than v3 leaves both at 0, which is also what "never sent one" means.
    pub audio_redundant_used: u32,
    pub audio_redundant_discarded: u32,
    /// v4 tail. A client slower than its stream: decoder resets to a keyframe because it fell
    /// behind, decoded frames left unpainted because a newer one was ready, the most time a
    /// frame lost on the client over the last second, and the state (`video_late_state_name`).
    /// Reported only; nothing on the host acts on them yet.
    pub video_catch_ups: u32,
    pub video_frames_stale: u32,
    pub video_late_peak_us: u32,
    pub video_late_state: u8,
}

pub mod client_flags {
    pub const SAB_AVAILABLE: u32 = 1 << 0;
    pub const CROSS_ORIGIN_ISOLATED: u32 = 1 << 1;
    pub const WAKE_LOCK: u32 = 1 << 2;
    pub const POINTER_LOCK: u32 = 1 << 3;
    pub const KEYBOARD_LOCK: u32 = 1 << 4;
    pub const FULLSCREEN: u32 = 1 << 5;
    pub const SET_SINK_ID: u32 = 1 << 6;
    pub const DECODER_HARDWARE: u32 = 1 << 7;
    pub const CLOCK_VALID: u32 = 1 << 8;
}

pub fn transport_tier_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "script-transform",
        2 => "encoded-streams",
        _ => "unknown",
    }
}

pub fn render_tier_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "offscreen-canvas",
        2 => "track-generator",
        3 => "video-element",
        _ => "unknown",
    }
}

/// Tier 0 is the one that matters: the client cannot ask for a keyframe at all.
pub fn key_tier_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "sendKeyFrameRequest",
        2 => "out-of-band",
        _ => "unknown",
    }
}

pub fn sink_tier_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "audiocontext-setsinkid",
        2 => "media-element-setsinkid",
        3 => "audio-session",
        4 => "select-audio-output",
        _ => "unknown",
    }
}

/// The v4 `video_late_state` byte: where a client that may be slower than its stream stands.
pub fn video_late_state_name(code: u8) -> &'static str {
    match code {
        0 => "in-step",
        1 => "skipping-stale-paints",
        2 => "behind",
        3 => "catching-up",
        _ => "unknown",
    }
}

/// Names for the v2 degradation bits, in bit order. Each is a silent fallback on the client: a
/// swallowed `catch` or an untaken branch.
pub const CLIENT_DEGRADED_NAMES: [&str; 25] = [
    "fullscreen_refused",
    "pointer_lock_refused",
    "no_unadjusted_movement",
    "no_wake_lock_api",
    "wake_lock_refused",
    "audio_worklet_failed",
    "no_opus_decoder",
    "audio_context_suspended",
    "output_latency_estimated",
    "no_audio_output_labels",
    "no_script_transform",
    "no_keyframe_request",
    "no_offscreen_canvas",
    "demoted_to_video_element",
    "no_coalesced_events",
    "ice_gathering_timed_out",
    "no_h264_fmtp",
    "no_hardware_decoder",
    "refresh_rate_estimated",
    "stepped_down_to_encoded_streams",
    "no_request_video_frame_callback",
    "no_keyboard_lock",
    "no_trickle_ice",
    "input_suspended_for_modal",
    "join_id_missing",
];

/// The degradation bits that are set, by name.
pub fn client_degraded_list(flags: u32) -> Vec<&'static str> {
    CLIENT_DEGRADED_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| flags & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect()
}

pub fn video_path_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "webcodecs+canvas",
        2 => "webcodecs+video",
        3 => "video-element",
        _ => "unknown",
    }
}

pub fn audio_path_name(code: u8) -> &'static str {
    match code {
        0 => "none",
        1 => "worklet+sab",
        2 => "worklet+postmessage",
        3 => "opus-track",
        // the private ring fed over a `MessagePort` (`AUDIO_PATH.workletPort` in index.html)
        4 => "worklet+port",
        _ => "unknown",
    }
}

/// One streaming session's telemetry. Cloned as an `Arc` into the capture thread, the encode
/// thread and the packetizer; every one of them only ever calls `record`/`fetch_add`.
#[derive(Default)]
pub struct SessionTelemetry {
    pub stages: Stages,
    pub degradation: Degradation,
    pub frames_encoded: AtomicU64,
    /// Frames the encode loop handed the encoder successfully, repeats included. Counted by the
    /// macOS encode loop only (absent from the JSON while zero), so `frames_submitted -
    /// frames_encoded` is what the encoder took and never returned with a picture.
    pub frames_submitted: AtomicU64,
    /// Host power state at session start (`low_power_mode=... thermal=...`), when the macOS
    /// `power_report` tuning candidate is on. Empty otherwise.
    pub host_power: Slot<String>,
    /// `(frames_encoded, host_ns)` at the last summary line, so the next one can report a
    /// delivered rate.
    last_rate_sample: std::sync::Mutex<(u64, u64)>,
    pub bytes_sent: AtomicU64,
    pub encoder: Slot<String>,
    /// The encoder implementation VideoToolbox picked (`h264.rtvc`, `ave.avc`, `h264.gva`, ...).
    /// macOS only; absent from the JSON while empty.
    pub encoder_id: Slot<String>,
    /// The macOS rate-control tier (`BaseFrameQp`, `ClampCeiling`, `LowLatencyBitrate`,
    /// `StandardBitrate`, `AdaptiveLowLatency`, `AdaptiveStandard`, `recipe`). Absent while empty.
    pub rate_tier: Slot<String>,
    /// The macOS encode loop's hold-push mode (`probing` / `immediate` / `suspended`). Absent
    /// while empty.
    pub hold_push_mode: Slot<String>,
    /// Why the hold push was last suspended, with the statistic it decided on. Absent while empty.
    pub hold_push_guard: Slot<String>,
    /// Encoded bytes of the hold pushes (macOS). Absent from the JSON while zero.
    pub hold_push_bytes: AtomicU64,
    /// Every encoded byte the macOS drain thread handed on (the QP governor's rate input).
    pub bytes_encoded: AtomicU64,
    /// The newest encoded frame sizes (bytes), a ring indexed by `frame_size_seq % 16`, written by
    /// the macOS drain thread and read by the QP governor's per-frame fast path.
    pub frame_sizes: [AtomicU64; 16],
    pub frame_size_seq: AtomicU64,
    /// The macOS QP governor (`BaseFrameQp` only): the QP in force, the user's QP, the ceiling,
    /// time spent above the user's QP and the number of changes. The block is absent while the
    /// ceiling is 0 (governor off, or another tier).
    pub qp_effective: AtomicU64,
    pub qp_user: AtomicU64,
    pub qp_ceiling_bps: AtomicU64,
    pub qp_above_user_ms: AtomicU64,
    pub qp_ceiling_steps: AtomicU64,
    /// Video frames whose RTP timestamp was not ahead of the previous one and was written one
    /// tick after it instead (the RTP writer keeps it strictly increasing). Absent while zero.
    pub rtp_ts_clamped: AtomicU64,
    pub capture_backend: Slot<String>,
    /// Frames the capture API handed over, before any gate or encode: WGC callbacks, DXGI
    /// acquires that returned a desktop surface. Moves on the first delivery, which is what tells
    /// an open capture that delivers nothing from a slow one.
    pub capture_frames_arrived: AtomicU64,
    /// The peer connection reached `Connected`: a client is there, picture or not.
    pub transport_connected: std::sync::atomic::AtomicBool,
    /// The session was answered before its capture opened (Windows, `answer_before_capture`):
    /// the client is told the picture is pending until the first frame leaves the host.
    pub picture_pending: std::sync::atomic::AtomicBool,
    /// That capture is still waiting for its monitor to hand out a capture item.
    pub capture_waiting: std::sync::atomic::AtomicBool,
    /// The capture backend that delivered the session's first frame. Empty until one has, and on
    /// backends that do not report it.
    pub first_frame_backend: Slot<String>,
    /// Which optional knobs of the chosen capture backend this OS build exposed. Empty where the
    /// backend has no such tiering.
    pub capture_features: Slot<String>,
    pub client: Slot<Option<ClientStats>>,
    /// The Windows QP and frame-rate governor: feedback in from the transport, the frame-rate
    /// divisor out to the capture gate, and its state for `status --json` (`link_governor`).
    #[cfg(windows)]
    pub link: crate::windows_utils::streamer::governor::LinkShared,
    /// Video retransmission under its byte budget (`rtx_budget`), for `status --json`.
    #[cfg(windows)]
    pub rtx: crate::windows_utils::streamer::rtx::RtxCounters,
}

/// One value behind a lock.
///
/// Slow paths only (a backend naming itself at startup, a client stats frame once a second, a
/// reader building a report), never the frame path, so a plain `Mutex` is enough.
#[derive(Default)]
pub struct Slot<T>(std::sync::Mutex<T>);

impl<T: Clone + Default> Slot<T> {
    pub fn set(&self, v: T) {
        if let Ok(mut g) = self.0.lock() {
            *g = v;
        }
    }

    pub fn get(&self) -> T {
        self.0.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

pub type SharedTelemetry = Arc<SessionTelemetry>;

/// `(host_now_ns, unix_ns)` read back to back: the host clock every stamp here uses, paired with
/// wall-clock time since the Unix epoch, so a harness on the same machine can check a client's
/// clock-offset estimate against the truth. The two reads are adjacent (sub-microsecond apart).
pub fn clock_pair() -> (u64, u64) {
    let host = crate::streamer::audio::host_now_ns();
    let unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    (host, unix)
}

impl SessionTelemetry {
    /// Answered before its capture opened, and no frame has left the host yet.
    pub fn picture_pending_now(&self) -> bool {
        self.picture_pending.load(Ordering::Relaxed) && self.bytes_sent.load(Ordering::Relaxed) == 0
    }

    /// Record a client stats frame, mirroring the two jitter counters into `degradation` so
    /// they show beside the host-side ones. The client reports cumulative totals, so these are
    /// stored, not added.
    pub fn set_client_stats(&self, stats: ClientStats) {
        self.degradation
            .client_jitter_underruns
            .store(stats.audio_underruns as u64, Ordering::Relaxed);
        self.degradation
            .client_jitter_overruns
            .store(stats.audio_overruns as u64, Ordering::Relaxed);
        self.client.set(Some(stats));
    }
}

impl SessionTelemetry {
    pub fn new_shared() -> SharedTelemetry {
        Arc::new(Self::default())
    }

    /// Fold one finished frame's stamps into every stage histogram it can supply.
    pub fn record_frame(&self, t: &FrameTiming) {
        if !t.repeated {
            self.stages
                .capture_delivery
                .record_span(t.capture_available_ns, t.capture_cb_entry_ns);
            self.stages
                .capture_to_encode_done
                .record_span(t.origin_ns(), t.encode_done_ns);
            self.stages
                .capture_to_socket
                .record_span(t.origin_ns(), t.socket_write_done_ns);
        }
        self.stages
            .scale
            .record_span(t.capture_cb_entry_ns, t.scale_done_ns);
        if t.repeated {
            // a repeat was never queued, and on a holding encoder what follows the submit is
            // not encode work: its own stage, out of `encode` and `encode_queue`
            self.stages
                .encode_repeat
                .record_span(t.encode_submit_ns, t.encode_done_ns);
        } else {
            self.stages.encode_queue.record_span(
                if t.scale_done_ns != 0 {
                    t.scale_done_ns
                } else {
                    t.capture_cb_entry_ns
                },
                t.encode_submit_ns,
            );
            self.stages
                .encode
                .record_span(t.encode_submit_ns, t.encode_done_ns);
        }
        self.stages
            .packetize
            .record_span(t.encode_done_ns, t.packetize_done_ns);
        self.stages
            .socket_write
            .record_span(t.packetize_done_ns, t.socket_write_done_ns);
        if frame_trace_enabled() {
            trace_frame(t);
        }
    }

    /// Delivered frames per second since the previous call, and re-arm for the next one.
    ///
    /// `None` on the first call of a session, and whenever no measurable time has passed.
    fn delivered_fps(&self) -> Option<f64> {
        let frames = self.frames_encoded.load(Ordering::Relaxed);
        let now_ns = crate::streamer::audio::host_now_ns();
        let mut last = self.last_rate_sample.lock().ok()?;
        let (prev_frames, prev_ns) = *last;
        *last = (frames, now_ns);
        if prev_ns == 0 || now_ns <= prev_ns {
            return None;
        }
        let secs = (now_ns - prev_ns) as f64 / 1.0e9;
        Some(frames.saturating_sub(prev_frames) as f64 / secs)
    }

    /// One line, all stages, `p50/p90/p99/max` in ms. Printed on a timer, never per frame.
    pub fn summary_line(&self) -> String {
        let features = self.capture_features.get();
        let fps = match self.delivered_fps() {
            Some(f) => format!(" fps={f:.1}"),
            None => String::new(),
        };
        let mut s = format!(
            "telemetry: capture={}{} encoder={} frames={}{fps}",
            self.capture_backend.get(),
            if features.is_empty() {
                String::new()
            } else {
                format!("[{features}]")
            },
            self.encoder.get(),
            self.frames_encoded.load(Ordering::Relaxed),
        );
        let (encoder_id, rate_tier) = (self.encoder_id.get(), self.rate_tier.get());
        if !encoder_id.is_empty() || !rate_tier.is_empty() {
            s.push_str(&format!(" vt=[{encoder_id} {rate_tier}]"));
        }
        for (name, hist) in self.stages.each() {
            let snap = hist.snapshot();
            if snap.is_empty() {
                continue;
            }
            s.push_str(&format!(" {name}={}", snap.brief()));
        }
        let degraded: Vec<String> = self
            .degradation
            .each()
            .into_iter()
            .filter(|(_, v)| *v != 0)
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        if !degraded.is_empty() {
            s.push_str(" | ");
            s.push_str(&degraded.join(" "));
        }
        if let Some(c) = self.client.get() {
            s.push_str(&format!(
                " | client video={} audio={} paint={:.2}ms clock={}",
                video_path_name(c.video_path),
                audio_path_name(c.audio_path),
                c.arrival_to_paint_p50_us as f64 / 1000.0,
                if c.clock_offset_valid {
                    "ok"
                } else {
                    "unsynced"
                },
            ));
            if c.version >= 2 {
                s.push_str(&format!(
                    " transport={} render={} key={} sink={}",
                    transport_tier_name(c.transport_tier),
                    render_tier_name(c.render_tier),
                    key_tier_name(c.key_tier),
                    sink_tier_name(c.sink_tier),
                ));
                // named, not a hex mask: this is where a support conversation starts
                let degraded = client_degraded_list(c.degraded_flags);
                if !degraded.is_empty() {
                    s.push_str(&format!(" degraded={}", degraded.join(",")));
                }
            }
            // read as a pair: `used` climbing while `concealed` stays flat is redundancy
            // working; `discarded` climbing with `used` at zero is payload spent on a link
            // that did not need it (the send-side threshold, not the client)
            if c.version >= 3 && (c.audio_redundant_used > 0 || c.audio_redundant_discarded > 0) {
                s.push_str(&format!(
                    " redundant={}used/{}discarded",
                    c.audio_redundant_used, c.audio_redundant_discarded
                ));
            }
            // a client that cannot keep up: printed only once it has had to act
            if c.version >= 4
                && (c.video_catch_ups > 0 || c.video_frames_stale > 0 || c.video_late_state != 0)
            {
                s.push_str(&format!(
                    " late={}/{}catchups/{}stale",
                    video_late_state_name(c.video_late_state),
                    c.video_catch_ups,
                    c.video_frames_stale
                ));
            }
        }
        s
    }

    pub fn to_json(&self) -> serde_json::Value {
        let stages: serde_json::Map<String, serde_json::Value> = self
            .stages
            .each()
            .into_iter()
            .filter_map(|(name, hist)| {
                let s = hist.snapshot();
                if s.is_empty() {
                    return None;
                }
                Some((
                    name.to_string(),
                    serde_json::json!({
                        "count": s.count,
                        "p50_ms": s.p50_ns as f64 / 1.0e6,
                        "p90_ms": s.p90_ns as f64 / 1.0e6,
                        "p99_ms": s.p99_ns as f64 / 1.0e6,
                        "max_ms": s.max_ns as f64 / 1.0e6,
                        "mean_ms": s.mean_ns as f64 / 1.0e6,
                    }),
                ))
            })
            .collect();
        let degradation: serde_json::Map<String, serde_json::Value> = self
            .degradation
            .each()
            .into_iter()
            .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
            .collect();
        let client = self.client.get().map(|c| {
            serde_json::json!({
                "version": c.version,
                "video_path": video_path_name(c.video_path),
                "audio_path": audio_path_name(c.audio_path),
                "sab_available": c.flags & client_flags::SAB_AVAILABLE != 0,
                "cross_origin_isolated": c.flags & client_flags::CROSS_ORIGIN_ISOLATED != 0,
                "wake_lock": c.flags & client_flags::WAKE_LOCK != 0,
                "pointer_lock": c.flags & client_flags::POINTER_LOCK != 0,
                "keyboard_lock": c.flags & client_flags::KEYBOARD_LOCK != 0,
                "fullscreen": c.flags & client_flags::FULLSCREEN != 0,
                "set_sink_id": c.flags & client_flags::SET_SINK_ID != 0,
                "decoder_hardware": c.flags & client_flags::DECODER_HARDWARE != 0,
                "clock_offset_valid": c.clock_offset_valid,
                "rtt_ms": c.rtt_us as f64 / 1000.0,
                "arrival_to_paint_ms": {
                    "p50": c.arrival_to_paint_p50_us as f64 / 1000.0,
                    "p90": c.arrival_to_paint_p90_us as f64 / 1000.0,
                    "p99": c.arrival_to_paint_p99_us as f64 / 1000.0,
                },
                "decode_ms": {
                    "p50": c.decode_p50_us as f64 / 1000.0,
                    "p90": c.decode_p90_us as f64 / 1000.0,
                    "p99": c.decode_p99_us as f64 / 1000.0,
                },
                "host_capture_to_paint_ms": {
                    "p50": c.host_to_paint_p50_us as f64 / 1000.0,
                    "p90": c.host_to_paint_p90_us as f64 / 1000.0,
                    "p99": c.host_to_paint_p99_us as f64 / 1000.0,
                },
                "frames_painted": c.frames_painted,
                "frames_dropped": c.frames_dropped,
                "decode_errors": c.decode_errors,
                "keyframe_requests": c.keyframe_requests,
                "audio_underruns": c.audio_underruns,
                "audio_overruns": c.audio_overruns,
                "transport_tier": transport_tier_name(c.transport_tier),
                "render_tier": render_tier_name(c.render_tier),
                "key_tier": key_tier_name(c.key_tier),
                "sink_tier": sink_tier_name(c.sink_tier),
                "tier_codes": [c.transport_tier, c.render_tier, c.key_tier, c.sink_tier],
                "suppressed_errors": c.suppressed_errors,
                "degraded_flags": c.degraded_flags,
                "degraded": client_degraded_list(c.degraded_flags),
                "audio_target_ms": c.audio_target_us as f64 / 1000.0,
                "audio_concealed": c.audio_concealed,
                "audio_output_latency_ms": c.audio_output_latency_us as f64 / 1000.0,
                "audio_redundant_used": c.audio_redundant_used,
                "audio_redundant_discarded": c.audio_redundant_discarded,
                "video_catch_ups": c.video_catch_ups,
                "video_frames_stale": c.video_frames_stale,
                "video_late_peak_ms": c.video_late_peak_us as f64 / 1000.0,
                "video_late_state": video_late_state_name(c.video_late_state),
                "video_late_state_code": c.video_late_state,
            })
        });
        let mut out = serde_json::json!({
            "capture_backend": self.capture_backend.get(),
            "capture_features": self.capture_features.get(),
            "encoder": self.encoder.get(),
            "frames_encoded": self.frames_encoded.load(Ordering::Relaxed),
            "bytes_sent": self.bytes_sent.load(Ordering::Relaxed),
            "stages": stages,
            "degradation": degradation,
            "client": client,
        });
        let submitted = self.frames_submitted.load(Ordering::Relaxed);
        if submitted > 0 {
            out["frames_submitted"] = serde_json::json!(submitted);
        }
        let power = self.host_power.get();
        if !power.is_empty() {
            out["host_power"] = serde_json::json!(power);
        }
        let encoder_id = self.encoder_id.get();
        if !encoder_id.is_empty() {
            out["encoder_id"] = serde_json::json!(encoder_id);
        }
        let first_frame = self.first_frame_backend.get();
        if !first_frame.is_empty() {
            out["first_frame_backend"] = serde_json::json!(first_frame);
        }
        let rate_tier = self.rate_tier.get();
        if !rate_tier.is_empty() {
            out["rate_tier"] = serde_json::json!(rate_tier);
        }
        let rtp_ts_clamped = self.rtp_ts_clamped.load(Ordering::Relaxed);
        if rtp_ts_clamped > 0 {
            out["rtp_ts_clamped"] = serde_json::json!(rtp_ts_clamped);
        }
        let hold_push_bytes = self.hold_push_bytes.load(Ordering::Relaxed);
        if hold_push_bytes > 0 {
            out["hold_push_bytes"] = serde_json::json!(hold_push_bytes);
        }
        let ceiling = self.qp_ceiling_bps.load(Ordering::Relaxed);
        if ceiling > 0 {
            out["qp_governor"] = serde_json::json!({
                "qp_effective": self.qp_effective.load(Ordering::Relaxed),
                "qp_user": self.qp_user.load(Ordering::Relaxed),
                "ceiling_mbps": ceiling as f64 / 1e6,
                "ms_above_user_qp": self.qp_above_user_ms.load(Ordering::Relaxed),
                "steps": self.qp_ceiling_steps.load(Ordering::Relaxed),
            });
        }
        #[cfg(windows)]
        if let Some(v) = self.link.to_json() {
            out["link_governor"] = v;
        }
        #[cfg(windows)]
        if let Some(v) = self.rtx.to_json() {
            out["retransmission"] = v;
        }
        let (host_now_ns, unix_ns) = clock_pair();
        out["clock"] = serde_json::json!({ "host_now_ns": host_now_ns, "unix_ns": unix_ns });
        let hold_push_guard = self.hold_push_guard.get();
        if !hold_push_guard.is_empty() {
            out["hold_push_guard"] = serde_json::json!(hold_push_guard);
        }
        let hold_push_mode = self.hold_push_mode.get();
        if !hold_push_mode.is_empty() {
            out["hold_push_mode"] = serde_json::json!(hold_push_mode);
        }
        out
    }
}

/// Process-wide registry, keyed the way sessions are: by join id.
///
/// The reader is usually another thread and sometimes another process: `ScreenExtend status
/// --json` asks the running host over the single-instance control socket, which reads this.
pub type Registry = std::sync::Mutex<std::collections::HashMap<String, SharedTelemetry>>;

static REGISTRY: std::sync::OnceLock<Registry> = std::sync::OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Default::default)
}

pub fn register(key: &str) -> SharedTelemetry {
    let t = SessionTelemetry::new_shared();
    publish(key, Arc::clone(&t));
    t
}

/// Put an existing handle in the registry. The pipeline makes its own at start, before it knows
/// the session's key, so the server publishes it once the session has one.
pub fn publish(key: &str, telemetry: SharedTelemetry) {
    if let Ok(mut g) = registry().lock() {
        g.insert(key.to_string(), telemetry);
    }
}

pub fn unregister(key: &str) {
    if let Ok(mut g) = registry().lock() {
        g.remove(key);
    }
}

/// Remove `key` only while it still names `mine`.
///
/// A renegotiated join publishes a second session's handle under the same join id, and the
/// superseded session's teardown runs after that (as soon as the new transport connects). A plain
/// `unregister` there would remove the live session's telemetry.
pub fn unregister_if(key: &str, mine: &SharedTelemetry) {
    if let Ok(mut g) = registry().lock() {
        if g.get(key).is_some_and(|t| Arc::ptr_eq(t, mine)) {
            g.remove(key);
        }
    }
}

pub fn get(key: &str) -> Option<SharedTelemetry> {
    registry().lock().ok()?.get(key).cloned()
}

pub fn all() -> Vec<(String, SharedTelemetry)> {
    registry()
        .lock()
        .map(|g| g.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect())
        .unwrap_or_default()
}

pub fn snapshot_json() -> serde_json::Value {
    let sessions: serde_json::Map<String, serde_json::Value> =
        all().into_iter().map(|(k, v)| (k, v.to_json())).collect();
    serde_json::json!({
        "sessions": sessions,
        // peer connections, ICE agents and injector threads not yet given back: equals the
        // number of live sessions unless something leaks
        "live_transport_sessions": crate::streamer::webrtc_session::live_sessions(),
    })
}

/// Interval for the verbose one-line summary. Chosen so a 30 s baseline run yields six lines.
pub const SUMMARY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Start the summary printer: a plain OS thread on a timer, deliberately not a task on the
/// streaming runtime.
pub fn spawn_summary_printer(stop: Arc<std::sync::atomic::AtomicBool>) {
    std::thread::Builder::new()
        .name("telemetry-summary".into())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(SUMMARY_INTERVAL);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                for (key, t) in all() {
                    if t.frames_encoded.load(Ordering::Relaxed) == 0 {
                        continue;
                    }
                    tprintln!("[{key}] {}", t.summary_line());
                }
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_and_ring_counters_extend_the_list_without_moving_it() {
        let t = SessionTelemetry::new_shared();
        t.degradation
            .ring_slot_quarantines
            .fetch_add(2, Ordering::Relaxed);
        let labels: Vec<&str> = t.degradation.each().iter().map(|(k, _)| *k).collect();
        let tail = &labels[labels.len() - 12..];
        assert_eq!(
            tail,
            [
                "input_parse_errors",
                "capture_access_denied_waits",
                "capture_format_skips",
                "capture_lost",
                "ring_slot_quarantines",
                "capture_stopped",
                "capture_first_frame_reopens",
                "input_contact_stale_drops",
                "capture_dark_rebuilds",
                "capture_never_ready",
                "keyframes_for_packet_distance",
                "packet_distance_asks_refused",
            ]
        );
        assert!(t.to_json().get("first_frame_backend").is_none());
        t.first_frame_backend.set("wgc".into());
        assert_eq!(t.to_json()["first_frame_backend"], "wgc");
        assert_eq!(t.to_json()["degradation"]["ring_slot_quarantines"], 2);
        assert_eq!(t.to_json()["degradation"]["encoder_resets"], 0);
    }

    #[test]
    fn buckets_are_monotonic_and_contain_their_values() {
        for ns in [0u64, 1, 7, 8, 9, 100, 999, 1_000, 16_667_000, 1_000_000_000] {
            let b = bucket_of(ns);
            let lo = bucket_lower_bound(b);
            let hi = bucket_lower_bound(b + 1);
            assert!(
                lo <= ns && ns < hi,
                "sample {ns} outside bucket {b} = {lo}..{hi}"
            );
        }
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
        let mut prev = bucket_of(0);
        for ns in 1..100_000u64 {
            let b = bucket_of(ns);
            assert!(b >= prev);
            prev = b;
        }
    }

    #[test]
    fn bucket_width_stays_under_ten_percent() {
        for ns in [1_000u64, 16_667_000, 500_000_000] {
            let b = bucket_of(ns);
            let lo = bucket_lower_bound(b);
            let hi = bucket_lower_bound(b + 1);
            assert!(
                (hi - lo) as f64 / lo.max(1) as f64 <= 0.13,
                "bucket {b} spans {lo}..{hi}, too wide to trust a p99"
            );
        }
    }

    #[test]
    fn percentiles_track_a_known_distribution() {
        let h = Histogram::new();
        // 99 samples at 1 ms, one at 100 ms: a mean would read 2 ms and describe neither
        for _ in 0..99 {
            h.record(1_000_000);
        }
        h.record(100_000_000);
        let s = h.snapshot();
        assert_eq!(s.count, 100);
        assert!((s.p50_ns as f64 - 1.0e6).abs() / 1.0e6 < 0.13);
        assert!((s.p90_ns as f64 - 1.0e6).abs() / 1.0e6 < 0.13);
        assert_eq!(s.max_ns, 100_000_000);
        assert!(s.mean_ns > 1_500_000);
    }

    #[test]
    fn empty_histogram_reports_nothing_rather_than_zero() {
        assert!(Histogram::new().snapshot().is_empty());
    }

    #[test]
    fn out_of_order_or_missing_stamps_are_dropped_not_wrapped() {
        let h = Histogram::new();
        h.record_span(0, 500);
        h.record_span(500, 0);
        h.record_span(900, 400);
        assert!(h.snapshot().is_empty());
        h.record_span(400, 900);
        assert_eq!(h.snapshot().count, 1);
    }

    #[test]
    fn a_repeated_frame_is_kept_out_of_the_capture_relative_stages() {
        let tel = SessionTelemetry::default();
        let mut t = FrameTiming::repeating();
        t.encode_submit_ns = t.capture_cb_entry_ns + 1_000_000;
        t.encode_done_ns = t.encode_submit_ns + 2_000_000;
        t.packetize_done_ns = t.encode_done_ns + 100_000;
        t.socket_write_done_ns = t.packetize_done_ns + 200_000;
        tel.record_frame(&t);
        // its encoder time is measured apart from real encodes; its transport as usual
        assert!(tel.stages.encode.snapshot().is_empty());
        assert!(tel.stages.encode_queue.snapshot().is_empty());
        assert_eq!(tel.stages.encode_repeat.snapshot().count, 1);
        assert_eq!(tel.stages.socket_write.snapshot().count, 1);
        // nothing was captured, so nothing capture-relative is claimed
        assert!(tel.stages.capture_delivery.snapshot().is_empty());
        assert!(tel.stages.capture_to_encode_done.snapshot().is_empty());
        assert!(tel.stages.capture_to_socket.snapshot().is_empty());
    }

    #[test]
    fn frame_timing_folds_into_every_stage_it_can() {
        let t = FrameTiming {
            repeated: false,
            capture_available_ns: 1_000_000,
            capture_cb_entry_ns: 2_000_000,
            scale_done_ns: 3_000_000,
            encode_submit_ns: 4_000_000,
            encode_done_ns: 9_000_000,
            packetize_done_ns: 9_500_000,
            socket_write_done_ns: 10_000_000,
            ..Default::default()
        };
        let tel = SessionTelemetry::default();
        tel.record_frame(&t);
        assert_eq!(tel.stages.capture_delivery.snapshot().count, 1);
        assert_eq!(tel.stages.encode.snapshot().count, 1);
        assert!(tel.stages.encode_repeat.snapshot().is_empty());
        let total = tel.stages.capture_to_socket.snapshot();
        assert_eq!(total.count, 1);
        // 9 ms from the OS frame time to the socket, not 8 ms from the callback
        assert!((total.max_ns as f64 - 9.0e6).abs() < 1.0e6);
    }

    #[test]
    fn a_backend_that_supplies_no_os_timestamp_still_measures_the_rest() {
        let t = FrameTiming {
            capture_cb_entry_ns: 2_000_000,
            encode_submit_ns: 2_500_000,
            encode_done_ns: 5_000_000,
            ..Default::default()
        };
        let tel = SessionTelemetry::default();
        tel.record_frame(&t);
        assert!(tel.stages.capture_delivery.snapshot().is_empty());
        assert_eq!(tel.stages.capture_to_encode_done.snapshot().count, 1);
        assert_eq!(t.origin_ns(), 2_000_000);
    }

    #[test]
    fn registry_round_trips_and_forgets() {
        let t = register("10.0.0.7");
        t.frames_encoded.store(3, Ordering::Relaxed);
        assert_eq!(
            get("10.0.0.7")
                .unwrap()
                .frames_encoded
                .load(Ordering::Relaxed),
            3
        );
        unregister("10.0.0.7");
        assert!(get("10.0.0.7").is_none());
    }

    /// Every value the page can report in the STATS frame has a name here, so `status --json`
    /// never says "unknown" for a tier the page really took.
    #[test]
    fn every_client_audio_path_has_a_name() {
        let page = include_str!("static/index.html");
        let start = page
            .find("const AUDIO_PATH = {")
            .expect("AUDIO_PATH in index.html");
        let body = &page[start..];
        let body = &body[body.find('{').unwrap() + 1..body.find('}').unwrap()];
        let codes: Vec<u8> = body
            .split(',')
            .filter_map(|kv| kv.split(':').nth(1)?.trim().parse().ok())
            .collect();
        assert!(codes.len() >= 5, "parsed {codes:?}");
        for code in codes {
            assert_ne!(audio_path_name(code), "unknown", "audio path {code}");
        }
    }

    #[test]
    fn superseded_session_does_not_unregister_its_successor() {
        let key = "join-renegotiated-test";
        let old = register(key);
        let new = SessionTelemetry::new_shared();
        publish(key, Arc::clone(&new));
        // the superseded session's teardown leaves the live one's handle alone ...
        unregister_if(key, &old);
        assert!(get(key).is_some_and(|t| Arc::ptr_eq(&t, &new)));
        // ... and the live one's own teardown removes it
        unregister_if(key, &new);
        assert!(get(key).is_none());
    }
}
