use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use tokio::sync::broadcast;

use crate::streamer::config::{Config, H264Profile};
use crate::streamer::lock::LockExt;
use crate::streamer::platform::BitrateApplied;
use crate::streamer::telemetry::{FrameTiming, SessionTelemetry, SharedTelemetry};

use super::config::{live_encoder_config, EncoderConfig};
use super::display;
use super::encoder::{Encoder, FrameTag, MAX_FRAMES_IN_FLIGHT};
use super::frame::{frame_channel, FrameSource};
use super::gpu::Gpu;
use super::{start_capture, CaptureBackend, CaptureConfig, DisplayId, PIXEL_FORMAT_420F};

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
    /// Per-stage stamps collected on the way here. The transport fills in the last two and folds
    /// the whole set into the session histograms; see `streamer::telemetry`.
    pub timing: FrameTiming,
}

const BROADCAST_CAPACITY: usize = 4;

/// How long a session may run without a single captured frame before the client is handed a
/// picture anyway.
///
/// A capture backend that starts and never delivers produces a session that negotiates, connects,
/// reports healthy and then shows nothing forever: the keepalive can only repeat a frame, and
/// there is no frame to repeat. A freshly created virtual display with nothing on it is exactly
/// that case — no window has moved on it yet, so nothing is dirty and nothing is delivered.
const FIRST_FRAME_DEADLINE: Duration = Duration::from_millis(750);

/// How long the encode loop waits for a frame before looking at its other work.
const IDLE_POLL: Duration = Duration::from_millis(4);

/// How often an unchanged screen is re-encoded so the client's decoder keeps ticking.
const KEEPALIVE: Duration = Duration::from_millis(200);

/// Consecutive submit failures before this encoder is written off and replaced.
const SUBMIT_ERRORS_BEFORE_RESET: u32 = 3;

/// How many times a session may rebuild its encoder before it ends instead.
const MAX_ENCODER_RESETS: u32 = 3;

/// How long the in-flight cap waits for the encoder to emit anything before it stands aside.
///
/// The cap assumes an encoder that returns roughly one frame for each one it is given. Not every
/// VideoToolbox implementation does: without low-latency rate control some hold several frames
/// before emitting the first, and against such an encoder a hard cap of two is a deadlock, not
/// back-pressure — which is what the old code's blocking `CompleteFrames` on the keepalive path
/// was really working around. So the cap yields to evidence: if nothing has come out for this
/// long, keep feeding (up to a much larger bound) and say so once.
const STALL_BYPASS: Duration = Duration::from_millis(100);

/// The bound that still applies while the cap is stood aside.
const HARD_MAX_FRAMES_IN_FLIGHT: usize = 16;

/// The drop guard's window: rate drops are compared with submits over the last ~2 s.
const DROP_GUARD_WINDOW_MS: u64 = 2_000;
/// How often the guard takes a snapshot; the window slides in steps of this.
const DROP_GUARD_STEP_MS: u64 = 250;
const DROP_GUARD_SNAPSHOTS: usize = (DROP_GUARD_WINDOW_MS / DROP_GUARD_STEP_MS) as usize + 1;
/// Fewer submits than this in a window is too few to judge (an idle desktop's keepalive is 5 fps).
const DROP_GUARD_MIN_SUBMITS: u64 = 30;
/// Trip when rate drops exceed this percentage of submits in a window.
const DROP_GUARD_PERCENT: u64 = 5;

/// Watches a low-latency constant-quality session for its rate controller dropping frames.
///
/// The low-latency encoder's rate controller keeps running under a QP clamp, and when its budget
/// runs out it drops the frame (status 0, `kVTEncodeInfo_FrameDropped`, no sample) — 86 % of a
/// recorded window drag with no `AverageBitRate`. The tier selection avoids that on every Mac we
/// measured, but not on every Mac there is, so the encode loop checks: more than
/// `DROP_GUARD_PERCENT` of the submits in any `DROP_GUARD_WINDOW_MS` window (sliding in
/// `DROP_GUARD_STEP_MS` steps) dropped means the session is rebuilt, once, on the standard
/// encoder. Pure: fed totals and a millisecond clock, so it is unit-tested without an encoder.
struct DropGuard {
    /// `(t_ms, submitted, rate_drops)`, a ring.
    snaps: [(u64, u64, u64); DROP_GUARD_SNAPSHOTS],
    len: usize,
    next: usize,
}

impl DropGuard {
    fn new() -> Self {
        DropGuard {
            snaps: [(0, 0, 0); DROP_GUARD_SNAPSHOTS],
            len: 0,
            next: 0,
        }
    }

    /// Forget the history (the encoder was replaced, and its counters restarted from zero).
    fn reset(&mut self) {
        self.len = 0;
        self.next = 0;
    }

    /// Feed the running totals. Returns `(submitted, dropped)` over the window that tripped.
    fn observe(&mut self, now_ms: u64, submitted: u64, drops: u64) -> Option<(u64, u64)> {
        let newest = (self.len > 0)
            .then(|| self.snaps[(self.next + DROP_GUARD_SNAPSHOTS - 1) % DROP_GUARD_SNAPSHOTS]);
        if newest.is_none_or(|(t, _, _)| now_ms.saturating_sub(t) >= DROP_GUARD_STEP_MS) {
            self.snaps[self.next] = (now_ms, submitted, drops);
            self.next = (self.next + 1) % DROP_GUARD_SNAPSHOTS;
            self.len = (self.len + 1).min(DROP_GUARD_SNAPSHOTS);
        }
        // The newest snapshot at least a window old is the window's start.
        let base = (1..=self.len)
            .map(|k| self.snaps[(self.next + DROP_GUARD_SNAPSHOTS - k) % DROP_GUARD_SNAPSHOTS])
            .find(|(t, _, _)| now_ms.saturating_sub(*t) >= DROP_GUARD_WINDOW_MS)?;
        let d_sub = submitted.saturating_sub(base.1);
        let d_drop = drops.saturating_sub(base.2);
        (d_sub >= DROP_GUARD_MIN_SUBMITS && d_drop * 100 > d_sub * DROP_GUARD_PERCENT)
            .then_some((d_sub, d_drop))
    }
}

// ---------------------------------------------------------------------------------------------
// Hold push: get the newest frame out of an encoder that keeps it until the next submit.
// ---------------------------------------------------------------------------------------------

/// The floor of the probing wait: a real frame alone in the encoder this long is pushed.
const PUSH_AFTER_FLOOR_NS: u64 = 50_000_000;
/// The probing wait is also at least this many times the encoder's own free-emission latency, so
/// an encoder that is merely slow is never pushed (and never taken for a holding one).
const PUSH_AFTER_EWMA_MULT: f64 = 3.0;
/// Weight of each new sample in the latency averages.
const PUSH_EWMA_ALPHA: f64 = 0.125;
/// The floor of the release window: a pushed frame that comes out within
/// `max(this, 2.5 × the push→emit average)` of its push was released by the push. A fixed 20 ms
/// was wrong for a slow encoder: `h264.gva` takes ~11 ms at 60 fps, ~16 ms at 30 fps and ~22 ms at
/// 10 fps (bigger deltas), so at 10 fps 44 % of frames got a wasted second push.
const PUSH_RELEASE_WINDOW_NS: u64 = 20_000_000;
const PUSH_RELEASE_EWMA_MULT: f64 = 2.5;
/// Released-by-a-push frames before every real frame is pushed at once (1 on an encoder already
/// confirmed holding in this process).
const PUSH_IMMEDIATE_AFTER: u32 = 3;
/// Pushes one real frame may get; past that the keepalive and the stall bypass own it.
const MAX_PUSHES_PER_FRAME: u8 = 2;
/// Steady-cadence learning: consecutive real frames each released only by the next real submit
/// (never freely) before one deliberate probe is armed.
const STEADY_SUSPECT_AFTER: u32 = 8;
/// Passing steady-cadence probes, each at least [`STEADY_PROBE_SPACING_NS`] apart, before
/// immediate mode (1 on an encoder already confirmed holding).
const STEADY_PROBES: u32 = 3;
const STEADY_PROBE_SPACING_NS: u64 = 1_000_000_000;
/// After a failed probe the next waits this long, doubling to [`STEADY_BACKOFF_MAX_NS`]: on an
/// encoder that is merely slow, probing costs one extra frame every minute at most.
const STEADY_BACKOFF_START_NS: u64 = 2_000_000_000;
const STEADY_BACKOFF_MAX_NS: u64 = 60_000_000_000;
/// A probe passes when the pushed frame came out at least this much sooner than its suspicious
/// predecessors: `max(5 ms, 30 % of their median submit→emit)`.
const STEADY_MARGIN_FLOOR_NS: u64 = 5_000_000;

/// The efficacy guard (r7). Pushing is judged against the measured alternative, `L0`: the median
/// real submit→emit of this encoder WITHOUT pushes in this session (probing or suspended frames
/// released by the next real one), not against one frame interval. The r6 criterion ("median ≥
/// 0.9 × interval") suspended pushes that were helping: Intel host, noise 1080p60, 19.3 ms with pushes
/// against 21–23 ms without, browser capture→paint 34.6 → 46.8 ms p50 once suspended.
///
/// The window: real, non-keyframe frames emitted in the last `GUARD_SPAN_NS`, at least
/// `GUARD_MIN_SAMPLES` of them; fewer keeps the current mode (an idle desktop at 3.5 fresh
/// frames/s is never judged on a window reaching back to the session's start).
const GUARD_SPAN_NS: u64 = 2_000_000_000;
const GUARD_MIN_SAMPLES: usize = 30;
/// `L0 = current median interval + median(work)`. On a holding encoder the no-push submit→emit
/// follows the frame interval, so what is kept is the part that does not: `work`, from the moment a
/// frame's releasing submit was in (and the frame before it out) to its own emission. Kept per
/// encoder id and size for the process, so a session that starts in a learned mode has it.
const GUARD_WORK_SAMPLES: usize = 15;
/// While pushing, one real frame this often is left unpushed under steady motion, so `work`
/// follows the load.
const GUARD_SAMPLE_EVERY_NS: u64 = 5_000_000_000;
/// Suspend once the with-push median has been at least this share of `L0` at every judgement for
/// `GUARD_SUSTAIN_NS`; a judgement below it, or no judgement for `GUARD_JUDGE_GAP_NS`, starts over.
const GUARD_SUSPEND_RATIO: f64 = 0.9;
const GUARD_SUSTAIN_NS: u64 = 1_000_000_000;
const GUARD_JUDGE_GAP_NS: u64 = 300_000_000;
/// Judge every this many real frames: the statistic is on the deadline-scheduled encode thread.
const GUARD_JUDGE_EVERY: u32 = 8;
/// The newest frames judged (a fixed array, no allocation per emission).
const GUARD_LAT_MAX: usize = 256;
/// Gaps kept for the interval estimate.
const GUARD_WINDOW: usize = 60;
/// Submit gaps longer than this are pauses, not the frame interval.
const GUARD_MAX_GAP_NS: u64 = 250_000_000;
/// Suspension back-off: first, doubling to the cap; reset after a long healthy stretch.
const SUSPEND_START_NS: u64 = 10_000_000_000;
const SUSPEND_MAX_NS: u64 = 120_000_000_000;
const SUSPEND_HEALTHY_RESET_NS: u64 = 300_000_000_000;
/// While suspended, a real frame not followed by another within this many intervals gets the one
/// end-of-burst push (what removes the 200 ms keepalive wait for the last frame of a burst).
const END_OF_BURST_INTERVALS: f64 = 1.25;

/// The client guard. The page's STATS frame (about once a second) carries arrival→paint p50 over
/// a sliding window of its last 512 painted frames — a count, not a time: ~8.5 s at 60 fps, ~4.3 s
/// at 120 fps with pushes. So a baseline (not pushing) is taken only once the window holds frames
/// painted entirely since pushing stopped, and a judgement (pushing) only once at least half of it
/// was painted while pushing.
const CLIENT_WINDOW_FRAMES: u64 = 512;
/// Fresh STATS in a row over the threshold before pushing is suspended for the client's sake.
const CLIENT_OVER_TO_SUSPEND: u32 = 3;
/// No fresh client STATS for this long: no evidence the client can take twice the frame rate.
const CLIENT_STALE_NS: u64 = 3_000_000_000;

/// Consecutive captured frames that had to wait for the in-flight cap before immediate mode
/// stands down: the encoder is not keeping up, and a push per frame would only add to its load.
const PUSH_CAP_WAITS_TO_LEAVE: u32 = 3;
/// ...and the waits must span at least this long: one slow frame is not overload.
const PUSH_CAP_WAIT_SPAN_NS: u64 = 150_000_000;
/// Cap waits do not count this long after the encoder's first submit (the session-start IDR:
/// ~580 KB and ~50 ms on `gva`, which made every immediate start fall out and spend ~2.2 s
/// re-learning), nor while a forced keyframe is in flight.
const PUSH_CAP_WAIT_GRACE_NS: u64 = 1_000_000_000;

/// How the encode loop treats a held frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HoldMode {
    /// Push only a real frame that has sat unemitted for [`HoldPush::push_after_ns`] with nothing
    /// newer submitted, or one steady-cadence probe frame. Also the evidence-gathering state.
    Probing,
    /// Follow every real frame with one push straight away.
    Immediate,
    /// Pushing was not working (the efficacy guard, or captured frames waiting for the cap): for a
    /// back-off only the end-of-burst push remains, then probing again.
    Suspended,
}

impl HoldMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HoldMode::Probing => "probing",
            HoldMode::Immediate => "immediate",
            HoldMode::Suspended => "suspended",
        }
    }
}

/// What a successful submit was, for [`HoldPush`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubmitKind {
    /// A captured frame.
    Real,
    /// A hold push (a repeat of the newest real frame, to release it).
    Push,
    /// Any other repeat: keepalive, a forced keyframe on an unchanged screen, the placeholder.
    Other,
}

/// What [`HoldPush::due`] wants done now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Due {
    Nothing,
    /// Submit one push (a repeat of the newest real frame).
    Push,
    /// The newest real frame is still held after [`MAX_PUSHES_PER_FRAME`] pushes. Reported once
    /// per frame; nothing more is pushed for it.
    Exhausted,
}

/// What a mode change says about the encoder, for the process-wide memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Evidence {
    /// Pushes released frames: the encoder holds. Confirms it for the process.
    Holding,
    /// A free emission: the encoder does not hold. Withdraws any confirmation.
    NotHolding,
    /// The encoder is not keeping up; says nothing about holding.
    Overload,
}

/// The transport tier a STATS frame stands for: its own from v2 on. A v1 frame (a cached older page)
/// has no tier byte, so it parses as 0, which is also what a v2+ page on the `<video>` path sends;
/// v1 does say which video path it paints through (1 / 2: WebCodecs, 3: `<video>`).
fn effective_transport_tier(version: u8, video_path: u8, tier: u8) -> u8 {
    if version < 2 {
        u8::from(matches!(video_path, 1 | 2))
    } else {
        tier
    }
}

/// A mode change and the evidence for it, for the log.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HoldTransition {
    pub from: HoldMode,
    pub to: HoldMode,
    pub evidence: Evidence,
    pub why: String,
}

#[derive(Clone, Copy, Debug)]
struct HeldReal {
    seq: u64,
    submit_ns: u64,
    /// The newest submit that belongs to this frame: itself, or its latest push.
    own_seq: u64,
    pushes: u8,
    first_push_ns: u64,
    last_push_ns: u64,
    /// The first push came after the full probing wait (condition (a) of the evidence).
    waited_full: bool,
    emitted: bool,
    exhausted: bool,
}

/// The hold-push state machine. Pure: fed submits, emissions and the clock (host ns), it says
/// when to push and when the mode changes. One per encoder instance; [`HoldPush::reset`] when
/// the encoder is replaced.
///
/// Emission `n` is submit `n`: VideoToolbox makes one output callback per accepted frame, in
/// decode order, which with frame reordering off is submit order (`Encoder::emit_time_ns`).
///
/// Evidence, all of it behavioural, every bound scaled by what this encoder has shown:
/// * A **free emission** — any frame that comes out with nothing submitted after it — is proof
///   the encoder does not hold. It voids all gathered evidence and leaves immediate mode; on a
///   real frame its latency feeds the average that sets the probing wait, so an encoder that is
///   slow (4K on an M1) pushes later and later and never doubles its own load.
/// * **Pause probe:** a real frame that sat unemitted for the whole probing wait with nothing
///   newer submitted, was pushed, and came out within [`HoldPush::pause_window_ns`] of the push.
///   One that did not come out that promptly resets the streak. A frame simply released by a
///   later frame proves nothing either way and leaves the streak alone.
/// * **Steady-cadence probe** ([`Steady`]), for continuous motion where no frame is ever alone.
/// * The **push→emit average** (every pushed real frame that came out after its push) sets the
///   release window and the second-push delay, so a slow holding encoder is neither missed nor
///   pushed twice.
/// * In immediate mode, [`PUSH_CAP_WAITS_TO_LEAVE`] captured frames in a row spanning at least
///   [`PUSH_CAP_WAIT_SPAN_NS`] that had to wait for the in-flight cap — outside the first second
///   and while no forced keyframe is in flight — mean the encoder is not keeping up.
/// * An encoder **confirmed holding** earlier in the process re-enters immediate mode on a
///   single passing probe of either kind.
pub(crate) struct HoldPush {
    mode: HoldMode,
    ewma_ns: Option<f64>,
    /// Average push→emit of pushed real frames.
    push_emit_ewma_ns: Option<f64>,
    real: Option<HeldReal>,
    last_seq: u64,
    /// `(seq, submit_ns, kind)` of submits not yet known to be emitted, plus the one after the
    /// newest emitted frame (to answer "was anything submitted after it, and when").
    recent: std::collections::VecDeque<(u64, u64, SubmitKind)>,
    streak: u32,
    /// Push→emit latencies of the current streak, for the transition's log line.
    streak_ms: Vec<f64>,
    /// Consecutive captured frames that waited for the in-flight cap (immediate mode only), and
    /// when the first of them did.
    cap_waits: u32,
    first_cap_wait_ns: u64,
    /// This encoder has emitted something. Probing never pushes before that: a new session's
    /// first frame is its warm-up (measured 50.9 ms on the M1's `h264.rtvc`, against ~8 ms for
    /// every later one), which says nothing about holding.
    any_emitted: bool,
    /// The encoder's first submit (host ns), for the cap-wait grace.
    born_ns: Option<u64>,
    /// The newest emitted frame, and the newest frame submitted as a forced keyframe.
    emitted_upto: u64,
    keyframe_seq: u64,
    /// Confirmed holding earlier in this process: one passing probe is enough.
    confirmed: bool,
    /// The efficacy guard (immediate mode): recent real submit→emit and real submit gaps.
    /// `(emit ns, submit→emit ns)` of real non-keyframe frames emitted in immediate mode.
    guard_lat: std::collections::VecDeque<(u64, u64)>,
    guard_gap: std::collections::VecDeque<u64>,
    /// `work` samples of unpushed real frames (see [`GUARD_WORK_SAMPLES`]).
    work: std::collections::VecDeque<u64>,
    prev_emit_ns: u64,
    /// The real frame deliberately left unpushed, and when the last one was.
    sample_seq: Option<u64>,
    last_sample_ns: Option<u64>,
    pub(crate) samples: u64,
    judge_in: u32,
    last_judged_ns: u64,
    /// Since when the with-push median has been over the suspend line.
    guard_over_since_ns: Option<u64>,
    last_real_submit_ns: Option<u64>,
    /// Suspension: until when, the next back-off, and since when immediate mode has been healthy.
    suspended_until_ns: u64,
    suspend_backoff_ns: u64,
    immediate_since_ns: Option<u64>,
    /// Suspensions so far (telemetry).
    pub(crate) suspensions: u64,
    /// The client guard (see [`HoldPush::client_stats`]). Off only in tests that model no client.
    client_gate: bool,
    last_client_ns: Option<u64>,
    client_baseline_us: Option<u64>,
    /// `(mode, frames painted)` when the current mode's first fresh STATS arrived.
    client_marker: Option<(HoldMode, u64)>,
    client_over: u32,
    /// Steady-cadence learning (see [`Steady`]).
    steady: Steady,
}

/// The steady-cadence probe's state. Under continuous motion a one-frame-hold encoder never
/// leaves a frame alone long enough for the pause probe, so suspicion is built from what *can* be
/// seen — every frame came out only after the next one went in — and then tested causally: one
/// frame gets a push straight away, and the probe passes only if that frame's submit→emit was
/// clearly below its predecessors' **and** below the frame interval. An encoder that is merely
/// slow emits each frame at its own pace whatever is submitted after it, so the push changes
/// nothing and the probe fails; one that holds emits the pushed frame an encode time after the
/// push, a whole frame interval early. No absolute bound: `gva` at 30 fps takes ~16 ms.
#[derive(Default)]
struct Steady {
    suspect: u32,
    /// submit→emit of the current run of suspicious frames, newest last (at most 8).
    lat_ns: std::collections::VecDeque<u64>,
    /// Submit-to-next-submit gaps of the same frames (the frame interval).
    gap_ns: std::collections::VecDeque<u64>,
    /// The next real frame becomes a probe.
    armed: bool,
    /// `(seq, baseline submit→emit ns, interval ns, pushed)` of the frame being probed.
    probe: Option<(u64, u64, u64, bool)>,
    passed: u32,
    /// `(probe submit→emit ms, baseline ms)` of the passes, for the log line.
    passes_ms: Vec<(f64, f64)>,
    next_ns: u64,
    backoff_ns: u64,
}

/// Median without allocating: every window it is used on holds at most 64 values.
fn median(v: &std::collections::VecDeque<u64>) -> u64 {
    let mut a = [0u64; 64];
    let n = v.len().min(a.len());
    for (d, x) in a.iter_mut().zip(v.iter()) {
        *d = *x;
    }
    if n == 0 {
        return 0;
    }
    *a[..n].select_nth_unstable(n / 2).1
}

impl Steady {
    /// Any free emission: the encoder does not hold. Everything gathered is void.
    fn cancel(&mut self) {
        self.suspect = 0;
        self.lat_ns.clear();
        self.gap_ns.clear();
        self.armed = false;
        self.probe = None;
        self.passed = 0;
        self.passes_ms.clear();
    }
}

/// The steady probe's causal test: the pushed frame's submit→emit `lat` is at least
/// `max(5 ms, 30 %)` below its predecessors' median `baseline`, and below the frame `interval`.
fn steady_probe_passes(lat: u64, baseline: u64, interval: u64) -> bool {
    let margin = STEADY_MARGIN_FLOOR_NS.max(baseline * 3 / 10);
    lat.saturating_add(margin) <= baseline && lat < interval
}

/// The window within which a push counts as having released its frame:
/// `max(20 ms, 2.5 × the push→emit average)`.
fn release_window_ns(push_emit_ewma_ns: Option<f64>) -> u64 {
    let scaled = push_emit_ewma_ns.map_or(0.0, |e| e * PUSH_RELEASE_EWMA_MULT) as u64;
    scaled.max(PUSH_RELEASE_WINDOW_NS)
}

impl HoldPush {
    pub(crate) fn new(mode: HoldMode) -> Self {
        HoldPush {
            mode,
            ewma_ns: None,
            push_emit_ewma_ns: None,
            real: None,
            last_seq: 0,
            recent: std::collections::VecDeque::with_capacity(32),
            streak: 0,
            streak_ms: Vec::with_capacity(PUSH_IMMEDIATE_AFTER as usize),
            cap_waits: 0,
            first_cap_wait_ns: 0,
            any_emitted: false,
            born_ns: None,
            emitted_upto: 0,
            keyframe_seq: 0,
            confirmed: false,
            guard_lat: std::collections::VecDeque::with_capacity(GUARD_LAT_MAX),
            work: std::collections::VecDeque::with_capacity(GUARD_WORK_SAMPLES),
            prev_emit_ns: 0,
            sample_seq: None,
            last_sample_ns: None,
            samples: 0,
            judge_in: GUARD_JUDGE_EVERY,
            last_judged_ns: 0,
            guard_over_since_ns: None,
            guard_gap: std::collections::VecDeque::with_capacity(GUARD_WINDOW),
            last_real_submit_ns: None,
            suspended_until_ns: 0,
            suspend_backoff_ns: SUSPEND_START_NS,
            immediate_since_ns: None,
            suspensions: 0,
            client_gate: true,
            last_client_ns: None,
            client_baseline_us: None,
            client_marker: None,
            client_over: 0,
            steady: Steady {
                backoff_ns: STEADY_BACKOFF_START_NS,
                ..Steady::default()
            },
        }
    }

    /// The encoder was replaced: its sequence numbers restart, and what it was is unknown again
    /// (the process-wide confirmation is re-applied by the caller).
    pub(crate) fn reset(&mut self) {
        // The back-off belongs to the session, not to one encoder instance.
        let (backoff, suspensions, samples) =
            (self.suspend_backoff_ns, self.suspensions, self.samples);
        *self = HoldPush::new(HoldMode::Probing);
        self.suspend_backoff_ns = backoff;
        self.suspensions = suspensions;
        self.samples = samples;
    }

    /// What earlier sessions on this encoder and size measured: `work`, and where the suspension
    /// back-off stood if that was recent.
    pub(crate) fn seed(&mut self, m: &HoldMemory, now_ns: u64) {
        self.work = m.work.clone();
        if m.backoff_ns > 0 && now_ns.saturating_sub(m.at_ns) < SUSPEND_HEALTHY_RESET_NS {
            self.suspend_backoff_ns = m.backoff_ns.clamp(SUSPEND_START_NS, SUSPEND_MAX_NS);
        }
    }

