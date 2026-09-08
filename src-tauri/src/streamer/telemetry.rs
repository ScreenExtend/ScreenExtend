//! Per-stage frame timing and degradation counters.
//!
//! Every timestamp here is nanoseconds off the one host clock the rest of the streamer already
//! shares, [`crate::streamer::audio::host_now_ns`] — the same epoch the video RTP timestamps and
//! the audio `capture_ns` ride, so a number from this module is directly comparable with one the
//! client reports back over `OP::STATS`.
//!
//! Two constraints shape the implementation:
//!
//! - **Nothing here allocates, locks, or logs.** `record` is three relaxed `fetch_add`s and a
//!   `fetch_max` into a fixed array. The video capture callbacks and the audio capture thread are
//!   real-time contexts; a mutex or a `tprintln!` on that path would cost more than the stage it
//!   is measuring (every logbus line is a JSON IPC event to the UI).
//! - **Distributions, not means.** An average hides exactly the tail that is felt. Histograms are
//!   fixed log-scale buckets, so p50/p90/p99 come out of a bounded array with no sample storage.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Sub-buckets per power of two. 8 gives at most ~9% width at any magnitude, which is finer than
/// the measurement noise on a millisecond-scale stage and still only 512 counters.
const SUB_BITS: u32 = 3;
const SUBS: usize = 1 << SUB_BITS;
const OCTAVES: usize = 64;
const BUCKETS: usize = OCTAVES * SUBS;

/// Lower bound of the bucket a value falls in, and back again.
///
/// Values below `SUBS` get their own bucket each, which keeps sub-microsecond stages honest.
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
    // `bucket_of` gives every value below `SUBS` its own index and maps everything else to an
    // octave of at least `SUB_BITS`, so indices in between are unreachable. Pin them to the
    // first reachable value; a caller walking indices then still sees a monotonic sequence.
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

    /// `p50/p90/p99/max` in milliseconds — never a bare mean, which is what hides the tail.
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

/// The derived form would print 512 buckets. Print what a reader wants instead.
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
    /// out of order. A stage that never fires reports `count == 0`, which is itself the finding.
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
/// `capture_available_ns` is the timestamp the OS attached to the frame itself — WGC's
/// `SystemRelativeTime`, DXGI's `LastPresentTime`, CGDisplayStream's `displayTime`, ScreenCaptureKit's
/// PTS. It is the only stamp that says when the pixels existed rather than when we noticed them,
/// so `capture_available_ns -> capture_cb_entry_ns` is the delivery latency the host itself adds
/// before doing any work. Zero means the backend did not supply one.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameTiming {
    /// True for a frame the encoder re-emitted because the picture did not change — the idle
    /// repeater and the ring keepalive. Such a frame was never captured, so its "capture"
    /// stamps are the moment the repeater woke up and the end-to-end spans would read as
    /// near-zero. On a static desktop repeats are the *majority* of frames (measured: 145 of
    /// 235 over 30 s), so folding them into the end-to-end histograms would put the p50 on the
    /// repeater and leave only the tail describing real capture. They are counted, and their
    /// encode and transport stages are still real, but the capture-relative spans are not.
    pub repeated: bool,
    pub capture_available_ns: u64,
    pub capture_cb_entry_ns: u64,
    pub scale_done_ns: u64,
    pub encode_submit_ns: u64,
    pub encode_done_ns: u64,
    pub packetize_done_ns: u64,
    pub socket_write_done_ns: u64,
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
    /// gave us one, else our own callback entry.
    #[inline]
    pub fn origin_ns(&self) -> u64 {
        if self.capture_available_ns != 0 {
            self.capture_available_ns
        } else {
            self.capture_cb_entry_ns
        }
    }
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
    packetize => "packetize",
    socket_write => "socket_write",
    pace_wait => "pace_wait",
    capture_to_encode_done => "capture_to_encode",
    capture_to_socket => "capture_to_socket",
    // Input. Recorded per event by `streamer::input`, not per frame, so these three are the
    // only stages `record_frame` does not feed. See `docs/INPUT-PROTOCOL.md`.
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
    frames_repeated => "frames_repeated",
    frames_dropped_ring => "frames_dropped_ring",
    frames_dropped_broadcast => "frames_dropped_broadcast",
    nacks_received => "nacks_received",
    keyframe_requests => "keyframe_requests",
    keyframe_requests_suppressed => "keyframe_requests_suppressed",
    client_keyframe_requests => "client_keyframe_requests",
    delay_overuse_events => "delay_overuse_events",
    pacer_deadline_misses => "pacer_deadline_misses",
    idrs_sent => "idrs_sent",
    encoder_resets => "encoder_resets",
    encoder_transient_errors => "encoder_transient_errors",
    bitrate_cuts => "bitrate_cuts",
    audio_broadcast_lag_drops => "audio_broadcast_lag_drops",
    client_jitter_underruns => "client_jitter_underruns",
    client_jitter_overruns => "client_jitter_overruns",
    input_queue_drops => "input_queue_drops",
    input_stale_drops => "input_stale_drops",
}

