use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void};
use std::fmt::Write as _;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use anyhow::{bail, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFArray,
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_EncoderID, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_NumberOfPendingFrames, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_UsingGPURegistryID,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Baseline_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
};

use crate::streamer::config::H264Profile;
use crate::streamer::lock::LockExt;
use crate::streamer::platform::BitrateApplied;
use crate::streamer::telemetry::FrameTiming;

use super::config::EncoderConfig;
use super::vt_recipe::{self, KeyKind, PropResult, Recipe, Val};

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_LENGTH_PREFIX: usize = 4;

/// `'avc1'`. Spelled as the literal FourCC rather than borrowed from the CoreMedia binding so the
/// hand-declared `VTCopy*` prototypes below do not depend on how that crate spells its newtype.
const CODEC_TYPE_H264: u32 = 0x6176_6331;

/// How many encoded access units may wait for the drain thread.
///
/// The channel used to be unbounded, which meant a drain thread that fell behind turned into
/// latency instead of into a visible drop: every queued frame is one the client will be shown
/// late. Four is one frame being written, one being read and two of slack.
const OUTPUT_CAPACITY: usize = 4;

/// Frames the encoder may hold at once before the loop stops feeding it.
///
/// VideoToolbox has no queue limit of its own, and a comparable product measured ~8 frames
/// resident when it was overfed — 130 ms at 60 fps, all of it invisible to every timer we have.
/// The alternative Apple offers, `MaxFrameDelayCount`, is documented as a *blocking* contract on
/// the calling thread, and that thread is the deadline-scheduled encode loop; counting here bounds
/// the same thing without ever blocking it.
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// `AverageBitRate` values tried, in order, for constant QP on the low-latency encoder — the
/// [`RateTier::ClampCeiling`] tier only. Superseded by [`RateTier::BaseFrameQp`] wherever the
/// session supports per-frame QP (measured: M1, macOS 26), which needs no ceiling at all.
///
/// Higher is not safer: the value decides the H.264 level the SPS declares, and 1 Gbps declares
/// 6.2, which many client decoders refuse (300 Mbps declares 5.1). Lower is not safer either: 200
/// and 120 Mbps dropped frames and 150 Mbps failed every replay with -12904, cause unknown.
/// Measured on one Mac only (macOS 26, Apple silicon).
const CONSTANT_QP_CEILINGS_BPS: [i32; 2] = [300_000_000, 250_000_000];

/// What [`Encoder::set_constant_qp_ceiling`] got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ceiling {
    Accepted(i32),
    /// Every candidate was refused (or `SE_VT_QP_BPS=0` named none): the session must not stay
    /// on `ClampCeiling`.
    NoneAccepted,
    /// `SE_VT_QP_BPS=uncapped` (test hook): stay on the clamp with no ceiling, on purpose, so the
    /// drop guard can be exercised.
    ForcedUncapped,
}

/// The ceilings to try, from the `SE_VT_QP_BPS` hook's value (already gated on `SE_TEST_HOOKS`).
#[derive(Clone, Debug, PartialEq, Eq)]
enum CeilingPlan {
    Try(Vec<i32>),
    Uncapped,
}

fn ceiling_plan(hook: Option<&str>) -> CeilingPlan {
    match hook.map(str::trim) {
        None => CeilingPlan::Try(CONSTANT_QP_CEILINGS_BPS.to_vec()),
        Some("uncapped") => CeilingPlan::Uncapped,
        Some(v) => match v.parse::<i32>() {
            Ok(0) => CeilingPlan::Try(Vec::new()),
            Ok(bps) if bps > 0 => CeilingPlan::Try(vec![bps]),
            _ => {
                teprintln!("[vt] SE_VT_QP_BPS={v} is not a bitrate or 'uncapped'; ignored");
                CeilingPlan::Try(CONSTANT_QP_CEILINGS_BPS.to_vec())
            }
        },
    }
}

/// `SE_VT_QP_BPS`, read once, honoured only with `SE_TEST_HOOKS=1`.
fn qp_bps_hook() -> Option<String> {
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| vt_recipe::hook_var("SE_VT_QP_BPS"))
        .clone()
}

/// The per-frame stamps that ride through VideoToolbox as `sourceFrameRefCon` and come back on
/// the output callback.
///
/// This used to be a `VecDeque` behind a `std::sync::Mutex` shared between the real-time encode
/// thread and the drain thread, which is a priority inversion waiting to happen and also assumes
/// submit order equals emit order. VideoToolbox carries an opaque pointer per frame for exactly
/// this purpose, so the stamps travel with their own frame and neither the lock nor the ordering
/// assumption is needed.
pub struct FrameTag {
    pub capture: Instant,
    pub timing: FrameTiming,
    /// A hold push (`pipeline.rs`), so the drain thread can count what pushes cost in bytes.
    pub hold_push: bool,
}

pub struct CompressedSample {
    sample: Option<CFRetained<CMSampleBuffer>>,
    /// `None` only if VideoToolbox emitted a frame we never tagged, which it should not.
    pub tag: Option<FrameTag>,
    /// The output callback's `OSStatus`. Non-zero means the frame failed inside the encoder.
    pub status: i32,
    /// `VTEncodeInfoFlags` as delivered (`kVTEncodeInfo_FrameDropped` = bit 1).
    pub info_flags: u32,
    /// Stamped on entry to the output callback, before anything else runs.
    pub callback_at: Instant,
    /// The frame was submitted with `ForceKeyFrame`.
    pub forced_idr: bool,
}

unsafe impl Send for CompressedSample {}

impl CompressedSample {
    /// The encoded sample, `None` for a dropped or failed frame.
    pub fn sample(&self) -> Option<&CMSampleBuffer> {
        self.sample.as_deref()
    }

    /// VideoToolbox emitted the frame with no picture (rate control dropped it).
    pub fn is_rate_drop(&self) -> bool {
        is_rate_drop_raw(self.status, self.info_flags, self.sample.is_some())
    }

    pub fn to_annexb(&self, out: &mut Vec<u8>) -> bool {
        out.clear();
        match &self.sample {
            Some(s) => sample_to_annexb_into(s, out),
            None => false,
        }
    }
}

struct OutputCtx {
    tx: Sender<CompressedSample>,
    /// The encoder's own end of the channel, used only to make room when it is full.
    rx: Receiver<CompressedSample>,
    in_flight: Arc<AtomicUsize>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    /// Frames VideoToolbox accepted and then declined to emit.
    rate_dropped: Arc<AtomicU64>,
    /// Of those, frames that carried `ForceKeyFrame`: the keyframe never happened.
    forced_dropped: Arc<AtomicU64>,
    /// When each emission happened, by emission number (see [`EmitLog`]).
    emit_log: Arc<EmitLog>,
    /// Callbacks completed, numbering the emissions for `emit_log`.
    completed: AtomicU64,
    /// `SE_VT_SIM_HOLD` (test hook): the output of frame k is withheld until frame k+1 has been
    /// submitted, imitating the one-frame-hold encoders (`h264.gva` on a 2012 MacBook Pro).
    sim: Option<Mutex<SimHold>>,
    sim_cv: std::sync::Condvar,
}

/// The emission clock: slot `n % EMIT_LOG_LEN` holds emission `n`'s host-clock time, tagged with
/// `n` so a reader can tell a slot that has been reused. Written by the output callback, read by
/// the encode loop (`Encoder::emit_time_ns`); lock-free.
pub struct EmitLog {
    seq: [AtomicU64; EMIT_LOG_LEN],
    ns: [AtomicU64; EMIT_LOG_LEN],
}

/// Comfortably more than `HARD_MAX_FRAMES_IN_FLIGHT` (16): the encode loop reads every emission
/// within a poll or two, and the in-flight bound caps how many can pile up in between.
const EMIT_LOG_LEN: usize = 64;

impl EmitLog {
    fn new() -> Arc<Self> {
        Arc::new(EmitLog {
            seq: std::array::from_fn(|_| AtomicU64::new(0)),
            ns: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }
}

/// The simulated hold's state. Test-only (`SE_TEST_HOOKS=1` + `SE_VT_SIM_HOLD=1`), so a lock on
/// the callback and the submit path is acceptable here and nowhere else.
#[derive(Default)]
struct SimHold {
    /// Frames submitted successfully so far (the newest one's submit number).
    submitted: u64,
    /// When recent frames were submitted, `(seq, at)`: a frame is released at the submit of the
    /// one after it.
    submit_at: std::collections::VecDeque<(u64, Instant)>,
    held: Option<Pending>,
    /// `SE_VT_SIM_HOLD_DELAY_MS`: released frames wait here until `release + delay`, in order,
    /// for the simulation thread to complete them.
    queue: std::collections::VecDeque<(Instant, Pending)>,
    last_deadline: Option<Instant>,
    /// `SE_VT_SIM_HOLD_COST_MS`: when the serialised simulated encoder is free again.
    busy_until: Option<Instant>,
    stop: bool,
}

/// `SE_VT_SIM_HOLD_COST_MS` (test hook, with `SE_VT_SIM_HOLD=1`): a SERIALISED per-frame encode
/// cost. Frame k finishes `cost` after the later of its own submit and the previous frame's
/// finish, and comes out at the later of that and its release (the next submit) — so a push
/// delays the next real frame, as on a GPU that cannot do twice the frame rate. Takes precedence
/// over `SE_VT_SIM_HOLD_DELAY_MS`.
fn sim_hold_cost() -> std::time::Duration {
    static D: OnceLock<std::time::Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let ms = vt_recipe::hook_var("SE_VT_SIM_HOLD_COST_MS")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if ms > 0 && sim_hold_enabled() {
            teprintln!(
                "[vt] SE_VT_SIM_HOLD_COST_MS={ms}: the simulated holder encodes serially at {ms} ms \
                 a frame"
            );
        }
        std::time::Duration::from_millis(ms)
    })
}

/// `SE_VT_SIM_HOLD_DELAY_MS` (test hook, with `SE_VT_SIM_HOLD=1`): a simulated encode time from
/// a frame's release (the next submit) to its output. 0 / unset: output as soon as both the real
/// encoder has finished and the frame has been released.
fn sim_hold_delay() -> std::time::Duration {
    static D: OnceLock<std::time::Duration> = OnceLock::new();
    *D.get_or_init(|| {
        let ms = vt_recipe::hook_var("SE_VT_SIM_HOLD_DELAY_MS")
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if ms > 0 && sim_hold_enabled() {
            teprintln!("[vt] SE_VT_SIM_HOLD_DELAY_MS={ms}: the simulated encoder takes {ms} ms");
        }
        std::time::Duration::from_millis(ms)
    })
}

/// One output callback's worth, before the bookkeeping and the hand-off to the drain thread.
struct Pending {
    /// The frame's submit number (1-based, per encoder), 0 when it was never tagged.
    seq: u64,
    forced: bool,
    tag: Option<FrameTag>,
    status: i32,
    info_flags: u32,
    sample: Option<CFRetained<CMSampleBuffer>>,
    callback_at: Instant,
}

unsafe impl Send for Pending {}

/// `SE_VT_SIM_HOLD=1` with `SE_TEST_HOOKS=1`. Read once.
fn sim_hold_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = vt_recipe::hook_var("SE_VT_SIM_HOLD").as_deref() == Some("1");
        if on {
            teprintln!(
                "[vt] !!!!! SE_VT_SIM_HOLD TEST HOOK ACTIVE !!!!! every encoder's output for frame \
                 k is withheld until frame k+1 is submitted; latency measured here describes a \
                 simulated one-frame-hold encoder, not this Mac"
            );
        }
        on
    })
}

impl OutputCtx {
    fn new(
        tx: Sender<CompressedSample>,
        rx: Receiver<CompressedSample>,
        counters: [&Arc<AtomicU64>; 4],
        in_flight: &Arc<AtomicUsize>,
        emit_log: &Arc<EmitLog>,
    ) -> Self {
        let [emitted, dropped, rate_dropped, forced_dropped] = counters;
        OutputCtx {
            tx,
            rx,
            in_flight: Arc::clone(in_flight),
            emitted: Arc::clone(emitted),
            dropped: Arc::clone(dropped),
            rate_dropped: Arc::clone(rate_dropped),
            forced_dropped: Arc::clone(forced_dropped),
            emit_log: Arc::clone(emit_log),
            completed: AtomicU64::new(0),
            sim: sim_hold_enabled().then(|| Mutex::new(SimHold::default())),
            sim_cv: std::sync::Condvar::new(),
        }
    }

    /// With a simulated encode time, a thread completes released frames at their deadlines. It
    /// holds only a `Weak`, so it ends with the encoder.
    fn start_sim_thread(ctx: &Arc<OutputCtx>) {
        if ctx.sim.is_none() || (sim_hold_delay().is_zero() && sim_hold_cost().is_zero()) {
            return;
        }
        let weak = Arc::downgrade(ctx);
        let _ = std::thread::Builder::new()
            .name("vt-sim-hold".into())
            .spawn(move || loop {
                let Some(ctx) = weak.upgrade() else { return };
                let Some(sim) = &ctx.sim else { return };
                let mut g = sim.lock_recover();
                if g.stop {
                    return;
                }
                let now = Instant::now();
                match g.queue.front().map(|(d, _)| *d) {
                    Some(d) if d <= now => {
                        let (_, p) = g.queue.pop_front().expect("front exists");
                        drop(g);
                        ctx.complete(p);
                    }
                    front => {
                        let wait = front
                            .map(|d| d - now)
                            .unwrap_or(std::time::Duration::from_millis(50))
                            .min(std::time::Duration::from_millis(50));
                        let _ = ctx.sim_cv.wait_timeout(g, wait);
                    }
                }
            });
    }

    /// The bookkeeping every emission gets, then the hand-off. Called from the output callback,
    /// or — under the hold simulation — when the frame is released.
    fn complete(&self, p: Pending) {
        let n = self.completed.fetch_add(1, Ordering::Relaxed) + 1;
        let slot = (n as usize) % EMIT_LOG_LEN;
        self.emit_log.ns[slot].store(crate::streamer::audio::host_now_ns(), Ordering::Relaxed);
        self.emit_log.seq[slot].store(n, Ordering::Release);
        self.emitted.fetch_add(1, Ordering::Release);
        let has_sample = p.sample.is_some();
        // kVTEncodeInfo_FrameDropped
        if is_rate_drop_raw(p.status, p.info_flags, has_sample) {
            self.rate_dropped.fetch_add(1, Ordering::Relaxed);
        }
        if forced_keyframe_lost(p.forced, p.status, p.info_flags, has_sample) {
            self.forced_dropped.fetch_add(1, Ordering::Relaxed);
        }
        // `fetch_update` is `try_update` from Rust 1.100; kept for older toolchains.
        #[allow(deprecated)]
        let _ = self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
        self.deliver(CompressedSample {
            sample: p.sample,
            tag: p.tag,
            status: p.status,
            info_flags: p.info_flags,
            callback_at: p.callback_at,
            forced_idr: p.forced,
        });
    }

    /// An output callback arrived: complete it, or (simulated hold) keep it until a later frame
    /// has been submitted.
    fn arrive(&self, p: Pending) {
        let Some(sim) = &self.sim else {
            self.complete(p);
            return;
        };
        let mut g = sim.lock_recover();
        // A held frame older than this one: a later frame was certainly submitted.
        if g.held.as_ref().is_some_and(|h| h.seq < p.seq) {
            if let Some(h) = g.held.take() {
                let at = Self::submit_time(&g, h.seq + 1);
                self.release(&mut g, h, at);
            }
        }
        if p.seq == 0 {
            drop(g);
            self.complete(p);
        } else if g.submitted > p.seq {
            let at = Self::submit_time(&g, p.seq + 1);
            self.release(&mut g, p, at);
        } else {
            g.held = Some(p);
        }
    }

    fn submit_time(g: &SimHold, seq: u64) -> Instant {
        g.submit_at
            .iter()
            .find(|(s, _)| *s == seq)
            .map_or_else(Instant::now, |(_, at)| *at)
    }

    /// The hold simulation's other half: frame `n` was submitted, so anything older comes out.
    fn sim_submitted(&self, n: u64) {
        let Some(sim) = &self.sim else { return };
        let mut g = sim.lock_recover();
        g.submitted = n;
        if g.submit_at.len() >= 64 {
            g.submit_at.pop_front();
        }
        let now = Instant::now();
        g.submit_at.push_back((n, now));
        if g.held.as_ref().is_some_and(|h| h.seq < n) {
            if let Some(h) = g.held.take() {
                self.release(&mut g, h, now);
            }
        }
    }