    pub(crate) fn memory(&self, now_ns: u64) -> HoldMemory {
        HoldMemory {
            work: self.work.clone(),
            backoff_ns: self.suspend_backoff_ns,
            at_ns: now_ns,
        }
    }

    pub(crate) fn mode(&self) -> HoldMode {
        self.mode
    }

    /// This encoder was confirmed holding earlier in the process (or that was withdrawn).
    pub(crate) fn set_confirmed(&mut self, confirmed: bool) {
        self.confirmed = confirmed;
    }

    fn passes_needed(&self, normal: u32) -> u32 {
        if self.confirmed {
            1
        } else {
            normal
        }
    }

    /// `max(50 ms, 3 × the free-emission latency average)`.
    pub(crate) fn push_after_ns(&self) -> u64 {
        let ewma = self.ewma_ns.map_or(0.0, |e| e * PUSH_AFTER_EWMA_MULT) as u64;
        ewma.max(PUSH_AFTER_FLOOR_NS)
    }

    /// The pause probe's release window: the scaled window, but never past the probing wait.
    pub(crate) fn pause_window_ns(&self) -> u64 {
        release_window_ns(self.push_emit_ewma_ns).min(self.push_after_ns())
    }

    /// The delay before a second push in immediate mode: the scaled window (which is at least one
    /// full push→emit average).
    pub(crate) fn second_push_after_ns(&self) -> u64 {
        release_window_ns(self.push_emit_ewma_ns)
    }

    /// `(seq, submit time)` of the newest real frame, for the log.
    pub(crate) fn newest_real(&self) -> Option<(u64, u64)> {
        self.real.map(|r| (r.seq, r.submit_ns))
    }

    /// A pending push (immediate mode, or a steady probe) gets a slot of its own on top of the
    /// in-flight cap, so it can go in straight after its frame instead of waiting for the previous
    /// frame to come out.
    pub(crate) fn reserved_slots(&self) -> usize {
        let probe_pending =
            self.steady.armed || self.steady.probe.is_some_and(|(_, _, _, pushed)| !pushed);
        usize::from(self.mode == HoldMode::Immediate || probe_pending)
    }

    /// Frame `seq` was submitted with `ForceKeyFrame`.
    pub(crate) fn keyframe_submitted(&mut self, seq: u64) {
        self.keyframe_seq = self.keyframe_seq.max(seq);
    }

    /// A captured frame is about to be submitted at `now_ns`; `waited` says whether it had to
    /// wait for the in-flight cap first. Call before [`HoldPush::submitted`] for every real frame.
    pub(crate) fn real_cap_wait(&mut self, waited: bool, now_ns: u64) -> Option<HoldTransition> {
        let in_grace = self
            .born_ns
            .is_none_or(|b| now_ns.saturating_sub(b) < PUSH_CAP_WAIT_GRACE_NS);
        let keyframe_in_flight = self.keyframe_seq > self.emitted_upto;
        if self.mode != HoldMode::Immediate || !waited || in_grace || keyframe_in_flight {
            self.cap_waits = 0;
            return None;
        }
        if self.cap_waits == 0 {
            self.first_cap_wait_ns = now_ns;
        }
        self.cap_waits += 1;
        let span = now_ns.saturating_sub(self.first_cap_wait_ns);
        if self.cap_waits < PUSH_CAP_WAITS_TO_LEAVE || span < PUSH_CAP_WAIT_SPAN_NS {
            return None;
        }
        self.cap_waits = 0;
        Some(self.suspend(
            now_ns,
            format!(
                "{PUSH_CAP_WAITS_TO_LEAVE}+ captured frames in a row over {:.0} ms had to wait for \
                 the in-flight cap (no keyframe in flight, past the first second): the encoder is \
                 not keeping up, and a push per frame would only add to its load",
                span as f64 / 1e6
            ),
        ))
    }

    /// Immediate → suspended for the current back-off (then doubled, to its cap).
    fn suspend(&mut self, now_ns: u64, why: String) -> HoldTransition {
        let backoff = self.suspend_backoff_ns;
        self.suspended_until_ns = now_ns.saturating_add(backoff);
        self.suspend_backoff_ns = (backoff * 2).min(SUSPEND_MAX_NS);
        self.mode = HoldMode::Suspended;
        self.suspensions += 1;
        self.immediate_since_ns = None;
        self.guard_lat.clear();
        self.guard_over_since_ns = None;
        self.streak = 0;
        self.streak_ms.clear();
        self.steady.cancel();
        HoldTransition {
            from: HoldMode::Immediate,
            to: HoldMode::Suspended,
            evidence: Evidence::Overload,
            why: format!(
                "{why}; suspended for {} s (only the end-of-burst push), then probing again",
                backoff / 1_000_000_000
            ),
        }
    }

    /// The clock moved: a suspension whose back-off has run out goes back to probing, where a
    /// probe must pass again before immediate mode. Call once per loop iteration.
    pub(crate) fn poll(&mut self, now_ns: u64) -> Option<HoldTransition> {
        if self.mode == HoldMode::Immediate && !self.client_ok(now_ns) {
            self.mode = HoldMode::Probing;
            self.immediate_since_ns = None;
            return Some(HoldTransition {
                from: HoldMode::Immediate,
                to: HoldMode::Probing,
                evidence: Evidence::Overload,
                why: format!(
                    "no fresh client STATS for {} s: no evidence the client can take twice the \
                     frame rate, so only the cheap pushes remain",
                    CLIENT_STALE_NS / 1_000_000_000
                ),
            });
        }
        if self.mode != HoldMode::Suspended || now_ns < self.suspended_until_ns {
            return None;
        }
        self.mode = HoldMode::Probing;
        Some(HoldTransition {
            from: HoldMode::Suspended,
            to: HoldMode::Probing,
            evidence: Evidence::Overload,
            why: "the suspension's back-off ran out; a probe must pass again before immediate mode"
                .into(),
        })
    }

    /// Whether a client is reporting (fresh STATS within [`CLIENT_STALE_NS`]); always true with the
    /// gate off. With no STATS yet the encoder's first submit counts as the start of the wait.
    fn client_ok(&self, now_ns: u64) -> bool {
        if !self.client_gate {
            return true;
        }
        match self.last_client_ns {
            Some(t) => now_ns.saturating_sub(t) < CLIENT_STALE_NS,
            // The page's first STATS lands about a second after the stream starts: until then the
            // wait counts from this encoder's first submit (and before any submit, nothing has
            // started yet).
            None => self
                .born_ns
                .is_none_or(|b| now_ns.saturating_sub(b) < CLIENT_STALE_NS),
        }
    }

    /// A fresh client STATS frame: arrival→paint p50 (µs) over the page's last 512 painted
    /// frames, and its cumulative painted-frame count. Not pushing (probing / suspended), a
    /// window painted entirely since pushing stopped becomes the baseline. Pushing, once at
    /// least half the window was painted while pushing, a p50 above
    /// `max(2 × baseline, baseline + half an interval)` in [`CLIENT_OVER_TO_SUSPEND`] frames in
    /// a row suspends pushing with the same back-off: a client that cannot process twice the
    /// frame rate builds a standing queue (measured: arrival→paint 11.7 -> 32.7 ms with pushes on
    /// a client 8 ms slower per frame), and the end-of-burst push remains.
    pub(crate) fn client_stats(
        &mut self,
        now_ns: u64,
        p50_us: u64,
        frames_painted: u64,
        transport_tier: u8,
    ) -> Option<HoldTransition> {
        // The page's `<video>` path cannot measure arrival→paint and sends 0: such a STATS frame is
        // no evidence that the client can take twice the frame rate, so it neither keeps
        // immediate mode eligible nor makes a baseline.
        if transport_tier == 0 || p50_us == 0 {
            self.client_marker = None;
            self.client_over = 0;
            return None;
        }
        self.last_client_ns = Some(now_ns);
        let start = match self.client_marker {
            Some((m, p)) if m == self.mode && frames_painted >= p => p,
            _ => {
                self.client_marker = Some((self.mode, frames_painted));
                self.client_over = 0;
                return None;
            }
        };
        let since = frames_painted - start;
        if self.mode != HoldMode::Immediate {
            if since >= CLIENT_WINDOW_FRAMES {
                self.client_baseline_us = Some(p50_us);
            }
            return None;
        }
        // A quarter of what the page's p50 covers (its newest 512 frames, or all there are) was
        // painted while pushing. The older frames are the faster ones, so a median over the line
        // despite them is stronger evidence, not weaker; three STATS in a row are still needed.
        if since * 4 < frames_painted.min(CLIENT_WINDOW_FRAMES) {
            return None;
        }
        let iv_us = self.interval_ns().map(|iv| iv / 1_000);
        // With no pushes-off window seen, judge against the frame interval: a client keeping up
        // paints within ~4 ms of arrival, a software decoder ~11 ms and still gains; one that
        // cannot take twice the frame rate queues (24.8 ms at 60 fps measured, 11.7 without
        // pushes — 1.5 intervals, the former bound, was 25 and let it pass).
        let (base, threshold) = match (self.client_baseline_us, iv_us) {
            (Some(b), iv) => (b, (2 * b).max(b + iv.unwrap_or(0) / 2)),
            (None, Some(iv)) => (0, iv * 6 / 5),
            (None, None) => return None,
        };
        if p50_us <= threshold {
            self.client_over = 0;
            return None;
        }
        self.client_over += 1;
        if self.client_over < CLIENT_OVER_TO_SUSPEND {
            return None;
        }
        self.client_over = 0;
        Some(self.suspend(
            now_ns,
            format!(
                "client: arrival->paint p50 {:.1} ms in {CLIENT_OVER_TO_SUSPEND} STATS in a row \
                 against {:.1} ms without pushes (0: none seen; threshold {:.1} ms): the client \
                 cannot process twice the frame rate",
                p50_us as f64 / 1e3,
                base as f64 / 1e3,
                threshold as f64 / 1e3
            ),
        ))
    }

    /// The median real frame interval seen recently, if any.
    fn interval_ns(&self) -> Option<u64> {
        (!self.guard_gap.is_empty()).then(|| median(&self.guard_gap))
    }

    /// `L0`, what a real frame's submit→emit would be without pushes at the current cadence.
    /// `None` until a `work` sample exists: there is nothing to judge pushes against.
    fn l0_ns(&self) -> Option<u64> {
        (!self.work.is_empty()).then_some(())?;
        Some(self.interval_ns()? + median(&self.work))
    }

    fn note_work(&mut self, work_ns: u64) {
        if self.work.len() >= GUARD_WORK_SAMPLES {
            self.work.pop_front();
        }
        self.work.push_back(work_ns);
    }

    /// `(median real submit→emit ms with pushes over the recent window, L0 ms)` — what the guard
    /// judges. `None` while the window is too sparse, or there is no `L0`.
    pub(crate) fn guard_stat(&self) -> Option<(f64, f64)> {
        if self.guard_lat.len() < GUARD_MIN_SAMPLES {
            return None;
        }
        let l0 = self.l0_ns()?;
        let mut a = [0u64; GUARD_LAT_MAX];
        let n = self.guard_lat.len().min(GUARD_LAT_MAX);
        for (d, e) in a.iter_mut().zip(self.guard_lat.iter()) {
            *d = e.1;
        }
        let lat = *a[..n].select_nth_unstable(n / 2).1;
        Some((lat as f64 / 1e6, l0 as f64 / 1e6))
    }

    /// Whether the real frame being submitted at `now_ns`, `gap_ns` after the last, is the one to
    /// leave unpushed: pushing, steady motion, no keyframe in flight, at most one per
    /// [`GUARD_SAMPLE_EVERY_NS`].
    fn sample_now(&self, now_ns: u64, gap_ns: u64) -> bool {
        self.mode == HoldMode::Immediate
            && self.guard_gap.len() >= GUARD_MIN_SAMPLES
            && self.keyframe_seq <= self.emitted_upto
            && self
                .last_sample_ns
                .is_none_or(|t| now_ns.saturating_sub(t) >= GUARD_SAMPLE_EVERY_NS)
            && self.interval_ns().is_some_and(|iv| gap_ns * 2 <= iv * 3)
    }

    /// A submit of `kind` was accepted as frame `seq` (1-based, per encoder) at `now_ns`.
    pub(crate) fn submitted(&mut self, kind: SubmitKind, seq: u64, now_ns: u64) {
        self.born_ns.get_or_insert(now_ns);
        self.last_seq = seq;
        if self.recent.len() >= 64 {
            self.recent.pop_front();
        }
        self.recent.push_back((seq, now_ns, kind));
        match kind {
            SubmitKind::Real => {
                if let Some(prev) = self.last_real_submit_ns.replace(now_ns) {
                    let gap = now_ns.saturating_sub(prev);
                    if self.sample_now(now_ns, gap) {
                        self.sample_seq = Some(seq);
                        self.last_sample_ns = Some(now_ns);
                        self.samples += 1;
                    }
                    if gap > 0 && gap < GUARD_MAX_GAP_NS {
                        if self.guard_gap.len() >= GUARD_WINDOW {
                            self.guard_gap.pop_front();
                        }
                        self.guard_gap.push_back(gap);
                    }
                }
                if self.steady.armed && self.mode == HoldMode::Probing {
                    self.steady.armed = false;
                    self.steady.probe = Some((
                        seq,
                        median(&self.steady.lat_ns),
                        median(&self.steady.gap_ns),
                        false,
                    ));
                }
                self.real = Some(HeldReal {
                    seq,
                    submit_ns: now_ns,
                    own_seq: seq,
                    pushes: 0,
                    first_push_ns: 0,
                    last_push_ns: 0,
                    waited_full: false,
                    emitted: false,
                    exhausted: false,
                });
            }
            SubmitKind::Push => {
                let push_after = self.push_after_ns();
                let probing = self.mode == HoldMode::Probing;
                if let Some((pseq, _, _, pushed @ false)) = self.steady.probe.as_mut() {
                    if self.real.is_some_and(|r| r.seq == *pseq) {
                        *pushed = true;
                    }
                }
                if let Some(r) = self.real.as_mut() {
                    if r.pushes == 0 {
                        r.first_push_ns = now_ns;
                        r.waited_full = probing && now_ns.saturating_sub(r.submit_ns) >= push_after;
                    }
                    r.pushes = r.pushes.saturating_add(1);
                    r.last_push_ns = now_ns;
                    r.own_seq = seq;
                }
            }
            SubmitKind::Other => {}
        }
    }

    fn enter_immediate(&mut self, now_ns: u64, why: String) -> Option<HoldTransition> {
        if !self.client_ok(now_ns) {
            // No client reporting: no evidence it can take twice the frame rate.
            self.streak = 0;
            self.streak_ms.clear();
            self.steady.cancel();
            return None;
        }
        self.mode = HoldMode::Immediate;
        self.immediate_since_ns = None;
        self.guard_lat.clear();
        self.guard_over_since_ns = None;
        self.streak = 0;
        self.streak_ms.clear();
        self.steady.cancel();
        Some(HoldTransition {
            from: HoldMode::Probing,
            to: HoldMode::Immediate,
            evidence: Evidence::Holding,
            why,
        })
    }

    /// Frame `seq` came out at `at_ns`. Call once per frame, in order.
    pub(crate) fn emitted(&mut self, seq: u64, at_ns: u64) -> Option<HoldTransition> {
        let this = self.recent.iter().find(|e| e.0 == seq).copied();
        let next = self.recent.iter().find(|e| e.0 == seq + 1).copied();
        while self.recent.front().is_some_and(|e| e.0 <= seq) {
            self.recent.pop_front();
        }
        self.emitted_upto = self.emitted_upto.max(seq);
        let prev_emit_ns = std::mem::replace(&mut self.prev_emit_ns, at_ns);
        let (_, submit_ns, kind) = this?;
        self.any_emitted = true;
        let free = next.is_none_or(|(_, ns, _)| ns > at_ns);
        let latency_ns = at_ns.saturating_sub(submit_ns);
        let newest = self.real.filter(|r| r.seq == seq);
        if newest.is_some() {
            if let Some(r) = self.real.as_mut() {
                r.emitted = true;
            }
        }
        // Every pushed real frame that came out after its push teaches the push→emit average.
        if let Some(r) = newest.filter(|r| r.pushes > 0 && at_ns >= r.first_push_ns) {
            let d = (at_ns - r.first_push_ns) as f64;
            self.push_emit_ewma_ns = Some(match self.push_emit_ewma_ns {
                Some(e) => e + PUSH_EWMA_ALPHA * (d - e),
                None => d,
            });
        }

        if free {
            if kind == SubmitKind::Real && newest.is_some() {
                let l = latency_ns as f64;
                self.ewma_ns = Some(match self.ewma_ns {
                    Some(e) => e + PUSH_EWMA_ALPHA * (l - e),
                    None => l,
                });
            }
            self.streak = 0;
            self.streak_ms.clear();
            self.steady.cancel();
            let was_confirmed = std::mem::take(&mut self.confirmed);
            if self.mode != HoldMode::Probing || was_confirmed {
                let from = std::mem::replace(&mut self.mode, HoldMode::Probing);
                return Some(HoldTransition {
                    from,
                    to: HoldMode::Probing,
                    evidence: Evidence::NotHolding,
                    why: format!(
                        "frame {seq} ({}) came out {:.1} ms after its submit with nothing \
                         submitted after it, so this encoder does not hold frames",
                        match kind {
                            SubmitKind::Real => "captured",
                            SubmitKind::Push => "a push",
                            SubmitKind::Other => "a repeat",
                        },
                        latency_ns as f64 / 1e6
                    ),
                });
            }
            return None;
        }

        let is_key = seq == self.keyframe_seq;
        let sampled = self.sample_seq == Some(seq);
        if sampled {
            self.sample_seq = None;
        }
        if let (SubmitKind::Real, false, Some((_, released_ns, SubmitKind::Real))) =
            (kind, is_key, next)
        {
            // Released by the next real frame with nothing pushed behind it.
            if sampled || self.mode != HoldMode::Immediate {
                self.note_work(at_ns.saturating_sub(released_ns.max(prev_emit_ns)));
            }
        }
        if kind == SubmitKind::Real && self.mode == HoldMode::Immediate && !is_key && !sampled {
            if let Some(t) = self.guard_emitted(at_ns, latency_ns) {
                return Some(t);
            }
        }

        if kind == SubmitKind::Real && self.mode == HoldMode::Probing {
            if let Some(t) = self.steady_emitted(seq, at_ns, submit_ns, latency_ns, next) {
                return Some(t);
            }
        }

        let r = newest.filter(|r| r.pushes > 0 && r.waited_full)?;
        let window = self.pause_window_ns();
        match at_ns.checked_sub(r.first_push_ns) {
            Some(d) if d <= window => {
                self.streak += 1;
                self.streak_ms.push(d as f64 / 1e6);
                let need = self.passes_needed(PUSH_IMMEDIATE_AFTER);
                if self.mode == HoldMode::Probing && self.streak >= need {
                    let ms: Vec<String> =
                        self.streak_ms.iter().map(|m| format!("{m:.1}")).collect();
                    let why = format!(
                        "{} frame(s) sat unemitted for >= {} ms with nothing newer submitted and \
                         each came out {} ms after one push (window {:.0} ms), with no frame \
                         coming out on its own in between{}: this encoder holds the newest frame \
                         until the next submit",
                        self.streak,
                        self.push_after_ns() / 1_000_000,
                        ms.join("/"),
                        window as f64 / 1e6,
                        if self.confirmed {
                            "; confirmed holding earlier in this process"
                        } else {
                            ""
                        }
                    );
                    return self.enter_immediate(at_ns, why);
                }
            }
            // Released by something other than the push, or not promptly: not this mechanism.
            _ => {
                self.streak = 0;
                self.streak_ms.clear();
            }
        }
        None
    }

    /// The efficacy guard, for a real non-keyframe frame emitted in immediate mode: pushing is
    /// worth its cost while real frames come out clearly sooner than they would without it
    /// (`L0`). See [`GUARD_SPAN_NS`] and [`GUARD_SUSPEND_RATIO`]. A long healthy stretch resets
    /// the back-off.
    fn guard_emitted(&mut self, at_ns: u64, latency_ns: u64) -> Option<HoldTransition> {
        let since = *self.immediate_since_ns.get_or_insert(at_ns);
        if at_ns.saturating_sub(since) >= SUSPEND_HEALTHY_RESET_NS {
            self.suspend_backoff_ns = SUSPEND_START_NS;
        }
        if self.guard_lat.len() >= GUARD_LAT_MAX {
            self.guard_lat.pop_front();
        }
        self.guard_lat.push_back((at_ns, latency_ns));
        while self
            .guard_lat
            .front()
            .is_some_and(|e| at_ns.saturating_sub(e.0) > GUARD_SPAN_NS)
        {
            self.guard_lat.pop_front();
        }
        self.judge_in = self.judge_in.saturating_sub(1);
        if self.judge_in > 0 && self.guard_lat.len() >= GUARD_MIN_SAMPLES {
            return None;
        }
        self.judge_in = GUARD_JUDGE_EVERY;
        let judged = std::mem::replace(&mut self.last_judged_ns, at_ns);
        if at_ns.saturating_sub(judged) > GUARD_JUDGE_GAP_NS {
            self.guard_over_since_ns = None;
        }
        let Some((lat_ms, l0_ms)) = self
            .guard_stat()
            .filter(|(lat, l0)| *lat >= GUARD_SUSPEND_RATIO * l0)
        else {
            self.guard_over_since_ns = None;
            return None;
        };
        let over = *self.guard_over_since_ns.get_or_insert(at_ns);
        if at_ns.saturating_sub(over) < GUARD_SUSTAIN_NS {
            return None;
        }
        let n = self.guard_lat.len();
        Some(self.suspend(
            at_ns,
            format!(
                "real frames' submit->emit median {lat_ms:.1} ms with pushes ({n} frames, last \
                 {:.0} s) is >= {:.0} % of {l0_ms:.1} ms without them (interval + measured work) \
                 continuously for {:.0} s: pushing \
                 does not make real frames clearly faster on this encoder under this load",
                GUARD_SPAN_NS as f64 / 1e9,
                GUARD_SUSPEND_RATIO * 100.0,
                GUARD_SUSTAIN_NS as f64 / 1e9
            ),
        ))
    }

    /// The steady-cadence half of [`HoldPush::emitted`], for a real frame that did not come out
    /// freely, in probing mode.
    fn steady_emitted(
        &mut self,
        seq: u64,
        at_ns: u64,
        submit_ns: u64,
        latency_ns: u64,
        next: Option<(u64, u64, SubmitKind)>,
    ) -> Option<HoldTransition> {
        let need = self.passes_needed(STEADY_PROBES);
        let confirmed = self.confirmed;
        let st = &mut self.steady;
        if let Some((pseq, baseline, interval, pushed)) = st.probe {
            if pseq != seq {
                return None;
            }
            st.probe = None;
            st.suspect = 0;
            st.lat_ns.clear();
            st.gap_ns.clear();
            if !(pushed && steady_probe_passes(latency_ns, baseline, interval)) {
                st.passed = 0;
                st.passes_ms.clear();
                st.next_ns = at_ns.saturating_add(st.backoff_ns);
                st.backoff_ns = (st.backoff_ns * 2).min(STEADY_BACKOFF_MAX_NS);
                return None;
            }
            st.passed += 1;
            st.passes_ms
                .push((latency_ns as f64 / 1e6, baseline as f64 / 1e6));
            st.next_ns = at_ns.saturating_add(STEADY_PROBE_SPACING_NS);
            if st.passed < need {
                return None;
            }
            let ev: Vec<String> = st
                .passes_ms
                .iter()
                .map(|(l, b)| format!("{l:.1} vs {b:.1}"))
                .collect();
            let why =
                format!(
                "at a steady cadence every frame came out only after the next one went in, and \
                 {} probe(s){} each released their frame clearly sooner than the frames before \
                 it and within one frame interval (submit->emit ms, probe vs median: {}){}: this \
                 encoder holds the newest frame until the next submit",
                ev.len(),
                if ev.len() > 1 { " spaced >= 1 s apart" } else { "" },
                ev.join(", "),
                if confirmed {
                    "; confirmed holding earlier in this process"
                } else {
                    ""
                }
            );
            return self.enter_immediate(at_ns, why);
        }
        // Released by the next captured frame (no push, no repeat in between).
        let (by_next_real, gap) = match next {
            Some((_, ns, SubmitKind::Real)) if ns <= at_ns => (true, ns.saturating_sub(submit_ns)),
            _ => (false, 0),
        };
        if !by_next_real {
            return None;
        }
        st.suspect += 1;
        if st.lat_ns.len() >= STEADY_SUSPECT_AFTER as usize {
            st.lat_ns.pop_front();
            st.gap_ns.pop_front();
        }
        st.lat_ns.push_back(latency_ns);
        st.gap_ns.push_back(gap);
        if st.suspect >= STEADY_SUSPECT_AFTER && !st.armed && at_ns >= st.next_ns {
            st.armed = true;
        }
        None
    }