/// What a client last told us about its own half of the path. Written by the `OP::STATS` parser,
/// read by `status --json` and the log summary; see `docs/TELEMETRY.md` for the wire layout.
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
    // v2 tail. All zero when the client only sent the 72-byte v1 prefix, which is also the
    // "not measured" value for every one of them, so a v1 client is indistinguishable from a
    // v2 client that could not measure anything — as it should be.
    pub transport_tier: u8,
    pub render_tier: u8,
    pub key_tier: u8,
    pub sink_tier: u8,
    pub suppressed_errors: u32,
    pub degraded_flags: u32,
    pub audio_target_us: u32,
    pub audio_concealed: u32,
    pub audio_output_latency_us: u32,
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

/// Names for the v2 degradation bits, in bit order. Every one was a bare `catch (_) {}` or an
/// untaken branch on the client before it became a bit here.
pub const CLIENT_DEGRADED_NAMES: [&str; 22] = [
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
    pub bytes_sent: AtomicU64,
    pub encoder: Slot<String>,
    pub capture_backend: Slot<String>,
    pub client: Slot<Option<ClientStats>>,
}

/// One value behind a lock.
///
/// Only slow paths touch these — a backend naming itself once at startup, a client stats frame
/// arriving once a second, a reader building a report — never the frame path, which is why a
/// plain `Mutex` is the right thing here and an atomic would not be.
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

impl SessionTelemetry {
    /// Record a client stats frame, mirroring the two jitter counters into `degradation` so a
    /// starving or spilling audio ring shows up beside the host-side degradations instead of
    /// only inside the client blob. The client reports cumulative totals, so these are stored,
    /// not added.
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
        self.stages
            .packetize
            .record_span(t.encode_done_ns, t.packetize_done_ns);
        self.stages
            .socket_write
            .record_span(t.packetize_done_ns, t.socket_write_done_ns);
    }

    /// One line, all stages, `p50/p90/p99/max` in ms. Printed on a timer, never per frame.
    pub fn summary_line(&self) -> String {
        let mut s = format!(
            "telemetry: capture={} encoder={} frames={}",
            self.capture_backend.get(),
            self.encoder.get(),
            self.frames_encoded.load(Ordering::Relaxed),
        );
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
                // A non-zero degradation field is the most useful line a support conversation
                // can start from, so it is named rather than printed as a hex mask.
                let degraded = client_degraded_list(c.degraded_flags);
                if !degraded.is_empty() {
                    s.push_str(&format!(" degraded={}", degraded.join(",")));
                }
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
            })
        });
        serde_json::json!({
            "capture_backend": self.capture_backend.get(),
            "encoder": self.encoder.get(),
            "frames_encoded": self.frames_encoded.load(Ordering::Relaxed),
            "bytes_sent": self.bytes_sent.load(Ordering::Relaxed),
            "stages": stages,
            "degradation": degradation,
            "client": client,
        })
    }
}

/// Process-wide registry, keyed the way sessions are: by client IP.
///
/// It exists because the reader is usually not the writer's thread and sometimes not even the
/// writer's process — `ScreenExtend status --json` asks the running host over the single-instance
/// control socket, and this is what that request reads.
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

/// Put an existing handle in the registry. The pipeline makes its own at start, before the
/// client IP is known to it, so the server publishes it once the session has a key.
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
        // Peer connections, ICE agents and injector threads that have not been given back.
        // It should equal the number of live sessions; a number that only climbs across joins
        // is the leak this counter exists to make falsifiable.
        "live_transport_sessions": crate::streamer::webrtc_session::live_sessions(),
    })
}

/// Interval for the verbose one-line summary. Chosen so a 30 s baseline run yields six lines.
pub const SUMMARY_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Start the summary printer. It is a plain OS thread on a 5 s timer, deliberately not a task on
/// the streaming runtime and never anywhere near the frame path.
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
        // 99 samples at 1 ms, one at 100 ms: p50 and p90 must sit at 1 ms, p99 at 1 ms and the
        // max at 100 ms. A mean would read 2 ms and describe neither.
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
        // Its encode and transport really happened and are measured.
        assert_eq!(tel.stages.encode.snapshot().count, 1);
        assert_eq!(tel.stages.socket_write.snapshot().count, 1);
        // Nothing was captured, so nothing capture-relative is claimed.
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
        };
        let tel = SessionTelemetry::default();
        tel.record_frame(&t);
        assert_eq!(tel.stages.capture_delivery.snapshot().count, 1);
        assert_eq!(tel.stages.encode.snapshot().count, 1);
        let total = tel.stages.capture_to_socket.snapshot();
        assert_eq!(total.count, 1);
        // 9 ms from the OS frame time to the socket, not 8 ms from our callback.
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
}