    /// Flush or teardown: whatever the simulation holds comes out, like `CompleteFrames` does on a
    /// real holding encoder.
    fn sim_release_all(&self) {
        let Some(sim) = &self.sim else { return };
        let mut g = sim.lock_recover();
        let mut out: Vec<Pending> = g.queue.drain(..).map(|(_, p)| p).collect();
        out.extend(g.held.take());
        g.last_deadline = None;
        drop(g);
        for mut p in out {
            p.callback_at = Instant::now();
            self.complete(p);
        }
    }

    /// Teardown: the simulation thread stops.
    fn sim_stop(&self) {
        if let Some(sim) = &self.sim {
            sim.lock_recover().stop = true;
            self.sim_cv.notify_all();
        }
    }

    /// Frame `p` was released (the next frame went in at `released_at`): out now, or — with a
    /// simulated encode time — at `max(real output, released_at + delay)`, never before an
    /// earlier frame.
    fn release(&self, g: &mut SimHold, mut p: Pending, released_at: Instant) {
        let delay = sim_hold_delay();
        let cost = sim_hold_cost();
        if delay.is_zero() && cost.is_zero() {
            p.callback_at = Instant::now();
            self.complete(p);
            return;
        }
        let mut deadline = if cost.is_zero() {
            (released_at + delay).max(p.callback_at)
        } else {
            let start = Self::submit_time(g, p.seq).max(g.busy_until.unwrap_or(released_at));
            let done = start + cost;
            g.busy_until = Some(done);
            done.max(released_at)
        };
        if let Some(last) = g.last_deadline {
            deadline = deadline.max(last);
        }
        g.last_deadline = Some(deadline);
        p.callback_at = deadline;
        g.queue.push_back((deadline, p));
        self.sim_cv.notify_all();
    }

    /// Hand an encoded access unit to the drain thread, making room if the channel is full.
    ///
    /// The oldest queued frame goes, not the newest: the newest is the one that still has a
    /// chance of being shown on time. Either way a dropped access unit breaks the reference
    /// chain, so the count is published for the encode loop to turn into an IDR.
    fn deliver(&self, sample: CompressedSample) {
        match self.tx.try_send(sample) {
            Ok(()) => {}
            Err(TrySendError::Full(sample)) => {
                let _ = self.rx.try_recv();
                self.dropped.fetch_add(1, Ordering::Relaxed);
                let _ = self.tx.try_send(sample);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    rx: Receiver<CompressedSample>,
    refcon: *mut OutputCtx,
    in_flight: Arc<AtomicUsize>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    rate_dropped: Arc<AtomicU64>,
    /// Frames submitted with `ForceKeyFrame` that rate control dropped (see `forced_keyframe_lost`).
    forced_dropped: Arc<AtomicU64>,
    fps: i32,
    frame_index: i64,
    /// Frames accepted by `VTCompressionSessionEncodeFrame` (each gets exactly one callback).
    submits: u64,
    emit_log: Arc<EmitLog>,
    /// The session was created with the *resolved* `EnableLowLatencyRateControl` specification
    /// key. Never a literal string: 10.15 has no such symbol, silently ignores the literal key and
    /// hands back the standard encoder, which is how the old ladder reported `low_latency=true`
    /// on `h264.gva`.
    low_latency: bool,
    /// `kVTVideoEncoderSpecification_EnableLowLatencyRateControl` resolved in this process.
    low_latency_key: bool,
    /// `low_latency` before [`Encoder::confirm_low_latency`] checked it against the read-back.
    low_latency_requested: bool,
    qp_mode: bool,
    /// The rate-control tier this session runs (`None` for a recipe-built session).
    rate_tier: Option<RateTier>,
    /// The `AverageBitRate` a constant-quality bitrate tier set (`None` otherwise).
    rate_bps: Option<u32>,
    /// What the tier was chosen from.
    caps: Capabilities,
    /// Complete frames after every submit (the per-family extras table can ask for it).
    flush_each_frame: bool,
    identity: EncoderIdentity,
    supports_ltr: bool,
    /// Whether *this* session is on software because the user asked, which is the difference
    /// between an expected outcome and the loud warning below it.
    software_requested: bool,
    /// Set only when the session was built from a [`Recipe`].
    recipe: Option<RecipeState>,
    /// Per-frame options changed mid-session by [`Encoder::set_frame_opt`]. `None` until then,
    /// so the shipped path builds its frame dictionaries exactly as it always has.
    frame_override: Option<FrameOverride>,
}

unsafe impl Send for Encoder {}

/// The per-frame option set after a mid-session change, with both dictionaries prebuilt so the
/// submit path still only clones a retained pointer.
struct FrameOverride {
    list: Vec<(String, Val)>,
    plain: Option<CFRetained<CFDictionary>>,
    idr: Option<CFRetained<CFDictionary>>,
}

/// What a recipe-built session did with each key, for the bench and the log.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RecipeReport {
    pub name: String,
    pub codec: vt_recipe::Codec,
    pub spec: Vec<PropResult>,
    pub props: Vec<PropResult>,
    pub frame_opts: Vec<PropResult>,
    /// Keys in the session's own supported-property dictionary (`None` if it could not be read).
    pub advertised_count: Option<usize>,
}

struct RecipeState {
    report: RecipeReport,
    advertised: Option<HashSet<String>>,
    frame_opts: Vec<(String, Val)>,
    flush_each_frame: bool,
    frame_plain: Option<CFRetained<CFDictionary>>,
    frame_idr: Option<CFRetained<CFDictionary>>,
}

/// `VTCompressionSessionCreate` refused a recipe's specification. Downcast from the
/// `anyhow::Error` that [`Encoder::new`] returns to read the status.
#[derive(Debug)]
pub struct SessionCreateFailed {
    pub status: i32,
    pub spec: Vec<PropResult>,
}

impl std::fmt::Display for SessionCreateFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "VTCompressionSessionCreate failed: OSStatus {}",
            self.status
        )
    }
}

impl std::error::Error for SessionCreateFailed {}

/// The raw result of one submit, for callers that record statuses rather than bail on them.
#[derive(Clone, Copy, Debug)]
pub struct SubmitStatus {
    /// `VTCompressionSessionEncodeFrame`'s status.
    pub encode: i32,
    /// `VTCompressionSessionCompleteFrames` after the submit, for a `flush_each_frame` recipe.
    pub flush: Option<i32>,
}