    /// Whether to push now. Never pushes a push: a push is always for the newest real frame, and
    /// only while nothing but its own pushes has been submitted after it.
    pub(crate) fn due(&mut self, now_ns: u64) -> Due {
        let push_after = self.push_after_ns();
        let second = self.second_push_after_ns();
        let mode = self.mode;
        let warm = self.any_emitted;
        let steady_probe = self.steady.probe.map(|(p, _, _, _)| p);
        let end_of_burst = self
            .interval_ns()
            .map_or(push_after, |iv| (iv as f64 * END_OF_BURST_INTERVALS) as u64);
        let sample_seq = self.sample_seq;
        let Some(r) = self.real.as_mut() else {
            return Due::Nothing;
        };
        if r.emitted || r.own_seq != self.last_seq {
            return Due::Nothing;
        }
        if mode == HoldMode::Suspended {
            // Only the end-of-burst push: one, for a real frame nothing followed.
            return if r.pushes == 0 && warm && now_ns.saturating_sub(r.submit_ns) >= end_of_burst {
                Due::Push
            } else {
                Due::Nothing
            };
        }
        let retry = match mode {
            HoldMode::Immediate => second,
            HoldMode::Probing | HoldMode::Suspended => push_after,
        };
        if r.pushes >= MAX_PUSHES_PER_FRAME {
            if !r.exhausted && now_ns.saturating_sub(r.last_push_ns) >= retry {
                r.exhausted = true;
                return Due::Exhausted;
            }
            return Due::Nothing;
        }
        let ready = match (mode, r.pushes) {
            // The sample frame is only pushed if the motion stopped right after it.
            (HoldMode::Immediate, 0) if sample_seq == Some(r.seq) => {
                now_ns.saturating_sub(r.submit_ns) >= end_of_burst
            }
            (HoldMode::Immediate, 0) => true,
            (HoldMode::Probing, 0) if steady_probe == Some(r.seq) => true,
            (HoldMode::Probing, 0) => warm && now_ns.saturating_sub(r.submit_ns) >= push_after,
            (_, _) => now_ns.saturating_sub(r.last_push_ns) >= retry,
        };
        if ready {
            Due::Push
        } else {
            Due::Nothing
        }
    }
}

/// Test hook: start every session's hold push in immediate mode, so a steady-cadence clip can be
/// measured in immediate mode in a fresh process. `SE_HOLD_PUSH_START=immediate` or the tuning
/// candidate `hold_push_start_immediate` (a bench plan's `"tune_on"`), honoured only with
/// `SE_TEST_HOOKS=1`. Recorded in the tuning report either way.
fn hold_push_start_immediate() -> bool {
    use super::vt_recipe;
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        let via_env = vt_recipe::hook_var("SE_HOLD_PUSH_START");
        if let Some(v) = via_env.as_deref() {
            if v != "immediate" && v != "probing" {
                teprintln!("[vt] SE_HOLD_PUSH_START={v} ignored (immediate | probing)");
            }
        }
        let via_tune = super::tune::enabled("hold_push_start_immediate");
        if via_tune && !vt_recipe::test_hooks() {
            teprintln!(
                "[vt] hold_push_start_immediate is switched on but IGNORED: it is a test hook and \
                 is honoured only together with SE_TEST_HOOKS=1"
            );
        }
        let on = via_env.as_deref() == Some("immediate") || (via_tune && vt_recipe::test_hooks());
        super::tune::record(
            "hold_push_start_immediate",
            on,
            serde_json::json!({
                "env": via_env,
                "tune_on": via_tune,
                "test_hooks": vt_recipe::test_hooks(),
            }),
        );
        on
    })
}

/// The mode each `(encoder id, width, height)` last settled on in this process, so the next
/// session on the same encoder starts there instead of re-learning it.
fn learned_hold_mode(key: &(String, u32, u32)) -> HoldMode {
    learned_hold_modes()
        .lock_recover()
        .get(key)
        .copied()
        .unwrap_or(HoldMode::Probing)
}

fn remember_hold_mode(key: (String, u32, u32), mode: HoldMode) {
    learned_hold_modes().lock_recover().insert(key, mode);
}

type HoldKey = (String, u32, u32);

/// What a session leaves for the next one on the same encoder and size (a renegotiation is one).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HoldMemory {
    work: std::collections::VecDeque<u64>,
    backoff_ns: u64,
    at_ns: u64,
}

fn hold_memories() -> &'static std::sync::Mutex<std::collections::HashMap<HoldKey, HoldMemory>> {
    static M: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<HoldKey, HoldMemory>>,
    > = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

fn save_hold_memory(key: &HoldKey, hold: &HoldPush) {
    let m = hold.memory(crate::streamer::audio::host_now_ns());
    hold_memories().lock_recover().insert(key.clone(), m);
}

fn seed_hold(hold: &mut HoldPush, key: &HoldKey) {
    if let Some(m) = hold_memories().lock_recover().get(key) {
        hold.seed(m, crate::streamer::audio::host_now_ns());
    }
}

type LearnedModes = std::sync::Mutex<std::collections::HashMap<HoldKey, HoldMode>>;

fn learned_hold_modes() -> &'static LearnedModes {
    static M: std::sync::OnceLock<LearnedModes> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// Encoders confirmed holding in this process, keyed like the learned modes: once one has been
/// seen to hold, re-entering immediate mode after any exit needs a single passing probe.
fn confirmed_holding() -> &'static std::sync::Mutex<std::collections::HashSet<HoldKey>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<HoldKey>>> =
        std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn is_confirmed_holding(key: &HoldKey) -> bool {
    confirmed_holding().lock_recover().contains(key)
}