/// `SE_VT_RECIPE=<file.json>`: put every production session on one recipe. Read once, and only
/// with `SE_TEST_HOOKS=1` beside it — a recipe replaces the whole shipped configuration, so a
/// stray variable in a user's environment must not be able to do that.
fn env_recipe() -> Option<Arc<Recipe>> {
    static RECIPE: OnceLock<Option<Arc<Recipe>>> = OnceLock::new();
    RECIPE
        .get_or_init(|| {
            let path = vt_recipe::hook_var("SE_VT_RECIPE")?;
            match std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|t| Recipe::from_json(&t))
            {
                Ok(r) => {
                    tprintln!(
                        "[vt] SE_VT_RECIPE: every session uses recipe '{}' from {path}",
                        r.name
                    );
                    Some(Arc::new(r))
                }
                Err(e) => {
                    teprintln!("[vt] SE_VT_RECIPE={path} ignored: {e}");
                    None
                }
            }
        })
        .clone()
}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        Self::build(cfg, false)
    }

    /// The same session without the low-latency rung: the drop guard's one-way fallback to
    /// [`RateTier::StandardBitrate`] (`pipeline.rs`). A recipe (`EncoderConfig.recipe`,
    /// `SE_VT_RECIPE`) still overrides everything, this included.
    pub fn new_standard(cfg: EncoderConfig) -> Result<Self> {
        Self::build(cfg, true)
    }

    fn build(cfg: EncoderConfig, standard_only: bool) -> Result<Self> {
        encoder_catalog();

        if let Some(recipe) = cfg.recipe.clone().or_else(env_recipe) {
            return Self::new_from_recipe(&cfg, recipe);
        }

        let (tx, rx) = bounded::<CompressedSample>(OUTPUT_CAPACITY);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let rate_dropped = Arc::new(AtomicU64::new(0));
        let forced_dropped = Arc::new(AtomicU64::new(0));
        let emit_log = EmitLog::new();
        let ctx = Arc::new(OutputCtx::new(
            tx,
            rx.clone(),
            [&emitted, &dropped, &rate_dropped, &forced_dropped],
            &in_flight,
            &emit_log,
        ));
        OutputCtx::start_sim_thread(&ctx);
        let refcon = Arc::into_raw(ctx) as *mut OutputCtx;

        // The rungs, best first. The low-latency rung exists only when the specification key's
        // SYMBOL resolves (`low_latency_key_resolves`): that is the capability probe. A literal
        // "EnableLowLatencyRateControl" string is not one — 10.15 has no such key, ignores it
        // without an error and creates the standard encoder, so the rung "succeeded" there and
        // the session was configured as a low-latency one it was not.
        //
        // When the user has asked for software encode there is no ladder: one rung that forbids
        // the hardware encoder outright. Falling back to hardware here would be the setting
        // silently refusing to do the one thing it says on the tin, so a failure to create that
        // session is reported as a failure rather than worked around.
        let low_latency_key = low_latency_key_resolves();
        let ladder: Vec<(bool, HwPolicy)> = if cfg.software_encode {
            vec![(false, HwPolicy::Forbid)]
        } else {
            let mut rungs = Vec::with_capacity(3);
            if low_latency_key && !standard_only {
                rungs.push((true, HwPolicy::Require));
            }
            rungs.push((false, HwPolicy::Require));
            rungs.push((false, HwPolicy::Enable));
            rungs
        };
        if standard_only {
            tprintln!(
                "[vt] this session skips the low-latency encoder (rate-control drop-guard fallback)"
            );
        } else if !low_latency_key && !cfg.software_encode {
            tprintln!(
                "[vt] kVTVideoEncoderSpecification_EnableLowLatencyRateControl does not resolve \
                 in this process; no low-latency rung, the standard encoder is the first choice"
            );
        }
        if cfg.software_encode {
            tprintln!(
                "[vt] software encode was requested, so this session forbids the hardware \
                 encoder (EnableHardwareAcceleratedVideoEncoder=false)"
            );
        }

        let mut session = None;
        let mut low_latency = false;
        let mut last_status = 0;
        for &(ll, hw) in &ladder {
            if ll && vt_recipe::deny().create_low_latency {
                last_status = vt_recipe::DENIED_CREATE_STATUS;
                teprintln!(
                    "[vt] low-latency rung refused by SE_VT_DENY create:lowlatency (OSStatus {})",
                    last_status
                );
                continue;
            }
            let Some(spec) = build_encoder_spec(ll, hw, cfg.gpu_registry_id) else {
                continue;
            };
            match create_session(&cfg, &spec, refcon) {
                Ok(s) => {
                    session = Some(s);
                    low_latency = ll;
                    break;
                }
                Err(status) => {
                    last_status = status;
                    if hw == HwPolicy::Require {
                        teprintln!(
                            "[vt] HW encoder unavailable for {}x{} (low_latency={ll}, \
                             OSStatus {status}); trying a weaker encoder spec",
                            cfg.width,
                            cfg.height
                        );
                    }
                }
            }
        }
        let Some(session) = session else {
            drop(unsafe { Arc::from_raw(refcon as *const OutputCtx) });
            bail!("VTCompressionSessionCreate failed: OSStatus {last_status}");
        };

        let mut enc = Encoder {
            session,
            rx,
            refcon,
            in_flight,
            emitted,
            dropped,
            rate_dropped,
            forced_dropped,
            fps: cfg.fps.max(1) as i32,
            frame_index: 0,
            submits: 0,
            emit_log: Arc::clone(&emit_log),
            low_latency,
            low_latency_key,
            low_latency_requested: false,
            qp_mode: cfg.qp.is_some(),
            rate_tier: None,
            rate_bps: None,
            caps: Capabilities::default(),
            flush_each_frame: false,
            identity: EncoderIdentity::default(),
            supports_ltr: false,
            software_requested: cfg.software_encode,
            recipe: None,
            frame_override: None,
        };
        enc.configure(&cfg)?;
        unsafe { enc.session.prepare_to_encode_frames() };
        tprintln!(
            "[vt] H.264 encoder ready: {}x{} @ {}fps, {}, profile={:?}, low_latency={}, \
             encoder={}, rate_tier={}",
            cfg.width,
            cfg.height,
            cfg.fps,
            enc.rate_summary(&cfg),
            cfg.profile,
            low_latency,
            enc.encoder_id_short(),
            enc.rate_tier_name(),
        );
        Ok(enc)
    }

    /// Build exactly what the recipe says: one create attempt with its codec and specification, no
    /// ladder, then its properties in order. Nothing from `configure` is applied.
    fn new_from_recipe(cfg: &EncoderConfig, recipe: Arc<Recipe>) -> Result<Self> {
        let (tx, rx) = bounded::<CompressedSample>(OUTPUT_CAPACITY);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let rate_dropped = Arc::new(AtomicU64::new(0));
        let forced_dropped = Arc::new(AtomicU64::new(0));
        let emit_log = EmitLog::new();
        let ctx = Arc::new(OutputCtx::new(
            tx,
            rx.clone(),
            [&emitted, &dropped, &rate_dropped, &forced_dropped],
            &in_flight,
            &emit_log,
        ));
        OutputCtx::start_sim_thread(&ctx);
        let refcon = Arc::into_raw(ctx) as *mut OutputCtx;

        let (spec, spec_report) = vt_recipe::build_dict(KeyKind::Spec, &recipe.spec, &[]);
        if recipe.spec_flag("EnableLowLatencyRateControl") && vt_recipe::deny().create_low_latency {
            drop(unsafe { Arc::from_raw(refcon as *const OutputCtx) });
            let status = vt_recipe::DENIED_CREATE_STATUS;
            teprintln!(
                "[vt] recipe '{}': low-latency create refused by SE_VT_DENY create:lowlatency \
                 (OSStatus {status})",
                recipe.name
            );
            return Err(anyhow::Error::new(SessionCreateFailed {
                status,
                spec: spec_report,
            }));
        }
        let session = match vt_recipe::create_session(
            cfg.width,
            cfg.height,
            recipe.codec.fourcc(),
            spec.as_deref(),
            output_callback,
            refcon.cast(),
        ) {
            Ok(s) => s,
            Err(status) => {
                drop(unsafe { Arc::from_raw(refcon as *const OutputCtx) });
                teprintln!(
                    "[vt] recipe '{}': VTCompressionSessionCreate {}x{} failed: OSStatus {status}",
                    recipe.name,
                    cfg.width,
                    cfg.height
                );
                return Err(anyhow::Error::new(SessionCreateFailed {
                    status,
                    spec: spec_report,
                }));
            }
        };

        let mut enc = Encoder {
            session,
            rx,
            refcon,
            in_flight,
            emitted,
            dropped,
            rate_dropped,
            forced_dropped,
            fps: cfg.fps.max(1) as i32,
            frame_index: 0,
            submits: 0,
            emit_log: Arc::clone(&emit_log),
            // Only when the key actually went into the specification: a recipe naming it on an OS
            // where the symbol does not resolve builds a standard session (`missing_symbol`).
            low_latency: recipe.spec_flag("EnableLowLatencyRateControl")
                && spec_report.iter().any(|r| {
                    r.key == "EnableLowLatencyRateControl"
                        && r.outcome == vt_recipe::Outcome::Included
                }),
            low_latency_key: low_latency_key_resolves(),
            low_latency_requested: false,
            qp_mode: cfg.qp.is_some() || recipe.pins_rate(),
            rate_tier: None,
            rate_bps: None,
            caps: Capabilities::default(),
            flush_each_frame: false,
            identity: EncoderIdentity::default(),
            supports_ltr: false,
            software_requested: cfg.software_encode,
            recipe: None,
            frame_override: None,
        };

        let advertised = vt_recipe::supported_keys(enc.vt_session());
        let props: Vec<PropResult> = recipe
            .props
            .iter()
            .map(|(k, v)| vt_recipe::set_prop(enc.vt_session(), k, v, advertised.as_ref(), false))
            .collect();
        let (frame_plain, frame_report) =
            vt_recipe::build_dict(KeyKind::FrameOpt, &recipe.frame_opts, &[]);
        let force_key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let (frame_idr, _) = vt_recipe::build_dict(
            KeyKind::FrameOpt,
            &recipe.frame_opts,
            &[(force_key, cfbool(true))],
        );
        enc.supports_ltr = advertised.as_ref().is_some_and(|a| a.contains("EnableLTR"));

        let mut line = format!("[vt] recipe '{}' properties:", recipe.name);
        for r in spec_report.iter().chain(&props).chain(&frame_report) {
            let _ = write!(line, " {}={}", r.key, r.short());
        }
        tprintln!("{line}");

        enc.recipe = Some(RecipeState {
            report: RecipeReport {
                name: recipe.name.clone(),
                codec: recipe.codec,
                spec: spec_report,
                props,
                frame_opts: frame_report,
                advertised_count: advertised.as_ref().map(HashSet::len),
            },
            advertised,
            frame_opts: recipe.frame_opts.clone(),
            flush_each_frame: recipe.flush_each_frame,
            frame_plain,
            frame_idr,
        });
        enc.read_identity();
        enc.confirm_low_latency();
        unsafe { enc.session.prepare_to_encode_frames() };
        tprintln!(
            "[vt] recipe encoder ready: {}x{} codec={:?} low_latency={}",
            cfg.width,
            cfg.height,
            recipe.codec,
            enc.low_latency
        );
        Ok(enc)
    }

    /// What the recipe did, key by key. `None` for a session built the production way.
    pub fn recipe_report(&self) -> Option<&RecipeReport> {
        self.recipe.as_ref().map(|r| &r.report)
    }

    /// Set any compression property mid-session, by short name, and report what happened.
    pub fn set_property(&mut self, name: &str, value: &Val) -> PropResult {
        let advertised = self.recipe.as_ref().and_then(|r| r.advertised.clone());
        let advertised = advertised.or_else(|| vt_recipe::supported_keys(self.vt_session()));
        vt_recipe::set_prop(self.vt_session(), name, value, advertised.as_ref(), true)
    }

    /// Add or replace one per-frame option from the next submit on (`None` removes it).
    ///
    /// Starts from the recipe's `frame_opts` (for a production session: the rate tier's own
    /// options, `BaseFrameQP` on [`RateTier::BaseFrameQp`], else empty), so the change is layered
    /// on whatever the session was built with. Returns what the rebuilt dictionary did
    /// with the key; `None` when the key was removed. Not for the encode loop's hot path: it
    /// builds two CF dictionaries.
    pub fn set_frame_opt(&mut self, key: &str, value: Option<Val>) -> Option<PropResult> {
        let mut list = match (&self.frame_override, &self.recipe) {
            (Some(f), _) => f.list.clone(),
            (None, Some(r)) => r.frame_opts.clone(),
            (None, None) => Vec::new(),
        };
        list.retain(|(k, _)| k != key);
        if let Some(v) = value {
            list.push((key.to_string(), v));
        }
        let (plain, report) = vt_recipe::build_dict(KeyKind::FrameOpt, &list, &[]);
        let force_key = unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame };
        let (idr, _) =
            vt_recipe::build_dict(KeyKind::FrameOpt, &list, &[(force_key, cfbool(true))]);
        let result = report.into_iter().find(|r| r.key == key);
        self.frame_override = Some(FrameOverride { list, plain, idr });
        result
    }

    /// Read any compression property by short name.
    pub fn read_property(&self, name: &str) -> PropResult {
        vt_recipe::read_prop(self.vt_session(), name)
    }

    fn vt_session(&self) -> &VTSession {
        unsafe { &*((&*self.session) as *const VTCompressionSession as *const VTSession) }
    }

    fn configure(&mut self, cfg: &EncoderConfig) -> Result<()> {
        let advertised = supported_properties(cfg);
        let mut report = PropReport::default();
        // Identity first: whether this really is a low-latency session, the profile, the rate
        // tier and the per-family extras are all chosen from it.
        self.read_identity();
        self.confirm_low_latency();
        {
            let s = self.vt_session();
            report.note(
                "RealTime",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_RealTime },
                    cfbool(true),
                ),
                &advertised,
            );
            report.note(
                "AllowFrameReordering",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
                    cfbool(false),
                ),
                &advertised,
            );

            let profile: &CFString = if self.low_latency {
                match cfg.profile {
                    H264Profile::High => {
                        vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedHigh_AutoLevel")
                            .unwrap_or(unsafe { kVTProfileLevel_H264_High_AutoLevel })
                    }
                    H264Profile::Main => unsafe { kVTProfileLevel_H264_Main_AutoLevel },
                    H264Profile::Baseline => {
                        vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel")
                            .unwrap_or(unsafe { kVTProfileLevel_H264_Baseline_AutoLevel })
                    }
                }
            } else {
                match cfg.profile {
                    H264Profile::Baseline => {
                        vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel")
                            .unwrap_or(unsafe { kVTProfileLevel_H264_Baseline_AutoLevel })
                    }
                    H264Profile::Main => unsafe { kVTProfileLevel_H264_Main_AutoLevel },
                    H264Profile::High => unsafe { kVTProfileLevel_H264_High_AutoLevel },
                }
            };
            report.note(
                "ProfileLevel",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_ProfileLevel },
                    profile,
                ),
                &advertised,
            );

            for key in ["ColorPrimaries", "TransferFunction", "YCbCrMatrix"] {
                let value = CFString::from_str("ITU_R_709_2");
                report.note(key, set_optional(s, key, &value), &advertised);
            }

            let n = CFNumber::new_i32(cfg.fps as i32);
            report.note(
                "ExpectedFrameRate",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
                    &n,
                ),
                &advertised,
            );

            // Keyframes are for joining clients and for loss recovery, both of which arrive as an
            // explicit request. A periodic IDR on top of that is a bitrate spike every interval
            // that buys nothing: 10 s was frequent enough to be visible on a constrained link.
            let n = CFNumber::new_i32((cfg.fps as i32).saturating_mul(600).max(1));
            report.note(
                "MaxKeyFrameInterval",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
                    &n,
                ),
                &advertised,
            );
            let n = CFNumber::new_f64(600.0);
            report.note(
                "MaxKeyFrameIntervalDuration",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
                    &n,
                ),
                &advertised,
            );

            // No `MaxFrameDelayCount`. Apple defines it as a hard contract on the *calling*
            // thread — "before the call to encode frame N returns, frame N-M must have been
            // emitted" — so the value that looks like the lowest latency (1) is the one that
            // blocks the deadline-scheduled encode loop on the encoder. `MAX_FRAMES_IN_FLIGHT`
            // bounds the same queue from outside without ever blocking.
            report.note(
                "MaximizePowerEfficiency",
                set_optional_bool(s, "MaximizePowerEfficiency", false),
                &advertised,
            );
            report.note(
                "PrioritizeEncodingSpeedOverQuality",
                set_optional_bool(s, "PrioritizeEncodingSpeedOverQuality", true),
                &advertised,
            );
            report.note("BaseLayerFrameRate", PropOutcome::NotRequested, &advertised);
            report.note("ConstantBitRate", PropOutcome::NotRequested, &advertised);

            // Long-term reference frames would let a lost frame be repaired against an
            // acknowledged older one instead of a full IDR. The encoder half is one property;
            // the other half is a client that acknowledges tokens over the control channel,
            // which belongs to the transport. Probe and report so the transport knows whether
            // the machine can do it at all.
            self.supports_ltr = advertised.contains("EnableLTR");
            report.note("EnableLTR", PropOutcome::NotRequested, &advertised);

            if cfg.intra_refresh {
                teprintln!(
                    "[vt] intra-refresh requested but unsupported on macOS VideoToolbox \
                     (no rolling intra-refresh); using PLI→IDR loss recovery instead"
                );
            }
        }

        self.configure_rate_control(cfg, &mut report, &advertised);
        report.emit();
        self.apply_family_extras();
        Ok(())
    }

    /// Choose this session's rate-control tier from what it can do, and apply it.
    ///
    /// Constant quality (`cfg.qp = Some(q)`) takes the first tier the session supports, in the
    /// order of [`select_rate_tier`]; every capability is probed on this session, never inferred
    /// from the OS version. Adaptive mode (`cfg.qp = None`) keeps the loss-based controller's
    /// `AverageBitRate`, with the QP floor and ceiling only on a low-latency session — on the
    /// standard encoders QP limits are rejected or, on `ave.avc`, stall `EncodeFrame`.
    fn configure_rate_control(
        &mut self,
        cfg: &EncoderConfig,
        report: &mut PropReport,
        advertised: &HashSet<String>,
    ) {
        let mut caps = Capabilities {
            low_latency_key_resolved: self.low_latency_key,
            // What was asked for; `select_rate_tier` confirms it against the read-back exactly as
            // `confirm_low_latency` did.
            low_latency_session: self.low_latency_requested,
            supports_base_frame_qp: self.identity.supports_base_frame_qp,
            frame_qp_key_resolved: vt_recipe::resolve_key(KeyKind::FrameOpt, "BaseFrameQP").is_ok(),
            max_qp: Probe::NotTried,
            min_qp: Probe::NotTried,
            encoder_id: self.identity.encoder_id.clone(),
        };

        let Some(qp) = cfg.qp else {
            let tier = select_rate_tier(&caps, false);
            self.configure_adaptive(cfg, report, advertised);
            self.finish_rate_tier(tier, caps, None, Some(cfg.bitrate_bps));
            return;
        };

        let q = qp.clamp(1, 51);
        let mut tier = select_rate_tier(&caps, true);
        if tier == RateTier::LowLatencyBitrate {
            // A low-latency session without per-frame QP: the QP clamp is the next best thing,
            // and whether it holds is only known by setting it and reading it back.
            let (max_qp, min_qp) = self.try_qp_clamp(q, report, advertised);
            caps.max_qp = max_qp;
            caps.min_qp = min_qp;
            tier = select_rate_tier(&caps, true);
        }
        let mut bps = None;
        if tier == RateTier::ClampCeiling {
            match self.set_constant_qp_ceiling(report, advertised) {
                Ceiling::Accepted(_) | Ceiling::ForcedUncapped => {}
                Ceiling::NoneAccepted => {
                    // A clamp with no ceiling is the configuration measured to drop 86 % of a
                    // window drag: the rate controller keeps running with no budget to spend. The
                    // clamp comes off again and the session takes the next tier down.
                    let s = self.vt_session();
                    clear_optional(s, "MaxAllowedFrameQP");
                    clear_optional(s, "MinAllowedFrameQP");
                    teprintln!(
                        "[vt] constant QP got no bitrate ceiling on this session; the QP clamp is \
                         cleared and the session falls to LowLatencyBitrate instead of running a \
                         rate controller with no budget"
                    );
                    tier = RateTier::LowLatencyBitrate;
                }
            }
        }
        match tier {
            RateTier::BaseFrameQp => {
                // No `AverageBitRate`, no QP limits, no `Quality`: the per-frame QP is the whole
                // of the rate control, and with no budget the encoder has nothing to drop for.
                let r = self.set_frame_opt("BaseFrameQP", Some(Val::Int(q as i64)));
                if r.as_ref().map(|r| r.outcome) != Some(vt_recipe::Outcome::Included) {
                    teprintln!("[vt] BaseFrameQP was not put in the frame options: {r:?}");
                }
            }
            RateTier::ClampCeiling => {}
            RateTier::LowLatencyBitrate | RateTier::StandardBitrate => {
                let target = constant_quality_bitrate(cfg.width, cfg.height, cfg.fps, q);
                let n = CFNumber::new_i32(target as i32);
                report.note(
                    "AverageBitRate",
                    set_named(
                        self.vt_session(),
                        unsafe { kVTCompressionPropertyKey_AverageBitRate },
                        &n,
                    ),
                    advertised,
                );
                bps = Some(target);
            }
            RateTier::AdaptiveLowLatency | RateTier::AdaptiveStandard => {}
        }
        self.finish_rate_tier(tier, caps, Some(q), bps);
    }

    /// Adaptive mode: the loss-based controller owns `AverageBitRate`.
    fn configure_adaptive(
        &self,
        cfg: &EncoderConfig,
        report: &mut PropReport,
        advertised: &HashSet<String>,
    ) {
        let s = self.vt_session();
        // Quality ceiling only: a floor on QP can only shrink the stream, so the loss-based
        // controller keeps the top end. Pinning MaxAllowedFrameQP too would be constant QP.
        if self.low_latency {
            if let Some(q) = cfg.min_qp.filter(|&q| q > 1) {
                let n = CFNumber::new_i32(q as i32);
                report.note(
                    "MinAllowedFrameQP",
                    set_optional(s, "MinAllowedFrameQP", &n),
                    advertised,
                );
            }
        }
        let n = CFNumber::new_i32(cfg.bitrate_bps as i32);
        report.note(
            "AverageBitRate",
            set_named(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &n),
            advertised,
        );
        // No `DataRateLimits`. It is a hard cap enforced by stuffing or by dropping quality
        // inside a short window, layered on top of a target the loss-based controller is already
        // steering from the network's own signal. Two controllers on one knob, and the fast one
        // could not see what the slow one had done.
        if self.low_latency {
            let n = CFNumber::new_i32(45);
            report.note(
                "MaxAllowedFrameQP",
                set_optional(s, "MaxAllowedFrameQP", &n),
                advertised,
            );
        }
    }

    /// Set `MaxAllowedFrameQP` and `MinAllowedFrameQP` to `q` and read both back. When either
    /// does not hold, the one that did is cleared again: half a clamp is neither tier.
    fn try_qp_clamp(
        &self,
        q: u8,
        report: &mut PropReport,
        advertised: &HashSet<String>,
    ) -> (Probe, Probe) {
        let s = self.vt_session();
        let n = CFNumber::new_i32(q as i32);
        let max = set_optional(s, "MaxAllowedFrameQP", &n);
        report.note("MaxAllowedFrameQP", max, advertised);
        let min = set_optional(s, "MinAllowedFrameQP", &n);
        report.note("MinAllowedFrameQP", min, advertised);
        let max_p = clamp_probe(s, "MaxAllowedFrameQP", max, q);
        let min_p = clamp_probe(s, "MinAllowedFrameQP", min, q);
        if !(max_p == Probe::Applied && min_p == Probe::Applied) {
            for (name, p) in [("MaxAllowedFrameQP", max_p), ("MinAllowedFrameQP", min_p)] {
                if p == Probe::Applied {
                    clear_optional(s, name);
                }
            }
        }
        (max_p, min_p)
    }

    /// Record the tier, say it once, and publish it for the compatibility report.
    fn finish_rate_tier(
        &mut self,
        tier: RateTier,
        caps: Capabilities,
        q: Option<u8>,
        bps: Option<u32>,
    ) {
        tprintln!(
            "[vt] rate control: {}",
            describe_rate_tier(tier, &caps, q, bps)
        );
        self.identity.rate_tier = Some(tier.as_str());
        self.identity.supports_base_frame_qp = caps.supports_base_frame_qp;
        *LAST_SESSION_RATE.lock_recover() = Some(format!(
            "{} {}",
            short_encoder_id(caps.encoder_id.as_deref()),
            tier.as_str()
        ));
        self.rate_tier = Some(tier);
        self.rate_bps = bps;
        self.caps = caps;
    }

    /// What drives the bitrate, for the "encoder ready" line: a constant-quality session must not
    /// print the adaptive default as if it applied.
    fn rate_summary(&self, cfg: &EncoderConfig) -> String {
        match (self.rate_tier, cfg.qp) {
            (Some(RateTier::BaseFrameQp), Some(q)) => format!("constant QP {q} (per-frame QP)"),
            (Some(RateTier::ClampCeiling), Some(q)) => format!("constant QP {q} (clamp + ceiling)"),
            (Some(_), Some(q)) => match self.rate_bps {
                Some(b) => format!("constant quality q={q} as {} kbps", b / 1000),
                None => format!("constant quality q={q}"),
            },
            _ => format!("{} kbps start (adaptive)", cfg.bitrate_bps / 1000),
        }
    }

    /// Apply this encoder family's entry in [`FAMILY_EXTRAS`], after the tier's own settings.
    fn apply_family_extras(&mut self) {
        let family = encoder_family(self.identity.encoder_id.as_deref(), self.identity.hardware);
        let Some(extras) = extras_for(FAMILY_EXTRAS, family) else {
            tprintln!(
                "[vt] encoder family {}: no per-family extras",
                family.as_str()
            );
            return;
        };
        let mut line = format!("[vt] encoder family {} extras:", family.as_str());
        for (name, value) in extras.props {
            let r = vt_recipe::set_prop(self.vt_session(), name, &value.to_val(), None, false);
            let _ = write!(line, " {name}={}", r.short());
        }
        if extras.flush_each_frame {
            self.flush_each_frame = true;
            line.push_str(" flush_each_frame=on");
        }
        tprintln!("{line}");
    }

    /// Give constant QP a bitrate it cannot run out of.
    ///
    /// The low-latency encoder has no constant-QP mode: it rejects `Quality`, and the QP limits
    /// only clamp a rate controller that keeps running. With no `AverageBitRate` that controller
    /// held a recorded 1080p60 window drag to about 2 Mbps and, unable to raise QP, dropped 86 %
    /// of the frames. A ceiling is not a target; the same take sends 10.9 Mbps under any of these.
    fn set_constant_qp_ceiling(
        &self,
        report: &mut PropReport,
        advertised: &HashSet<String>,
    ) -> Ceiling {
        let plan = ceiling_plan(qp_bps_hook().as_deref());
        let candidates: &[i32] = match &plan {
            CeilingPlan::Uncapped => {
                teprintln!(
                    "[vt] !!!!! SE_VT_QP_BPS=uncapped TEST HOOK: constant QP runs the clamp with \
                     NO bitrate ceiling, the configuration measured to drop most frames"
                );
                return Ceiling::ForcedUncapped;
            }
            CeilingPlan::Try(list) => list,
        };
        let key = unsafe { kVTCompressionPropertyKey_AverageBitRate };
        for &bps in candidates {
            let n = CFNumber::new_i32(bps);
            let outcome = set_named(self.vt_session(), key, &n);
            let accepted = matches!(outcome, PropOutcome::Set);
            report.note("AverageBitRate", outcome, advertised);
            if accepted {
                tprintln!(
                    "[vt] constant QP runs under a {} Mbps ceiling",
                    bps / 1_000_000
                );
                return Ceiling::Accepted(bps);
            }
        }
        Ceiling::NoneAccepted
    }

    pub fn submit(
        &mut self,
        image_buffer: &CVImageBuffer,
        force_idr: bool,
        tag: FrameTag,
    ) -> Result<()> {
        let st = self.submit_ex(image_buffer, force_idr, tag);
        if st.encode != 0 {
            bail!(
                "VTCompressionSessionEncodeFrame failed: OSStatus {}",
                st.encode
            );
        }
        Ok(())
    }

    /// [`Encoder::submit`] returning the raw statuses instead of an error.
    pub fn submit_ex(
        &mut self,
        image_buffer: &CVImageBuffer,
        force_idr: bool,
        tag: FrameTag,
    ) -> SubmitStatus {
        let pts = CMTime {
            value: self.frame_index,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let dur = CMTime {
            value: 1,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        self.frame_index += 1;

        let frame_props = match (&self.frame_override, &self.recipe) {
            (Some(f), _) if force_idr => f.idr.clone(),
            (Some(f), _) => f.plain.clone(),
            (None, Some(r)) if force_idr => r.frame_idr.clone(),
            (None, Some(r)) => r.frame_plain.clone(),
            (None, None) if force_idr && !vt_recipe::denied(KeyKind::FrameOpt, "ForceKeyFrame") => {
                dict_from_pairs(&[(
                    unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame },
                    cfbool(true),
                )])
            }
            (None, None) => None,
        };

        let seq = self.submits + 1;
        let source_refcon = Box::into_raw(Box::new(SubmittedTag {
            tag,
            forced: force_idr,
            seq,
        }))
        .cast::<c_void>();
        self.in_flight.fetch_add(1, Ordering::AcqRel);

        let mut info = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                image_buffer,
                pts,
                dur,
                frame_props.as_deref(),
                source_refcon,
                &mut info,
            )
        };
        if status != 0 {
            // A rejected frame never reaches the output callback, so its tag and its slot come
            // back here or they leak.
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            drop(unsafe { Box::from_raw(source_refcon.cast::<SubmittedTag>()) });
            return SubmitStatus {
                encode: status,
                flush: None,
            };
        }
        self.submits = seq;
        self.sim().sim_submitted(seq);
        let flush_each =
            self.flush_each_frame || self.recipe.as_ref().is_some_and(|r| r.flush_each_frame);
        let flush = match flush_each {
            true => Some(unsafe {
                self.session.complete_frames(CMTime {
                    value: 0,
                    timescale: 0,
                    flags: CMTimeFlags(0),
                    epoch: 0,
                })
            }),
            false => None,
        };
        SubmitStatus {
            encode: status,
            flush,
        }
    }

    pub fn output(&self) -> Receiver<CompressedSample> {
        self.rx.clone()
    }

    /// Frames VideoToolbox has been handed and not yet emitted.
    pub fn frames_in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Frames VideoToolbox has emitted, whether or not they carried data. The encode loop watches
    /// it to tell an encoder that is keeping up from one that is holding frames.
    pub fn frames_emitted(&self) -> u64 {
        self.emitted.load(Ordering::Acquire)
    }

    /// The host-clock time (`streamer::audio::host_now_ns`) of emission number `n` (1-based,
    /// counting every callback this encoder made), if the slot still holds it. VideoToolbox makes
    /// exactly one output callback per accepted frame, in decode order, and with
    /// `AllowFrameReordering = false` that is submit order — so emission `n` is submit `n`.
    pub fn emit_time_ns(&self, n: u64) -> Option<u64> {
        let slot = (n as usize) % EMIT_LOG_LEN;
        (self.emit_log.seq[slot].load(Ordering::Acquire) == n)
            .then(|| self.emit_log.ns[slot].load(Ordering::Relaxed))
    }

    /// Frames accepted since this encoder was created; frame `k` has been emitted once
    /// `frames_emitted() >= k`.
    pub fn frames_submitted(&self) -> u64 {
        self.submits
    }

    fn sim(&self) -> &OutputCtx {
        // SAFETY: `refcon` is created non-null in both constructors and freed only in `Drop`,
        // after the last use of this.
        unsafe { &*self.refcon }
    }

    /// Encoded access units discarded because the drain thread fell behind. Any non-zero value
    /// means the reference chain is broken and the stream needs an IDR.
    pub fn output_drops(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Frames VideoToolbox took and returned with no picture. The reference chain is intact, so
    /// unlike `output_drops` this needs no IDR; it is a frame the viewer never gets.
    pub fn rate_drops(&self) -> u64 {
        self.rate_dropped.load(Ordering::Relaxed)
    }

    /// Of [`Encoder::rate_drops`], the frames that were submitted with `ForceKeyFrame`. Unlike an
    /// ordinary dropped frame this one matters: the keyframe somebody asked for never happened,
    /// so the encode loop re-arms the request when this moves.
    pub fn forced_keyframe_drops(&self) -> u64 {
        self.forced_dropped.load(Ordering::Relaxed)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.complete_pending()
    }

    /// `VTCompressionSessionCompleteFrames`' raw status, for callers that record it.
    pub fn complete_frames_status(&mut self) -> i32 {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        let st = unsafe { self.session.complete_frames(invalid) };
        self.sim().sim_release_all();
        st
    }

    /// Block until VideoToolbox has emitted everything it holds.
    ///
    /// Only for teardown and for the probe harness. Never call it from the encode loop: it is
    /// synchronous, unbounded, and that loop runs on the deadline scheduler.
    pub fn complete_pending(&mut self) -> Result<()> {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        let st = unsafe { self.session.complete_frames(invalid) };
        self.sim().sim_release_all();
        if st != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: OSStatus {st}");
        }
        Ok(())
    }

    /// Retarget the loss-based controller (adaptive mode only).
    ///
    /// Every constant-quality tier ignores it and says so with `IgnoredConstantQp` — including
    /// [`RateTier::LowLatencyBitrate`] and [`RateTier::StandardBitrate`], which do set an
    /// `AverageBitRate`: there it is derived from the quality slider
    /// ([`constant_quality_bitrate`]), so the slider, not the network, owns the rate.
    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.qp_mode {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        let s = self.vt_session();
        let n = CFNumber::new_i32(bps as i32);
        set_cftype(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &n);
        Ok(BitrateApplied::Applied)
    }

    pub fn low_latency(&self) -> bool {
        self.low_latency
    }

    /// The rate-control tier, `None` for a recipe-built session.
    pub fn rate_tier(&self) -> Option<RateTier> {
        self.rate_tier
    }

    /// The tier's name for logs and telemetry; `recipe` for a recipe-built session.
    pub fn rate_tier_name(&self) -> &'static str {
        self.rate_tier.map_or("recipe", RateTier::as_str)
    }

    /// The capabilities the tier was chosen from.
    pub fn capabilities(&self) -> &Capabilities {
        &self.caps
    }

    /// `EncoderID` without the `com.apple.videotoolbox.videoencoder.` prefix (`h264.rtvc`).
    pub fn encoder_id_short(&self) -> &str {
        short_encoder_id(self.identity.encoder_id.as_deref())
    }

    /// Whether this session's encoder advertises long-term reference frames. The transport needs
    /// it to decide between a PLI→IDR and a token-acknowledged refresh.
    pub fn supports_ltr(&self) -> bool {
        self.supports_ltr
    }

    /// Keep `low_latency` only if VideoToolbox's answer agrees.
    ///
    /// "Created with the resolved key" is what we *asked* for. An OS that resolves the symbol but
    /// does not honour it for this size or this machine hands back a standard encoder without an
    /// error — the same failure the literal-string ladder had on 10.15. So the session counts as
    /// low-latency only when its `EncoderID` is the low-latency family (`rtvc`) or it reads
    /// `SupportsBaseFrameQP` true; otherwise it is a standard session everywhere (profile, rate
    /// tier, adaptive configuration). Reads `SupportsBaseFrameQP` once, for the tier as well.
    fn confirm_low_latency(&mut self) {
        let bfq = read_bool_prop(self.vt_session(), "SupportsBaseFrameQP");
        self.identity.supports_base_frame_qp = bfq;
        self.low_latency_requested = self.low_latency;
        let confirmed = low_latency_confirmed(self.identity.encoder_id.as_deref(), bfq);
        if self.low_latency && !confirmed {
            teprintln!(
                "[vt] the session was created with EnableLowLatencyRateControl but VideoToolbox \
                 returned {} (SupportsBaseFrameQP={}); treating it as a standard session",
                self.encoder_id_short(),
                match bfq {
                    Some(v) => v.to_string(),
                    None => "unreadable".into(),
                }
            );
            self.low_latency = false;
        }
    }

    /// Ask the session what it actually became.
    ///
    /// VideoToolbox picks the encoder implementation for us and the choice is worth more than
    /// any property: forum 748918 measured one-in/one-out on `…h264.gva` against a 4-to-16-frame
    /// hold on `…h264.ave.avc` under an identical configuration. `EncoderID` (10.13) names it,
    /// `UsingHardwareAcceleratedVideoEncoder` (10.9) says whether it is hardware at all, and
    /// `UsingGPURegistryID` (10.15) says which GPU it landed on — NULL there means *either* an
    /// integrated-GPU encoder or software, so it can never stand in for the hardware flag.
    fn read_identity(&mut self) {
        let s = self.vt_session();
        self.identity = EncoderIdentity {
            rate_tier: self.identity.rate_tier,
            supports_base_frame_qp: self.identity.supports_base_frame_qp,
            encoder_id: copy_string(s, unsafe { kVTCompressionPropertyKey_EncoderID }),
            hardware: copy_bool(s, unsafe {
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder
            }),
            gpu_registry_id: copy_i64(s, unsafe { kVTCompressionPropertyKey_UsingGPURegistryID }),
        };
        tprintln!(
            "[vt] encoder identity: id={} hardware={} gpu_registry_id={}",
            self.identity.encoder_id.as_deref().unwrap_or("unknown"),
            match self.identity.hardware {
                Some(v) => v.to_string(),
                None => "unknown".to_string(),
            },
            match self.identity.gpu_registry_id {
                Some(v) => v.to_string(),
                None => "none (integrated or software)".to_string(),
            }
        );
        if self.identity.hardware == Some(false) {
            if self.software_requested {
                tprintln!(
                    "[vt] this session is on the software H.264 encoder because software encode \
                     is switched on. Expect higher latency and CPU; that is the setting working."
                );
            } else {
                teprintln!(
                    "[vt] this session is on the SOFTWARE H.264 encoder. Expect higher latency \
                     and CPU. The usual causes are an encode size below this Mac's hardware \
                     minimum (~640x480 on Intel) and another application already holding the \
                     hardware encoder; both are machine-wide, not specific to this device."
                );
            }
        }
    }

    pub fn identity(&self) -> &EncoderIdentity {
        &self.identity
    }

    /// The tier string reported to `CompatibilityReport` and the telemetry summary.
    pub fn backend_name(&self) -> &'static str {
        match self.identity.hardware {
            Some(true) => "videotoolbox-hardware",
            Some(false) => "videotoolbox-software",
            None => "videotoolbox",
        }
    }

    /// Live count of frames VideoToolbox is holding, by its own reckoning. Cross-check against
    /// [`Encoder::frames_in_flight`]: if the two disagree the encoder is holding frames for a
    /// reason of its own and no external cap will help.
    pub fn pending_frames(&self) -> Option<i64> {
        copy_i64(self.vt_session(), unsafe {
            kVTCompressionPropertyKey_NumberOfPendingFrames
        })
    }
}
/// What VideoToolbox turned our request into.
#[derive(Clone, Debug, Default)]
pub struct EncoderIdentity {
    pub encoder_id: Option<String>,
    pub hardware: Option<bool>,
    pub gpu_registry_id: Option<i64>,
    /// [`RateTier::as_str`] of the tier the session runs; `None` for a recipe-built session.
    pub rate_tier: Option<&'static str>,
    /// The session's `SupportsBaseFrameQP` read-back (`None` = not readable / no symbol).
    pub supports_base_frame_qp: Option<bool>,
}

// ---------------------------------------------------------------------------
// Rate-control tiers: chosen per session from what the session can do.
// ---------------------------------------------------------------------------

/// What rides through VideoToolbox as `sourceFrameRefCon`: the caller's stamps plus whether the
/// frame carried `ForceKeyFrame`, so the output callback can tell a lost keyframe from a lost
/// frame.
struct SubmittedTag {
    tag: FrameTag,
    forced: bool,
    /// The submit number (1-based, per encoder), for the hold simulation.
    seq: u64,
}

/// `kVTEncodeInfo_FrameDropped` (bit 1), or success with no sample: rate control declined to emit.
fn is_rate_drop_raw(status: i32, info_flags: u32, has_sample: bool) -> bool {
    status == 0 && (info_flags & 2 != 0 || !has_sample)
}

/// A frame submitted with `ForceKeyFrame` was dropped by rate control: the IDR never happened and
/// must be asked for again. An ordinary dropped frame needs nothing — it never became a
/// reference, so the chain is intact. (WebKit re-requests in exactly this case.)
fn forced_keyframe_lost(forced: bool, status: i32, info_flags: u32, has_sample: bool) -> bool {
    forced && is_rate_drop_raw(status, info_flags, has_sample)
}

/// How a session's bitrate is controlled. The first four are constant-quality (`cfg.qp`) tiers,
/// best first; the last two are adaptive mode, named so the telemetry always carries a tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RateTier {
    /// Low-latency session, `SupportsBaseFrameQP` reads true and
    /// `kVTEncodeFrameOptionKey_BaseFrameQP` resolves: QP in every frame's options, no budget.
    /// True constant QP (measured on M1 / macOS 26: 0 drops, level 4.2, `max_num_ref_frames` 4).
    BaseFrameQp,
    /// Low-latency session without per-frame QP whose Min/MaxAllowedFrameQP both set and read
    /// back: the clamp plus a bitrate ceiling ([`CONSTANT_QP_CEILINGS_BPS`], `SE_VT_QP_BPS`).
    ClampCeiling,
    /// Low-latency session whose QP keys are missing or refused: `AverageBitRate` from q.
    LowLatencyBitrate,
    /// Any session not created with the resolved low-latency key (`ave.*`, `gva`, software):
    /// `AverageBitRate` from q; no `Quality`, no QP limits (rejected there, or a stall on
    /// `ave.avc`).
    StandardBitrate,
    /// Adaptive mode on a low-latency session: the loss-based controller's `AverageBitRate`,
    /// with the QP floor and the 45 ceiling.
    AdaptiveLowLatency,
    /// Adaptive mode on a standard session: `AverageBitRate` only.
    AdaptiveStandard,
}