/// `(encoder id, tier, width, height)` of every session the drop guard moved to the standard
/// encoder in this process. A later session that lands on the same encoder and tier at the same
/// size starts on the fallback, instead of repeating ~2 s of dropped frames to learn it again.
type FallbackKey = (String, &'static str, u32, u32);

fn drop_guard_fallbacks() -> &'static std::sync::Mutex<std::collections::HashSet<FallbackKey>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<FallbackKey>>> =
        std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

fn fallback_key(enc: &Encoder, cfg: &EncoderConfig) -> FallbackKey {
    (
        enc.encoder_id_short().to_string(),
        enc.rate_tier_name(),
        cfg.width,
        cfg.height,
    )
}

/// At most one line per `every` from a path that could otherwise log per frame.
struct RateLimitedLog {
    last: Option<Instant>,
    suppressed: u64,
}

impl RateLimitedLog {
    const fn new() -> Self {
        RateLimitedLog {
            last: None,
            suppressed: 0,
        }
    }

    /// `Some(lines suppressed since the last one)` when a line may be printed now.
    fn ready(&mut self, every: Duration) -> Option<u64> {
        if self.last.is_some_and(|t| t.elapsed() < every) {
            self.suppressed += 1;
            return None;
        }
        self.last = Some(Instant::now());
        Some(std::mem::take(&mut self.suppressed))
    }
}

/// Re-arms of one keyframe request after rate control dropped the forced frame. Past this the
/// request is let go: the client's own PLI will ask again.
const MAX_IDR_REARMS: u32 = 3;

// ---------------------------------------------------------------------------------------------
// QP governor: a bitrate ceiling for the per-frame-QP tier, for pathological content only.
// ---------------------------------------------------------------------------------------------

/// The window the encoded rate is measured over.
const QP_GOV_WINDOW_NS: u64 = 500_000_000;
const QP_GOV_SNAP_NS: u64 = 25_000_000;
/// The per-frame fast path: this many consecutive frames each over `QP_GOV_FAST_K` times the
/// per-frame budget (`C / fps`) step at once from those frames' rate, instead of waiting for the
/// first full window (~31 MB went out at 365–507 Mbps before the first step on noise). One large
/// frame — a keyframe: 441 KB in the recording, 1.58 MB in the stress clip at 1080p60, against a
/// 4 × 104 KB threshold there — followed by ordinary frames never meets it.
const QP_GOV_FAST_FRAMES: u32 = 3;
const QP_GOV_FAST_K: u64 = 4;
/// After a fast step, long enough for the few frames already in flight at the old QP to come out
/// (at most three); noise does not shrink by the six-steps-an-octave rule (QP 38 still 347 Mbps at
/// 1080p60), so the next step must not wait a whole window.
const QP_GOV_FAST_COOLDOWN_NS: u64 = 100_000_000;
/// Stepping back down needs the rate under this share of the ceiling for a full second per step.
const QP_GOV_DOWN_RATIO: f64 = 0.6;
const QP_GOV_DOWN_HOLD_NS: u64 = 1_000_000_000;

/// The ceiling for a session: `clamp(4 × clamp(w·h·fps/10, 6, 40 Mbps), 24, 160 Mbps)` — 1080p60
/// 49.8 Mbps, 1440p60 88 Mbps, 4K60 160 Mbps. At 1080p60 and QP 15 ordinary content is ~9–11 Mbps
/// on average and ~27 Mbps in its worst second; four times the pixels is ~108 Mbps in that second,
/// which the former 80 Mbps cap would have caught at 4K60 — so the cap scales with the pixel rate
/// and the ceiling keeps a margin over heavy ordinary content there too. Incompressible content
/// (371 Mbps measured at 800x600) is above it at every size.
pub(crate) fn qp_ceiling_bps(width: u32, height: u32, fps: u32) -> u64 {
    let base = (width as u64 * height as u64 * fps as u64 / 10).clamp(6_000_000, 40_000_000);
    (4 * base).clamp(24_000_000, 160_000_000)
}

/// `SE_QP_CEILING_MBPS`: a number of Mbps replaces the computed ceiling, `0` turns the governor
/// off. Read once (plain env, like `SE_PERF_HOLD`).
fn qp_ceiling_override() -> Option<Option<u64>> {
    static V: std::sync::OnceLock<Option<Option<u64>>> = std::sync::OnceLock::new();
    *V.get_or_init(|| qp_ceiling_from(std::env::var("SE_QP_CEILING_MBPS").ok().as_deref()))
}

/// `None`: no override. `Some(None)`: off. `Some(Some(bps))`: that ceiling.
fn qp_ceiling_from(v: Option<&str>) -> Option<Option<u64>> {
    let mbps = v?.trim().parse::<f64>().ok().filter(|m| *m >= 0.0)?;
    Some((mbps > 0.0).then_some((mbps * 1e6) as u64))
}

/// Raises the per-frame QP while the encoded rate over [`QP_GOV_WINDOW_NS`] is above the ceiling
/// — in one move sized to the overshoot (`ceil(6·log2(rate/C))`: six QP steps halve the bits) —
/// and steps back down by one per full second under `0.6 × C`, never below the user's `q` and
/// never above 51. After a raise it waits one window before judging again, so it acts on the new
/// rate rather than the one that triggered it; the band between 0.6·C and C is the hysteresis.
/// A keyframe counts like any frame, but through the windowed rate, not its own size. Pure.
pub(crate) struct QpGovernor {
    q_user: u8,
    q: u8,
    ceiling_bps: u64,
    snaps: std::collections::VecDeque<(u64, u64)>,
    cooldown_until_ns: u64,
    /// The part of the cooldown the fast path obeys: one it set itself.
    fast_until_ns: u64,
    /// The newest real frames' sizes.
    newest: [u64; 5],
    newest_n: usize,
    big_start_ns: u64,
    below_since_ns: Option<u64>,
    last_ns: Option<u64>,
    /// Time spent above the user's q, and how many raises/steps were made.
    pub(crate) above_ns: u64,
    pub(crate) steps: u64,
    fps: u32,
    big_run: u32,
    big_bytes: u64,
}

impl QpGovernor {
    pub(crate) fn new(q_user: u8, ceiling_bps: u64, fps: u32) -> Self {
        QpGovernor {
            fps: fps.max(1),
            big_run: 0,
            big_bytes: 0,
            q_user,
            q: q_user,
            ceiling_bps,
            snaps: std::collections::VecDeque::with_capacity(32),
            cooldown_until_ns: 0,
            fast_until_ns: 0,
            newest: [0; 5],
            newest_n: 0,
            big_start_ns: 0,
            below_since_ns: None,
            last_ns: None,
            above_ns: 0,
            steps: 0,
        }
    }

    pub(crate) fn q(&self) -> u8 {
        self.q
    }

    pub(crate) fn ceiling_bps(&self) -> u64 {
        self.ceiling_bps
    }

    /// Carry the session's totals over an encoder rebuild.
    fn carry(mut self, from: Option<&QpGovernor>) -> Self {
        if let Some(g) = from {
            self.above_ns = g.above_ns;
            self.steps = g.steps;
        }
        self
    }

    /// One encoded real frame of `bytes` seen at `now_ns` (repeats and hold pushes are not fed: a
    /// near-empty repeat between two big frames would break the run): the fast path. Its
    /// cooldown is its own — the windowed path's +1 must not delay the step that matters.
    /// `Some((new q, why))` on a step.
    pub(crate) fn frame(&mut self, now_ns: u64, bytes: u64) -> Option<(u8, String)> {
        self.newest[self.newest_n % 5] = bytes;
        self.newest_n += 1;
        let budget = self.ceiling_bps / 8 / self.fps as u64;
        if bytes > QP_GOV_FAST_K * budget {
            if self.big_run == 0 {
                self.big_start_ns = now_ns;
                self.big_bytes = 0;
            }
            self.big_run += 1;
            self.big_bytes += bytes;
        } else {
            self.big_run = 0;
            self.big_bytes = 0;
        }
        if self.big_run < QP_GOV_FAST_FRAMES || now_ns < self.fast_until_ns || self.q >= 51 {
            return None;
        }
        // The rate these frames really came at: big frames at 10 fps are not the rate of big
        // frames at 60.
        let span = now_ns
            .saturating_sub(self.big_start_ns)
            .max((self.big_run as u64 - 1) * 1_000_000_000 / self.fps as u64);
        let per = self.big_bytes as f64 / self.big_run as f64;
        let rate = per * 8.0 * (self.big_run - 1) as f64 / (span as f64 / 1e9);
        if rate <= self.ceiling_bps as f64 {
            return None;
        }
        let c = self.ceiling_bps as f64;
        let delta = (6.0 * (rate / c).log2()).ceil().max(1.0) as u8;
        let from = self.q;
        self.q = self.q.saturating_add(delta).min(51);
        self.cooldown_until_ns = now_ns + QP_GOV_WINDOW_NS;
        self.fast_until_ns = now_ns + QP_GOV_FAST_COOLDOWN_NS;
        // The window restarts: it must not judge the new QP by bytes of the old one.
        self.snaps.clear();
        self.below_since_ns = None;
        self.steps += 1;
        let n = self.big_run;
        self.big_run = 0;
        self.big_bytes = 0;
        Some((
            self.q,
            format!(
                "{n} frames in a row over {QP_GOV_FAST_K}x the {:.0} KB per-frame budget ({:.1} \
                 Mbps at {} fps) > the {:.1} Mbps ceiling: QP {from} -> {} at once",
                budget as f64 / 1e3,
                rate / 1e6,
                self.fps,
                c / 1e6,
                self.q
            ),
        ))
    }

    /// The encoded byte total at `now_ns`. `Some((new q, why))` when the QP should change.
    pub(crate) fn observe(&mut self, now_ns: u64, total_bytes: u64) -> Option<(u8, String)> {
        if let Some(last) = self.last_ns.replace(now_ns) {
            if self.q > self.q_user {
                self.above_ns += now_ns.saturating_sub(last);
            }
        }
        if self
            .snaps
            .back()
            .is_none_or(|(t, _)| now_ns.saturating_sub(*t) >= QP_GOV_SNAP_NS)
        {
            self.snaps.push_back((now_ns, total_bytes));
        }
        // Keep exactly one snapshot at or before the window's start.
        while self.snaps.len() > 2 && now_ns.saturating_sub(self.snaps[1].0) >= QP_GOV_WINDOW_NS {
            self.snaps.pop_front();
        }
        let (t0, b0) = *self.snaps.front()?;
        let span = now_ns.saturating_sub(t0);
        if span < QP_GOV_WINDOW_NS * 4 / 5 {
            return None;
        }
        let rate = total_bytes.saturating_sub(b0) as f64 * 8.0 / (span as f64 / 1e9);
        let c = self.ceiling_bps as f64;
        if rate > c {
            self.below_since_ns = None;
            if now_ns < self.cooldown_until_ns || self.q >= 51 {
                return None;
            }
            // Sized from the newest frames when they are bigger than the window's average: the
            // window is still mostly the content before the change. A median, so one keyframe
            // does not size it.
            let mut newest = self.newest;
            newest.sort_unstable();
            let now_rate = if self.newest_n >= 5 {
                newest[2] as f64 * 8.0 * self.fps as f64
            } else {
                0.0
            };
            let delta = (6.0 * (rate.max(now_rate) / c).log2()).ceil().max(1.0) as u8;
            let from = self.q;
            self.q = self.q.saturating_add(delta).min(51);
            self.cooldown_until_ns = now_ns + QP_GOV_WINDOW_NS;
            self.snaps.clear();
            self.steps += 1;
            return Some((
                self.q,
                format!(
                    "encoded {:.1} Mbps over {} ms > the {:.1} Mbps ceiling: QP {from} -> {}",
                    rate / 1e6,
                    QP_GOV_WINDOW_NS / 1_000_000,
                    c / 1e6,
                    self.q
                ),
            ));
        }
        if rate < QP_GOV_DOWN_RATIO * c && self.q > self.q_user {
            let since = *self.below_since_ns.get_or_insert(now_ns);
            if now_ns.saturating_sub(since) >= QP_GOV_DOWN_HOLD_NS {
                let from = self.q;
                self.q -= 1;
                self.below_since_ns = Some(now_ns);
                self.steps += 1;
                return Some((
                    self.q,
                    format!(
                        "encoded {:.1} Mbps, under {:.0} % of the ceiling for a second: QP {from} -> {}",
                        rate / 1e6,
                        QP_GOV_DOWN_RATIO * 100.0,
                        self.q
                    ),
                ));
            }
        } else {
            self.below_since_ns = None;
        }
        None
    }
}

#[derive(Clone)]
pub struct Pipeline {
    pub tx: broadcast::Sender<EncodedFrame>,
    pub frame_duration: Duration,
    pub telemetry: SharedTelemetry,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    wake: crossbeam_channel::Sender<()>,
    pub max_bitrate_bps: u32,
    pub h264_profile: H264Profile,
    /// Loss recovery that is not a keyframe. The transport writes receipts into it from
    /// transport-wide feedback; the encode thread reads them at its next submit. Inert — and the
    /// transport falls back to its keyframe request — on any encoder with no long-term reference
    /// slots, which `RepairState::set_ltr_slots(0)` is how a backend says.
    pub repair: Arc<crate::streamer::repair::RepairState>,
}

impl Pipeline {
    pub fn request_idr(&self) {
        self.idr_request.store(true, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }

    pub fn set_target_bitrate(&self, bps: u32) {
        self.target_bitrate.store(bps, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }

    /// Ask for a repair frame instead of a keyframe, if the client is known to hold a reference.
    ///
    /// Returns whether the request was taken; `false` means the caller must fall back to
    /// `request_idr`, which is what every session did before this existed.
    pub fn request_repair(&self) -> bool {
        if !self.repair.request_repair() {
            return false;
        }
        let _ = self.wake.try_send(());
        true
    }
}

pub struct SessionCapture {
    pub pipeline: Pipeline,
    control: Option<Box<dyn CaptureBackend>>,
    stop: Arc<AtomicBool>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
    _activity: Option<super::activity::LatencyActivity>,
}

impl SessionCapture {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut backend) = self.control.take() {
            backend.stop();
        }
        if let Some(handle) = self.encode_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SessionCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `pub(crate)` for the bench's `pipe` stage, which feeds it recorded frames instead of a capture
/// backend.
pub(crate) fn spawn_pipeline(
    enc: EncoderConfig,
    source: FrameSource,
    telemetry: SharedTelemetry,
) -> Result<(Pipeline, Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr_request = Arc::new(AtomicBool::new(false));
    let repair = Arc::new(crate::streamer::repair::RepairState::new());
    let target_bitrate = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let frame_duration = Duration::from_nanos(1_000_000_000 / enc.fps.max(1) as u64);

    let wake = source.wake_sender();
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        telemetry: Arc::clone(&telemetry),
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: enc.max_bitrate_bps,
        h264_profile: enc.profile,
        // Zero slots, so `request_repair` falls through to a keyframe — but not because
        // VideoToolbox cannot do this. It can, from macOS 12.0, in a shape that does not fit a
        // slot bitmap: `EnableLTR` makes the encoder mark and manage its own long-term
        // references and hand back an opaque token per picture
        // (`kVTSampleAttachmentKey_RequireLTRAcknowledgementToken`); the host acknowledges tokens
        // on a later submit and the encoder simply never predicts from an unacknowledged one.
        // There is no "mark this picture", no slot number and no explicit invalidation, so
        // `RepairAction`'s `mark_ltr` / `use_ltr_bitmap` have nothing to map onto — the adapter is
        // the acknowledgement direction plus `ForceLTRRefresh` for `request_repair`, specified in
        // `docs/OPTIMIZATIONS.md` and deferred for want of a Mac that reports `EnableLTR`.
        //
        // The state still has to exist regardless: the transport writes feedback receipts into it
        // unconditionally, and `Encoder::supports_ltr()` already reports per session whether this
        // Mac's encoder advertises the property at all.
        repair: Arc::clone(&repair),
    };

    if super::tune::enabled("power_report") {
        let line = super::tune::power_line();
        tprintln!("[tune] session power state: {line}");
        telemetry.host_power.set(line.clone());
        super::tune::record("power_report", true, serde_json::json!({ "line": line }));
    } else {
        super::tune::record_skipped("power_report");
    }

    // The host clock against wall time, for a harness checking the client's clock offset.
    let (host_now_ns, unix_ns) = crate::streamer::telemetry::clock_pair();
    tprintln!("[clock] host_now_ns={host_now_ns} unix_ns={unix_ns}");
    let mut encoder = Encoder::new(enc.clone()).context("creating VideoToolbox encoder")?;
    // A session on an encoder, tier and size the drop guard already moved off in this process
    // starts on the fallback. The low-latency session is released before the standard one is
    // asked for, for the same reason `reset_encoder` does it in that order.
    let mut standard_only = false;
    let key = fallback_key(&encoder, &enc);
    if drop_guard_fallbacks().lock_recover().contains(&key) {
        tprintln!(
            "[vt] drop guard: {} {} at {}x{} fell back earlier in this process; starting this \
             session on the standard encoder (StandardBitrate) instead of re-learning it",
            key.0,
            key.1,
            key.2,
            key.3
        );
        drop(encoder);
        encoder = Encoder::new_standard(enc.clone())
            .context("creating the remembered standard-encoder fallback")?;
        standard_only = true;
    }
    // The tier that actually started, not the one that was asked for: a session that fell back to
    // the software encoder reports "videotoolbox-software" here and in the compatibility report.
    note_encoder(&telemetry, &encoder);

    let stop_thread = Arc::clone(&stop);
    let encode_thread = std::thread::Builder::new()
        .name("videotoolbox-encode".to_string())
        .spawn(move || {
            run_encode_loop(EncodeLoop {
                encoder,
                cfg: enc,
                source,
                tx,
                idr_request,
                target_bitrate,
                stop: stop_thread,
                frame_duration,
                telemetry,
                standard_only,
            })
        })
        .context("spawn encode thread")?;

    Ok((pipeline, stop, encode_thread))
}

struct EncodeLoop {
    encoder: Encoder,
    cfg: EncoderConfig,
    source: FrameSource,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    frame_duration: Duration,
    telemetry: SharedTelemetry,
    /// The session starts on the drop guard's standard-encoder fallback (remembered).
    standard_only: bool,
}

/// Turn compressed samples into broadcast frames.
///
/// Its own thread because `to_annexb` walks the whole access unit and the encode loop is on the
/// deadline scheduler; the two share nothing but the encoder's output channel, which is the point
/// — the shared `Mutex<VecDeque>` that used to carry per-frame timestamps between them is gone,
/// the stamps now ride with their own frame through VideoToolbox.
fn spawn_drain(
    output: crossbeam_channel::Receiver<super::encoder::CompressedSample>,
    tx: broadcast::Sender<EncodedFrame>,
    telemetry: SharedTelemetry,
    frame_duration: Duration,
    newest_real_capture: Arc<AtomicU64>,
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("videotoolbox-drain".to_string())
        .spawn(move || {
            tune_drain_thread(frame_duration);
            // One buffer for the life of the thread. `to_annexb` clears it first, so the only
            // per-frame allocation left is the `Bytes` the transport takes ownership of.
            let mut au: Vec<u8> = Vec::with_capacity(256 * 1024);
            let mut real_encode = RealEncodeTime::default();
            while let Ok(mut sample) = output.recv() {
                let (mut capture, mut timing, hold_push) = match sample.tag.take() {
                    Some(t) => (t.capture, t.timing, t.hold_push),
                    None => (Instant::now(), FrameTiming::entering_capture(), false),
                };
                let emit = sample.callback_at;
                let emit_ns = crate::streamer::audio::host_instant_to_ns(emit);
                if timing.repeated {
                    // A re-submitted picture: stamped as if it had been captured one real
                    // encode time before it came out, but never at or after a real frame
                    // submitted after it (see `repeat_capture_ns`).
                    let submit_ns = crate::streamer::audio::host_instant_to_ns(capture);
                    let stamp_ns = repeat_capture_ns(
                        submit_ns,
                        emit_ns,
                        real_encode.get().map(|d| d.as_nanos() as u64),
                        frame_duration.as_nanos() as u64,
                        newest_real_capture.load(Ordering::Relaxed),
                    );
                    capture = if stamp_ns <= emit_ns {
                        emit.checked_sub(Duration::from_nanos(emit_ns - stamp_ns))
                            .unwrap_or(capture)
                    } else {
                        emit + Duration::from_nanos(stamp_ns - emit_ns)
                    };
                } else if timing.encode_submit_ns != 0 {
                    real_encode.note(emit_ns.saturating_sub(timing.encode_submit_ns));
                }
                timing.mark_encode_done();
                sample.to_annexb(&mut au);
                if au.is_empty() {
                    continue;
                }
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                telemetry
                    .bytes_encoded
                    .fetch_add(au.len() as u64, Ordering::Relaxed);
                let n = telemetry.frame_size_seq.load(Ordering::Relaxed) + 1;
                telemetry.frame_sizes[(n % 16) as usize].store(
                    (au.len() as u64) << 1 | u64::from(timing.repeated || hold_push),
                    Ordering::Relaxed,
                );
                telemetry.frame_size_seq.store(n, Ordering::Release);
                if hold_push {
                    // What pushes cost: on `gva` ~21 KB each (it refines the repeated picture and
                    // has no QP control), on the M1 a near-empty P frame.
                    telemetry
                        .hold_push_bytes
                        .fetch_add(au.len() as u64, Ordering::Relaxed);
                }
                let _ = tx.send(EncodedFrame {
                    data: Bytes::copy_from_slice(&au),
                    capture,
                    timing,
                });
            }
        })
        .inspect_err(|e| teprintln!("[vt] could not spawn the encode-drain thread: {e}"))
        .ok()
}

/// The capture instant (host ns) a re-submitted picture (keepalive repeat, forced-keyframe
/// repeat, hold push) carries, which the transport turns into its RTP timestamp and the client
/// into "host capture → paint": `max(own submit, emit − E)`, `E` this session's recent real
/// encode time. Not the original frame's capture: a still screen's repeats then read seconds old
/// on the client, whose A/V sync followed them to its 400 ms audio-target cap. Not the submit
/// alone either: on a one-frame-hold encoder a keepalive comes out ~200 ms after its submit (when
/// the next one goes in), and would read 200 ms late. With no real frame timed yet, `E` is this
/// picture's own submit→emit, i.e. the submit instant.
///
/// And it yields to real frames: when a captured frame was submitted after this picture
/// (`newest_real_ns` later than its own submit — on a holding encoder in immediate mode that frame
/// is what released this push), the stamp is capped [`REPEAT_STAMP_MARGIN_NS`] before that frame's
/// capture. Otherwise `emit − E` lands a few µs after it and the RTP writer's strictly-increasing
/// clamp had to nudge the *real* frame forward (171 of ~1200 outputs in a simulated-holder run).
/// Pure.
fn repeat_capture_ns(
    submit_ns: u64,
    emit_ns: u64,
    real_encode_ns: Option<u64>,
    frame_interval_ns: u64,
    newest_real_ns: u64,
) -> u64 {
    // An encoder that sustains the frame rate cannot take longer than one frame interval per
    // frame. In a session that starts on a still screen the only real frames are the slow first
    // ones (E ~54 ms on a holding encoder), which made its keepalives read 71 ms instead of ~25.
    let stamp = match real_encode_ns.map(|e| e.min(frame_interval_ns.max(1))) {
        Some(e) => emit_ns.saturating_sub(e).max(submit_ns),
        None => submit_ns,
    };
    if newest_real_ns > submit_ns {
        stamp.min(newest_real_ns.saturating_sub(REPEAT_STAMP_MARGIN_NS))
    } else {
        stamp
    }
}

/// More than one 90 kHz RTP tick (11.1 µs), so a capped repeat's timestamp stays strictly below
/// the real frame's.
const REPEAT_STAMP_MARGIN_NS: u64 = 12_000;

/// This session's real-frame encode time (submit → emit of captured frames only): the minimum of
/// the last [`RealEncodeTime::WINDOW`], so a ~2 MB first keyframe does not sit in it and the
/// estimate follows a faster encoder at once.
#[derive(Default)]
struct RealEncodeTime {
    ns: [u64; RealEncodeTime::WINDOW],
    len: usize,
    next: usize,
}

impl RealEncodeTime {
    const WINDOW: usize = 8;

    fn note(&mut self, ns: u64) {
        self.ns[self.next] = ns;
        self.next = (self.next + 1) % Self::WINDOW;
        self.len = (self.len + 1).min(Self::WINDOW);
    }

    fn get(&self) -> Option<Duration> {
        self.ns[..self.len]
            .iter()
            .min()
            .map(|&n| Duration::from_nanos(n))
    }
}

/// The drain thread's scheduling: the shipped QoS + precedence, each switchable off, or the
/// `drain_time_constraint` candidate in their place. Every outcome is recorded in `tune`.
fn tune_drain_thread(frame_duration: Duration) {
    use super::tune;
    if tune::enabled("drain_time_constraint") {
        let period = frame_duration.as_nanos() as u64;
        // The drain's work is a memcpy and a channel send: a quarter of the period is a generous
        // reservation, and the constraint is the period itself.
        let rc = super::qos::pin_current_thread_time_constraint_with(period, period / 4, period);
        tune::record(
            "drain_time_constraint",
            rc == 0,
            serde_json::json!({ "rc": rc, "period_ns": period, "computation_ns": period / 4 }),
        );
    } else {
        tune::record_skipped("drain_time_constraint");
        if tune::enabled("drain_qos") {
            tune::record_rc(
                "drain_qos",
                super::qos::pin_current_thread_user_interactive(),
            );
        } else {
            tune::record_skipped("drain_qos");
        }
        if tune::enabled("drain_precedence") {
            tune::record_rc(
                "drain_precedence",
                super::qos::raise_current_thread_precedence(),
            );
        } else {
            tune::record_skipped("drain_precedence");
        }
    }
    if tune::enabled("encode_affinity") {
        tune::record_rc(
            "encode_affinity",
            super::qos::pin_current_thread_encode_affinity(),
        );
    } else {
        tune::record_skipped("encode_affinity");
    }
}

/// The encode thread's scheduling: the deadline scheduler (shipped parameters, or the
/// `encode_tc_short` candidate's), and the affinity tag. Every outcome is recorded in `tune`.
fn tune_encode_thread(frame_duration: Duration) {
    use super::tune;
    let period = frame_duration.as_nanos() as u64;
    if tune::enabled("encode_time_constraint") {
        let short = tune::enabled("encode_tc_short");
        let rc = if short {
            let computation = (period / 4).min(2_000_000);
            super::qos::pin_current_thread_time_constraint_with(period, computation, period / 2)
        } else {
            super::qos::pin_current_thread_time_constraint(period)
        };
        let eff = super::qos::current_time_constraint_ns();
        tune::record(
            "encode_time_constraint",
            rc == 0,
            serde_json::json!({
                "rc": rc,
                "effective_ns": eff.map(|(p, c, k, pre)| serde_json::json!({
                    "period": p, "computation": c, "constraint": k, "preemptible": pre,
                })),
            }),
        );
        if short {
            tune::record("encode_tc_short", rc == 0, serde_json::json!({ "rc": rc }));
        } else {
            tune::record_skipped("encode_tc_short");
        }
    } else {
        tune::record_skipped("encode_time_constraint");
        tune::record_skipped("encode_tc_short");
    }
    if tune::enabled("encode_affinity") {
        tune::record_rc(
            "encode_affinity",
            super::qos::pin_current_thread_encode_affinity(),
        );
    } else {
        tune::record_skipped("encode_affinity");
    }
}

/// Publish which encoder and rate tier this session is on.
fn note_encoder(telemetry: &SharedTelemetry, encoder: &Encoder) {
    telemetry.encoder.set(encoder.backend_name().into());
    telemetry
        .encoder_id
        .set(encoder.encoder_id_short().to_string());
    telemetry
        .rate_tier
        .set(encoder.rate_tier_name().to_string());
}

/// What a replacement encoder's drain thread is built from.
struct DrainSetup {
    tx: broadcast::Sender<EncodedFrame>,
    telemetry: SharedTelemetry,
    frame_duration: Duration,
    newest_real_capture: Arc<AtomicU64>,
}

/// Tear the encoder down and stand a new one up.
///
/// The old session is released *before* the replacement is asked for. The hardware encoder is a
/// machine-wide, instance-limited resource, so creating the new one first is exactly how a
/// recoverable error turns into a permanent software fallback. `standard_only` builds it without
/// the low-latency rung (the drop guard's fallback, kept for every later reset of the session).
fn reset_encoder(
    encoder: &mut Option<Encoder>,
    drain: &mut Option<std::thread::JoinHandle<()>>,
    cfg: EncoderConfig,
    standard_only: bool,
    d: &DrainSetup,
) -> bool {
    let (tx, telemetry) = (&d.tx, &d.telemetry);
    *encoder = None;
    if let Some(handle) = drain.take() {
        let _ = handle.join();
    }
    let made = if standard_only {
        Encoder::new_standard(cfg)
    } else {
        Encoder::new(cfg)
    };
    match made {
        Ok(e) => {
            note_encoder(telemetry, &e);
            let Some(handle) = spawn_drain(
                e.output(),
                tx.clone(),
                Arc::clone(telemetry),
                d.frame_duration,
                Arc::clone(&d.newest_real_capture),
            ) else {
                return false;
            };
            *drain = Some(handle);
            *encoder = Some(e);
            true
        }
        Err(e) => {
            teprintln!("[vt] the encoder could not be recreated: {e:?}");
            false
        }
    }
}

fn run_encode_loop(l: EncodeLoop) {
    let EncodeLoop {
        encoder,
        cfg,
        source,
        tx,
        idr_request,
        target_bitrate,
        stop,
        frame_duration,
        telemetry,
        standard_only,
    } = l;

    // One scheduling scheme per thread. This one is a deadline thread and deliberately takes no
    // QoS class: a QoS class is how the kernel *manages* a thread's priority and a time-constraint
    // reservation is how that management is taken away, so setting both only means the last
    // writer wins and nothing in the log says which.
    tune_encode_thread(frame_duration);
    // The foreground task role while streaming, re-asserted about once a second below.
    super::tune::assert_task_role();
    let mut role_asserted_at = Instant::now();
    let mut last_client_rx_ns = 0u64;
    let _perf_hold = super::perf_hold::begin();
    let _keep_awake = if super::tune::enabled("keep_awake") {
        Some(super::power::KeepAwake::begin())
    } else {
        super::tune::record_skipped("keep_awake");
        None
    };

    // The capture instant (host ns) of the newest captured frame submitted, for the drain's
    // stamping of re-submitted pictures (`repeat_capture_ns`).
    let newest_real_capture = Arc::new(AtomicU64::new(0));
    let mut drain = spawn_drain(
        encoder.output(),
        tx.clone(),
        Arc::clone(&telemetry),
        frame_duration,
        Arc::clone(&newest_real_capture),
    );
    if drain.is_none() {
        return;
    }
    let drain_setup = DrainSetup {
        tx: tx.clone(),
        telemetry: Arc::clone(&telemetry),
        frame_duration,
        newest_real_capture: Arc::clone(&newest_real_capture),
    };
    let mut guard_active = encoder.rate_tier().is_some_and(|t| t.drop_guarded());
    let mut standard_only = standard_only;
    let mut drop_guard = DropGuard::new();
    let mut submitted_since_create = 0u64;
    let mut last_forced_drops = 0u64;
    let mut idr_rearms = 0u32;
    let mut rearm_log = RateLimitedLog::new();

    // Hold push (see `HoldPush`). Starts in the mode this encoder settled on earlier in the
    // process, if any.
    let hold_enabled = super::tune::enabled("hold_push");
    let mut hold_key = (
        encoder.encoder_id_short().to_string(),
        cfg.width,
        cfg.height,
    );
    let start_immediate = hold_enabled && hold_push_start_immediate();
    let initial_mode = if start_immediate {
        HoldMode::Immediate
    } else if hold_enabled {
        learned_hold_mode(&hold_key)
    } else {
        HoldMode::Probing
    };
    let mut hold = HoldPush::new(initial_mode);
    hold.set_confirmed(hold_enabled && is_confirmed_holding(&hold_key));
    if hold_enabled {
        seed_hold(&mut hold, &hold_key);
    }
    let mut hold_seen_emitted = 0u64;
    let mut hold_pushes = 0u64;
    let mut hold_transitions = 0u64;
    if hold_enabled {
        telemetry.hold_push_mode.set(initial_mode.as_str().into());
        if start_immediate {
            teprintln!(
                "[vt] !!!!! hold push starts in IMMEDIATE mode by test hook \
                 (SE_HOLD_PUSH_START=immediate / SE_TUNE_ON=hold_push_start_immediate); the normal \
                 exit rules still apply"
            );
        } else if initial_mode == HoldMode::Immediate {
            tprintln!(
                "[vt] hold push: starting in immediate mode, learned earlier in this process for \
                 {} {}x{}",
                hold_key.0,
                hold_key.1,
                hold_key.2
            );
        }
        super::tune::record(
            "hold_push",
            true,
            serde_json::json!({ "initial_mode": initial_mode.as_str() }),
        );
    } else {
        super::tune::record_skipped("hold_push");
    }
    let mut qp_gov = new_qp_governor(&encoder, &cfg, &telemetry);
    let mut qp_seen_seq = telemetry.frame_size_seq.load(Ordering::Relaxed);
    let mut encoder = Some(encoder);

    let mut last_frame: Option<Arc<super::frame::Frame>> = None;
    let mut bitrate_warned = false;
    let mut last_emit = Instant::now();
    let started = Instant::now();
    let mut seen_capture = false;
    let mut submit_errors = 0u32;
    let mut resets = 0u32;
    let mut last_output_drops = 0u64;
    let mut last_rate_drops = 0u64;
    let mut backpressure_skips = 0u64;
    let mut last_emitted = 0u64;
    let mut last_output_at = Instant::now();
    let mut max_in_flight_seen = 0usize;
    let mut stall_reported = false;
    let mut hold_exhausted_logged = false;
    let mut fresh_waited = false;
    let mut probing_push_logs = 0u32;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(enc) = encoder.as_mut() else {
            break;
        };

        source.wait(IDLE_POLL);
        if role_asserted_at.elapsed() >= Duration::from_secs(1) {
            // macOS resets an app's task role on activation changes; one mach call a second.
            role_asserted_at = Instant::now();
            super::tune::assert_task_role();
            // The client guard reads the newest STATS frame on the same slow path.
            if hold_enabled {
                if let Some(c) = telemetry.client.get() {
                    if c.received_host_ns != last_client_rx_ns {
                        last_client_rx_ns = c.received_host_ns;
                        if let Some(t) = hold.client_stats(
                            crate::streamer::audio::host_now_ns(),
                            c.arrival_to_paint_p50_us as u64,
                            c.frames_painted as u64,
                            effective_transport_tier(c.version, c.video_path, c.transport_tier),
                        ) {
                            hold_transitions += 1;
                            note_hold_transition(&t, &hold_key, &telemetry, &hold);
                        }
                    }
                }
            }
        }
        let mut force_idr = idr_request.swap(false, Ordering::Relaxed);
        if force_idr {
            // A new request: it gets its own re-arms.
            idr_rearms = 0;
        }

        // A keyframe somebody asked for was dropped by rate control, so it never happened: ask
        // again on this frame — at most `MAX_IDR_REARMS` times per request, because an encoder
        // that drops every forced keyframe would otherwise make every frame an IDR attempt; the
        // client's own PLI asks again later. (An ordinary dropped frame needs nothing; it never
        // became a reference.)
        let forced_drops = enc.forced_keyframe_drops();
        if forced_drops != last_forced_drops {
            last_forced_drops = forced_drops;
            let rearm = idr_rearms < MAX_IDR_REARMS;
            if rearm {
                idr_rearms += 1;
                force_idr = true;
            }
            if let Some(suppressed) = rearm_log.ready(Duration::from_secs(5)) {
                let also = if suppressed > 0 {
                    format!(" ({suppressed} similar line(s) suppressed)")
                } else {
                    String::new()
                };
                if rearm {
                    teprintln!(
                        "[vt] rate control dropped a forced keyframe; re-arming the IDR request \
                         ({idr_rearms} of {MAX_IDR_REARMS}){also}"
                    );
                } else {
                    teprintln!(
                        "[vt] rate control dropped a forced keyframe again; {MAX_IDR_REARMS} \
                         re-arms spent, leaving it to the client's next keyframe request{also}"
                    );
                }
            }
        }

        // A discarded access unit breaks the reference chain for everything after it. The only
        // repair is a fresh IDR, and the drop count is the only place the condition is visible.
        let rate_drops = enc.rate_drops();
        if rate_drops != last_rate_drops {
            telemetry
                .degradation
                .encoder_rate_drops
                .fetch_add(rate_drops - last_rate_drops, Ordering::Relaxed);
            last_rate_drops = rate_drops;
        }
        // The drop guard: a low-latency constant-quality session whose rate controller drops
        // frames is rebuilt, once, on the standard encoder, and never goes back.
        if guard_active {
            let now_ms = started.elapsed().as_millis() as u64;
            if let Some((sub, dropped)) =
                drop_guard.observe(now_ms, submitted_since_create, rate_drops)
            {
                let tier = enc.rate_tier_name();
                let id = enc.encoder_id_short().to_string();
                if tier == "BaseFrameQp" {
                    teprintln!(
                        "[vt] !!!!! UNEXPECTED: the BaseFrameQp tier dropped {dropped} of {sub} \
                         frames in {DROP_GUARD_WINDOW_MS} ms on {id}; per-frame QP was never \
                         measured to drop. Falling back to StandardBitrate."
                    );
                } else {
                    teprintln!(
                        "[vt] drop guard: {tier} on {id} dropped {dropped} of {sub} frames in \
                         {DROP_GUARD_WINDOW_MS} ms; rebuilding this session once on the standard \
                         encoder (StandardBitrate) and forcing an IDR"
                    );
                }
                telemetry
                    .degradation
                    .encoder_tier_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                drop_guard_fallbacks()
                    .lock_recover()
                    .insert(fallback_key(enc, &cfg));
                guard_active = false;
                standard_only = true;
                if !reset_encoder(&mut encoder, &mut drain, cfg.clone(), true, &drain_setup) {
                    teprintln!("[vt] the standard-encoder fallback could not be created; ending");
                    break;
                }
                // The new encoder's counters start at zero.
                last_rate_drops = 0;
                last_output_drops = 0;
                last_forced_drops = 0;
                last_emitted = 0;
                submitted_since_create = 0;
                drop_guard.reset();
                restart_hold(
                    &mut hold,
                    &mut hold_seen_emitted,
                    &mut hold_key,
                    encoder.as_ref(),
                    &telemetry,
                    hold_enabled,
                );
                qp_gov = encoder
                    .as_ref()
                    .and_then(|e| new_qp_governor(e, &cfg, &telemetry))
                    .map(|g| g.carry(qp_gov.as_ref()));
                idr_request.store(true, Ordering::Relaxed);
                continue;
            }
        }

        let drops = enc.output_drops();
        if drops != last_output_drops {
            teprintln!(
                "[vt] the drain thread fell behind; {} encoded frame(s) discarded, forcing an IDR",
                drops - last_output_drops
            );
            last_output_drops = drops;
            force_idr = true;
            telemetry
                .degradation
                .encoder_output_drops
                .fetch_add(1, Ordering::Relaxed);
        }

        if let Some(g) = qp_gov.as_mut() {
            let now = crate::streamer::audio::host_now_ns();
            // The per-frame fast path first, over every frame emitted since the last look.
            let seq = telemetry.frame_size_seq.load(Ordering::Acquire);
            let mut fast = None;
            for k in (seq.saturating_sub(15).max(qp_seen_seq + 1))..=seq {
                let v = telemetry.frame_sizes[(k % 16) as usize].load(Ordering::Relaxed);
                let (b, repeat) = (v >> 1, v & 1 == 1);
                if repeat {
                    continue;
                }
                if let Some(c) = g.frame(now, b) {
                    fast = Some(c);
                }
            }
            qp_seen_seq = seq;
            let change =
                fast.or_else(|| g.observe(now, telemetry.bytes_encoded.load(Ordering::Relaxed)));
            if let Some((q, why)) = change {
                let r =
                    enc.set_frame_opt("BaseFrameQP", Some(super::vt_recipe::Val::Int(q as i64)));
                if r.as_ref().map(|r| r.outcome) != Some(super::vt_recipe::Outcome::Included) {
                    teprintln!("[vt] QP governor: BaseFrameQP {q} not applied: {r:?}");
                }
                let was = telemetry.qp_effective.swap(q as u64, Ordering::Relaxed);
                // Every change is logged: they are rare by construction (a raise waits a window,
                // a step down a full second).
                let what = if was == g.q_user as u64 {
                    "engaged"
                } else if q == g.q_user {
                    "released"
                } else {
                    "moved"
                };
                tprintln!(
                    "[vt] QP governor {what}: {why} ({:.1} MB encoded so far)",
                    telemetry.bytes_encoded.load(Ordering::Relaxed) as f64 / 1e6
                );
                telemetry.qp_ceiling_steps.store(g.steps, Ordering::Relaxed);
            }
            telemetry
                .qp_above_user_ms
                .store(g.above_ns / 1_000_000, Ordering::Relaxed);
        }
        let pending = target_bitrate.swap(0, Ordering::Relaxed);
        if pending != 0 {
            match enc.set_bitrate(pending) {
                Ok(BitrateApplied::Applied) => {}
                Ok(BitrateApplied::IgnoredConstantQp) => {
                    if !bitrate_warned {
                        bitrate_warned = true;
                        teprintln!(
                            "bitrate adaptation is inert: the encoder is in explicit constant-QP \
                             mode (--qp), so congestion control cannot shrink the stream"
                        );
                    }
                }
                Err(e) => {
                    telemetry
                        .degradation
                        .encoder_transient_errors
                        .fetch_add(1, Ordering::Relaxed);
                    teprintln!("set_bitrate failed (target_bps={pending}): {e:?}")
                }
            }
        }

        // Back-pressure the capture side rather than the encoder's own queue. VideoToolbox will
        // accept frames until memory runs out, and the only Apple-provided limit,
        // MaxFrameDelayCount, is documented as blocking the *calling* thread — which is this one.
        let emitted = enc.frames_emitted();
        if emitted != last_emitted {
            last_emitted = emitted;
            last_output_at = Instant::now();
        }
        if hold_enabled {
            while hold_seen_emitted < emitted {
                hold_seen_emitted += 1;
                let at = enc
                    .emit_time_ns(hold_seen_emitted)
                    .unwrap_or_else(crate::streamer::audio::host_now_ns);
                if let Some(t) = hold.emitted(hold_seen_emitted, at) {
                    hold_transitions += 1;
                    note_hold_transition(&t, &hold_key, &telemetry, &hold);
                }
            }
        }
        let in_flight = enc.frames_in_flight();
        if in_flight > max_in_flight_seen {
            max_in_flight_seen = in_flight;
            // The single most informative number on this path, and it used to be invisible: how
            // deep the encoder's own hidden queue gets. Cross-check it against VideoToolbox's own
            // count — a disagreement means the encoder is holding frames for a reason of its own.
            tprintln!(
                "[vt] frames in flight reached {in_flight} (VideoToolbox reports {:?} pending)",
                enc.pending_frames()
            );
        }
        let stalled = last_output_at.elapsed() >= STALL_BYPASS;
        if stalled && !stall_reported {
            stall_reported = true;
            tprintln!(
                "[vt] no output for {} ms with {in_flight} frame(s) in flight; this encoder holds \
                 frames, so the in-flight cap stands aside up to {HARD_MAX_FRAMES_IN_FLIGHT}",
                STALL_BYPASS.as_millis()
            );
        }
        // In immediate hold-push mode the encoder keeps the latest push indefinitely, so that
        // push gets a slot of its own on top of the cap. Steady state at any frame rate on a
        // one-frame-hold encoder: the previous push (released by this frame's submit, encoding),
        // this real frame (released by its push, encoding) and its push (held) — three, of which
        // only two are work. With the plain cap of two the push for a new frame would wait for
        // the previous push to come out, a whole encode time added to every frame; with the
        // extra slot neither a real frame nor its push ever waits on a held one. Probing mode
        // keeps the plain cap: a probing push is at most one per pause, and holds a slot only
        // until the next frame arrives and releases it. The stall bypass is unchanged and still
        // the backstop for an encoder that holds more than one frame.
        let cap = if stalled {
            HARD_MAX_FRAMES_IN_FLIGHT
        } else {
            MAX_FRAMES_IN_FLIGHT
                + if hold_enabled {
                    hold.reserved_slots()
                } else {
                    0
                }
        };
        let at_capacity = in_flight >= cap;
        if at_capacity {
            backpressure_skips += 1;
            if hold_enabled && source.peek_has_frame() {
                fresh_waited = true;
            }
        }
        let fresh = if at_capacity {
            None
        } else {
            source.try_take_latest()
        };

        let mut submitted = None;
        let mut submitted_kind = SubmitKind::Other;
        let submit_ns = crate::streamer::audio::host_now_ns();
        if let Some(frame) = fresh {
            // The wake channel counts tokens; a token left behind by a frame that has already
            // been taken makes the next wait return instantly and the loop spin.
            source.drain_wake();
            if let Some(pixbuf) = frame.pixel_buffer() {
                let mut timing = FrameTiming::entering_capture();
                timing.capture_available_ns = frame.os_time_ns;
                if hold_enabled {
                    let now = crate::streamer::audio::host_now_ns();
                    if let Some(t) = hold.real_cap_wait(std::mem::take(&mut fresh_waited), now) {
                        hold_transitions += 1;
                        note_hold_transition(&t, &hold_key, &telemetry, &hold);
                    }
                }
                newest_real_capture.fetch_max(
                    crate::streamer::audio::host_instant_to_ns(frame.captured_at),
                    Ordering::Relaxed,
                );
                submitted = Some(submit_one(
                    enc,
                    frame.captured_at,
                    timing,
                    pixbuf,
                    force_idr,
                    false,
                ));
                submitted_kind = SubmitKind::Real;
                // Only a captured frame starts or extends the performance hold's linger.
                super::perf_hold::real_frame_submitted();
            }
            seen_capture = true;
            last_frame = Some(frame);
        } else if force_idr || last_emit.elapsed() >= KEEPALIVE {
            if let Some(frame) = last_frame.as_ref() {
                if let Some(pixbuf) = frame.pixel_buffer() {
                    // Re-encoding the frame we already have: the picture did not change, so it
                    // is counted and kept out of the capture-relative histograms.
                    telemetry
                        .degradation
                        .frames_repeated
                        .fetch_add(1, Ordering::Relaxed);
                    // Its own submit instant, not the repeated frame's capture: the transport derives
                    // the RTP timestamp from it, and a still screen's repeat stamped with the
                    // original capture read as a frame captured seconds ago on the client.
                    submitted = Some(submit_one(
                        enc,
                        Instant::now(),
                        FrameTiming::repeating(),
                        pixbuf,
                        force_idr,
                        false,
                    ));
                }
            } else if !seen_capture && started.elapsed() >= FIRST_FRAME_DEADLINE {
                // Nothing has ever been captured. Give the client a decodable picture so its
                // video element starts and the session's silence is visible rather than mute.
                if let Some(pb) = placeholder_frame(cfg.width as usize, cfg.height as usize) {
                    telemetry
                        .degradation
                        .capture_no_first_frame
                        .fetch_add(1, Ordering::Relaxed);
                    submitted = Some(submit_one(
                        enc,
                        Instant::now(),
                        FrameTiming::repeating(),
                        &pb,
                        true,
                        false,
                    ));
                }
            }
        }

        match submitted {
            Some(true) => {
                telemetry.frames_submitted.fetch_add(1, Ordering::Relaxed);
                submitted_since_create += 1;
                if hold_enabled {
                    hold.submitted(submitted_kind, submitted_since_create, submit_ns);
                    if force_idr {
                        hold.keyframe_submitted(submitted_since_create);
                    }
                }
                submit_errors = 0;
                last_emit = Instant::now();
                if force_idr {
                    telemetry
                        .degradation
                        .idrs_sent
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            Some(false) => {
                telemetry
                    .degradation
                    .encoder_transient_errors
                    .fetch_add(1, Ordering::Relaxed);
                // The keyframe request was taken off the flag at the top of this iteration and
                // the frame carrying it was refused, so put it back: a client waiting on a PLI
                // would otherwise stay broken until it thought to ask again.
                if force_idr {
                    idr_request.store(true, Ordering::Relaxed);
                }
                submit_errors += 1;
                // One rejected frame is not a reason to stop streaming forever, which is what
                // breaking out of this loop used to mean: the session stayed up, the transport
                // stayed connected, and the picture simply stopped.
                if submit_errors >= SUBMIT_ERRORS_BEFORE_RESET {
                    resets += 1;
                    telemetry
                        .degradation
                        .encoder_resets
                        .fetch_add(1, Ordering::Relaxed);
                    if resets > MAX_ENCODER_RESETS
                        || !reset_encoder(
                            &mut encoder,
                            &mut drain,
                            cfg.clone(),
                            standard_only,
                            &drain_setup,
                        )
                    {
                        teprintln!(
                            "[vt] giving up after {resets} encoder reset(s); ending the stream so \
                             clients are told rather than left on a frozen picture"
                        );
                        break;
                    }
                    submit_errors = 0;
                    idr_request.store(true, Ordering::Relaxed);
                    // The replacement's counters start at zero; without this the next delta
                    // underflowed.
                    last_rate_drops = 0;
                    last_output_drops = 0;
                    last_forced_drops = 0;
                    last_emitted = 0;
                    submitted_since_create = 0;
                    drop_guard.reset();
                    restart_hold(
                        &mut hold,
                        &mut hold_seen_emitted,
                        &mut hold_key,
                        encoder.as_ref(),
                        &telemetry,
                        hold_enabled,
                    );
                    qp_gov = encoder
                        .as_ref()
                        .and_then(|e| new_qp_governor(e, &cfg, &telemetry))
                        .map(|g| g.carry(qp_gov.as_ref()));
                    guard_active = encoder
                        .as_ref()
                        .and_then(Encoder::rate_tier)
                        .is_some_and(|t| t.drop_guarded());
                }
            }
            None => {}
        }

        // Hold push: in immediate mode right after the real frame went in (this iteration), in
        // probing mode once it has sat alone for the probing wait. Same pixels, repeat stamps,
        // so it is counted and kept out of the capture-relative histograms like a keepalive.
        if hold_enabled {
            if let Some(t) = hold.poll(crate::streamer::audio::host_now_ns()) {
                hold_transitions += 1;
                note_hold_transition(&t, &hold_key, &telemetry, &hold);
            }
        }
        if hold_enabled {
            if let (Some(enc), Some(frame)) = (encoder.as_mut(), last_frame.as_ref()) {
                let now = crate::streamer::audio::host_now_ns();
                match hold.due(now) {
                    Due::Push if enc.frames_in_flight() < cap => {
                        if hold.mode() == HoldMode::Probing && probing_push_logs < 3 {
                            probing_push_logs += 1;
                            if let Some((seq, at)) = hold.newest_real() {
                                let age_ns = now.saturating_sub(at);
                                let why = if age_ns >= hold.push_after_ns() {
                                    format!(
                                        "pause probe: alone >= {} ms",
                                        hold.push_after_ns() / 1_000_000
                                    )
                                } else {
                                    "steady-cadence probe: frames keep coming out only when \
                                     the next one goes in"
                                        .to_string()
                                };
                                tprintln!(
                                    "[vt] hold push (probing): frame {seq} not out {:.1} ms \
                                     after its submit, nothing newer submitted; pushing ({why}; \
                                     first 3 per session logged)",
                                    age_ns as f64 / 1e6
                                );
                            }
                        }
                        if let Some(pixbuf) = frame.pixel_buffer() {
                            // Its own instant (RTP timestamp), never the real frame's: a frame
                            // and its push must not share one, and the push is not late.
                            if submit_one(
                                enc,
                                Instant::now(),
                                FrameTiming::repeating(),
                                pixbuf,
                                false,
                                true,
                            ) {
                                telemetry.frames_submitted.fetch_add(1, Ordering::Relaxed);
                                telemetry
                                    .degradation
                                    .frames_repeated
                                    .fetch_add(1, Ordering::Relaxed);
                                telemetry
                                    .degradation
                                    .frames_hold_pushed
                                    .fetch_add(1, Ordering::Relaxed);
                                submitted_since_create += 1;
                                hold.submitted(SubmitKind::Push, submitted_since_create, now);
                                hold_pushes += 1;
                                last_emit = Instant::now();
                            } else {
                                telemetry
                                    .degradation
                                    .encoder_transient_errors
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Due::Exhausted => {
                        if !hold_exhausted_logged {
                            hold_exhausted_logged = true;
                            tprintln!(
                                "[vt] hold push: a frame on {} is still held after \
                                 {MAX_PUSHES_PER_FRAME} pushes; this encoder holds more than one \
                                 frame, so the keepalive and the stall bypass handle it (logged \
                                 once per session)",
                                hold_key.0
                            );
                        }
                    }
                    Due::Push | Due::Nothing => {}
                }
            }
        }
    }

    if let Some(mut enc) = encoder.take() {
        let _ = enc.flush();
    }
    // Dropping the encoder closes its output channel, which is what lets the drain thread finish.
    if let Some(handle) = drain.take() {
        let _ = handle.join();
    }
    tprintln!(
        "[vt] encode loop finished: max {max_in_flight_seen} frame(s) in flight, \
         {backpressure_skips} capture(s) skipped for back-pressure, {hold_pushes} hold push(es), \
         hold-push mode {}",
        hold.mode().as_str()
    );
    if hold_enabled {
        save_hold_memory(&hold_key, &hold);
        super::tune::record(
            "hold_push",
            true,
            serde_json::json!({
                "no_push_samples": hold.samples,
                "initial_mode": initial_mode.as_str(),
                "final_mode": hold.mode().as_str(),
                "pushes": hold_pushes,
                "transitions": hold_transitions,
                "suspensions": hold.suspensions,
                "push_after_ms": hold.push_after_ns() as f64 / 1e6,
            }),
        );
    }
}

/// The QP governor for a `BaseFrameQp` session at the user's constant QP (`None` on every other
/// tier, or with `SE_QP_CEILING_MBPS=0`). Publishes the ceiling and the QP in force.
fn new_qp_governor(
    enc: &Encoder,
    cfg: &EncoderConfig,
    telemetry: &SharedTelemetry,
) -> Option<QpGovernor> {
    let q = cfg.qp?.clamp(1, 51);
    let ceiling = match qp_ceiling_override() {
        Some(o) => o,
        None => Some(qp_ceiling_bps(cfg.width, cfg.height, cfg.fps)),
    };
    let on = enc.rate_tier() == Some(super::encoder::RateTier::BaseFrameQp);
    let ceiling = ceiling.filter(|_| on);
    telemetry
        .qp_ceiling_bps
        .store(ceiling.unwrap_or(0), Ordering::Relaxed);
    telemetry.qp_user.store(q as u64, Ordering::Relaxed);
    telemetry.qp_effective.store(q as u64, Ordering::Relaxed);
    let c = ceiling?;
    tprintln!(
        "[vt] QP governor armed: QP {q}, ceiling {:.1} Mbps over {} ms",
        c as f64 / 1e6,
        QP_GOV_WINDOW_NS / 1_000_000
    );
    Some(QpGovernor::new(q, c, cfg.fps))
}

/// The encoder was replaced: the hold-push state starts again in probing mode, keyed by the new
/// encoder (whatever was learned stays remembered for the next session).
fn restart_hold(
    hold: &mut HoldPush,
    seen_emitted: &mut u64,
    key: &mut (String, u32, u32),
    encoder: Option<&Encoder>,
    telemetry: &SharedTelemetry,
    enabled: bool,
) {
    if enabled {
        save_hold_memory(key, hold);
    }
    hold.reset();
    *seen_emitted = 0;
    if let Some(e) = encoder {
        key.0 = e.encoder_id_short().to_string();
    }
    hold.set_confirmed(enabled && is_confirmed_holding(key));
    if enabled {
        seed_hold(hold, key);
        telemetry
            .hold_push_mode
            .set(HoldMode::Probing.as_str().into());
    }
}

/// One line per mode change, naming the evidence; the mode goes into the telemetry and is
/// remembered for the next session on this encoder, and the evidence confirms (or withdraws) the
/// encoder as holding for the process.
fn note_hold_transition(
    t: &HoldTransition,
    key: &HoldKey,
    telemetry: &SharedTelemetry,
    hold: &HoldPush,
) {
    save_hold_memory(key, hold);
    tprintln!(
        "[vt] hold push: {} -> {} on {} {}x{}: {}",
        t.from.as_str(),
        t.to.as_str(),
        key.0,
        key.1,
        key.2,
        t.why
    );
    telemetry.hold_push_mode.set(t.to.as_str().into());
    learn_from(t, key);
    if t.to == HoldMode::Suspended {
        // Session-local: the next session on this encoder still starts where it learned, and the
        // guard decides again from its own frames.
        telemetry
            .degradation
            .hold_push_suspensions
            .fetch_add(1, Ordering::Relaxed);
        telemetry.hold_push_guard.set(t.why.clone());
    }
}

/// What a transition teaches the process: only evidence about holding changes the learned mode
/// and the confirmation. A suspension, its expiry and a client that stopped reporting say nothing
/// about the encoder.
fn learn_from(t: &HoldTransition, key: &HoldKey) {
    match t.evidence {
        Evidence::Holding => {
            remember_hold_mode(key.clone(), t.to);
            confirmed_holding().lock_recover().insert(key.clone());
        }
        Evidence::NotHolding => {
            remember_hold_mode(key.clone(), t.to);
            confirmed_holding().lock_recover().remove(key);
        }
        Evidence::Overload => {}
    }
}

/// Refuse to start capture on a definite Screen Recording denial.
///
/// Starting a stream anyway gives the client wallpaper at best, and on macOS 15+ starting capture
/// while unauthorized raises the system's own prompt from an automatic path. Silent: this only
/// reads (`ensure_screen_recording_access` never requests), and an unknown state does not block.
fn require_screen_recording() -> Result<()> {
    if super::ensure_screen_recording_access() {
        return Ok(());
    }
    anyhow::bail!(
        "Screen Recording permission is denied for ScreenExtend, so this display cannot be \
         captured. Re-approve ScreenExtend under System Settings → Privacy & Security → Screen \
         Recording (quit and reopen ScreenExtend if the toggle is already on), then join again."
    )
}

/// A solid black frame, for a session whose capture backend has produced nothing.
fn placeholder_frame(w: usize, h: usize) -> Option<CFRetained<CVPixelBuffer>> {
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    const BGRA: u32 = 0x4247_5241;
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    let r = unsafe {
        CVPixelBufferCreate(
            objc2_core_foundation::kCFAllocatorDefault,
            w.max(2),
            h.max(2),
            BGRA,
            None,
            std::ptr::NonNull::from(&mut out),
        )
    };
    let out = std::ptr::NonNull::new(out)?;
    let pb = unsafe { CFRetained::from_raw(out) };
    if r != 0 {
        return None;
    }
    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
        if !base.is_null() {
            let bpr = CVPixelBufferGetBytesPerRow(&pb);
            std::ptr::write_bytes(base, 0, bpr * h.max(2));
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
    }
    Some(pb)
}

/// The session's telemetry has to exist before capture starts, not after: the capture backend
/// counts what WindowServer drops, and a counter created afterwards would miss the burst that
/// happens while the pipeline is still warming up.
fn new_session_telemetry() -> SharedTelemetry {
    let telemetry = SessionTelemetry::new_shared();
    telemetry.encoder.set("videotoolbox".into());
    telemetry
}

/// Record which capture tier started, and whether reaching it cost a fallback.
fn note_capture_backend(pipeline: &Pipeline, backend: &dyn CaptureBackend) {
    let name = backend.name();
    pipeline.telemetry.capture_backend.set(name.into());
    if name == "cgdisplaystream" && super::screencapturekit_available() {
        pipeline
            .telemetry
            .degradation
            .capture_fallback
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn submit_one(
    encoder: &mut Encoder,
    captured_at: Instant,
    mut timing: FrameTiming,
    pixbuf: &objc2_core_video::CVImageBuffer,
    force_idr: bool,
    hold_push: bool,
) -> bool {
    timing.mark_encode_submit();
    // The raw frame recorder is a bench-only diagnostic and does not exist in a shipped build.
    #[cfg(feature = "bench")]
    super::raw_capture::record(pixbuf, force_idr, &timing);
    match encoder.submit(
        pixbuf,
        force_idr,
        FrameTag {
            capture: captured_at,
            timing,
            hold_push,
        },
    ) {
        Ok(()) => true,
        Err(e) => {
            teprintln!("[vt] encode submit failed: {e:?}");
            false
        }
    }
}

/// Start a pipeline on the primary display. Only the probe harness uses it; the session path
/// is `start_on_monitor`. It returns the same `SessionCapture` its sibling does, because the
/// version that returned a bare `Pipeline` had to `mem::forget` the capture backend and the
/// latency activity to keep them alive — leaking a capture stream, a VideoToolbox session, a
/// real-time thread, a process-wide App Nap suppression and two power assertions per call.
pub fn start(cfg: &Config) -> Result<SessionCapture> {
    require_screen_recording()?;
    let display = select_display(cfg.monitor)?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let mut enc = live_encoder_config(native_w, native_h, refresh, cfg);
    // Ask VideoToolbox for an encoder on the GPU that drives this display, so the captured
    // IOSurface does not have to migrate to another one before it can be read.
    enc.gpu_registry_id = display::metal_registry_id(display);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();

    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let telemetry = new_session_telemetry();
    let activity = super::activity::begin_if_enabled();
    let backend = start_capture(display, gpu, sink, cap_cfg, Arc::clone(&telemetry))
        .context("starting capture backend")?;

    let (pipeline, stop, encode_thread) = spawn_pipeline(enc, source, telemetry)?;
    note_capture_backend(&pipeline, backend.as_ref());
    Ok(SessionCapture {
        pipeline,
        control: Some(backend),
        stop,
        encode_thread: Some(encode_thread),
        _activity: activity,
    })
}

pub fn start_on_monitor(cfg: &Config, device_name: &str) -> Result<SessionCapture> {
    require_screen_recording()?;
    let display =
        display_by_name(device_name).with_context(|| format!("resolving display {device_name}"))?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let mut enc = live_encoder_config(native_w, native_h, refresh, cfg);
    // Ask VideoToolbox for an encoder on the GPU that drives this display, so the captured
    // IOSurface does not have to migrate to another one before it can be read.
    enc.gpu_registry_id = display::metal_registry_id(display);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();

    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let telemetry = new_session_telemetry();
    let activity = super::activity::begin_if_enabled();
    let backend = start_capture(display, gpu, sink, cap_cfg, Arc::clone(&telemetry))
        .context("starting capture backend")?;

    let (pipeline, stop, encode_thread) = spawn_pipeline(enc, source, telemetry)?;
    note_capture_backend(&pipeline, backend.as_ref());
    Ok(SessionCapture {
        pipeline,
        control: Some(backend),
        stop,
        encode_thread: Some(encode_thread),
        _activity: activity,
    })
}

pub fn probe_bitrate(cfg: &Config) -> Result<()> {
    let (w, h) = (1280u32, 720u32);
    let low = 2_000_000u32;
    let high = 8_000_000u32;
    let enc = EncoderConfig {
        width: w,
        height: h,
        fps: 60,
        bitrate_bps: low,
        max_bitrate_bps: high,
        profile: cfg.h264_profile,
        qp: cfg.qp,
        min_qp: cfg.quality_qp,
        intra_refresh: cfg.intra_refresh,
        gpu_registry_id: None,
        software_encode: cfg.disable_gpu_encode,
        recipe: None,
    };
    let mut encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;
    let out = encoder.output();

    let phase = |encoder: &mut Encoder,
                 out: &crossbeam_channel::Receiver<super::encoder::CompressedSample>,
                 tick0: usize|
     -> usize {
        let mut buf: Vec<u8> = Vec::new();
        let mut bytes = 0usize;
        for i in 0..120usize {
            if let Some(pb) = make_synthetic_bgra(w as usize, h as usize, tick0 + i) {
                let _ = encoder.submit(&pb, i == 0, probe_tag());
                if !encoder.low_latency() {
                    let _ = encoder.submit(&pb, false, probe_tag());
                }
            }
            while let Ok(cs) = out.try_recv() {
                cs.to_annexb(&mut buf);
                bytes += buf.len();
            }
        }
        let _ = encoder.flush();
        while let Ok(cs) = out.try_recv() {
            cs.to_annexb(&mut buf);
            bytes += buf.len();
        }
        bytes
    };

    let low_bytes = phase(&mut encoder, &out, 0);
    encoder.set_bitrate(high).context("set_bitrate")?;
    let high_bytes = phase(&mut encoder, &out, 1000);

    let kbps = |bytes: usize| (bytes as f64 * 8.0 / 2.0) / 1000.0;
    tprintln!(
        "[probe-bitrate] {w}x{h}@60: low target={} kbps achieved≈{:.0} kbps; \
         high target={} kbps achieved≈{:.0} kbps",
        low / 1000,
        kbps(low_bytes),
        high / 1000,
        kbps(high_bytes)
    );
    Ok(())
}

pub fn probe_live(cfg: &Config, path: &str) -> Result<()> {
    use std::io::Write;

    let display = select_display(cfg.monitor)?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let mut enc = live_encoder_config(native_w, native_h, refresh, cfg);
    // Ask VideoToolbox for an encoder on the GPU that drives this display, so the captured
    // IOSurface does not have to migrate to another one before it can be read.
    enc.gpu_registry_id = display::metal_registry_id(display);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();
    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let mut backend = start_capture(display, gpu, sink, cap_cfg, new_session_telemetry())
        .context("starting capture backend")?;
    let mut encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;
    let out = encoder.output();

    let mut file = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut frames = 0usize;
    let mut bytes = 0usize;
    let mut first = true;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && frames < 150 {
        source.wait(Duration::from_millis(8));
        if let Some(frame) = source.try_take_latest() {
            if let Some(pixbuf) = frame.pixel_buffer() {
                encoder
                    .submit(pixbuf, first, probe_tag())
                    .context("submit")?;
                first = false;
                if !encoder.low_latency() {
                    let _ = encoder.submit(pixbuf, false, probe_tag());
                }
            }
        }
        while let Ok(cs) = out.try_recv() {
            cs.to_annexb(&mut buf);
            if !buf.is_empty() {
                file.write_all(&buf)?;
                bytes += buf.len();
                frames += 1;
            }
        }
    }
    encoder.flush().ok();
    while let Ok(cs) = out.try_recv() {
        cs.to_annexb(&mut buf);
        if !buf.is_empty() {
            file.write_all(&buf)?;
            bytes += buf.len();
            frames += 1;
        }
    }
    backend.stop();

    tprintln!("[probe-live] wrote {frames} frames, {bytes} bytes to {path}");
    Ok(())
}

/// The probe harness drives the encoder directly, so it still has to hand each frame a tag.
fn probe_tag() -> FrameTag {
    FrameTag {
        capture: Instant::now(),
        hold_push: false,
        timing: FrameTiming::entering_capture(),
    }
}

fn make_synthetic_bgra(w: usize, h: usize, tick: usize) -> Option<CFRetained<CVPixelBuffer>> {
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    const BGRA: u32 = 0x4247_5241; // kCVPixelFormatType_32BGRA
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    let r = unsafe {
        CVPixelBufferCreate(
            objc2_core_foundation::kCFAllocatorDefault,
            w,
            h,
            BGRA,
            None,
            std::ptr::NonNull::from(&mut out),
        )
    };
    let out = std::ptr::NonNull::new(out)?;
    if r != 0 {
        return None;
    }
    let pb = unsafe { CFRetained::from_raw(out) };
    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
        if !base.is_null() {
            let bpr = CVPixelBufferGetBytesPerRow(&pb);
            for y in 0..h {
                let row = base.add(y * bpr);
                for x in 0..w {
                    let p = row.add(x * 4);
                    *p = (x + tick) as u8;
                    *p.add(1) = (y + tick) as u8;
                    *p.add(2) = tick as u8;
                    *p.add(3) = 255;
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
    }
    Some(pb)
}

use crate::macos_utils::virtual_display::reconfigure_display;

const HIDPI_THRESHOLD_PERCENT: u32 = 150;

fn select_display(monitor: u32) -> Result<DisplayId> {
    display::select_display(monitor).context("no active display to capture")
}

fn display_by_name(device_name: &str) -> Result<DisplayId> {
    display::display_by_name(device_name)
        .with_context(|| format!("display {device_name} is not active"))
}

fn display_geometry(display: DisplayId) -> (u32, u32, u32) {
    display::display_geometry(display)
}

pub fn monitor_device_names() -> Vec<String> {
    display::device_names()
}

pub fn monitor_dimensions(device_name: &str) -> Option<(u32, u32)> {
    display::dimensions_by_name(device_name)
}

pub fn monitor_rect(device_name: &str) -> Option<(i32, i32, u32, u32)> {
    let id = display::display_by_name(device_name)?;
    Some(display::display_bounds(id))
}

fn pixel_dims(device_name: &str) -> Result<(u32, u32, u32)> {
    let id = display::display_by_name(device_name)
        .with_context(|| format!("display {device_name} is not active"))?;
    let (w, h, refresh) = display::display_geometry(id);
    if w == 0 || h == 0 {
        anyhow::bail!("display {device_name} reports zero geometry");
    }
    Ok((w, h, refresh))
}

/// Geometry in **backing pixels**, plus refresh and the HiDPI flag. `pixel_dims` reads
/// `CGDisplayPixelsWide`, which reports points, so on a HiDPI display it is half the size that was
/// applied — feeding it back into `reconfigure_display` halves the display again. Displays we
/// created remember what was applied to them; anything else falls back to the point size at 1x.
fn backing_dims(device_name: &str) -> Result<(u32, u32, u32, u32)> {
    if let Some(dims) = crate::macos_utils::virtual_display::applied_backing(device_name) {
        return Ok(dims);
    }
    let (w, h, refresh) = pixel_dims(device_name)?;
    Ok((w, h, refresh, 0))
}

/// Geometry changes must not silently reset the display's HiDPI flag: the join path sets it from
/// the client's DPR, and dropping it here forced a second full reconfiguration to put it back.
fn current_hidpi(device_name: &str) -> u32 {
    crate::macos_utils::virtual_display::applied_backing(device_name)
        .map(|(_, _, _, hidpi)| hidpi)
        .unwrap_or(0)
}

pub fn set_display_resolution(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
) -> Result<()> {
    let hidpi = current_hidpi(device_name);
    reconfigure_display(device_name, width, height, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))
}

/// Returns the refresh rate actually in effect afterwards, which the Windows sibling reads back
/// because an IDD monitor can refuse a rate and land on another. Here `reconfigure_display`
/// either applies the mode it was given or fails, so the answer is the rate that was asked for.
pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<u32> {
    let (w, h) = if portrait {
        (height, width)
    } else {
        (width, height)
    };
    let hidpi = current_hidpi(device_name);
    reconfigure_display(device_name, w, h, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))?;
    Ok(refresh)
}

pub fn set_display_orientation(device_name: &str, portrait: bool) -> Result<()> {
    let (w, h, refresh, hidpi) = backing_dims(device_name)?;
    let is_portrait_now = h > w;
    if is_portrait_now == portrait {
        return Ok(());
    }
    reconfigure_display(device_name, h, w, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))
}

/// Returns the scale actually in effect. macOS HiDPI is a binary flag rather than a ladder, so
/// the answer is only ever 100 or 200 — which is what `platform::snap_display_dpr` already
/// clamps a requested ratio to there.
pub fn set_display_scale(device_name: &str, percent: u32) -> Result<u32> {
    let hidpi = if percent >= HIDPI_THRESHOLD_PERCENT {
        1
    } else {
        0
    };
    crate::macos_utils::virtual_display::set_display_hidpi(device_name, hidpi)
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(if hidpi == 1 { 200 } else { 100 })
}

pub fn set_display_topology_extend() -> bool {
    false
}

// --- the identity-checked display writes -----------------------------------------------------
//
// On Windows a display's `\\.\DISPLAYn` is a slot the OS reuses, so a name cached when a display
// was created can name a different monitor — a physical one included — by the time a reconfigure
// writes to it, and every write there re-derives its target from the monitor's stable device
// path. A `device_name` here is the `CGDirectDisplayID` of the display itself, so it *is* the
// stable identity and the forwarding below is the whole of it: a display that has gone away is
// not found by `display_by_name`, which is already how these fail.
//
// UNVERIFIED (not compiled) — written on Windows, mirroring the Windows surface so
// `streamer/pipeline.rs` re-exports one set of names.

pub fn attached_display_paths() -> Vec<String> {
    display::device_names()
}

pub fn claim_display_identity(device_name: &str, _before: &[String]) -> Option<String> {
    display::display_by_name(device_name).map(|_| device_name.to_string())
}

pub fn forget_display_identity(_device_path: &str) {}

pub fn set_display_mode_owned(
    target: &crate::streamer::session::DisplayTarget,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<u32> {
    set_display_mode(&target.device_name, width, height, refresh, portrait)
}

pub fn set_display_scale_owned(
    target: &crate::streamer::session::DisplayTarget,
    percent: u32,
) -> Result<u32> {
    set_display_scale(&target.device_name, percent)
}

pub fn monitor_dimensions_owned(
    target: &crate::streamer::session::DisplayTarget,
) -> Option<(u32, u32)> {
    monitor_dimensions(&target.device_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::config::H264Profile;
    use std::time::Duration;

    use super::super::cgds::CgDisplayStreamBackend;
    use super::super::config::EncoderConfig;
    use super::super::encoder::Encoder;
    use super::super::{CaptureBackend, CaptureConfig, PIXEL_FORMAT_420F};

    fn enc_cfg(w: u32, h: u32, fps: u32, display: DisplayId) -> EncoderConfig {
        EncoderConfig {
            width: w & !1,
            height: h & !1,
            fps,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 12_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            min_qp: None,
            intra_refresh: false,
            gpu_registry_id: display::metal_registry_id(display),
            software_encode: false,
            recipe: None,
        }
    }

    #[test]
    #[ignore = "requires a console session, Screen Recording and a display that is changing; run with --ignored --nocapture"]
    fn end_to_end_latency() {
        let display = display::select_display(0).expect("main display");
        let (w, h, refresh) = display::display_geometry(display);
        let fps = if refresh == 0 { 60 } else { refresh };
        println!("display {display}: {w}x{h} @ {fps}fps");

        let gpu = Gpu::new().unwrap();
        let (sink, source) = frame_channel();
        let cap_cfg = CaptureConfig {
            width: w as usize,
            height: h as usize,
            fps: fps as i32,
            pixel_format: PIXEL_FORMAT_420F,
        };
        let _cursor = display::CursorAnimation::start(display, w, h);
        let mut backend =
            CgDisplayStreamBackend::new(display, gpu, sink, cap_cfg, new_session_telemetry())
                .unwrap();
        backend.start().expect("capture starts");

        let mut encoder = Encoder::new(enc_cfg(w, h, fps, display)).expect("encoder");
        let out = encoder.output();

        // No side queue of timestamps: each frame's stamps ride with it through VideoToolbox as
        // its `sourceFrameRefCon`, which is also what the production drain thread reads.
        let drain = std::thread::spawn(move || {
            let mut lat: Vec<f64> = Vec::new();
            let mut bytes = 0usize;
            let mut keyframes = 0usize;
            let mut new_frames = 0usize;
            let mut buf: Vec<u8> = Vec::new();
            while let Ok(cs) = out.recv() {
                let tag = cs.tag.as_ref().map(|t| (t.capture, t.timing.repeated));
                let is_keyframe = cs.to_annexb(&mut buf);
                if buf.is_empty() {
                    continue;
                }
                if let Some((captured, repeated)) = tag {
                    if !repeated {
                        lat.push(captured.elapsed().as_secs_f64() * 1000.0);
                        new_frames += 1;
                    }
                }
                bytes += buf.len();
                if is_keyframe {
                    keyframes += 1;
                }
            }
            (lat, bytes, keyframes, new_frames)
        });

        let idle = Duration::from_millis(1);
        let keepalive_budget = 4u32;
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut first = true;
        let mut submitted = 0usize;
        let mut last_frame: Option<Arc<super::super::frame::Frame>> = None;
        let mut budget = 0u32;
        while Instant::now() < deadline {
            source.wait(idle);
            if let Some(frame) = source.try_take_latest() {
                source.drain_wake();
                if let Some(pixbuf) = frame.pixel_buffer() {
                    let mut timing = FrameTiming::entering_capture();
                    timing.capture_available_ns = frame.os_time_ns;
                    encoder
                        .submit(
                            pixbuf,
                            first,
                            FrameTag {
                                capture: frame.captured_at,
                                hold_push: false,
                                timing,
                            },
                        )
                        .expect("submit");
                    first = false;
                    submitted += 1;
                    last_frame = Some(frame);
                    budget = keepalive_budget; // refill the flush burst
                }
            } else if budget > 0 {
                if let Some(lf) = last_frame.as_ref() {
                    if let Some(pixbuf) = lf.pixel_buffer() {
                        encoder
                            .submit(
                                pixbuf,
                                false,
                                FrameTag {
                                    capture: lf.captured_at,
                                    hold_push: false,
                                    timing: FrameTiming::repeating(),
                                },
                            )
                            .ok();
                    }
                    budget -= 1;
                }
            }
        }
        encoder.flush().ok();
        let captured = backend.frames_captured();
        let cap_drops = backend.frames_dropped();
        let output_drops = encoder.output_drops();
        let pending_at_flush = encoder.pending_frames();
        backend.stop();
        drop(encoder);
        let (mut lat, bytes, keyframes, new_frames) = drain.join().unwrap();

        println!(
            "captured={captured} windowserver_drops={cap_drops} submitted={submitted} \
             new_frames={new_frames} keyframes={keyframes} bytes={bytes} \
             output_drops={output_drops} vt_pending_at_flush={pending_at_flush:?}"
        );
        assert!(
            submitted > 0,
            "no frames captured (idle screen? add activity)"
        );
        assert!(!lat.is_empty(), "no encoded frames");
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = lat.iter().sum::<f64>() / lat.len() as f64;
        let p50 = lat[lat.len() / 2];
        let p99 = lat[((lat.len() as f64 * 0.99) as usize).min(lat.len() - 1)];
        println!(
            "capture-to-encoded latency ms (new frames): mean={mean:.3} p50={p50:.3} p99={p99:.3} max={:.3}",
            lat.last().unwrap()
        );
    }

    #[test]
    #[ignore = "requires a console session and Screen Recording; run with --ignored --nocapture"]
    fn production_pipeline_smoke() {
        let display = display::select_display(0).expect("main display");
        let (w, h, refresh) = display::display_geometry(display);
        let fps = if refresh == 0 { 60 } else { refresh };

        let gpu = Gpu::new().unwrap();
        let (sink, source) = frame_channel();
        let cap_cfg = CaptureConfig {
            width: w as usize,
            height: h as usize,
            fps: fps as i32,
            pixel_format: PIXEL_FORMAT_420F,
        };
        let _cursor = display::CursorAnimation::start(display, w, h);
        let mut backend =
            CgDisplayStreamBackend::new(display, gpu, sink, cap_cfg, new_session_telemetry())
                .unwrap();
        backend.start().expect("capture starts");

        let telemetry = new_session_telemetry();
        let (pipeline, stop, encode_thread) =
            spawn_pipeline(enc_cfg(w, h, fps, display), source, Arc::clone(&telemetry))
                .expect("pipeline");
        let mut rx = pipeline.tx.subscribe();
        pipeline.request_idr();

        let mut count = 0usize;
        let mut bytes = 0usize;
        let mut lat: Vec<f64> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ef) => {
                    count += 1;
                    bytes += ef.data.len();
                    lat.push(ef.capture.elapsed().as_secs_f64() * 1000.0);
                }
                Err(_) => std::thread::sleep(Duration::from_micros(500)),
            }
        }

        stop.store(true, Ordering::Relaxed);
        backend.stop();
        let _ = encode_thread.join();

        println!("production pipeline: broadcast {count} EncodedFrames, {bytes} bytes");
        for (label, value) in telemetry.degradation.each() {
            if value != 0 {
                println!("  degradation {label} = {value}");
            }
        }
        if !lat.is_empty() {
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = lat.iter().sum::<f64>() / lat.len() as f64;
            println!(
                "submit-to-broadcast latency ms: mean={mean:.3} p50={:.3} max={:.3}",
                lat[lat.len() / 2],
                lat.last().unwrap()
            );
        }
        assert!(count > 0, "production pipeline produced no encoded frames");
    }

    /// A session whose capture backend never delivers still has to paint something.
    #[test]
    fn the_placeholder_frame_has_the_geometry_it_was_asked_for() {
        use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};
        let pb = placeholder_frame(64, 32).expect("a placeholder buffer");
        assert_eq!(unsafe { CVPixelBufferGetWidth(&pb) }, 64);
        assert_eq!(unsafe { CVPixelBufferGetHeight(&pb) }, 32);
    }

    /// Feed the guard a steady stream: `fps` submits a second, `drop_every`-th one dropped
    /// (0 = none), for `secs` seconds from `t0_ms`, sampling every 4 ms like the idle poll.
    fn run_guard(
        g: &mut DropGuard,
        t0_ms: u64,
        secs: u64,
        fps: u64,
        drop_every: u64,
        totals: &mut (u64, u64),
    ) -> Option<(u64, (u64, u64))> {
        let mut next_frame_ms = t0_ms;
        for t in (t0_ms..t0_ms + secs * 1000).step_by(4) {
            while next_frame_ms <= t {
                totals.0 += 1;
                if drop_every != 0 && totals.0.is_multiple_of(drop_every) {
                    totals.1 += 1;
                }
                next_frame_ms += 1000 / fps;
            }
            if let Some(hit) = g.observe(t, totals.0, totals.1) {
                return Some((t, hit));
            }
        }
        None
    }

    #[test]
    fn the_drop_guard_ignores_a_clean_or_lightly_dropping_session() {
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        assert_eq!(run_guard(&mut g, 0, 10, 60, 0, &mut totals), None);
        // 1 in 25 (4 %) is under the 5 % line.
        assert_eq!(run_guard(&mut g, 10_000, 10, 60, 25, &mut totals), None);
        // The stress clip's one drop in 600 under the 300 Mbps ceiling.
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        assert_eq!(run_guard(&mut g, 0, 5, 60, 0, &mut totals), None);
        totals.1 += 1;
        assert_eq!(run_guard(&mut g, 5_000, 5, 60, 0, &mut totals), None);
    }

    #[test]
    fn the_drop_guard_trips_on_heavy_drops_after_one_window() {
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        // Every frame dropped (150 Mbps measured that): trips once a full window exists.
        let (t, (sub, dropped)) = run_guard(&mut g, 0, 10, 60, 1, &mut totals).expect("trips");
        assert!(
            (DROP_GUARD_WINDOW_MS..DROP_GUARD_WINDOW_MS + 300).contains(&t),
            "t={t}"
        );
        assert!(sub >= DROP_GUARD_MIN_SUBMITS && dropped * 100 > sub * DROP_GUARD_PERCENT);
        // 1 in 10 (10 %) trips too.
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        assert!(run_guard(&mut g, 0, 10, 60, 10, &mut totals).is_some());
    }

    #[test]
    fn the_drop_guard_sees_a_burst_inside_any_window_and_needs_enough_frames() {
        // 8 s clean, then a 1.5 s burst of every frame dropped: caught although it starts mid
        // window, because the window slides.
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        assert_eq!(run_guard(&mut g, 0, 8, 60, 0, &mut totals), None);
        assert!(run_guard(&mut g, 8_000, 2, 60, 1, &mut totals).is_some());
        // An idle keepalive (5 fps) dropping everything is too few frames to judge.
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        assert_eq!(run_guard(&mut g, 0, 10, 5, 1, &mut totals), None);
        // After a reset (new encoder, counters from zero) the history is gone.
        let mut g = DropGuard::new();
        let mut totals = (0, 0);
        assert_eq!(run_guard(&mut g, 0, 1, 60, 1, &mut totals), None);
        g.reset();
        assert_eq!(g.observe(1_000, 0, 0), None);
    }

    // -----------------------------------------------------------------------------------------
    // Hold push (pure; simulated encoders).
    // -----------------------------------------------------------------------------------------

    const MS: u64 = 1_000_000;

    /// How a simulated encoder emits frame `k`.
    enum Model {
        /// Emits frame k `enc_ms` after frame k+`n` is submitted (n = 1: the 2012 MacBook Pro's
        /// `h264.gva`).
        Hold { n: usize, enc_ms: u64 },
        /// Emits each frame `lat(k)` after its own submit, in order.
        Free(fn(u64) -> u64),
        /// A one-frame holder whose encodes are SERIALISED at `cost_ms` each (a push delays the
        /// next real frame, as on a GPU that cannot do twice the frame rate): frame k finishes
        /// `cost` after the later of its submit and the previous finish, and comes out at the
        /// later of that and the next submit.
        SerialHold { cost_ms: u64 },
    }

    struct Sim {
        hp: HoldPush,
        models: Vec<(u64, Model)>,
        submits: Vec<(u64, SubmitKind)>,
        emit_at: Vec<u64>,
        pushes_for_real: Vec<u8>,
        real_lat_ms: Vec<(u64, f64)>,
        transitions: Vec<(u64, usize, HoldMode)>,
        exhausted: usize,
        ever_immediate: bool,
        /// SerialHold: when each frame's encode finished.
        done: Vec<u64>,
        /// One STATS frame a second, `(p50 µs, tier)`, from second 1 on.
        stats: Vec<Option<(u64, u8)>>,
    }

    impl Sim {
        fn new(mode: HoldMode, model: Model) -> Self {
            let mut hp = HoldPush::new(mode);
            // These model the encoder only; the client guard has its own tests.
            hp.client_gate = false;
            Sim {
                hp,
                models: vec![(0, model)],
                submits: Vec::new(),
                emit_at: Vec::new(),
                pushes_for_real: Vec::new(),
                real_lat_ms: Vec::new(),
                transitions: Vec::new(),
                exhausted: 0,
                ever_immediate: mode == HoldMode::Immediate,
                done: Vec::new(),
                stats: Vec::new(),
            }
        }

        fn model_at(&self, t: u64) -> &Model {
            &self
                .models
                .iter()
                .rev()
                .find(|(from, _)| *from <= t)
                .unwrap()
                .1
        }

        /// When frame `k` (0-based) comes out, if that is decided yet.
        fn emit_time(&self, k: usize) -> Option<u64> {
            let prev = if k == 0 { 0 } else { self.emit_at[k - 1] };
            let (sub, _) = self.submits[k];
            let t = match self.model_at(sub) {
                Model::Hold { n, enc_ms } => self.submits.get(k + n).map(|s| s.0 + enc_ms * MS)?,
                Model::Free(lat) => sub + lat(k as u64) * MS,
                Model::SerialHold { .. } => {
                    self.submits.get(k + 1).map(|s| s.0.max(self.done[k]))?
                }
            };
            Some(t.max(prev))
        }

        fn in_flight(&self) -> usize {
            self.submits.len() - self.emit_at.len()
        }

        fn cap(&self) -> usize {
            MAX_FRAMES_IN_FLIGHT + self.hp.reserved_slots()
        }

        fn submit(&mut self, kind: SubmitKind, t: u64) {
            if kind == SubmitKind::Push {
                // Never a push of a push, and never a push after another kind of repeat: since
                // the newest real frame only that frame's own pushes were submitted.
                let last_real = self
                    .submits
                    .iter()
                    .rposition(|s| s.1 == SubmitKind::Real)
                    .expect("a push needs a real frame");
                assert!(
                    self.submits[last_real + 1..]
                        .iter()
                        .all(|s| s.1 == SubmitKind::Push),
                    "pushed something other than the newest real frame"
                );
                *self.pushes_for_real.last_mut().unwrap() += 1;
            }
            if kind == SubmitKind::Real {
                self.pushes_for_real.push(0);
            }
            let cost = match self.model_at(t) {
                Model::SerialHold { cost_ms } => cost_ms * MS,
                _ => 0,
            };
            let prev_done = self.done.last().copied().unwrap_or(0);
            self.done.push(t.max(prev_done) + cost);
            self.submits.push((t, kind));
            self.hp.submitted(kind, self.submits.len() as u64, t);
        }

        fn deliver(&mut self, now: u64) {
            while self.emit_at.len() < self.submits.len() {
                let k = self.emit_at.len();
                match self.emit_time(k) {
                    Some(at) if at <= now => {
                        self.emit_at.push(at);
                        if self.submits[k].1 == SubmitKind::Real {
                            self.real_lat_ms
                                .push((self.submits[k].0, (at - self.submits[k].0) as f64 / 1e6));
                        }
                        if let Some(tr) = self.hp.emitted(k as u64 + 1, at) {
                            let reals = self.pushes_for_real.len();
                            self.transitions.push((at, reals, tr.to));
                        }
                    }
                    _ => break,
                }
            }
        }

        /// Run until `end_ms`, real frames at `frames` (ms), a keepalive after 200 ms idle,
        /// stepping 1 ms; the loop order is the encode loop's (take a frame, then push).
        fn run(&mut self, frames: &[u64], end_ms: u64) {
            let mut next = 0;
            let mut pending = false;
            let mut waited = false;
            for t_ms in 0..=end_ms {
                let t = t_ms * MS;
                self.deliver(t);
                if t_ms > 0 && t_ms % 1_000 == 0 {
                    let i = (t_ms / 1_000 - 1) as usize;
                    if let Some((p50, tier)) = self.stats.get(i).copied().flatten() {
                        let n = self.emit_at.len() as u64;
                        if let Some(tr) = self.hp.client_stats(t, p50, n, tier) {
                            self.transitions
                                .push((t, self.pushes_for_real.len(), tr.to));
                        }
                    }
                }
                if let Some(tr) = self.hp.poll(t) {
                    self.transitions
                        .push((t, self.pushes_for_real.len(), tr.to));
                }
                while next < frames.len() && frames[next] <= t_ms {
                    pending = true;
                    next += 1;
                }
                if pending && self.in_flight() < self.cap() {
                    pending = false;
                    if let Some(tr) = self.hp.real_cap_wait(std::mem::take(&mut waited), t) {
                        self.transitions
                            .push((t, self.pushes_for_real.len(), tr.to));
                    }
                    self.submit(SubmitKind::Real, t);
                } else if pending {
                    waited = true;
                } else if !self.submits.is_empty() && t - self.submits.last().unwrap().0 >= 200 * MS
                {
                    self.submit(SubmitKind::Other, t);
                }
                match self.hp.due(t) {
                    Due::Push if self.in_flight() < self.cap() => self.submit(SubmitKind::Push, t),
                    Due::Exhausted => self.exhausted += 1,
                    _ => {}
                }
                self.ever_immediate |= self.hp.mode() == HoldMode::Immediate;
            }
        }

        fn ever_immediate_after(&self, t_ms: u64) -> bool {
            self.transitions
                .iter()
                .any(|t| t.0 >= t_ms * MS && t.2 == HoldMode::Immediate)
        }

        fn pushes(&self) -> usize {
            self.submits
                .iter()
                .filter(|s| s.1 == SubmitKind::Push)
                .count()
        }

        /// Median latency of real frames submitted in `[from_ms, to_ms)`.
        fn p50_ms(&self, from_ms: u64, to_ms: u64) -> f64 {
            let mut v: Vec<f64> = self
                .real_lat_ms
                .iter()
                .filter(|(s, _)| (from_ms * MS..to_ms * MS).contains(s))
                .map(|(_, l)| *l)
                .collect();
            assert!(!v.is_empty(), "no real frames in {from_ms}..{to_ms} ms");
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        }
    }

    fn every(interval_ms: u64, until_ms: u64) -> Vec<u64> {
        (0..until_ms).step_by(interval_ms as usize).collect()
    }

    /// Bursts of `n` frames `interval_ms` apart, separated by `pause_ms` of stillness.
    fn bursts(interval_ms: u64, n: usize, pause_ms: u64, until_ms: u64) -> Vec<u64> {
        let mut v = Vec::new();
        let mut t = 0;
        while t < until_ms {
            for _ in 0..n {
                v.push(t);
                t += interval_ms;
            }
            t += pause_ms;
        }
        v
    }

    #[test]
    fn a_holding_encoder_converges_to_immediate_in_three_frames() {
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 7 });
        sim.run(&every(100, 2_000), 2_100);
        assert_eq!(sim.transitions.len(), 1, "{:?}", sim.transitions);
        let (_, reals, to) = sim.transitions[0];
        assert_eq!(to, HoldMode::Immediate);
        // Frame 1 is the warm-up and never pushed (it comes out when frame 2 goes in); frames
        // 2, 3 and 4 are the three qualifying frames.
        assert_eq!(reals, 4, "entered on the third qualifying frame");
        assert_eq!(sim.hp.mode(), HoldMode::Immediate);
        // Before: every frame waits the 50 ms probing wait plus the encode; after: the encode.
        let before = sim.p50_ms(100, 400);
        let after = sim.p50_ms(500, 2_000);
        assert!((55.0..60.0).contains(&before), "before {before}");
        assert!(after <= 8.0, "after {after}");
        // One push per real frame after the warm-up frame, which gets none.
        assert_eq!(sim.pushes_for_real[0], 0);
        assert!(
            sim.pushes_for_real[1..].iter().all(|&p| p == 1),
            "{:?}",
            sim.pushes_for_real
        );
    }

    #[test]
    fn a_holding_encoder_at_60_fps_learns_from_pauses_and_then_pushes_every_frame() {
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 7 });
        let frames = bursts(16, 30, 300, 6_000);
        sim.run(&frames, 6_200);
        assert_eq!(sim.hp.mode(), HoldMode::Immediate);
        let entered = sim.transitions[0].0 / MS;
        assert!(
            entered < 3 * 800,
            "entered at {entered} ms, after three pauses"
        );
        // During motion before: one frame interval + encode (16 + 7); after: the encode.
        let before = sim.p50_ms(0, 400);
        let after = sim.p50_ms(entered + 100, 6_000);
        assert!((21.0..25.0).contains(&before), "before {before}");
        assert!(after <= 8.0, "after {after}");
        assert!(sim
            .pushes_for_real
            .iter()
            .all(|&p| p <= MAX_PUSHES_PER_FRAME));
    }

    #[test]
    fn a_fast_encoder_that_does_not_hold_is_never_pushed() {
        for frames in [
            every(16, 3_000),
            bursts(16, 20, 400, 5_000),
            every(100, 3_000),
        ] {
            // 7 ms a frame, after an 80 ms warm-up frame (the M1 measured 50.9 ms).
            let mut sim = Sim::new(
                HoldMode::Probing,
                Model::Free(|k| if k == 0 { 80 } else { 7 }),
            );
            sim.run(&frames, 5_500);
            assert_eq!(sim.pushes(), 0);
            assert!(!sim.ever_immediate);
        }
    }

    #[test]
    fn a_slow_encoder_that_does_not_hold_never_enters_immediate() {
        // 25–60 ms per frame (a deterministic scramble), frames every 16 ms in bursts with
        // pauses: the encoder never keeps up during a burst and is often still busy after the
        // probing wait, yet what it emits on its own is what keeps it out of immediate mode.
        fn lat(k: u64) -> u64 {
            25 + (k.wrapping_mul(2_654_435_761) >> 7) % 36
        }
        for frames in [
            every(16, 4_000),
            bursts(16, 12, 250, 8_000),
            bursts(16, 1, 120, 4_000),
        ] {
            let mut sim = Sim::new(HoldMode::Probing, Model::Free(lat));
            sim.run(&frames, 8_500);
            assert!(!sim.ever_immediate, "transitions {:?}", sim.transitions);
            // And its load is never doubled: far fewer pushes than real frames.
            assert!(
                sim.pushes() * 4 <= sim.pushes_for_real.len(),
                "{} pushes for {} frames",
                sim.pushes(),
                sim.pushes_for_real.len()
            );
        }
        // Even a session that starts in (wrongly learned) immediate mode leaves it — into the
        // suspension, by the efficacy guard (real frames far slower than the interval) or by
        // three cap waits after the first-second grace, whichever comes first.
        let mut sim = Sim::new(HoldMode::Immediate, Model::Free(lat));
        sim.run(&every(16, 3_000), 3_100);
        // (A free emission later proves it does not hold at all, which ends in probing.)
        assert_ne!(sim.hp.mode(), HoldMode::Immediate);
        assert_eq!(sim.transitions[0].2, HoldMode::Suspended);
        let left = sim.transitions[0].0 / MS;
        assert!(left < 2_000, "left at {left} ms");
        assert!(!sim.ever_immediate_after(left + 1), "{:?}", sim.transitions);
    }

    #[test]
    fn immediate_mode_is_left_on_evidence_and_after_a_reset() {
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 7 });
        sim.models.push((1_000 * MS, Model::Free(|_| 7)));
        sim.run(&every(100, 2_000), 2_100);
        let modes: Vec<HoldMode> = sim.transitions.iter().map(|t| t.2).collect();
        assert_eq!(modes, vec![HoldMode::Immediate, HoldMode::Probing]);
        let left = sim.transitions[1].0 / MS;
        assert!((1_000..1_100).contains(&left), "left at {left} ms");
        assert_eq!(sim.hp.mode(), HoldMode::Probing);
        // An encoder reset forgets the mode.
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        hp.reset();
        assert_eq!(hp.mode(), HoldMode::Probing);
        assert_eq!(hp.due(10 * MS), Due::Nothing);
    }

    #[test]
    fn a_frame_gets_at_most_two_pushes_and_never_a_push_of_a_push() {
        // Holds three frames: two pushes cannot release it.
        for mode in [HoldMode::Probing, HoldMode::Immediate] {
            let mut sim = Sim::new(mode, Model::Hold { n: 3, enc_ms: 7 });
            sim.run(&every(300, 3_000), 3_100);
            assert!(
                sim.pushes_for_real.iter().all(|&p| p <= 2),
                "{:?}",
                sim.pushes_for_real
            );
            if mode == HoldMode::Immediate {
                // The reserved slot lets the second push in, and then it stops.
                assert!(sim.pushes_for_real.contains(&2));
                assert!(sim.exhausted >= 1);
            } else {
                // Probing keeps the plain cap of two: the real frame and one push (none for the
                // warm-up frame).
                assert!(
                    sim.pushes_for_real[1..].iter().all(|&p| p == 1),
                    "{:?}",
                    sim.pushes_for_real
                );
            }
            // Pushes that do not release the frame are no evidence of a one-frame hold.
            assert!(!sim.transitions.iter().any(|t| t.2 == HoldMode::Immediate));
        }
        // Directly: after a push, `due` keeps naming the real frame, never the push.
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        assert_eq!(hp.due(0), Due::Push);
        hp.submitted(SubmitKind::Push, 2, 0);
        assert_eq!(hp.due(MS), Due::Nothing);
        assert_eq!(hp.due(PUSH_RELEASE_WINDOW_NS), Due::Push);
        hp.submitted(SubmitKind::Push, 3, PUSH_RELEASE_WINDOW_NS);
        assert_eq!(hp.due(PUSH_RELEASE_WINDOW_NS * 2), Due::Exhausted);
        assert_eq!(hp.due(PUSH_RELEASE_WINDOW_NS * 3), Due::Nothing);
        // A keepalive repeat after the real frame supersedes it: nothing to push.
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        hp.submitted(SubmitKind::Other, 2, 0);
        assert_eq!(hp.due(MS), Due::Nothing);
    }

    #[test]
    fn the_probing_wait_grows_with_the_free_emission_latency() {
        let mut hp = HoldPush::new(HoldMode::Probing);
        assert_eq!(hp.push_after_ns(), PUSH_AFTER_FLOOR_NS);
        let mut seq = 0;
        for i in 0..40u64 {
            seq += 1;
            hp.submitted(SubmitKind::Real, seq, i * 200 * MS);
            assert_eq!(hp.emitted(seq, i * 200 * MS + 40 * MS), None);
        }
        let pa = hp.push_after_ns() / MS;
        assert!((115..=120).contains(&pa), "push_after {pa} ms");
    }

    #[test]
    fn the_rate_limited_log_suppresses_and_counts() {
        let mut l = RateLimitedLog::new();
        assert_eq!(l.ready(Duration::from_secs(60)), Some(0));
        assert_eq!(l.ready(Duration::from_secs(60)), None);
        assert_eq!(l.ready(Duration::from_secs(60)), None);
        assert_eq!(l.ready(Duration::ZERO), Some(2));
    }

    #[test]
    fn learned_modes_are_kept_per_encoder_and_size() {
        let k = ("test.hold".to_string(), 1234, 567);
        assert_eq!(learned_hold_mode(&k), HoldMode::Probing);
        remember_hold_mode(k.clone(), HoldMode::Immediate);
        assert_eq!(learned_hold_mode(&k), HoldMode::Immediate);
        assert_eq!(
            learned_hold_mode(&("test.hold".to_string(), 1234, 568)),
            HoldMode::Probing
        );
    }
    #[test]
    fn a_non_holding_encoder_started_in_immediate_leaves_on_the_first_free_emission() {
        // The `SE_HOLD_PUSH_START=immediate` test hook on an M1-like encoder.
        let mut sim = Sim::new(HoldMode::Immediate, Model::Free(|_| 7));
        sim.run(&every(16, 2_000), 2_100);
        assert_eq!(sim.transitions.len(), 1, "{:?}", sim.transitions);
        let (at, reals, to) = sim.transitions[0];
        assert_eq!(to, HoldMode::Probing);
        assert_eq!(
            reals, 1,
            "left on the first frame's push coming out on its own"
        );
        assert!(at <= 16 * MS, "left at {} ms", at / MS);
        assert_eq!(sim.pushes(), 1);
    }

    #[test]
    fn a_holding_encoder_at_a_steady_cadence_learns_from_three_spaced_probes() {
        for interval in [8u64, 16, 33] {
            let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 7 });
            sim.run(&every(interval, 6_000), 6_100);
            assert_eq!(sim.hp.mode(), HoldMode::Immediate, "interval {interval}");
            let entered = sim.transitions[0].0 / MS;
            // 8 suspicious frames, then probes >= 1 s apart: the third lands after ~2 s.
            assert!(
                (2_000..3_000).contains(&entered),
                "interval {interval}: {entered} ms"
            );
            // Exactly the three probe pushes before it.
            let before: usize = sim
                .submits
                .iter()
                .filter(|s| s.1 == SubmitKind::Push && s.0 < entered * MS)
                .count();
            assert_eq!(before, STEADY_PROBES as usize, "interval {interval}");
            let after = sim.p50_ms(entered + 100, 6_000);
            assert!(after <= 8.0, "interval {interval}: after {after}");
            let before_lat = sim.p50_ms(0, entered - 100);
            assert!(
                (before_lat - (interval as f64 + 7.0)).abs() < 1.5,
                "interval {interval}: before {before_lat}"
            );
        }
    }

    #[test]
    fn a_slow_encoder_at_a_steady_cadence_fails_the_probe_and_backs_off() {
        // Just slower than the frame interval (18 ms at 16 ms), and a bimodal one: each frame
        // comes out at its own time whatever is submitted after it.
        fn bimodal(k: u64) -> u64 {
            if k.is_multiple_of(3) {
                12
            } else {
                21
            }
        }
        for model in [
            Model::Free(|_| 18),
            Model::Free(bimodal),
            Model::Free(|_| 30),
        ] {
            let mut sim = Sim::new(HoldMode::Probing, model);
            sim.run(&every(16, 60_000), 60_100);
            assert!(!sim.ever_immediate, "{:?}", sim.transitions);
            // Backoff 2, 4, 8, 16, 32 s: a handful of pushes in a minute.
            assert!(sim.pushes() <= 6, "{} pushes", sim.pushes());
        }
        // A multi-frame holder: the push does not release the frame.
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 3, enc_ms: 7 });
        sim.run(&every(16, 20_000), 20_100);
        assert!(!sim.ever_immediate, "{:?}", sim.transitions);
        assert!(sim.pushes() <= 6, "{} pushes", sim.pushes());
    }

    #[test]
    fn the_steady_probe_is_a_causal_test_with_no_absolute_bound() {
        // Holding encoder at 60 fps (M1-like): 23 ms before, 7 ms with the push.
        assert!(steady_probe_passes(7 * MS, 23 * MS, 16 * MS));
        // `gva` at 30 fps: ~16 ms encode against 33 + 16 before.
        assert!(steady_probe_passes(16 * MS, 49 * MS, 33 * MS));
        // `gva` at 10 fps: 22 ms encode is fine too.
        assert!(steady_probe_passes(22 * MS, 122 * MS, 100 * MS));
        // Slow encoder: the push changes nothing.
        assert!(!steady_probe_passes(18 * MS, 18 * MS, 16 * MS));
        // Faster, but not by the margin (5 ms floor, then 30 %).
        assert!(!steady_probe_passes(10 * MS, 14 * MS, 16 * MS));
        assert!(!steady_probe_passes(30 * MS, 40 * MS, 50 * MS));
        // A clear speed-up that is still not inside one frame interval.
        assert!(!steady_probe_passes(20 * MS, 100 * MS, 16 * MS));
    }

    #[test]
    fn the_release_window_and_the_second_push_scale_with_the_encoder() {
        assert_eq!(release_window_ns(None), 20 * MS);
        assert_eq!(release_window_ns(Some(4.0 * MS as f64)), 20 * MS);
        assert_eq!(release_window_ns(Some(22.0 * MS as f64)), 55 * MS);
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.push_emit_ewma_ns = Some(22.0 * MS as f64);
        assert_eq!(hp.second_push_after_ns(), 55 * MS);
        // The pause window never runs past the probing wait.
        assert_eq!(hp.pause_window_ns(), hp.push_after_ns());
        hp.push_emit_ewma_ns = Some(8.0 * MS as f64);
        assert_eq!(hp.pause_window_ns(), 20 * MS);
    }

    #[test]
    fn a_slow_holding_encoder_is_learned_and_never_pushed_twice() {
        // `gva` at 10 fps: ~22 ms encode after the release. The fixed 20 ms window missed every
        // release and the 20 ms second-push delay pushed 44 % of frames twice.
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 22 });
        sim.run(&every(100, 4_000), 4_100);
        assert_eq!(sim.hp.mode(), HoldMode::Immediate, "{:?}", sim.transitions);
        let (entered_ns, reals, _) = sim.transitions[0];
        let entered = entered_ns / MS;
        assert!(entered < 800, "entered at {entered} ms");
        assert!(
            sim.pushes_for_real[reals..].iter().all(|p| *p <= 1)
                // Every frame pushed once, but for the one no-push sample.
                && sim.pushes_for_real[reals..].iter().filter(|p| **p == 0).count() <= 1,
            "second pushes: {:?}",
            sim.pushes_for_real
        );
        assert!(sim.p50_ms(entered + 200, 4_000) <= 23.0);
        // Started immediate at 10 fps: no second pushes once the first frame has shown the
        // encoder's push->emit time (that first frame, with nothing learned, may get two).
        let mut sim = Sim::new(HoldMode::Immediate, Model::Hold { n: 1, enc_ms: 22 });
        sim.run(&every(100, 3_000), 3_100);
        assert!(
            sim.pushes_for_real[1..].iter().all(|&p| p == 1),
            "{:?}",
            sim.pushes_for_real
        );
        assert!(!sim.transitions.iter().any(|t| t.2 == HoldMode::Probing));
        // `gva` at 30 fps (~16 ms): the steady-cadence learner engages.
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 16 });
        sim.run(&every(33, 6_000), 6_100);
        assert_eq!(sim.hp.mode(), HoldMode::Immediate, "{:?}", sim.transitions);
        let entered = sim.transitions[0].0 / MS;
        assert!(sim.p50_ms(entered + 100, 6_000) <= 17.0);
    }

    #[test]
    fn cap_waits_do_not_count_in_the_first_second_or_under_a_keyframe_and_must_span_150_ms() {
        let wait = |hp: &mut HoldPush, t_ms: u64| hp.real_cap_wait(true, t_ms * MS);
        // First second: never.
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        for t in [100, 200, 300, 400, 900] {
            assert_eq!(wait(&mut hp, t), None);
        }
        // Past it, three waits within 150 ms are not enough; spanning 150 ms they are.
        assert_eq!(wait(&mut hp, 1_100), None);
        assert_eq!(wait(&mut hp, 1_120), None);
        assert_eq!(wait(&mut hp, 1_140), None);
        let t = wait(&mut hp, 1_260).expect("leaves");
        assert_eq!(
            (t.from, t.to, t.evidence),
            (HoldMode::Immediate, HoldMode::Suspended, Evidence::Overload)
        );
        // A forced keyframe in flight: no count until it is out.
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        hp.submitted(SubmitKind::Real, 2, 1_500 * MS);
        hp.keyframe_submitted(2);
        hp.submitted(SubmitKind::Push, 3, 1_500 * MS);
        for t in [1_510, 1_600, 1_700, 1_800] {
            assert_eq!(wait(&mut hp, t), None);
        }
        hp.emitted(1, 1_801 * MS);
        hp.emitted(2, 1_802 * MS);
        for t in [1_810, 1_900] {
            assert_eq!(wait(&mut hp, t), None);
        }
        assert!(wait(&mut hp, 2_000).is_some());
        // A frame that did not wait breaks the streak.
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        assert_eq!(wait(&mut hp, 1_100), None);
        assert_eq!(wait(&mut hp, 1_200), None);
        assert_eq!(hp.real_cap_wait(false, 1_250 * MS), None);
        assert_eq!(wait(&mut hp, 1_300), None);
    }

    #[test]
    fn a_big_first_keyframe_does_not_end_an_immediate_start() {
        // The session-start IDR takes ~50 ms on `gva` (here 60 ms), so the frames behind it
        // wait for the cap; then 11 ms per frame, held until the next submit.
        let mut sim = Sim::new(HoldMode::Immediate, Model::Hold { n: 1, enc_ms: 11 });
        sim.models = vec![
            (0, Model::Hold { n: 1, enc_ms: 60 }),
            (MS, Model::Hold { n: 1, enc_ms: 11 }),
        ];
        sim.run(&every(16, 3_000), 3_100);
        assert!(
            !sim.transitions.iter().any(|t| t.2 == HoldMode::Probing),
            "{:?}",
            sim.transitions
        );
        assert!(sim.p50_ms(500, 3_000) <= 12.0);
    }

    #[test]
    fn a_confirmed_encoder_re_enters_immediate_on_one_probe() {
        // Steady cadence: 8 suspicious frames, then a single probe.
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 7 });
        sim.hp.set_confirmed(true);
        sim.run(&every(16, 2_000), 2_100);
        assert_eq!(sim.hp.mode(), HoldMode::Immediate);
        let entered = sim.transitions[0].0 / MS;
        assert!(entered < 300, "entered at {entered} ms");
        let probes = sim
            .submits
            .iter()
            .filter(|s| s.1 == SubmitKind::Push && s.0 < entered * MS)
            .count();
        assert_eq!(probes, 1);
        // Pause probe: one qualifying frame (frame 2; frame 1 is the warm-up).
        let mut sim = Sim::new(HoldMode::Probing, Model::Hold { n: 1, enc_ms: 7 });
        sim.hp.set_confirmed(true);
        sim.run(&every(100, 1_000), 1_100);
        assert_eq!(sim.transitions[0].1, 2, "{:?}", sim.transitions);
        // A free emission withdraws the confirmation (and says so).
        let mut sim = Sim::new(HoldMode::Probing, Model::Free(|_| 7));
        sim.hp.set_confirmed(true);
        sim.run(&every(16, 500), 600);
        assert_eq!(sim.transitions.len(), 1, "{:?}", sim.transitions);
        assert!(!sim.hp.confirmed);
        assert!(!sim.ever_immediate);
    }

    #[test]
    fn a_repeat_is_stamped_one_real_encode_time_before_it_came_out() {
        const MS: u64 = 1_000_000;
        // 60 fps.
        const IV: u64 = 16_666_667;
        let t0 = 1_000 * MS;
        // A session that started on a still screen: the only real frames were the slow first
        // ones (E 54 ms), but an encoder that keeps up takes at most one interval: emit - 16.7.
        assert_eq!(
            repeat_capture_ns(t0, t0 + 200 * MS, Some(54 * MS), IV, t0 - 50 * MS),
            t0 + 200 * MS - IV
        );
        // No real frame submitted after the repeat in these four (newest real capture older).
        let old_real = t0 - 50 * MS;
        // Free-emitting encoder (M1): emitted 6 ms after its submit, real encodes ~7 ms: its own
        // submit.
        assert_eq!(
            repeat_capture_ns(t0, t0 + 6 * MS, Some(7 * MS), IV, old_real),
            t0
        );
        // Holding encoder, keepalive released by the next keepalive 200 ms later, real encode
        // ~11 ms: emit - 11 (unchanged by the cap: no real frame came after it).
        assert_eq!(
            repeat_capture_ns(t0, t0 + 200 * MS, Some(11 * MS), IV, old_real),
            t0 + 189 * MS
        );
        // No real frame timed yet: its own submit.
        assert_eq!(repeat_capture_ns(t0, t0 + 200 * MS, None, IV, old_real), t0);
        // A push released by the next real frame: that frame captured at t0 + 16.7 ms and
        // submitted a few µs later released the push, which came out 11 ms after that. `emit - E`
        // alone lands after the real frame's capture; capped, it is strictly before it (by more
        // than one RTP tick) and still after the frame the push follows (captured before t0).
        let next_real_capture = t0 + 16_700_000;
        let emit = next_real_capture + 20_000 + 11 * MS;
        let uncapped = emit - 11 * MS;
        assert!(uncapped > next_real_capture);
        let push = repeat_capture_ns(t0, emit, Some(11 * MS), IV, next_real_capture);
        assert!(push < next_real_capture, "{}", next_real_capture - push);
        assert!(next_real_capture - push >= 1_000_000_000 / 90_000);
        assert!(push >= t0);
        // The RTP timestamps then increase without the writer's clamp.
        use crate::streamer::audio::host_ns_to_rtp90k as rtp;
        assert!((rtp(next_real_capture).wrapping_sub(rtp(push)) as i32) > 0);
    }

    #[test]
    fn the_real_encode_time_is_a_short_window_minimum() {
        let mut e = RealEncodeTime::default();
        assert_eq!(e.get(), None);
        // A 50 ms first keyframe, then 11 ms frames: the estimate is 11 ms at once.
        e.note(50_000_000);
        assert_eq!(e.get(), Some(Duration::from_millis(50)));
        e.note(11_000_000);
        assert_eq!(e.get(), Some(Duration::from_millis(11)));
        // A slower spell takes over once the window has turned.
        for _ in 0..RealEncodeTime::WINDOW {
            e.note(16_000_000);
        }
        assert_eq!(e.get(), Some(Duration::from_millis(16)));
    }

    #[test]
    fn a_fast_holding_encoder_stays_immediate_under_the_efficacy_guard() {
        // 5 ms a frame serially: a real frame and its push take 10 ms of a 16 ms interval.
        let mut sim = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 5 });
        sim.run(&every(16, 30_000), 30_100);
        assert!(sim.transitions.is_empty(), "{:?}", sim.transitions);
        assert_eq!(sim.hp.suspensions, 0);
        assert!(sim.p50_ms(1_000, 30_000) <= 6.0);
    }

    /// An immediate-mode `HoldPush` with a 16.7 ms interval and `L0` measured at `l0_ms`.
    fn guard_rig(l0_ms: f64) -> HoldPush {
        let mut hp = HoldPush::new(HoldMode::Immediate);
        for _ in 0..30 {
            hp.guard_gap.push_back(16_666_667);
        }
        hp.note_work((l0_ms * 1e6) as u64 - 16_666_667);
        hp
    }

    /// Feed real frames at 60 fps from `t0` for `secs` with submit→emit `lat(k)` ms. Returns the
    /// transitions' times.
    fn guard_feed(hp: &mut HoldPush, t0: u64, secs: f64, lat: &dyn Fn(u64) -> f64) -> Vec<u64> {
        let mut out = Vec::new();
        let n = (secs * 60.0) as u64;
        for k in 0..n {
            let at = t0 + k * 16_666_667;
            if hp.mode != HoldMode::Immediate {
                break;
            }
            if let Some(t) = hp.guard_emitted(at, (lat(k) * 1e6) as u64) {
                assert_eq!(t.to, HoldMode::Suspended, "{}", t.why);
                out.push(at);
            }
        }
        out
    }

    #[test]
    fn the_guard_keeps_pushes_that_beat_the_no_push_latency() {
        // M1 noise 1080p60: 15 and 19 ms with pushes against 23 ms without — both stay.
        for with in [15.0, 19.0] {
            let mut hp = guard_rig(23.0);
            assert!(
                guard_feed(&mut hp, 0, 30.0, &|_| with).is_empty(),
                "{with} vs 23"
            );
            assert_eq!(hp.mode, HoldMode::Immediate);
        }
        // 25 ms with pushes (the in-process decoder at 1080p60) is a loss: suspended, once the
        // line has held for a second.
        let mut hp = guard_rig(23.0);
        let t = guard_feed(&mut hp, 0, 30.0, &|_| 25.0);
        assert_eq!(t.len(), 1);
        assert!(t[0] >= GUARD_SUSTAIN_NS && t[0] < 2_000_000_000, "{}", t[0]);
        assert_eq!(hp.mode, HoldMode::Suspended);
    }

    #[test]
    fn a_guard_value_near_the_line_does_not_flap() {
        // Oscillating around 0.9 × L0 (20.7 ms): 19.5 / 21.9 alternating, then blocks of each.
        let mut hp = guard_rig(23.0);
        let t = guard_feed(&mut hp, 0, 30.0, &|k| {
            if (k / 7) % 2 == 0 {
                19.5
            } else {
                21.9
            }
        });
        assert!(t.len() <= 1, "{t:?}");
        // Within the dead band (between 0.8 and 0.9 × L0) nothing happens at all.
        let mut hp = guard_rig(23.0);
        assert!(guard_feed(&mut hp, 0, 30.0, &|_| 19.5).is_empty());
    }

    #[test]
    fn a_sparse_window_is_never_judged() {
        // An idle desktop: 3.5 fresh frames a second, every one slow. Never 30 in 2 s: no
        // judgement, mode kept.
        let mut hp = guard_rig(23.0);
        for k in 0..200u64 {
            let at = k * 285_714_286;
            assert!(hp.guard_emitted(at, 40_000_000).is_none());
        }
        assert_eq!(hp.mode, HoldMode::Immediate);
        assert!(hp.guard_stat().is_none());
    }

    #[test]
    fn without_a_work_sample_there_is_no_judgement() {
        let mut hp = guard_rig(23.0);
        hp.work.clear();
        assert!(guard_feed(&mut hp, 0, 5.0, &|_| 60.0).is_empty());
        assert!(hp.guard_stat().is_none());
    }

    #[test]
    fn a_cadence_change_does_not_misjudge_pushes() {
        // `work` 6 ms whatever the cadence. 120 -> 30 fps with pushes at 16.3 ms: no-push there is
        // 39 ms, so it stays (an L0 of 13.2 ms remembered from 120 fps suspended it)...
        let rig = |iv_ms: f64| {
            let mut hp = guard_rig(16.666667 + 6.0);
            hp.guard_gap.clear();
            for _ in 0..GUARD_WINDOW {
                hp.guard_gap.push_back((iv_ms * 1e6) as u64);
            }
            hp
        };
        assert!(guard_feed(&mut rig(33.3), 0, 20.0, &|_| 16.3).is_empty());
        // ...and 30 -> 60 fps with pushes at 25 ms against 22.7: suspended (an L0 of 42.6 ms
        // remembered from 30 fps never would have).
        assert_eq!(guard_feed(&mut rig(16.7), 0, 20.0, &|_| 25.0).len(), 1);
    }

    #[test]
    fn the_sustain_needs_one_unbroken_second() {
        // 0.5 s over, then in the 0.8-0.9 band, then 0.6 s over: never a full second.
        let mut hp = guard_rig(23.0);
        let t = guard_feed(&mut hp, 0, 1.2, &|_| 22.0); // window fills at 0.5 s, 0.7 s over
        assert!(t.is_empty());
        hp.guard_lat.clear();
        assert!(guard_feed(&mut hp, 1_300_000_000, 1.2, &|_| 19.5).is_empty());
        assert_eq!(hp.guard_over_since_ns, None, "below 0.9 clears it");
        hp.guard_lat.clear();
        assert!(guard_feed(&mut hp, 2_600_000_000, 1.2, &|_| 22.0).is_empty());
        assert!(hp.guard_over_since_ns.is_some());
        // A pause: no judgement for longer than the gap starts over too.
        assert!(guard_feed(&mut hp, 6_000_000_000, 1.2, &|_| 22.0).is_empty());
    }

    #[test]
    fn a_second_session_starting_immediate_on_a_saturated_encoder_suspends() {
        let mut one = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 12 });
        one.run(&every(16, 8_000), 8_100);
        assert!(one.hp.suspensions >= 1);
        let m = one.hp.memory(8_000 * MS);
        assert!(!m.work.is_empty());
        // Seeded (a renegotiation): as fast as the window and the sustain allow.
        let mut two = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 12 });
        two.hp.seed(&m, 9_000 * MS);
        two.run(&every(16, 8_000), 8_100);
        let t = two.transitions.first().expect("suspended").0;
        assert!(t < 2_500 * MS, "{} ms", t / MS);
        // The back-off went on from where session one left it.
        assert!(
            two.hp.suspend_backoff_ns >= 4 * SUSPEND_START_NS,
            "{}",
            two.hp.suspend_backoff_ns
        );
        // Unseeded: it samples its own.
        let mut three = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 12 });
        three.run(&every(16, 8_000), 8_100);
        assert!(three.hp.suspensions >= 1);
    }

    #[test]
    fn sampling_leaves_at_most_one_frame_in_five_seconds_unpushed() {
        let mut sim = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 5 });
        sim.run(&every(16, 60_000), 60_100);
        let unpushed = sim.pushes_for_real.iter().filter(|p| **p == 0).count();
        assert!((10..=13).contains(&unpushed), "{unpushed} in 60 s");
        assert_eq!(sim.hp.samples as usize, unpushed);
        assert!(sim.transitions.is_empty(), "{:?}", sim.transitions);
    }

    #[test]
    fn a_suspension_or_a_silent_client_does_not_unlearn_the_mode() {
        let k = ("test.learn".to_string(), 111, 222);
        let t = |from, to, evidence| HoldTransition {
            from,
            to,
            evidence,
            why: String::new(),
        };
        learn_from(
            &t(HoldMode::Probing, HoldMode::Immediate, Evidence::Holding),
            &k,
        );
        for (from, to) in [
            (HoldMode::Immediate, HoldMode::Suspended),
            (HoldMode::Suspended, HoldMode::Probing),
            (HoldMode::Immediate, HoldMode::Probing),
        ] {
            learn_from(&t(from, to, Evidence::Overload), &k);
            assert_eq!(learned_hold_mode(&k), HoldMode::Immediate);
            assert!(is_confirmed_holding(&k));
        }
        learn_from(
            &t(HoldMode::Immediate, HoldMode::Probing, Evidence::NotHolding),
            &k,
        );
        assert_eq!(learned_hold_mode(&k), HoldMode::Probing);
        assert!(!is_confirmed_holding(&k));
    }

    /// A session that went immediate at 2 s with no baseline (W20). `(suspensions, first, why)`.
    fn no_baseline_run(p50_us: u64, share: f64, secs: u64) -> (u64, Option<u64>, String) {
        let mut hp = immediate_with_interval(0);
        let mut painted = 120.0f64;
        hp.client_stats(1_000 * MS, 12_800, 12, 1);
        hp.client_stats(2_000 * MS, 12_800, 110, 1);
        hp.mode = HoldMode::Immediate;
        let (mut first, mut why) = (None, String::new());
        for s in 3..secs {
            let pushing = hp.mode() == HoldMode::Immediate;
            let n = if pushing { 120 } else { 60 };
            painted += n as f64 * if pushing { share } else { 1.0 };
            let p = if pushing { p50_us } else { p50_us.min(11_700) };
            let now = s * 1_000 * MS;
            if let Some(t) = hp.client_stats(now, p, painted as u64, 1) {
                if first.is_none() {
                    (first, why) = (Some(s - 2), t.why);
                }
            }
            hp.poll(now);
            if hp.mode() == HoldMode::Probing {
                hp.enter_immediate(now, "a probe passed".into());
            }
        }
        (hp.suspensions, first, why)
    }

    #[test]
    fn an_overloaded_client_without_a_baseline_is_suspended_in_seconds() {
        // W20: arrival->paint 24.8 ms, 0.944 of the frames painted — it queues, it does not drop.
        let (n, first, why) = no_baseline_run(24_800, 0.944, 300);
        assert!((3..=5).contains(&first.expect("suspended")), "{first:?} s");
        assert!(why.starts_with("client:"), "{why}");
        assert!(n <= 6, "flapped: {n} suspensions in five minutes");
        // A software decoder that keeps up: never.
        assert_eq!(no_baseline_run(11_400, 1.0, 300).0, 0);
        // Painting fewer frames than were sent, at a healthy p50, is not a reason: the two counters
        // are read up to a second apart, and bursty content made a client that painted every
        // frame read as 129 of 166.
        assert_eq!(no_baseline_run(9_000, 0.78, 300).0, 0);
    }

    #[test]
    fn which_stats_frames_count_as_evidence() {
        // (version, video path, tier byte): script transform, encodedStreams, `<video>`.
        assert_eq!(effective_transport_tier(3, 1, 1), 1);
        assert_eq!(effective_transport_tier(3, 1, 2), 2);
        assert_eq!(effective_transport_tier(3, 3, 0), 0);
        // A v1 page has no tier byte: WebCodecs counts, `<video>` does not.
        assert_eq!(effective_transport_tier(1, 1, 0), 1);
        assert_eq!(effective_transport_tier(1, 2, 0), 1);
        assert_eq!(effective_transport_tier(1, 3, 0), 0);
    }

    /// A holding encoder at 60 fps with the client gate ON and STATS once a second. `(mode at the
    /// end, was ever immediate, transitions out of immediate)`.
    fn gated_sim(
        start: HoldMode,
        secs: u64,
        stats: &dyn Fn(u64) -> Option<(u64, u8)>,
    ) -> (HoldMode, bool, usize) {
        let mut sim = Sim::new(start, Model::Hold { n: 1, enc_ms: 6 });
        sim.hp.client_gate = true;
        sim.stats = (0..secs).map(stats).collect();
        sim.run(&every(16, secs * 1_000), secs * 1_000);
        let mut was = start;
        let mut left = 0;
        for t in &sim.transitions {
            left += usize::from(was == HoldMode::Immediate && t.2 != HoldMode::Immediate);
            was = t.2;
        }
        (sim.hp.mode(), sim.ever_immediate, left)
    }

    #[test]
    fn a_healthy_client_lets_a_holding_encoder_go_immediate() {
        let healthy = |_| Some((4_000, 1));
        let (mode, entered, left) = gated_sim(HoldMode::Probing, 30, &healthy);
        assert!(
            entered && mode == HoldMode::Immediate && left == 0,
            "{mode:?} {left}"
        );
        // The legacy createEncodedStreams transport is tier 2.
        assert!(gated_sim(HoldMode::Probing, 30, &|_| Some((4_000, 2))).1);
        // No STATS at all: entered only inside the 3 s grace, and not kept.
        assert_ne!(
            gated_sim(HoldMode::Probing, 30, &|_| None).0,
            HoldMode::Immediate
        );
    }

    #[test]
    fn a_zero_p50_in_the_first_second_costs_nothing() {
        let late = |s: u64| Some(if s < 2 { (0, 1) } else { (4_000, 1) });
        // Learned immediate: neither suspended nor left because of it.
        let (mode, _, left) = gated_sim(HoldMode::Immediate, 30, &late);
        assert!(mode == HoldMode::Immediate && left == 0, "{mode:?} {left}");
        // Learning: still gets there.
        let (mode, _, left) = gated_sim(HoldMode::Probing, 30, &late);
        assert!(mode == HoldMode::Immediate && left == 0, "{mode:?} {left}");
    }

    #[test]
    fn a_client_that_cannot_measure_paint_is_no_evidence() {
        for (p50, tier) in [(0u64, 1u8), (4_000, 0)] {
            let mut hp = immediate_with_interval(0);
            let mut painted = 0;
            for s in 1..40u64 {
                painted += 60;
                assert!(hp
                    .client_stats(s * 1_000 * MS, p50, painted, tier)
                    .is_none());
            }
            assert_eq!(hp.client_baseline_us, None);
            assert!(hp.enter_immediate(40_000 * MS, "t".into()).is_none());
            // Started there (learned): out after the grace.
            let mut hp = HoldPush::new(HoldMode::Immediate);
            hp.submitted(SubmitKind::Real, 1, 0);
            hp.client_stats(1_000 * MS, p50, 60, tier);
            hp.client_stats(2_000 * MS, p50, 120, tier);
            assert!(hp.poll(3_100 * MS).is_some());
        }
    }

    #[test]
    fn noise_starting_mid_session_steps_at_once_not_after_a_plus_one() {
        // Busy ordinary content near the ceiling, then noise: the windowed path may trip first
        // with +1/+2, and its cooldown must not hold the fast path back.
        for (w, h, base_mbps, noise_mbps) in [
            (800u32, 600u32, 3.0, 371.0),
            (800, 600, 20.0, 371.0),
            (1920, 1080, 27.0, 1600.0),
        ] {
            let c = qp_ceiling_bps(w, h, 60);
            let mut g = QpGovernor::new(15, c, 60);
            let (mut total, mut sent, mut real_step) = (0u64, 0u64, None);
            for k in 0..420u64 {
                let t = k * 16_666_667;
                let mbps = if k < 300 {
                    base_mbps
                } else {
                    noise_mbps * 2f64.powf(-(g.q() as f64 - 15.0) / 6.0)
                };
                let b = (mbps * 1e6 / 8.0 / 60.0) as u64;
                total += b;
                let before = g.q();
                let ch = g.frame(t, b).or_else(|| g.observe(t, total));
                if k >= 300 && real_step.is_none() {
                    sent += b;
                    if ch.is_some_and(|(q, _)| q >= before + 6) {
                        real_step = Some(k - 300);
                    }
                }
            }
            assert!(
                real_step.is_some_and(|k| k <= 5),
                "{w}x{h} {base_mbps}: {real_step:?}"
            );
            assert!(sent < 15_000_000, "{w}x{h}: {sent}");
        }
    }

    #[test]
    fn big_frames_at_a_low_rate_do_not_step() {
        // 400 KB frames at 10 fps are 32 Mbps, under the 1080p60 ceiling.
        let mut g = QpGovernor::new(15, qp_ceiling_bps(1920, 1080, 60), 60);
        for k in 0..100u64 {
            assert!(g.frame(k * 100_000_000, 400_000).is_none(), "frame {k}");
        }
    }

    #[test]
    fn governor_totals_survive_a_rebuild() {
        let mut g = QpGovernor::new(15, 24_000_000, 60);
        (g.steps, g.above_ns) = (7, 9);
        let n = QpGovernor::new(15, 24_000_000, 60).carry(Some(&g));
        assert_eq!((n.steps, n.above_ns), (7, 9));
    }

    #[test]
    fn a_busy_holding_encoder_that_still_helps_stays_immediate() {
        // 8 ms a frame serially at 60 fps: each push costs the encoder 8 ms of a 16.7 ms
        // interval, but a real frame still comes out well before the next one would release it.
        let mut sim = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 8 });
        sim.run(&every(16, 60_000), 60_100);
        assert!(sim.transitions.is_empty(), "{:?}", sim.transitions);
        assert_eq!(sim.hp.suspensions, 0);
        let p50 = sim.p50_ms(1_000, 60_000);
        assert!(p50 < 0.9 * 24.0, "with pushes p50 {p50} vs no-push ~24");
    }

    #[test]
    fn a_saturated_holding_encoder_is_suspended_and_real_frames_recover() {
        // 12 ms a frame serially: a real frame and its push need 24 ms of every 16 ms interval,
        // so each push delays the next real frame (the Intel Mac's 1080p60 under real load).
        let mut sim = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 12 });
        sim.run(&every(16, 300_000), 300_100);
        let first = sim.transitions.first().expect("suspended");
        assert_eq!(first.2, HoldMode::Suspended);
        assert!(first.0 < 3_000 * MS, "suspended after {} ms", first.0 / MS);
        assert!(sim.hp.guard_stat().is_none() || sim.hp.mode() != HoldMode::Immediate);
        // Real frames go back to the no-push value: released by the next real frame, one
        // interval after their submit.
        let after = sim.p50_ms(3_000, 300_000);
        assert!(after <= 17.0, "after suspension p50 {after}");
        // No flapping over five minutes.
        assert!(sim.transitions.len() <= 6, "{:?}", sim.transitions);
        assert!(sim.hp.suspensions <= 3, "{}", sim.hp.suspensions);
        // And it no longer pushes every frame.
        let late_pushes: usize = sim
            .submits
            .iter()
            .filter(|s| s.1 == SubmitKind::Push && s.0 > 3_000 * MS)
            .count();
        assert!(late_pushes < 300, "{late_pushes} pushes after suspension");
    }

    #[test]
    fn a_suspended_hold_push_still_pushes_the_end_of_a_burst() {
        // Saturated, bursts of motion with pauses: after the suspension the last frame of each
        // burst is still pushed about an interval after it went in, instead of waiting ~200 ms
        // for the keepalive.
        let mut sim = Sim::new(HoldMode::Immediate, Model::SerialHold { cost_ms: 12 });
        let frames = bursts(16, 120, 600, 60_000);
        sim.run(&frames, 60_100);
        let suspended_at = sim
            .transitions
            .iter()
            .find(|t| t.2 == HoldMode::Suspended)
            .expect("suspended")
            .0;
        // The last frame of every burst that started while suspended came out within ~30 ms.
        let burst_ends: Vec<u64> = frames
            .windows(2)
            .filter(|w| w[1] - w[0] > 100)
            .map(|w| w[0])
            .filter(|&t| t * MS > suspended_at && t * MS < suspended_at + SUSPEND_START_NS)
            .collect();
        assert!(!burst_ends.is_empty());
        for end in burst_ends {
            // The newest real frame submitted at or just after the burst's last offer (it may
            // have waited for the cap).
            let lat = sim
                .real_lat_ms
                .iter()
                .rfind(|(s, _)| *s >= end * MS && *s < end * MS + 50 * MS)
                .map(|x| x.1)
                .expect("emitted");
            assert!(lat <= 35.0, "burst end at {end} ms came out after {lat} ms");
        }
    }

    #[test]
    fn the_suspension_backs_off_and_resets_after_a_healthy_stretch() {
        let mut hp = HoldPush::new(HoldMode::Immediate);
        let t = hp.suspend(0, "test".into());
        assert_eq!((t.from, t.to), (HoldMode::Immediate, HoldMode::Suspended));
        assert!(t.why.contains("suspended for 10 s"));
        assert_eq!(hp.poll(9_999 * MS), None);
        let back = hp.poll(10_000 * MS).expect("back to probing");
        assert_eq!(back.to, HoldMode::Probing);
        // Next one is 20 s, then 40, ... to the 120 s cap.
        hp.mode = HoldMode::Immediate;
        assert!(hp.suspend(20_000 * MS, "t".into()).why.contains("20 s"));
        for _ in 0..5 {
            hp.mode = HoldMode::Immediate;
            hp.suspend(0, "t".into());
        }
        assert_eq!(hp.suspend_backoff_ns, SUSPEND_MAX_NS);
        // Five healthy minutes in immediate mode reset it.
        hp.mode = HoldMode::Immediate;
        hp.immediate_since_ns = None;
        hp.guard_emitted(1_000 * MS, MS);
        hp.guard_emitted(1_000 * MS + SUSPEND_HEALTHY_RESET_NS, MS);
        assert_eq!(hp.suspend_backoff_ns, SUSPEND_START_NS);
    }

    /// Feed a governor frames at `fps` for `secs` from `t0_ms`, each frame's size from `bytes(q)`
    /// (content whose bits depend on the QP in force). Returns the QP trace (ms, q) on changes.
    fn drive_governor(
        g: &mut QpGovernor,
        total: &mut u64,
        t0_ms: u64,
        secs: u64,
        fps: u64,
        bytes: &dyn Fn(u8, u64) -> u64,
    ) -> Vec<(u64, u8)> {
        let mut trace = Vec::new();
        let frames = secs * fps;
        for k in 0..frames {
            let t = t0_ms * MS + k * 1_000_000_000 / fps;
            *total += bytes(g.q(), k);
            if let Some((q, _)) = g.observe(t, *total) {
                trace.push((t / MS, q));
            }
        }
        trace
    }

    #[test]
    fn the_qp_ceiling_is_four_times_the_adaptive_default_within_bounds() {
        assert_eq!(qp_ceiling_bps(1920, 1080, 60), 49_766_400);
        assert_eq!(qp_ceiling_bps(800, 600, 60), 24_000_000);
        assert_eq!(qp_ceiling_bps(3840, 2160, 60), 160_000_000);
        assert_eq!(qp_ceiling_from(None), None);
        assert_eq!(qp_ceiling_from(Some("0")), Some(None));
        assert_eq!(qp_ceiling_from(Some("100")), Some(Some(100_000_000)));
        assert_eq!(qp_ceiling_from(Some("x")), None);
    }

    #[test]
    fn the_qp_governor_never_touches_ordinary_content() {
        // 1080p60 at QP 15: ~10 Mbps average with a 27 Mbps worst second and a 2 MB keyframe.
        let c = qp_ceiling_bps(1920, 1080, 60);
        let mut g = QpGovernor::new(15, c, 60);
        let mut total = 0;
        let bytes = |_q: u8, k: u64| -> u64 {
            if k.is_multiple_of(600) {
                2_000_000
            } else if (k / 60) % 10 == 3 {
                27_000_000 / 8 / 60
            } else {
                10_000_000 / 8 / 60
            }
        };
        let trace = drive_governor(&mut g, &mut total, 0, 60, 60, &bytes);
        assert!(trace.is_empty(), "{trace:?}");
        assert_eq!(g.q(), 15);
    }

    #[test]
    fn noise_settles_under_the_ceiling_quickly_and_stays_then_returns_to_q() {
        // Incompressible content: 371 Mbps at QP 15 (800x600 measured), halving every 6 QP.
        let c = qp_ceiling_bps(800, 600, 60);
        let noise = |q: u8, _k: u64| -> u64 {
            (371e6 * 2f64.powf(-(q as f64 - 15.0) / 6.0) / 8.0 / 60.0) as u64
        };
        let mut g = QpGovernor::new(15, c, 60);
        let mut total = 0;
        let trace = drive_governor(&mut g, &mut total, 0, 30, 60, &noise);
        let settled = trace.first().expect("engaged").0;
        assert!(settled <= 1_000, "first move at {settled} ms");
        // Settled under the ceiling within a second, with no oscillation afterwards.
        let final_q = g.q();
        let rate_at = |q: u8| 371e6 * 2f64.powf(-(q as f64 - 15.0) / 6.0);
        assert!(rate_at(final_q) <= c as f64, "q {final_q}");
        assert!(
            trace.iter().all(|(t, _)| *t <= 1_500),
            "kept moving: {trace:?}"
        );
        assert!(trace.len() <= 2, "{trace:?}");
        assert!(final_q <= 51);
        // The content calms down (ordinary 10 Mbps at q 15): back to q, one step a second.
        let calm = |q: u8, _k: u64| -> u64 {
            (10e6 * 2f64.powf(-(q as f64 - 15.0) / 6.0) / 8.0 / 60.0) as u64
        };
        let back = drive_governor(&mut g, &mut total, 30_000, 40, 60, &calm);
        assert_eq!(g.q(), 15, "{back:?}");
        assert!(
            back.windows(2).all(|w| w[1].1 + 1 == w[0].1),
            "one step at a time: {back:?}"
        );
        assert!(g.above_ns > 0);
    }

    /// Feed `hp` one fresh STATS a second from `t0_s` for `secs`, painting `fps` frames a second,
    /// with arrival->paint p50 from `p50_ms(t_s, mode)`. Returns the transitions.
    fn feed_client(
        hp: &mut HoldPush,
        t0_s: u64,
        secs: u64,
        fps: u64,
        painted: &mut u64,
        p50_ms: &dyn Fn(u64, HoldMode) -> f64,
    ) -> Vec<(u64, HoldMode)> {
        let mut out = Vec::new();
        for t in t0_s..t0_s + secs {
            let now = t * 1_000 * MS;
            if let Some(tr) = hp.poll(now) {
                out.push((t, tr.to));
            }
            let per_s = if hp.mode() == HoldMode::Immediate {
                2 * fps
            } else {
                fps
            };
            *painted += per_s;
            let p = (p50_ms(t, hp.mode()) * 1e3) as u64;
            if let Some(tr) = hp.client_stats(now, p, *painted, 1) {
                out.push((t, tr.to));
            }
        }
        out
    }

    fn immediate_with_interval(t0_ns: u64) -> HoldPush {
        let mut hp = HoldPush::new(HoldMode::Probing);
        // A 60 fps cadence for the interval estimate.
        for k in 0..30u64 {
            hp.submitted(SubmitKind::Real, k + 1, t0_ns + k * 16_666_667);
        }
        hp
    }

    #[test]
    fn a_healthy_client_keeps_immediate_mode() {
        let mut hp = immediate_with_interval(0);
        let mut painted = 0;
        // Probing for 15 s at 60 fps: baseline 4 ms (window fully non-push after ~8.5 s).
        let none = feed_client(&mut hp, 1, 15, 60, &mut painted, &|_, _| 4.0);
        assert!(none.is_empty(), "{none:?}");
        assert_eq!(hp.client_baseline_us, Some(4_000));
        hp.mode = HoldMode::Immediate;
        // Pushing, the client keeps up: 4.5 ms.
        let tr = feed_client(&mut hp, 16, 120, 60, &mut painted, &|_, _| 4.5);
        assert!(tr.is_empty(), "{tr:?}");
        assert_eq!(hp.mode(), HoldMode::Immediate);
    }

    #[test]
    fn a_slow_client_suspends_pushing_within_seconds_and_does_not_flap() {
        let mut hp = immediate_with_interval(0);
        let mut painted = 0;
        // Baseline 11.7 ms without pushes; 32.7 ms with (measured, client 8 ms slower a frame).
        let model = |_t: u64, m: HoldMode| if m == HoldMode::Immediate { 32.7 } else { 11.7 };
        feed_client(&mut hp, 1, 15, 60, &mut painted, &model);
        hp.mode = HoldMode::Immediate;
        let tr = feed_client(&mut hp, 16, 300, 60, &mut painted, &model);
        let (t, to) = tr.first().copied().expect("suspended");
        assert_eq!(to, HoldMode::Suspended);
        // Half a 512-frame window at 120 fps is ~2 s, then three STATS: about 5 s.
        assert!(t - 16 <= 6, "suspended after {} s", t - 16);
        // After the back-off it only probes (immediate needs a probe this test never passes), so
        // nothing flaps.
        assert!(tr.len() <= 2, "{tr:?}");
        assert!(hp.mode() != HoldMode::Immediate);
    }

    #[test]
    fn immediate_mode_needs_a_reporting_client() {
        // Before the page's first STATS: a 3 s grace from the encoder's first submit (a session
        // started in immediate mode must not drop out at its first loop iteration).
        let mut hp = HoldPush::new(HoldMode::Immediate);
        hp.submitted(SubmitKind::Real, 1, 0);
        assert_eq!(hp.poll(2_900 * MS), None);
        assert_eq!(hp.mode(), HoldMode::Immediate);
        assert!(
            hp.poll(3_000 * MS).is_some(),
            "no STATS at all: leaves after the grace"
        );
        // No STATS at all: never entered, and left 3 s after the last one.
        let mut hp = immediate_with_interval(0);
        assert!(!hp.client_ok(4_000 * MS));
        assert_eq!(hp.enter_immediate(4_000 * MS, "t".into()), None);
        assert_eq!(hp.mode(), HoldMode::Probing);
        let mut painted = 0;
        feed_client(&mut hp, 1, 3, 60, &mut painted, &|_, _| 4.0);
        assert!(hp.enter_immediate(4_000 * MS, "t".into()).is_some());
        // The STATS stop.
        assert_eq!(hp.poll(5_000 * MS), None);
        let left = hp.poll(7_000 * MS).expect("left");
        assert_eq!(
            (left.from, left.to),
            (HoldMode::Immediate, HoldMode::Probing)
        );
        assert!(left.why.contains("no fresh client STATS"));
    }

    #[test]
    fn a_slow_client_is_caught_even_with_no_pushes_off_baseline() {
        // Started in immediate mode (learned, or the start hook): no baseline ever. A client
        // whose queue grows reads far beyond 1.5 intervals and is suspended; a fast one is not.
        for (p50_ms, expect_suspend) in [(32.7, true), (6.0, false)] {
            let mut hp = immediate_with_interval(0);
            hp.mode = HoldMode::Immediate;
            let mut painted = 0;
            let tr = feed_client(&mut hp, 1, 30, 60, &mut painted, &|_, _| p50_ms);
            assert_eq!(
                tr.iter().any(|t| t.1 == HoldMode::Suspended),
                expect_suspend,
                "p50 {p50_ms}: {tr:?}"
            );
            if expect_suspend {
                assert!(tr[0].0 <= 8, "suspended after {} s", tr[0].0);
            }
        }
    }

    #[test]
    fn the_governor_fast_path_steps_on_three_big_frames_but_not_on_a_keyframe() {
        let c = qp_ceiling_bps(1920, 1080, 60);
        let mut g = QpGovernor::new(15, c, 60);
        let t = |k: u64| k * 16_666_667;
        // The stress clip's 1.58 MB keyframe, then ordinary 20 KB frames: nothing.
        assert!(g.frame(t(0), 1_580_000).is_none());
        for k in 1..60 {
            assert!(g.frame(t(k), 20_000).is_none());
        }
        // The recording's 441 KB keyframe, same.
        assert!(g.frame(t(60), 441_000).is_none());
        assert!(g.frame(t(61), 20_000).is_none());
        // Two big frames then an ordinary one: still nothing.
        assert!(g.frame(t(62), 800_000).is_none());
        assert!(g.frame(t(63), 800_000).is_none());
        assert!(g.frame(t(64), 20_000).is_none());
        assert_eq!(g.q(), 15);
        // Noise at 1080p60: ~850 KB a frame. The third in a row steps at once.
        assert!(g.frame(t(65), 850_000).is_none());
        assert!(g.frame(t(66), 850_000).is_none());
        let (q, why) = g.frame(t(67), 850_000).expect("steps at the third frame");
        assert!(q > 15 && why.contains("at once"), "{q} {why}");
        // And then waits a window before stepping again.
        assert!(g.frame(t(68), 850_000).is_none());
        assert!(g.frame(t(69), 850_000).is_none());
        assert!(g.frame(t(70), 850_000).is_none());
    }
}