impl RateTier {
    pub fn as_str(self) -> &'static str {
        match self {
            RateTier::BaseFrameQp => "BaseFrameQp",
            RateTier::ClampCeiling => "ClampCeiling",
            RateTier::LowLatencyBitrate => "LowLatencyBitrate",
            RateTier::StandardBitrate => "StandardBitrate",
            RateTier::AdaptiveLowLatency => "AdaptiveLowLatency",
            RateTier::AdaptiveStandard => "AdaptiveStandard",
        }
    }

    /// The tiers the encode loop's drop guard watches: a low-latency constant-quality session,
    /// whose rate controller can still drop frames. (`BaseFrameQp` never has, but is watched the
    /// same way.) The guard's fallback, `StandardBitrate`, is not watched: it never flaps back.
    pub fn drop_guarded(self) -> bool {
        matches!(
            self,
            RateTier::BaseFrameQp | RateTier::ClampCeiling | RateTier::LowLatencyBitrate
        )
    }
}

/// The outcome of setting a property and reading it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Probe {
    #[default]
    NotTried,
    Applied,
    Refused,
}

impl Probe {
    fn as_str(self) -> &'static str {
        match self {
            Probe::NotTried => "not tried",
            Probe::Applied => "applied",
            Probe::Refused => "refused",
        }
    }
}

/// Everything the tier is chosen from, each probed on the session itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// `kVTVideoEncoderSpecification_EnableLowLatencyRateControl` resolves by `dlsym`.
    pub low_latency_key_resolved: bool,
    /// The session was created with that resolved key. Only counts as low-latency when the
    /// read-back confirms it ([`low_latency_confirmed`]).
    pub low_latency_session: bool,
    /// `SupportsBaseFrameQP` read back from the session (`None`: no symbol, or the read failed —
    /// `ave.avc` answers -12900).
    pub supports_base_frame_qp: Option<bool>,
    /// `kVTEncodeFrameOptionKey_BaseFrameQP` resolves by `dlsym`.
    pub frame_qp_key_resolved: bool,
    /// `MaxAllowedFrameQP = q`, set and read back.
    pub max_qp: Probe,
    /// `MinAllowedFrameQP = q`, set and read back.
    pub min_qp: Probe,
    /// `EncoderID` as read back.
    pub encoder_id: Option<String>,
}

/// The tier for a session with these capabilities. Pure, so every branch is unit-tested and the
/// bench's `SE_VT_DENY` hook can drive each one on a Mac that has everything.
///
/// The QP probes are only consulted when per-frame QP is unavailable: on a session that gets
/// [`RateTier::BaseFrameQp`] no QP limit is ever set, so they stay `NotTried` there (and a
/// `NotTried` clamp selects [`RateTier::LowLatencyBitrate`], which is the caller's cue to probe).
pub fn select_rate_tier(c: &Capabilities, constant_quality: bool) -> RateTier {
    let low_latency = c.low_latency_session
        && c.low_latency_key_resolved
        && low_latency_confirmed(c.encoder_id.as_deref(), c.supports_base_frame_qp);
    if !constant_quality {
        return if low_latency {
            RateTier::AdaptiveLowLatency
        } else {
            RateTier::AdaptiveStandard
        };
    }
    if !low_latency {
        return RateTier::StandardBitrate;
    }
    if c.supports_base_frame_qp == Some(true) && c.frame_qp_key_resolved {
        return RateTier::BaseFrameQp;
    }
    if c.max_qp == Probe::Applied && c.min_qp == Probe::Applied {
        return RateTier::ClampCeiling;
    }
    RateTier::LowLatencyBitrate
}

/// Whether a session created with the low-latency key really is one, by what VideoToolbox
/// returned: the low-latency encoder family, or per-frame QP support (a low-latency-only
/// capability).
pub fn low_latency_confirmed(
    encoder_id: Option<&str>,
    supports_base_frame_qp: Option<bool>,
) -> bool {
    encoder_family(encoder_id, None) == EncoderFamily::Rtvc || supports_base_frame_qp == Some(true)
}

/// `AverageBitRate` standing in for constant quality `q` on an encoder with no QP control.
///
/// `base = clamp(w·h·fps / 10, 6 Mbps, 30 Mbps)` (the adaptive default, `live_encoder_config`),
/// then `base · 1.6 · 2^((15 − q) / 6)` — the usual "6 QP steps double the bits" — clamped to
/// 2 … 62.5 Mbps. **The constants are a first fit, to be refined from data**: on the 2012 Intel
/// MacBook Pro (`h264.gva`, 10.15.8) 20 Mbps gave 47.6 dB luma PSNR on the synthetic stress clip,
/// the picture quality the M1 reaches at QP 15; 1080p60 at q = 15 lands at 19.9 Mbps here.
pub fn constant_quality_bitrate(width: u32, height: u32, fps: u32, q: u8) -> u32 {
    let base = ((width as u64 * height as u64 * fps as u64) / 10).clamp(6_000_000, 30_000_000);
    let scale = 1.6 * 2f64.powf((15.0 - q.clamp(1, 51) as f64) / 6.0);
    (base as f64 * scale).clamp(2_000_000.0, 62_500_000.0) as u32
}

/// `EncoderID` without the common prefix: `com.apple.videotoolbox.videoencoder.h264.rtvc` →
/// `h264.rtvc`. `unknown` when it was not readable.
pub fn short_encoder_id(id: Option<&str>) -> &str {
    match id {
        Some(id) => id
            .strip_prefix("com.apple.videotoolbox.videoencoder.")
            .unwrap_or(id),
        None => "unknown",
    }
}

/// The one log line naming the tier and why.
fn describe_rate_tier(tier: RateTier, c: &Capabilities, q: Option<u8>, bps: Option<u32>) -> String {
    let id = short_encoder_id(c.encoder_id.as_deref());
    let bfq = match c.supports_base_frame_qp {
        Some(v) => v.to_string(),
        None => "unreadable".to_string(),
    };
    let q = q.map(|q| format!(" q={q}")).unwrap_or_default();
    let rate = bps
        .map(|b| format!(" AverageBitRate={:.1} Mbps", b as f64 / 1e6))
        .unwrap_or_default();
    let why_not_ll = if !c.low_latency_key_resolved {
        "EnableLowLatencyRateControl does not resolve on this OS"
    } else if c.low_latency_session {
        "created with EnableLowLatencyRateControl, but VideoToolbox returned a standard encoder"
    } else {
        "not a low-latency session (create refused, skipped, or the drop-guard fallback)"
    };
    match tier {
        RateTier::BaseFrameQp => format!("BaseFrameQp{q} ({id}, SupportsBaseFrameQP={bfq})"),
        RateTier::ClampCeiling => format!(
            "ClampCeiling{q} ({id}, SupportsBaseFrameQP={bfq}, BaseFrameQP frame key {}; \
             Min/MaxAllowedFrameQP {}/{})",
            if c.frame_qp_key_resolved {
                "resolves"
            } else {
                "missing"
            },
            c.min_qp.as_str(),
            c.max_qp.as_str()
        ),
        RateTier::LowLatencyBitrate => {
            format!(
            "LowLatencyBitrate{q}{rate} ({id}, SupportsBaseFrameQP={bfq}, BaseFrameQP frame key \
             {}; Min/MaxAllowedFrameQP {}/{})",
            if c.frame_qp_key_resolved { "resolves" } else { "missing" },
            c.min_qp.as_str(),
            c.max_qp.as_str()
        )
        }
        RateTier::StandardBitrate => format!("StandardBitrate{q}{rate} ({id}, {why_not_ll})"),
        RateTier::AdaptiveLowLatency => format!("AdaptiveLowLatency{rate} start ({id})"),
        RateTier::AdaptiveStandard => {
            format!("AdaptiveStandard{rate} start ({id}, {why_not_ll})")
        }
    }
}

/// The encoder families the per-family extras table is keyed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderFamily {
    /// `…h264.rtvc`, the low-latency encoder (Apple silicon, recent Intel).
    Rtvc,
    /// `…ave.avc` and siblings, Apple silicon's standard hardware encoder.
    Ave,
    /// `…h264.gva`, Intel Quick Sync.
    Gva,
    Software,
    Other,
}

impl EncoderFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            EncoderFamily::Rtvc => "rtvc",
            EncoderFamily::Ave => "ave",
            EncoderFamily::Gva => "gva",
            EncoderFamily::Software => "software",
            EncoderFamily::Other => "other",
        }
    }
}

pub fn encoder_family(encoder_id: Option<&str>, hardware: Option<bool>) -> EncoderFamily {
    let id = short_encoder_id(encoder_id);
    if id.ends_with("rtvc") {
        EncoderFamily::Rtvc
    } else if id.starts_with("ave.") || id.contains(".ave.") {
        EncoderFamily::Ave
    } else if id.ends_with("gva") {
        EncoderFamily::Gva
    } else if hardware == Some(false) {
        EncoderFamily::Software
    } else {
        EncoderFamily::Other
    }
}

/// A property value the extras table can hold as a constant.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))] // the shipped table is empty
pub enum ExtraVal {
    Bool(bool),
    Int(i64),
    Float(f64),
}

impl ExtraVal {
    fn to_val(self) -> Val {
        match self {
            ExtraVal::Bool(b) => Val::Bool(b),
            ExtraVal::Int(i) => Val::Int(i),
            ExtraVal::Float(f) => Val::Float(f),
        }
    }
}

/// Extra properties for one encoder family, applied after the rate tier's own settings, each
/// outcome logged.
///
/// **Undocumented properties are dangerous per encoder**: on `gva`, `EncoderUsage = 1` segfaults
/// the process and a rejected `EncoderUsage = 2` leaves the session returning -12903 for every
/// frame; on `rtvc`, `NumberOfSlices = 8` drops every frame. So nothing goes in here unless it was
/// measured on that exact family, and nothing is ever *tried* on a session that will carry frames.
#[cfg_attr(not(test), allow(dead_code))] // the shipped table is empty
pub struct FamilyExtras {
    pub family: EncoderFamily,
    /// Short `kVTCompressionPropertyKey_` names, set in order through `vt_recipe::set_prop`.
    pub props: &'static [(&'static str, ExtraVal)],
    /// `VTCompressionSessionCompleteFrames` after every submit.
    pub flush_each_frame: bool,
}

/// Per-family extras. **Ships empty**: to be filled from a quiet-machine latency round.
/// `SE_VT_RECIPE` still overrides everything.
pub const FAMILY_EXTRAS: &[FamilyExtras] = &[];

fn extras_for(
    table: &'static [FamilyExtras],
    family: EncoderFamily,
) -> Option<&'static FamilyExtras> {
    table.iter().find(|e| e.family == family)
}

/// `"<encoder id> <tier>"` of the most recent production session, for the compatibility report.
static LAST_SESSION_RATE: Mutex<Option<String>> = Mutex::new(None);

/// The capability probe for the low-latency rung: the specification key's symbol resolves.
/// Honours `SE_VT_DENY spec:EnableLowLatencyRateControl`.
fn low_latency_key_resolves() -> bool {
    vt_recipe::resolve_key(KeyKind::Spec, "EnableLowLatencyRateControl").is_ok()
}

/// A boolean compression property by short name (`None`: no symbol, unreadable or not a bool).
fn read_bool_prop(session: &VTSession, short_key: &str) -> Option<bool> {
    match vt_recipe::read_prop(session, short_key).readback {
        Some(serde_json::Value::Bool(b)) => Some(b),
        _ => None,
    }
}

/// An integer compression property by short name.
fn read_i64_prop(session: &VTSession, short_key: &str) -> Option<i64> {
    let key = vt_optional_cfstring(&format!("kVTCompressionPropertyKey_{short_key}"))?;
    copy_i64(session, key)
}

/// Whether a QP limit set to `q` holds: accepted, and a read-back (where one is possible) agrees.
fn clamp_probe(session: &VTSession, short_key: &str, outcome: PropOutcome, q: u8) -> Probe {
    if outcome != PropOutcome::Set {
        return Probe::Refused;
    }
    match read_i64_prop(session, short_key) {
        Some(v) if v != q as i64 => {
            teprintln!("[vt] {short_key} accepted {q} but reads back {v}; treating it as refused");
            Probe::Refused
        }
        Some(_) => Probe::Applied,
        None => {
            tprintln!(
                "[vt] {short_key} accepted {q}; no read-back available, taking the set status"
            );
            Probe::Applied
        }
    }
}

/// Take a QP limit off again: back to its default (NULL), or — where the encoder refuses NULL, as
/// `h264.rtvc` does (-50 / -12902, measured on M1 / macOS 26) — to the widest value, 51 for the
/// ceiling and 1 for the floor, read back. Raise the ceiling before lowering the floor.
fn clear_optional(session: &VTSession, short_key: &str) {
    let Some(k) = vt_optional_cfstring(&format!("kVTCompressionPropertyKey_{short_key}")) else {
        return;
    };
    let st = unsafe { VTSessionSetProperty(session, k, None) };
    if st == 0 {
        tprintln!("[vt] cleared {short_key}");
        return;
    }
    let widest = qp_limit_widest(short_key);
    let n = CFNumber::new_i32(widest);
    let st2 = unsafe { VTSessionSetProperty(session, k, Some(&n)) };
    let back = read_i64_prop(session, short_key);
    let held = st2 == 0 && back.is_none_or(|v| v == widest as i64);
    let line = format!(
        "[vt] {short_key}: NULL refused (OSStatus {st}); set to {widest} instead: OSStatus {st2}, \
         reads back {back:?}"
    );
    if held {
        tprintln!("{line}");
    } else {
        teprintln!("{line} — the limit may still be in force");
    }
}

/// The value that makes a QP limit no limit at all.
fn qp_limit_widest(short_key: &str) -> i32 {
    if short_key.starts_with("Min") {
        1
    } else {
        51
    }
}

/// A log line's worth of encoder detail: the tier this Mac *has* ([`probe_encoder_backend`]),
/// what the rate-tier probes that need no session say, and the most recent session's encoder and
/// tier once one has run. Logged by `check_system_requirements`; the report's `encoder_backend`
/// itself stays the bare backend name.
pub fn encoder_backend_detail() -> String {
    let mut s = format!(
        "{} (low-latency rate control key: {}; BaseFrameQP frame key: {}",
        probe_encoder_backend(),
        if low_latency_key_resolves() {
            "present"
        } else {
            "absent"
        },
        if vt_recipe::resolve_key(KeyKind::FrameOpt, "BaseFrameQP").is_ok() {
            "present"
        } else {
            "absent"
        }
    );
    if let Some(last) = LAST_SESSION_RATE.lock_recover().as_deref() {
        let _ = write!(s, "; last session: {last}");
    }
    s.push(')');
    s
}

// ---------------------------------------------------------------------------
// Asking VideoToolbox what it can do, instead of asking `sw_vers` what year it is.
// ---------------------------------------------------------------------------

/// What `VTCopyVideoEncoderList` says this Mac has, enumerated and logged once.
///
/// The hardware encoder is a machine-wide resource with a per-encoder instance limit, so "why did
/// my session land on software" is usually answered by this list plus the identity read back after
/// creation — and neither was ever printed.
#[derive(Default)]
struct EncoderCatalog {
    has_hardware: bool,
    instance_limited: bool,
}

fn encoder_catalog() -> &'static EncoderCatalog {
    static CATALOG: OnceLock<EncoderCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut catalog = EncoderCatalog::default();
        let Some(list) = video_encoder_list() else {
            tprintln!("[vt] VTCopyVideoEncoderList unavailable; encoder inventory unknown");
            return catalog;
        };
        let key_id = vt_optional_cfstring("kVTVideoEncoderList_EncoderID");
        let key_codec = vt_optional_cfstring("kVTVideoEncoderList_CodecType");
        let key_hw = vt_optional_cfstring("kVTVideoEncoderList_IsHardwareAccelerated");
        let key_gpu = vt_optional_cfstring("kVTVideoEncoderList_GPURegistryID");
        let key_limit = vt_optional_cfstring("kVTVideoEncoderList_InstanceLimit");

        let mut found = 0usize;
        for i in 0..list.count() {
            let entry = unsafe { list.value_at_index(i) } as *const CFDictionary;
            let Some(entry) = (unsafe { entry.as_ref() }) else {
                continue;
            };
            if let Some(k) = key_codec {
                if dict_i64(entry, k) != Some(CODEC_TYPE_H264 as i64) {
                    continue;
                }
            }
            found += 1;
            let hardware = key_hw.and_then(|k| dict_bool(entry, k));
            // `InstanceLimit` is a CFBoolean, not a count: present and true means "this encoder is
            // a scarce resource which may potentially be unavailable". That is the documented
            // mechanism behind "it only works when I quit everything else".
            let capped = key_limit.and_then(|k| dict_bool(entry, k)).unwrap_or(false);
            catalog.has_hardware |= hardware.unwrap_or(false);
            catalog.instance_limited |= capped;
            tprintln!(
                "[vt] encoder: id={} hardware={} gpu_registry_id={} instance_limited={capped}",
                key_id
                    .and_then(|k| dict_string(entry, k))
                    .unwrap_or_else(|| "unknown".into()),
                hardware
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                key_gpu
                    .and_then(|k| dict_i64(entry, k))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "none".into()),
            );
        }
        if found == 0 {
            tprintln!("[vt] no H.264 encoder listed by VTCopyVideoEncoderList");
        }
        catalog
    })
}

/// The encoder tier this Mac *has*, answered without creating a session.
///
/// `CompatibilityReport` is built before any device joins, so this is the honest answer there. The
/// tier a given session actually got is separate, read back per session and reported through the
/// telemetry `encoder` slot, because a busy hardware encoder makes the two differ.
pub fn probe_encoder_backend() -> &'static str {
    if encoder_catalog().has_hardware {
        "videotoolbox-hardware"
    } else {
        "videotoolbox-software"
    }
}

fn video_encoder_list() -> Option<CFRetained<CFArray>> {
    type CopyEncoderListFn = unsafe extern "C" fn(*const CFDictionary, *mut *const CFArray) -> i32;
    let sym = dlsym_fn("VTCopyVideoEncoderList")?;
    let f: CopyEncoderListFn = unsafe { std::mem::transmute(sym) };
    let mut out: *const CFArray = ptr::null();
    let st = unsafe { f(ptr::null(), &mut out) };
    if st != 0 {
        return None;
    }
    NonNull::new(out as *mut CFArray).map(|p| unsafe { CFRetained::from_raw(p) })
}

/// The property keys this encoder advertises for this exact frame size.
///
/// `VTCopySupportedPropertyDictionaryForEncoder` has existed since 10.13 and answers directly what
/// the code used to infer from the OS version — and inference was wrong in both directions, since
/// the property set depends on the encoder VideoToolbox picks, which depends on the size and the
/// machine, not on the year.
fn supported_properties(cfg: &EncoderConfig) -> HashSet<String> {
    type CopySupportedFn = unsafe extern "C" fn(
        i32,
        i32,
        u32,
        *const CFDictionary,
        *mut *const CFString,
        *mut *const CFDictionary,
    ) -> i32;
    let mut set = HashSet::new();
    let denied = &vt_recipe::deny().props;
    let Some(sym) = dlsym_fn("VTCopySupportedPropertyDictionaryForEncoder") else {
        return set;
    };
    let f: CopySupportedFn = unsafe { std::mem::transmute(sym) };
    let spec = build_encoder_spec(false, HwPolicy::Enable, cfg.gpu_registry_id);
    let mut id_out: *const CFString = ptr::null();
    let mut props_out: *const CFDictionary = ptr::null();
    let st = unsafe {
        f(
            cfg.width as i32,
            cfg.height as i32,
            CODEC_TYPE_H264,
            spec.as_deref()
                .map_or(ptr::null(), |d| d as *const CFDictionary),
            &mut id_out,
            &mut props_out,
        )
    };
    let id = NonNull::new(id_out as *mut CFString).map(|p| unsafe { CFRetained::from_raw(p) });
    let props =
        NonNull::new(props_out as *mut CFDictionary).map(|p| unsafe { CFRetained::from_raw(p) });
    if st != 0 {
        tprintln!("[vt] VTCopySupportedPropertyDictionaryForEncoder failed: OSStatus {st}");
        return set;
    }
    if let Some(props) = props.as_deref() {
        set.extend(
            dictionary_keys(props)
                .into_iter()
                .filter(|k| !denied.contains(k)),
        );
    }
    tprintln!(
        "[vt] {}x{} would use encoder {} advertising {} properties",
        cfg.width,
        cfg.height,
        id.as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".into()),
        set.len()
    );
    set
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PropOutcome {
    Set,
    Rejected(i32),
    MissingSymbol,
    /// Deliberately not set. The report still names it, so the log answers "was this tried?"
    /// rather than leaving the reader to grep the source.
    NotRequested,
}

/// One line per session saying what each latency-relevant property actually did.
///
/// Every optional property used to be set through a helper whose `bool` return was discarded, so
/// a key whose symbol did not resolve on this OS was indistinguishable from one that applied.
#[derive(Default)]
struct PropReport {
    entries: Vec<(&'static str, PropOutcome, bool)>,
}

impl PropReport {
    fn note(&mut self, name: &'static str, outcome: PropOutcome, advertised: &HashSet<String>) {
        self.entries
            .push((name, outcome, advertised.contains(name)));
    }

    fn emit(&self) {
        let mut line = String::from("[vt] properties:");
        for (name, outcome, advertised) in &self.entries {
            let state = match outcome {
                PropOutcome::Set => "ok".to_string(),
                PropOutcome::Rejected(st) => format!("rejected({st})"),
                PropOutcome::MissingSymbol => "no-symbol".to_string(),
                PropOutcome::NotRequested if *advertised => "available".to_string(),
                PropOutcome::NotRequested => "absent".to_string(),
            };
            let _ = write!(line, " {name}={state}");
        }
        tprintln!("{line}");
    }
}

fn copy_property(session: &VTSession, key: &CFString) -> Option<CFRetained<CFType>> {
    if vt_recipe::copy_raw_denied(key) {
        return None;
    }
    let mut value: *const CFType = ptr::null();
    let st = unsafe { VTSessionCopyProperty(session, key, ptr::null(), &mut value) };
    if st != 0 {
        return None;
    }
    NonNull::new(value as *mut CFType).map(|p| unsafe { CFRetained::from_raw(p) })
}

fn copy_bool(session: &VTSession, key: &CFString) -> Option<bool> {
    let v = copy_property(session, key)?;
    v.downcast_ref::<CFBoolean>().map(|b| b.as_bool())
}

fn copy_string(session: &VTSession, key: &CFString) -> Option<String> {
    let v = copy_property(session, key)?;
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

fn copy_i64(session: &VTSession, key: &CFString) -> Option<i64> {
    let v = copy_property(session, key)?;
    v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64())
}

fn dict_value<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    let v = unsafe { dict.value((key as *const CFString).cast()) };
    unsafe { (v as *const CFType).as_ref() }
}

/// A dictionary's keys, for the ones that are CFStrings.
///
/// Turns VideoToolbox's supported-property dictionary into something a membership test can run
/// against once per session rather than a CF call per property.
fn dictionary_keys(dict: &CFDictionary) -> Vec<String> {
    let count = dict.count();
    if count <= 0 {
        return Vec::new();
    }
    let mut keys: Vec<*const c_void> = vec![ptr::null(); count as usize];
    unsafe { dict.keys_and_values(keys.as_mut_ptr(), ptr::null_mut()) };
    keys.into_iter()
        .filter_map(|k| unsafe { (k as *const CFType).as_ref() })
        .filter_map(|t| t.downcast_ref::<CFString>().map(|s| s.to_string()))
        .collect()
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    dict_value(dict, key)?
        .downcast_ref::<CFString>()
        .map(|s| s.to_string())
}

fn dict_bool(dict: &CFDictionary, key: &CFString) -> Option<bool> {
    dict_value(dict, key)?
        .downcast_ref::<CFBoolean>()
        .map(|b| b.as_bool())
}

fn dict_i64(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    dict_value(dict, key)?
        .downcast_ref::<CFNumber>()
        .and_then(|n| n.as_i64())
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        unsafe {
            let _ = self.session.complete_frames(invalid);
            self.session.invalidate();
        }
        if !self.refcon.is_null() {
            self.sim().sim_release_all();
            self.sim().sim_stop();
            drop(unsafe { Arc::from_raw(self.refcon as *const OutputCtx) });
        }
    }
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    src_frame_refcon: *mut c_void,
    status: i32,
    info: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    let callback_at = Instant::now();
    // kVTEncodeInfo_FrameDropped = 1 << 1. Diagnostic only: under SE_FRAME_TRACE=1, name every
    // frame VideoToolbox refused to emit (its rate control dropping to meet a budget).
    if crate::streamer::telemetry::frame_trace_enabled() && (info.0 & 2 != 0 || sample.is_null()) {
        tprintln!(
            "[frame-trace] vt_drop status={status} info=0x{:x} sample_null={}",
            info.0,
            sample.is_null()
        );
    }
    // Reclaim the per-frame stamps first: they were boxed in `submit` and this is the only place
    // that frees them, whatever else the callback decides to do.
    let submitted = NonNull::new(src_frame_refcon.cast::<SubmittedTag>())
        .map(|p| *unsafe { Box::from_raw(p.as_ptr()) });
    if refcon.is_null() {
        return;
    }
    let ctx = unsafe { &*(refcon as *const OutputCtx) };
    let (seq, forced, tag) = match submitted {
        Some(t) => (t.seq, t.forced, Some(t.tag)),
        None => (0, false, None),
    };
    let sample = if status == 0 {
        NonNull::new(sample).map(|p| unsafe { CFRetained::retain(p) })
    } else {
        None
    };
    ctx.arrive(Pending {
        seq,
        forced,
        tag,
        status,
        info_flags: info.0,
        sample,
        callback_at,
    });
}

fn sample_to_annexb_into(sample: &CMSampleBuffer, out: &mut Vec<u8>) -> bool {
    let Some(bb) = (unsafe { sample.data_buffer() }) else {
        return false;
    };
    let mut total: usize = 0;
    let mut base: *mut c_char = ptr::null_mut();
    let st = unsafe { bb.data_pointer(0, ptr::null_mut(), &mut total, &mut base) };
    if st != 0 || base.is_null() {
        return false;
    }
    let data = unsafe { std::slice::from_raw_parts(base as *const u8, total) };

    let mut is_keyframe = false;
    let mut off = 0usize;
    while off + NAL_LENGTH_PREFIX <= data.len() {
        let nal_len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += NAL_LENGTH_PREFIX;
        if nal_len == 0 || off + nal_len > data.len() {
            break;
        }
        if (data[off] & 0x1F) == 5 {
            is_keyframe = true;
            break;
        }
        off += nal_len;
    }

    out.reserve(total + if is_keyframe { 256 } else { 16 });

    if is_keyframe {
        if let Some(fmt) = unsafe { sample.format_description() } {
            let mut count: usize = 0;
            let _ = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &fmt,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut count,
                    ptr::null_mut(),
                )
            };
            for i in 0..count {
                let mut p: *const u8 = ptr::null();
                let mut len: usize = 0;
                let mut hdr: c_int = 0;
                let st = unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &fmt,
                        i,
                        &mut p,
                        &mut len,
                        ptr::null_mut(),
                        &mut hdr,
                    )
                };
                if st == 0 && !p.is_null() && len > 0 {
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(unsafe { std::slice::from_raw_parts(p, len) });
                }
            }
        }
    }

    let mut off = 0usize;
    while off + NAL_LENGTH_PREFIX <= data.len() {
        let nal_len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        off += NAL_LENGTH_PREFIX;
        if nal_len == 0 || off + nal_len > data.len() {
            break;
        }
        if (data[off] & 0x1F) == 9 {
            off += nal_len;
            continue;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[off..off + nal_len]);
        off += nal_len;
    }

    is_keyframe
}

/// What the session specification should say about the hardware encoder.
///
/// Three states, not two, because "prefer hardware" and "refuse hardware" are different requests
/// and VideoToolbox spells them with different keys. The middle rung exists so a machine whose
/// hardware encoder is busy still gets a session; the last one exists because the user asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HwPolicy {
    /// `RequireHardwareAcceleratedVideoEncoder` - fail the create rather than land quietly on
    /// software. This is what makes the fallback a logged decision instead of an invisible one.
    Require,
    /// `EnableHardwareAcceleratedVideoEncoder = true` - take hardware if any is free.
    Enable,
    /// `EnableHardwareAcceleratedVideoEncoder = false` - the user asked for software encode.
    Forbid,
}

fn build_encoder_spec(
    low_latency: bool,
    hw: HwPolicy,
    gpu_registry_id: Option<u64>,
) -> Option<CFRetained<CFDictionary>> {
    let hw_key = if hw == HwPolicy::Require {
        unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }
    } else {
        unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder }
    };
    // The resolved constant, never a literal string (see `low_latency_key_resolves`). A missing
    // symbol — or `SE_VT_DENY spec:EnableLowLatencyRateControl` — makes the rung not exist.
    let ll_key = if low_latency {
        match vt_recipe::resolve_key(KeyKind::Spec, "EnableLowLatencyRateControl") {
            Ok((k, _)) => Some(k),
            Err(sym) => {
                teprintln!("[vt] low-latency rung skipped: {sym} does not resolve");
                return None;
            }
        }
    } else {
        None
    };
    let hw_denied = vt_recipe::denied(
        KeyKind::Spec,
        if hw == HwPolicy::Require {
            "RequireHardwareAcceleratedVideoEncoder"
        } else {
            "EnableHardwareAcceleratedVideoEncoder"
        },
    );
    // `Preferred`, never `Required`: the header says Required silently overrides Preferred and
    // would defeat the fallback ladder on a machine whose chosen GPU has no encoder.
    let gpu_key =
        vt_optional_cfstring("kVTVideoEncoderSpecification_PreferredEncoderGPURegistryID");
    let gpu_num = gpu_registry_id.map(|id| CFNumber::new_i64(id as i64));

    let mut pairs: Vec<(&CFString, &CFType)> = Vec::with_capacity(3);
    if !hw_denied {
        pairs.push((hw_key, cfbool(hw != HwPolicy::Forbid)));
    }
    if let Some(k) = ll_key.as_deref() {
        pairs.push((k, cfbool(true)));
    }
    if let (Some(k), Some(n)) = (gpu_key, gpu_num.as_deref()) {
        let v: &CFType = n;
        pairs.push((k, v));
    }
    dict_from_pairs(&pairs)
}

fn create_session(
    cfg: &EncoderConfig,
    spec: &CFDictionary,
    refcon: *mut OutputCtx,
) -> Result<CFRetained<VTCompressionSession>, i32> {
    let mut session_ptr: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            kCFAllocatorDefault,
            cfg.width as i32,
            cfg.height as i32,
            kCMVideoCodecType_H264,
            Some(spec),
            None,
            None,
            Some(output_callback),
            refcon as *mut c_void,
            NonNull::from(&mut session_ptr),
        )
    };
    match NonNull::new(session_ptr) {
        Some(p) if status == 0 => Ok(unsafe { CFRetained::from_raw(p) }),
        _ => Err(status),
    }
}

fn cfbool(v: bool) -> &'static CFBoolean {
    CFBoolean::new(v)
}

unsafe extern "C" {
    fn VTSessionCopyProperty(
        session: &VTSession,
        property_key: &CFString,
        allocator: *const c_void,
        property_value_out: *mut *const CFType,
    ) -> i32;
}

fn dlsym_fn(symbol: &str) -> Option<*mut c_void> {
    let cname = std::ffi::CString::new(symbol).ok()?;
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
    (!p.is_null()).then_some(p)
}

fn vt_optional_cfstring(symbol: &str) -> Option<&'static CFString> {
    let p = dlsym_fn(symbol)?;
    unsafe { (*(p as *const *const CFString)).as_ref() }
}

/// Set a property, naming the key when it is refused.
///
/// `OSStatus -12900` on an anonymous key is un-diagnosable; the same status next to
/// `PrioritizeEncodingSpeedOverQuality` says exactly which encoder does not honour what.
fn set_cftype(session: &VTSession, key: &CFString, value: &CFType) -> i32 {
    if vt_recipe::prop_key_denied(key) {
        teprintln!(
            "[vt] rejected {key}: OSStatus {} (SE_VT_DENY)",
            vt_recipe::DENIED_STATUS
        );
        return vt_recipe::DENIED_STATUS;
    }
    let st = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if st != 0 {
        teprintln!("[vt] rejected {key}: OSStatus {st}");
    }
    st
}

fn set_named(session: &VTSession, key: &CFString, value: &CFType) -> PropOutcome {
    match set_cftype(session, key, value) {
        0 => PropOutcome::Set,
        st => PropOutcome::Rejected(st),
    }
}

/// Set a compression property named by its short key, resolving the constant at runtime.
///
/// The `kVTCompressionPropertyKey_` prefix is added here so the report and the call sites read as
/// the property names Apple documents.
fn set_optional(session: &VTSession, short_key: &str, value: &CFType) -> PropOutcome {
    if vt_recipe::denied(KeyKind::Prop, short_key) {
        return PropOutcome::MissingSymbol;
    }
    match vt_optional_cfstring(&format!("kVTCompressionPropertyKey_{short_key}")) {
        Some(k) => set_named(session, k, value),
        None => PropOutcome::MissingSymbol,
    }
}

fn set_optional_bool(session: &VTSession, short_key: &str, value: bool) -> PropOutcome {
    set_optional(session, short_key, cfbool(value))
}

fn dict_from_pairs(pairs: &[(&CFString, &CFType)]) -> Option<CFRetained<CFDictionary>> {
    let keys: Vec<*const c_void> = pairs
        .iter()
        .map(|(k, _)| (*k as *const CFString).cast())
        .collect();
    let values: Vec<*const c_void> = pairs
        .iter()
        .map(|(_, v)| (*v as *const CFType).cast())
        .collect();
    unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            pairs.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::config::H264Profile;
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };

    const BGRA: u32 = 0x4247_5241; // kCVPixelFormatType_32BGRA

    fn tag() -> FrameTag {
        FrameTag {
            capture: Instant::now(),
            timing: FrameTiming::entering_capture(),
            hold_push: false,
        }
    }

    fn make_bgra_frame(w: usize, h: usize, tick: usize) -> CFRetained<CVPixelBuffer> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = unsafe {
            CVPixelBufferCreate(
                kCFAllocatorDefault,
                w,
                h,
                BGRA,
                None,
                NonNull::from(&mut out),
            )
        };
        assert_eq!(r, 0, "CVPixelBufferCreate");
        let pb = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
            let bpr = CVPixelBufferGetBytesPerRow(&pb);
            for y in 0..h {
                let row = base.add(y * bpr);
                for x in 0..w {
                    let p = row.add(x * 4);
                    *p = (x + tick) as u8; // B
                    *p.add(1) = (y + tick) as u8; // G
                    *p.add(2) = tick as u8; // R
                    *p.add(3) = 255; // A
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        }
        pb
    }

    fn nal_types(au: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
                if i + 4 < au.len() {
                    types.push(au[i + 4] & 0x1F);
                }
                i += 4;
            } else {
                i += 1;
            }
        }
        types
    }

    /// Pull the first NAL of the given type out of an Annex B access unit, payload only.
    fn first_nal(au: &[u8], want: u8) -> Option<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 4 <= au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
                starts.push(i + 4);
                i += 4;
            } else {
                i += 1;
            }
        }
        for (n, &s) in starts.iter().enumerate() {
            if s >= au.len() || (au[s] & 0x1F) != want {
                continue;
            }
            let end = starts.get(n + 1).map(|&e| e - 4).unwrap_or(au.len());
            return Some(au[s + 1..end].to_vec());
        }
        None
    }

    // -----------------------------------------------------------------------
    // A minimal SPS reader.
    //
    // `AllowFrameReordering = false` is a request; `max_num_reorder_frames` in the SPS VUI is
    // what a decoder actually obeys, and a non-zero value there costs the client that many frames
    // of output delay no matter how fast the encoder is. Only the bitstream can answer it.
    // -----------------------------------------------------------------------

    struct BitReader {
        data: Vec<u8>,
        bit: usize,
    }

    impl BitReader {
        /// Strip H.264 emulation-prevention bytes before reading.
        fn new(rbsp_with_epb: &[u8]) -> BitReader {
            let mut data = Vec::with_capacity(rbsp_with_epb.len());
            let mut zeros = 0;
            for &b in rbsp_with_epb {
                if zeros == 2 && b == 0x03 {
                    zeros = 0;
                    continue;
                }
                if b == 0 {
                    zeros += 1;
                } else {
                    zeros = 0;
                }
                data.push(b);
            }
            BitReader { data, bit: 0 }
        }

        fn u1(&mut self) -> u32 {
            let byte = self.bit / 8;
            let shift = 7 - (self.bit % 8);
            self.bit += 1;
            match self.data.get(byte) {
                Some(b) => ((b >> shift) & 1) as u32,
                None => 0,
            }
        }

        fn u(&mut self, n: u32) -> u64 {
            let mut v = 0u64;
            for _ in 0..n {
                v = (v << 1) | self.u1() as u64;
            }
            v
        }

        fn flag(&mut self) -> bool {
            self.u1() == 1
        }

        fn ue(&mut self) -> u64 {
            let mut leading = 0u32;
            while self.u1() == 0 && leading < 32 && self.bit / 8 < self.data.len() {
                leading += 1;
            }
            if leading == 0 {
                return 0;
            }
            (1u64 << leading) - 1 + self.u(leading)
        }

        fn se(&mut self) -> i64 {
            let k = self.ue();
            let sign = if k.is_multiple_of(2) { -1i64 } else { 1 };
            sign * k.div_ceil(2) as i64
        }
    }

    #[derive(Debug, Default)]
    struct SpsVui {
        video_full_range_flag: bool,
        colour_primaries: u64,
        matrix_coefficients: u64,
        max_num_reorder_frames: Option<u64>,
    }

    fn parse_sps_vui(sps: &[u8]) -> SpsVui {
        let mut r = BitReader::new(sps);
        let profile_idc = r.u(8);
        let _constraints = r.u(8);
        let _level = r.u(8);
        let _sps_id = r.ue();
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            let chroma_format_idc = r.ue();
            if chroma_format_idc == 3 {
                let _separate_colour_plane = r.flag();
            }
            let _bit_depth_luma = r.ue();
            let _bit_depth_chroma = r.ue();
            let _qpprime = r.flag();
            if r.flag() {
                let lists = if chroma_format_idc != 3 { 8 } else { 12 };
                for i in 0..lists {
                    if r.flag() {
                        let size = if i < 6 { 16 } else { 64 };
                        let mut next = 8i64;
                        let mut last = 8i64;
                        for _ in 0..size {
                            if next != 0 {
                                let delta = r.se();
                                next = (last + delta + 256) % 256;
                            }
                            last = if next == 0 { last } else { next };
                        }
                    }
                }
            }
        }
        let _log2_max_frame_num = r.ue();
        let poc_type = r.ue();
        if poc_type == 0 {
            let _log2_max_poc_lsb = r.ue();
        } else if poc_type == 1 {
            let _delta_always_zero = r.flag();
            let _offset_non_ref = r.se();
            let _offset_top_bottom = r.se();
            let cycle = r.ue();
            for _ in 0..cycle.min(256) {
                let _ = r.se();
            }
        }
        let _max_num_ref_frames = r.ue();
        let _gaps = r.flag();
        let _w_mbs = r.ue();
        let _h_map_units = r.ue();
        let frame_mbs_only = r.flag();
        if !frame_mbs_only {
            let _mb_adaptive = r.flag();
        }
        let _direct8x8 = r.flag();
        if r.flag() {
            for _ in 0..4 {
                let _ = r.ue();
            }
        }
        let mut vui = SpsVui::default();
        if !r.flag() {
            return vui;
        }
        // vui_parameters()
        if r.flag() {
            let idc = r.u(8);
            if idc == 255 {
                let _ = r.u(16);
                let _ = r.u(16);
            }
        }
        if r.flag() {
            let _overscan_appropriate = r.flag();
        }
        if r.flag() {
            let _video_format = r.u(3);
            vui.video_full_range_flag = r.flag();
            if r.flag() {
                vui.colour_primaries = r.u(8);
                let _transfer = r.u(8);
                vui.matrix_coefficients = r.u(8);
            }
        }
        if r.flag() {
            let _ = r.ue();
            let _ = r.ue();
        }
        if r.flag() {
            let _num_units_in_tick = r.u(32);
            let _time_scale = r.u(32);
            let _fixed_frame_rate = r.flag();
        }
        let nal_hrd = r.flag();
        if nal_hrd {
            hrd_parameters(&mut r);
        }
        let vcl_hrd = r.flag();
        if vcl_hrd {
            hrd_parameters(&mut r);
        }
        if nal_hrd || vcl_hrd {
            let _low_delay = r.flag();
        }
        let _pic_struct_present = r.flag();
        if r.flag() {
            let _mv_over_boundaries = r.flag();
            let _max_bytes_per_pic_denom = r.ue();
            let _max_bits_per_mb_denom = r.ue();
            let _log2_mv_h = r.ue();
            let _log2_mv_v = r.ue();
            vui.max_num_reorder_frames = Some(r.ue());
            let _max_dec_frame_buffering = r.ue();
        }
        vui
    }

    fn hrd_parameters(r: &mut BitReader) {
        let cpb_cnt_minus1 = r.ue();
        let _bit_rate_scale = r.u(4);
        let _cpb_size_scale = r.u(4);
        for _ in 0..=cpb_cnt_minus1.min(31) {
            let _ = r.ue();
            let _ = r.ue();
            let _ = r.flag();
        }
        let _ = r.u(5);
        let _ = r.u(5);
        let _ = r.u(5);
        let _ = r.u(5);
    }

    fn cfg_at(w: u32, h: u32) -> EncoderConfig {
        EncoderConfig {
            width: w,
            height: h,
            fps: 60,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 6_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            min_qp: None,
            intra_refresh: false,
            gpu_registry_id: None,
            software_encode: false,
            recipe: None,
        }
    }

    #[test]
    fn the_bit_reader_agrees_with_the_exp_golomb_table() {
        // 0 -> "1", 1 -> "010", 2 -> "011", 3 -> "00100", 4 -> "00101"
        let bytes = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.ue(), 0);
        assert_eq!(r.ue(), 1);
        assert_eq!(r.ue(), 2);
        assert_eq!(r.ue(), 3);
        assert_eq!(r.ue(), 4);
    }

    #[test]
    fn emulation_prevention_bytes_are_stripped() {
        let r = BitReader::new(&[0x00, 0x00, 0x03, 0x01]);
        assert_eq!(r.data, vec![0x00, 0x00, 0x01]);
    }

    #[test]
    #[ignore = "requires a working VideoToolbox H.264 encoder and writes /tmp/screenextend_test.h264; run with --ignored --nocapture"]
    fn encode_synthetic() {
        let (w, h) = (640usize, 480usize);
        let mut cfg = cfg_at(w as u32, h as u32);
        cfg.profile = H264Profile::Baseline;
        let mut enc = Encoder::new(cfg).expect("encoder creates (HW required)");
        let out = enc.output();

        let mut per_frame: Vec<Vec<u8>> = Vec::new();
        let mut bitstream: Vec<u8> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        fn drain(
            out: &Receiver<CompressedSample>,
            per_frame: &mut Vec<Vec<u8>>,
            bitstream: &mut Vec<u8>,
            buf: &mut Vec<u8>,
        ) {
            while let Ok(cs) = out.try_recv() {
                cs.to_annexb(buf);
                if buf.is_empty() {
                    continue;
                }
                per_frame.push(nal_types(buf));
                bitstream.extend_from_slice(buf);
            }
        }

        // Per-submit timing, so this test produces a number as well as a verdict. Until now the
        // only encode timing in the repo came from the Windows x264 test, which is why the CI
        // perf gate had no macOS row at all — not a platform judgement, a data problem.
        // `scripts/perf-gate.mjs` parses the `vt_encode_p95=<n>us` printed below as
        // `vt_p95_encode_us`.
        //
        // `submit` hands the frame to the compression session and returns; the encode itself is
        // asynchronous, which is exactly what makes this the right thing to watch. It is the
        // cost the capture thread pays per frame, and a regression in it stalls capture whatever
        // the encoder does afterwards. End-to-end latency is `pipeline.rs`'s `end_to_end_latency`.
        let mut submit_us: Vec<u128> = Vec::with_capacity(60);
        let started = std::time::Instant::now();
        for i in 0..60usize {
            let pb = make_bgra_frame(w, h, i);
            let t0 = std::time::Instant::now();
            enc.submit(&pb, i == 30, tag()).expect("submit frame");
            submit_us.push(t0.elapsed().as_micros());
            drain(&out, &mut per_frame, &mut bitstream, &mut buf);
        }
        let wall_ms = started.elapsed().as_millis();
        enc.flush().expect("flush");
        while let Ok(cs) = out.recv_timeout(std::time::Duration::from_millis(500)) {
            cs.to_annexb(&mut buf);
            if buf.is_empty() {
                continue;
            }
            per_frame.push(nal_types(&buf));
            bitstream.extend_from_slice(&buf);
        }

        std::fs::write("/tmp/screenextend_test.h264", &bitstream).expect("write bitstream");
        let p = crate::test_support::percentiles(&submit_us);
        println!(
            "videotoolbox encode OK: {} frames {w}x{h}, {} bytes, output drops {}, vt_encode_p50={}us vt_encode_p95={}us vt_encode_p99={}us vt_encode_max={}us, wall={wall_ms}ms",
            per_frame.len(),
            bitstream.len(),
            enc.output_drops(),
            p.p50,
            p.p95,
            p.p99,
            p.max,
        );
        let keyframes: Vec<usize> = per_frame
            .iter()
            .enumerate()
            .filter(|(_, n)| n.contains(&5))
            .map(|(i, _)| i)
            .collect();
        println!("frame 0 NALs: {:?}", per_frame[0]);
        println!("keyframe indices: {keyframes:?}");

        assert!(
            per_frame.len() >= 55,
            "expected ~60 frames, got {}",
            per_frame.len()
        );
        assert!(per_frame[0].contains(&7), "frame 0 missing SPS");
        assert!(per_frame[0].contains(&8), "frame 0 missing PPS");
        assert!(per_frame[0].contains(&5), "frame 0 missing IDR slice");
        assert!(per_frame[1].contains(&1), "frame 1 not a P-frame");
        assert!(!per_frame[1].contains(&5), "frame 1 unexpectedly an IDR");
        assert!(
            keyframes.len() <= 3,
            "too many keyframes: {keyframes:?} (P-frames not used)"
        );

        for (i, nals) in per_frame.iter().enumerate() {
            for &t in nals {
                assert!(
                    matches!(t, 1 | 5 | 6 | 7 | 8 | 9),
                    "frame {i} unexpected NAL type {t}"
                );
            }
        }
    }

    /// What the decoder is actually told about buffering and colour.
    #[test]
    #[ignore = "requires a working VideoToolbox H.264 encoder; run with --ignored --nocapture"]
    fn the_sps_promises_no_reordering() {
        let (w, h) = (640usize, 480usize);
        let mut enc = Encoder::new(cfg_at(w as u32, h as u32)).expect("encoder");
        let out = enc.output();
        for i in 0..4usize {
            let pb = make_bgra_frame(w, h, i);
            enc.submit(&pb, i == 0, tag()).expect("submit");
        }
        enc.flush().expect("flush");

        let mut sps = None;
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(cs) = out.recv_timeout(std::time::Duration::from_millis(500)) {
            cs.to_annexb(&mut buf);
            if let Some(nal) = first_nal(&buf, 7) {
                sps = Some(nal);
                break;
            }
        }
        let sps = sps.expect("an SPS in the first access unit");
        let vui = parse_sps_vui(&sps);
        println!("SPS VUI: {vui:?}");
        match vui.max_num_reorder_frames {
            Some(n) => assert_eq!(
                n, 0,
                "the decoder is told to hold {n} frames before output; AllowFrameReordering=false \
                 did not reach the bitstream"
            ),
            None => println!(
                "no bitstream_restriction in the VUI: the decoder is not told how many frames to \
                 hold, so it may apply the level's DPB size. Recorded, not asserted."
            ),
        }
    }

    #[test]
    #[ignore = "creates and destroys eight 720p hardware sessions; run with --ignored --nocapture"]
    fn recreate_churn_releases_hw() {
        for i in 0..8 {
            let enc = Encoder::new(cfg_at(1280, 720));
            assert!(
                enc.is_ok(),
                "create #{i} at 1280x720 failed: {:?}",
                enc.err()
            );
            drop(enc);
            println!("ok: churn iteration {i} created+released at 1280x720");
        }
    }

    fn hw_create_status(w: u32, h: u32) -> Result<(), i32> {
        let (tx, rx) = bounded::<CompressedSample>(OUTPUT_CAPACITY);
        let z = || Arc::new(AtomicU64::new(0));
        let (a, b, c, d) = (z(), z(), z(), z());
        let ctx = Arc::new(OutputCtx::new(
            tx,
            rx,
            [&a, &b, &c, &d],
            &Arc::new(AtomicUsize::new(0)),
            &EmitLog::new(),
        ));
        OutputCtx::start_sim_thread(&ctx);
        let refcon = Arc::into_raw(ctx) as *mut OutputCtx;
        let spec = build_encoder_spec(false, HwPolicy::Require, None).expect("encoder spec");
        let r = create_session(&cfg_at(w, h), &spec, refcon);
        let out = match r {
            Ok(s) => {
                unsafe { s.invalidate() };
                Ok(())
            }
            Err(e) => Err(e),
        };
        drop(unsafe { Arc::from_raw(refcon as *const OutputCtx) });
        out
    }

    #[test]
    #[ignore = "requires VideoToolbox and exercises the software fallback below the hardware floor; run with --ignored --nocapture"]
    fn creates_small_via_software() {
        for (w, h) in [(480u32, 270u32), (448u32, 252u32), (160u32, 90u32)] {
            let hw = hw_create_status(w, h);
            let enc = Encoder::new(cfg_at(w, h));
            assert!(
                enc.is_ok(),
                "Encoder::new must succeed at {w}x{h}: {:?}",
                enc.err()
            );
            println!("ok: {w}x{h} created (require-HW was {hw:?}; Encoder::new used SW fallback)");
        }
    }

    #[test]
    #[ignore = "sweeps every resolution and holds six concurrent hardware sessions; run with --ignored --nocapture"]
    fn hw_capability_probe() {
        for (w, h) in [
            (1280u32, 720u32),
            (1024, 576),
            (960, 540),
            (854, 480),
            (768, 432),
            (720, 480),
            (640, 360),
            (576, 324),
            (512, 288),
            (480, 270),
            (426, 240),
            (320, 180),
        ] {
            match hw_create_status(w, h) {
                Ok(()) => println!("ladder {w}x{h}: HW OK"),
                Err(e) => println!("ladder {w}x{h}: HW FAILED (OSStatus {e})"),
            }
        }
        let mut held: Vec<Encoder> = Vec::new();
        for n in 1..=6 {
            match Encoder::new(cfg_at(1280, 720)) {
                Ok(enc) => {
                    held.push(enc);
                    println!("concurrency: {n} simultaneous HW session(s) OK");
                }
                Err(e) => {
                    println!(
                        "concurrency: capped at {} simultaneous HW session(s) ({e})",
                        n - 1
                    );
                    break;
                }
            }
        }
        drop(held);
    }

    // -----------------------------------------------------------------------
    // Rate-control tiers (pure; no session).
    // -----------------------------------------------------------------------

    fn caps(ll_key: bool, ll: bool, bfq: Option<bool>, frame_key: bool) -> Capabilities {
        Capabilities {
            low_latency_key_resolved: ll_key,
            low_latency_session: ll,
            supports_base_frame_qp: bfq,
            frame_qp_key_resolved: frame_key,
            max_qp: Probe::NotTried,
            min_qp: Probe::NotTried,
            encoder_id: Some("com.apple.videotoolbox.videoencoder.h264.rtvc".into()),
        }
    }

    #[test]
    fn every_capability_combination_selects_the_expected_tier() {
        use Probe::*;
        let probes = [NotTried, Applied, Refused];
        let mut seen = HashSet::new();
        for ll_key in [false, true] {
            for ll in [false, true] {
                for bfq in [None, Some(false), Some(true)] {
                    for frame_key in [false, true] {
                        for max_qp in probes {
                            for min_qp in probes {
                                let c = Capabilities {
                                    max_qp,
                                    min_qp,
                                    ..caps(ll_key, ll, bfq, frame_key)
                                };
                                let low_latency = ll_key && ll;
                                let want = if !low_latency {
                                    RateTier::StandardBitrate
                                } else if bfq == Some(true) && frame_key {
                                    RateTier::BaseFrameQp
                                } else if max_qp == Applied && min_qp == Applied {
                                    RateTier::ClampCeiling
                                } else {
                                    RateTier::LowLatencyBitrate
                                };
                                assert_eq!(select_rate_tier(&c, true), want, "{c:?}");
                                seen.insert(want);
                                let adaptive = if low_latency {
                                    RateTier::AdaptiveLowLatency
                                } else {
                                    RateTier::AdaptiveStandard
                                };
                                assert_eq!(select_rate_tier(&c, false), adaptive, "{c:?}");
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(seen.len(), 4, "every constant-quality tier is reachable");
    }

    #[test]
    fn the_measured_machines_land_on_their_measured_tiers() {
        // M1, macOS 26: rtvc, SupportsBaseFrameQP true, frame key resolves.
        assert_eq!(
            select_rate_tier(&caps(true, true, Some(true), true), true),
            RateTier::BaseFrameQp
        );
        // 10.15: the spec symbol does not exist. Even a session that "succeeded" with the key is
        // not low-latency (the old bug), and the result is the standard tier.
        assert_eq!(
            select_rate_tier(&caps(false, true, None, false), true),
            RateTier::StandardBitrate
        );
        // ave.avc (create:lowlatency / spec deny): SupportsBaseFrameQP reads -12900.
        assert_eq!(
            select_rate_tier(&caps(true, false, None, true), true),
            RateTier::StandardBitrate
        );
        // read:SupportsBaseFrameQP deny, clamp holds.
        let mut c = caps(true, true, None, true);
        assert_eq!(select_rate_tier(&c, true), RateTier::LowLatencyBitrate);
        c.max_qp = Probe::Applied;
        c.min_qp = Probe::Applied;
        assert_eq!(select_rate_tier(&c, true), RateTier::ClampCeiling);
        // frame:BaseFrameQP deny, clamp holds.
        let mut c = caps(true, true, Some(true), false);
        c.max_qp = Probe::Applied;
        c.min_qp = Probe::Applied;
        assert_eq!(select_rate_tier(&c, true), RateTier::ClampCeiling);
        // Half a clamp is not a clamp.
        c.min_qp = Probe::Refused;
        assert_eq!(select_rate_tier(&c, true), RateTier::LowLatencyBitrate);
    }

    #[test]
    fn only_low_latency_constant_quality_tiers_are_drop_guarded() {
        assert!(RateTier::BaseFrameQp.drop_guarded());
        assert!(RateTier::ClampCeiling.drop_guarded());
        assert!(RateTier::LowLatencyBitrate.drop_guarded());
        assert!(
            !RateTier::StandardBitrate.drop_guarded(),
            "never flaps back"
        );
        assert!(!RateTier::AdaptiveLowLatency.drop_guarded());
        assert!(!RateTier::AdaptiveStandard.drop_guarded());
    }

    #[test]
    fn constant_quality_bitrate_follows_the_first_fit() {
        // 1080p60: base 12.44 Mbps, x1.6 at q = 15.
        assert_eq!(constant_quality_bitrate(1920, 1080, 60, 15), 19_906_560);
        // Six QP steps halve / double it.
        let b21 = constant_quality_bitrate(1920, 1080, 60, 21);
        assert!((b21 as i64 - 9_953_280).abs() <= 1, "{b21}");
        let b9 = constant_quality_bitrate(1920, 1080, 60, 9);
        assert!((b9 as i64 - 39_813_120).abs() <= 1, "{b9}");
        // The base clamps (6 .. 30 Mbps) and the output clamps (2 .. 62.5 Mbps).
        assert_eq!(constant_quality_bitrate(640, 360, 30, 15), 9_600_000);
        assert_eq!(constant_quality_bitrate(3840, 2160, 120, 15), 48_000_000);
        assert_eq!(constant_quality_bitrate(3840, 2160, 120, 1), 62_500_000);
        assert_eq!(constant_quality_bitrate(1920, 1080, 60, 51), 2_000_000);
        // q is clamped to 1..=51 like BaseFrameQP.
        assert_eq!(
            constant_quality_bitrate(1920, 1080, 60, 0),
            constant_quality_bitrate(1920, 1080, 60, 1)
        );
        // Monotonic in q.
        let mut last = u32::MAX;
        for q in 1..=51u8 {
            let b = constant_quality_bitrate(1920, 1080, 60, q);
            assert!(b <= last, "q={q}");
            last = b;
        }
    }

    #[test]
    fn a_dropped_forced_keyframe_is_rearmed_and_an_ordinary_drop_is_not() {
        // Rate-control drop: status 0, kVTEncodeInfo_FrameDropped, no sample.
        assert!(forced_keyframe_lost(true, 0, 2, false));
        // Status 0 with no sample and no flag counts as a drop too.
        assert!(forced_keyframe_lost(true, 0, 0, false));
        // An ordinary dropped frame never became a reference: no IDR.
        assert!(!forced_keyframe_lost(false, 0, 2, false));
        // A forced frame that came out: nothing to re-arm.
        assert!(!forced_keyframe_lost(true, 0, 0, true));
        // An encoder error is not a rate-control drop.
        assert!(!forced_keyframe_lost(true, -12903, 0, false));
    }

    #[test]
    fn encoder_ids_map_to_families() {
        let f = |id: &str, hw| encoder_family(Some(id), hw);
        assert_eq!(
            f("com.apple.videotoolbox.videoencoder.h264.rtvc", Some(true)),
            EncoderFamily::Rtvc
        );
        assert_eq!(
            f("com.apple.videotoolbox.videoencoder.ave.avc", Some(true)),
            EncoderFamily::Ave
        );
        assert_eq!(
            f("com.apple.videotoolbox.videoencoder.h264.gva", Some(true)),
            EncoderFamily::Gva
        );
        assert_eq!(
            f("com.apple.videotoolbox.videoencoder.h264", Some(false)),
            EncoderFamily::Software
        );
        assert_eq!(encoder_family(None, None), EncoderFamily::Other);
        assert_eq!(short_encoder_id(None), "unknown");
        assert_eq!(
            short_encoder_id(Some("com.apple.videotoolbox.videoencoder.h264.rtvc")),
            "h264.rtvc"
        );
    }

    #[test]
    fn the_shipped_extras_table_is_empty_and_lookup_works() {
        assert!(FAMILY_EXTRAS.is_empty());
        assert!(extras_for(FAMILY_EXTRAS, EncoderFamily::Rtvc).is_none());
        static TABLE: &[FamilyExtras] = &[FamilyExtras {
            family: EncoderFamily::Gva,
            props: &[("RealTime", ExtraVal::Bool(true))],
            flush_each_frame: true,
        }];
        let e = extras_for(TABLE, EncoderFamily::Gva).expect("gva row");
        assert!(e.flush_each_frame);
        assert_eq!(e.props[0].1.to_val(), Val::Bool(true));
        assert_eq!(ExtraVal::Int(3).to_val(), Val::Int(3));
        assert_eq!(ExtraVal::Float(0.5).to_val(), Val::Float(0.5));
        assert!(extras_for(TABLE, EncoderFamily::Ave).is_none());
    }

    #[test]
    fn the_tier_line_names_the_tier_and_why() {
        let c = caps(true, true, Some(true), true);
        assert_eq!(
            describe_rate_tier(RateTier::BaseFrameQp, &c, Some(15), None),
            "BaseFrameQp q=15 (h264.rtvc, SupportsBaseFrameQP=true)"
        );
        let c = Capabilities {
            encoder_id: Some("com.apple.videotoolbox.videoencoder.h264.gva".into()),
            ..caps(false, false, None, false)
        };
        let line = describe_rate_tier(RateTier::StandardBitrate, &c, Some(15), Some(19_906_560));
        assert!(line.starts_with("StandardBitrate q=15 AverageBitRate=19.9 Mbps (h264.gva"));
        assert!(line.contains("does not resolve"), "{line}");
    }

    #[test]
    fn a_low_latency_create_counts_only_when_the_read_back_confirms_it() {
        let gva = Some("com.apple.videotoolbox.videoencoder.h264.gva".to_string());
        let ave = Some("com.apple.videotoolbox.videoencoder.ave.avc".to_string());
        // Created with the resolved key, but VideoToolbox handed back Quick Sync with no
        // per-frame QP: a standard session for every decision.
        let c = Capabilities {
            encoder_id: gva.clone(),
            ..caps(true, true, None, true)
        };
        assert_eq!(select_rate_tier(&c, true), RateTier::StandardBitrate);
        assert_eq!(select_rate_tier(&c, false), RateTier::AdaptiveStandard);
        let c = Capabilities {
            encoder_id: ave.clone(),
            ..caps(true, true, Some(false), true)
        };
        assert_eq!(select_rate_tier(&c, true), RateTier::StandardBitrate);
        // Per-frame QP support confirms it whatever the id says.
        let c = Capabilities {
            encoder_id: gva,
            ..caps(true, true, Some(true), true)
        };
        assert_eq!(select_rate_tier(&c, true), RateTier::BaseFrameQp);
        // The rtvc family confirms it even with SupportsBaseFrameQP unreadable (the
        // `read:SupportsBaseFrameQP` deny case, which must still reach ClampCeiling).
        let mut c = caps(true, true, None, true);
        c.max_qp = Probe::Applied;
        c.min_qp = Probe::Applied;
        assert_eq!(select_rate_tier(&c, true), RateTier::ClampCeiling);
        assert!(low_latency_confirmed(
            Some("com.apple.videotoolbox.videoencoder.h264.rtvc"),
            None
        ));
        assert!(!low_latency_confirmed(None, None));
        assert!(!low_latency_confirmed(ave.as_deref(), Some(false)));
        // The tier line says why.
        let c = Capabilities {
            encoder_id: Some("com.apple.videotoolbox.videoencoder.h264.gva".into()),
            ..caps(true, true, None, true)
        };
        let line = describe_rate_tier(RateTier::StandardBitrate, &c, Some(15), Some(1));
        assert!(line.contains("returned a standard encoder"), "{line}");
    }

    #[test]
    fn the_qp_ceiling_hook_parses_to_a_plan() {
        let shipped = CeilingPlan::Try(CONSTANT_QP_CEILINGS_BPS.to_vec());
        assert_eq!(ceiling_plan(None), shipped);
        // 0 names no ceiling: the session must fall to LowLatencyBitrate, not run uncapped.
        assert_eq!(ceiling_plan(Some("0")), CeilingPlan::Try(Vec::new()));
        assert_eq!(
            ceiling_plan(Some("150000000")),
            CeilingPlan::Try(vec![150_000_000])
        );
        // The only way to the measured-bad configuration is to ask for it by name.
        assert_eq!(ceiling_plan(Some("uncapped")), CeilingPlan::Uncapped);
        assert_eq!(ceiling_plan(Some("-5")), shipped);
        assert_eq!(ceiling_plan(Some("lots")), shipped);
    }

    #[test]
    fn a_qp_limit_is_widened_to_no_limit() {
        assert_eq!(qp_limit_widest("MaxAllowedFrameQP"), 51);
        assert_eq!(qp_limit_widest("MinAllowedFrameQP"), 1);
    }
}
