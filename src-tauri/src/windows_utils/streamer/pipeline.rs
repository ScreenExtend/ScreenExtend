use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::windows_capture::capture::{
    CaptureControl, Context, GraphicsCaptureApiError, GraphicsCaptureApiHandler,
};
use crate::windows_capture::d3d11::StagingTexture;
use crate::windows_capture::frame::Frame;
use crate::windows_capture::graphics_capture_api::{wgc_features, InternalCaptureControl};
use crate::windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use anyhow::{anyhow, bail, Context as _, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

use super::amd::encoder::Encoder as AmfEncoder;
use super::capture::{select_monitor, select_monitor_by_device_name, MonitorInfo};
use super::cursor_overlay::CursorOverlay;
use super::dxgi::{DuplicationNotReady, Duplicator, NotReady, PollStatus};
use super::intel::encoder::Encoder as IntelEncoder;
use super::keyed::{self, Acquired};
use super::nvidia::encoder::{
    Encoder, EncoderConfig, RefInvalidation, SlotEncodeError, KEY_ENCODER, KEY_TIMEOUT_MS,
    KEY_WRITER,
};
use super::scaler::{ReadbackFormat, Scaler, TextureReader};
use super::tune;
use super::tuning;
use super::x264::encoder::X264Encoder;
use crate::streamer::config::{Config, H264Profile, ScalePercent};
use crate::streamer::lock::LockExt;
use crate::streamer::platform::BitrateApplied;
use crate::streamer::telemetry::{FrameTiming, SessionTelemetry, SharedTelemetry};
use crate::windows_capture::monitor::Monitor;

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
    /// Per-stage stamps; the transport fills in the last two (`streamer::telemetry`).
    pub timing: FrameTiming,
}

const SP_WIDTH: u32 = 1280;
const SP_HEIGHT: u32 = 720;
const SP_FPS: u32 = 30;
const SP_BITRATE_BPS: u32 = 6_000_000;
pub(crate) const BROADCAST_CAPACITY: usize = 4;

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
    /// Loss recovery without a keyframe: the transport writes receipts, the encode thread reads
    /// them at its next submit. Inert on an encoder with no LTR slots (`set_ltr_slots(0)`).
    pub repair: Arc<crate::streamer::repair::RepairState>,
    /// With `pipeline_weak_sender`, where sessions subscribe. `tx` is then a sender of nothing,
    /// so only the capture's own threads hold the frame channel open and their exit closes it.
    frames: Option<Arc<Mutex<broadcast::Receiver<EncodedFrame>>>>,
}

impl Pipeline {
    pub fn subscribe(&self) -> broadcast::Receiver<EncodedFrame> {
        match &self.frames {
            Some(template) => template.lock_recover().resubscribe(),
            None => self.tx.subscribe(),
        }
    }

    /// Stop this handle holding the frame channel open, so a capture that ends closes it and
    /// the session's writer sees that (`pipeline_weak_sender`).
    fn release_sender(&mut self) {
        let template = self.tx.subscribe();
        self.tx = broadcast::channel(1).0;
        self.frames = Some(Arc::new(Mutex::new(template)));
    }

    pub fn request_idr(&self) {
        self.idr_request.store(true, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }

    pub fn set_target_bitrate(&self, bps: u32) {
        self.target_bitrate.store(bps, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }

    /// Ask for a repair frame instead of a keyframe, if the client is known to hold a reference.
    /// `false` means the caller must fall back to `request_idr`.
    pub fn request_repair(&self) -> bool {
        if !self.repair.request_repair() {
            return false;
        }
        let _ = self.wake.try_send(());
        true
    }
}

/// Once per encode thread: every logbus line is an IPC event to the UI.
fn warn_constant_qp_once(warned: &mut bool) {
    if !*warned {
        *warned = true;
        teprintln!(
            "bitrate adaptation is inert: the encoder is in explicit constant-QP mode (--qp), \
             so congestion control cannot shrink the stream"
        );
    }
}

fn warn_bitrate_fixed_once(warned: &mut bool) {
    if !*warned {
        *warned = true;
        teprintln!(
            "bitrate adaptation is inert: this encoder cannot change its bitrate in a running \
             session, so congestion control cannot shrink the stream"
        );
    }
}

fn apply_pending_bitrate(
    backend: &mut Backend,
    target_bitrate: &AtomicU32,
    current: &mut u32,
    warned: &mut bool,
    retry: &mut RetryBackoff,
) {
    // a refused reconfigure backs off (Quick Sync under its CBR fallback refuses a live `Reset`),
    // except for a deeper cut than the refused one: holding the old rate there builds queue delay
    if !retry.ready_now()
        && !bitrate_bypasses_backoff(
            target_bitrate.load(Ordering::Relaxed),
            *current,
            retry.refused(),
        )
    {
        return;
    }
    let pending = target_bitrate.swap(0, Ordering::Relaxed);
    if pending == 0 || pending == *current {
        return;
    }
    match backend.set_bitrate(pending) {
        Ok(BitrateApplied::Applied) => {
            if retry.enabled && retry.failures() > 0 {
                tprintln!(
                    "set_bitrate succeeded after {} refused attempt(s)",
                    retry.failures()
                );
            }
            retry.succeeded();
            tprintln!("adapting bitrate: {} -> {pending} bps", *current);
            *current = pending;
        }
        Ok(BitrateApplied::IgnoredConstantQp) => {
            retry.succeeded();
            warn_constant_qp_once(warned);
            *current = pending;
        }
        Ok(BitrateApplied::Unsupported) => {
            retry.succeeded();
            warn_bitrate_fixed_once(warned);
            *current = pending;
        }
        Ok(BitrateApplied::Clamped { applied_bps }) => {
            retry.succeeded();
            tprintln!(
                "adapting bitrate: {} -> {pending} bps, held to {applied_bps} bps by the encoder",
                *current
            );
            *current = pending;
        }
        Err(e) => {
            retry.set_refused(pending);
            if retry.failed(Instant::now()) {
                teprintln!(
                    "set_bitrate failed (target_bps={pending}, attempt {}): {e:?}; will retry{}",
                    retry.failures(),
                    retry.describe_wait()
                );
            }
            restore_pending_bitrate(target_bitrate, pending);
        }
    }
}

/// Whether a pending target may be tried while a refused one is backing off: only a cut below
/// both the rate in force and the refused target. A chain of bypasses is strictly decreasing,
/// so an encoder that refuses everything still sees few attempts.
pub(crate) fn bitrate_bypasses_backoff(pending: u32, current: u32, refused: u32) -> bool {
    pending != 0 && pending < current && pending < refused
}

/// Backoff for an operation the encode path retries on its own: a refused bitrate reconfigure,
/// or an idle repeat the encoder keeps failing.
///
/// Disabled, it holds nothing back and logs every failure. Enabled, the first failure waits
/// `first`, each further one doubles the wait up to `max`, and only the 1st, 2nd, 4th, 8th...
/// failure is logged.
#[derive(Debug, Clone)]
pub(crate) struct RetryBackoff {
    enabled: bool,
    first: Duration,
    max: Duration,
    failures: u32,
    wait: Duration,
    not_before: Option<Instant>,
    /// The value the last failure was for (the bitrate applier's refused target); 0 when
    /// nothing is backing off.
    refused: u32,
}

impl RetryBackoff {
    pub(crate) fn new(enabled: bool, first: Duration, max: Duration) -> Self {
        Self {
            enabled,
            first,
            max,
            failures: 0,
            wait: Duration::ZERO,
            not_before: None,
            refused: 0,
        }
    }

    pub(crate) fn ready(&self, now: Instant) -> bool {
        self.not_before.is_none_or(|t| now >= t)
    }

    /// [`ready`](Self::ready) without reading the clock when nothing is pending, which is every
    /// frame of a healthy session.
    pub(crate) fn ready_now(&self) -> bool {
        self.not_before.is_none() || self.ready(Instant::now())
    }

    /// Record a failure at `now`. Returns whether this one should be logged.
    pub(crate) fn failed(&mut self, now: Instant) -> bool {
        self.failures = self.failures.saturating_add(1);
        if !self.enabled {
            return true;
        }
        let doublings = (self.failures - 1).min(16);
        self.wait = self.first.saturating_mul(1u32 << doublings).min(self.max);
        self.not_before = Some(now + self.wait);
        self.failures.is_power_of_two()
    }

    pub(crate) fn succeeded(&mut self) {
        self.failures = 0;
        self.wait = Duration::ZERO;
        self.not_before = None;
        self.refused = 0;
    }

    pub(crate) fn set_refused(&mut self, value: u32) {
        self.refused = value;
    }

    pub(crate) fn refused(&self) -> u32 {
        self.refused
    }

    pub(crate) fn failures(&self) -> u32 {
        self.failures
    }

    fn describe_wait(&self) -> String {
        if self.enabled {
            format!(" in {} ms", self.wait.as_millis())
        } else {
            String::new()
        }
    }
}

/// First wait and ceiling for a refused bitrate reconfigure.
const BITRATE_RETRY_FIRST: Duration = Duration::from_millis(100);
const BITRATE_RETRY_MAX: Duration = Duration::from_secs(5);
/// First wait and ceiling for an idle repeat that failed. The first wait is the keepalive period,
/// so a single failure costs the client one keepalive and no more.
const REPEAT_RETRY_FIRST: Duration = Duration::from_millis(200);
const REPEAT_RETRY_MAX: Duration = Duration::from_secs(5);

/// State the encode path keeps beside the encoder, built once per [`EncodeCore`] with the
/// switches read there (never per frame).
pub(crate) struct CoreHealth {
    telemetry: SharedTelemetry,
    /// `bitrate_retry_backoff` (ships on).
    bitrate_retry: RetryBackoff,
    /// `repeat_error_backoff` (ships on).
    repeat_retry: RetryBackoff,
    /// `cpu_bridge_mapped_readback` (candidate): hand the encoder the mapped staging memory
    /// instead of a copy of it.
    mapped_readback: bool,
    /// `x264_nv12_no_copy` (ships on): x264 encodes the scaler's NV12 readback in place and its
    /// repeats come from that same buffer, so libx264 is fed without a second full-frame copy.
    /// Only without `mapped_readback`, which leaves no readback buffer to repeat from.
    x264_no_copy: bool,
    qp: Option<super::governor::QpControl>,
}

impl CoreHealth {
    pub(crate) fn new(telemetry: SharedTelemetry) -> Self {
        let mapped_readback = tune::candidate("cpu_bridge_mapped_readback");
        let qp = super::governor::QpControl::from_telemetry(&telemetry);
        Self {
            qp,
            telemetry,
            bitrate_retry: RetryBackoff::new(
                tune::on("bitrate_retry_backoff"),
                BITRATE_RETRY_FIRST,
                BITRATE_RETRY_MAX,
            ),
            repeat_retry: RetryBackoff::new(
                tune::on("repeat_error_backoff"),
                REPEAT_RETRY_FIRST,
                REPEAT_RETRY_MAX,
            ),
            mapped_readback,
            x264_no_copy: tune::on("x264_nv12_no_copy") && !mapped_readback,
        }
    }
}

/// [`Backend::encode_cpu`] for a frame the scaler read back into its own buffer. With
/// `x264_no_copy`, x264 encodes that buffer in place; it stays the scaler's until the next
/// frame, which is what the repeats in [`repeat_from_scaler`] read.
pub(crate) fn encode_scaled(
    backend: &mut Backend,
    data: &[u8],
    row_pitch: u32,
    format: ReadbackFormat,
    force_idr: bool,
    x264_no_copy: bool,
) -> Result<Vec<u8>> {
    match backend {
        Backend::X264 { encoder } if x264_no_copy && format == ReadbackFormat::Nv12 => {
            encoder.encode_nv12_unretained(data, row_pitch, force_idr)
        }
        _ => backend.encode_cpu(data, row_pitch, format, force_idr),
    }
}

/// The repeat that goes with [`encode_scaled`]: x264 retained nothing, so the last picture is
/// re-encoded from the scaler's readback. `None` when this backend or frame did not take that
/// path, and the encoder's own repeat applies.
pub(crate) fn repeat_from_scaler(
    backend: &mut Backend,
    scaler: Option<&Scaler>,
    force_idr: bool,
) -> Option<Result<Vec<u8>>> {
    let Backend::X264 { encoder } = backend else {
        return None;
    };
    let (data, row_pitch) = scaler?.last_nv12_readback()?;
    Some(encoder.encode_nv12_unretained(data, row_pitch, force_idr))
}

/// Settle a forced keyframe after the encode that was meant to carry it.
///
/// The request is consumed (`swap(false)`) before the encode, so a failed encode puts it back
/// (the transport's PLI backoff can hold the next ask for seconds) and a success is counted.
/// An empty access unit counts as a failure: every caller drops it, so no keyframe went out.
pub(crate) fn settle_forced_idr(
    result: &Result<Vec<u8>>,
    force_idr: bool,
    idr_request: &AtomicBool,
    idrs_sent: &AtomicU64,
) {
    if !force_idr {
        return;
    }
    match result {
        Ok(au) if !au.is_empty() => {
            idrs_sent.fetch_add(1, Ordering::Relaxed);
        }
        _ => idr_request.store(true, Ordering::Relaxed),
    }
}

/// What a keyed-mutex acquire that must own the mutex makes of the outcome.
///
/// Only `Owned` may go on to touch the shared texture or call `ReleaseSync`. Timeout and
/// abandonment are both success codes to the `windows` crate's wrapper. Neither error mentions
/// "device busy", so neither is mistaken for a transient encoder overload.
pub(crate) fn require_owned(outcome: Acquired, what: &str, timeout_ms: u32) -> Result<()> {
    match outcome {
        Acquired::Owned => Ok(()),
        Acquired::TimedOut => bail!(
            "{what}: the other device did not release the keyed mutex within {timeout_ms} ms \
             (GPU hang or driver fault); the shared texture was not touched"
        ),
        Acquired::Abandoned => bail!(
            "{what}: the keyed mutex was abandoned by its previous owner; the shared texture is \
             no longer consistent and was not touched"
        ),
    }
}

/// Puts a failed target back so the next tick retries it: both appliers consume the request
/// with `swap(0)` before the reconfigure. `compare_exchange` so a newer target still wins.
fn restore_pending_bitrate(target_bitrate: &AtomicU32, pending: u32) {
    let _ = target_bitrate.compare_exchange(0, pending, Ordering::Relaxed, Ordering::Relaxed);
}

pub(crate) const MAX_TRANSIENT_ENCODE_DROPS: u32 = 60;

/// `MinUpdateInterval` is a floor on the gap between delivered frames, not a throttle to a rate:
/// WGC's default delivery is capped near 60 Hz whatever the panel does, and this raises the
/// ceiling. A floor near the target period caps delivery below it (`windows-capture` issue
/// #190: a 120 Hz panel delivered ~52 fps with the floor at 1/60 and ~59 at 1/120), hence a
/// flat 4 ms (250 Hz, as Sunshine does). Two thirds of the stream period cost a 60 Hz session
/// 8 painted fps with two displays (49 vs 57) at level latency.
const WGC_MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(4);

/// Whether the host draws the pointer itself instead of letting WGC composite it
/// (`SE_CURSOR_OVERLAY=1`). Off by default: WGC's pointer is event-driven (a move is a frame),
/// the host's is polled on the encode thread's idle tick, so it is quantised to the frame
/// period and never drawn while real frames keep arriving. A pointer sweep paints 85 fps with
/// WGC compositing against 60 with the overlay. The overlay's one gain is a keepalive repeat
/// that carries the pointer where it is now.
fn host_composites_cursor() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::var("SE_CURSOR_OVERLAY").as_deref() == Ok("1"))
}

/// Frame-pool depth for a handler that encodes inline: one buffer the compositor writes, one the
/// handler reads (OBS and Sunshine both use 2). The pool is a queue handed out oldest-first, so
/// depth is latency for anything the consumer cannot keep up with.
const FRAME_POOL_BUFFERS_INLINE: u32 = 2;

/// Frame-pool depth for the ring path, which only stages a copy in the callback and encodes
/// elsewhere: one more buffer so the copy and the encode overlap.
const FRAME_POOL_BUFFERS_RING: u32 = 3;

/// How many consecutive "nothing changed" reports to trust before encoding anyway. The DXGI path
/// carries the same valve for the same reason: a driver that under-reports dirty regions would
/// otherwise serve a stale picture forever.
const MAX_CONSECUTIVE_UNCHANGED: u32 = 5;

/// Fraction of a frame period a frame may arrive early and still count as on-cadence, as a
/// numerator over [`CADENCE_DEN`].
///
/// A 120 Hz compose against a 60 Hz cadence lands alternately 8.33 ms and 16.67 ms after the
/// last kept frame; an exact `>= 16.67 ms` test would keep every third one (40 fps for a 60
/// request). Three quarters of a period keeps every second frame and absorbs scheduler jitter.
/// A ratio because the period is per-session.
pub(crate) const CADENCE_NUM: u32 = 3;
pub(crate) const CADENCE_DEN: u32 = 4;

/// What [`FrameGate::admit`] decided for one delivered frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admit {
    /// Stage or encode it.
    Stage,
    /// Inside the cadence window and dropped (counted as a cadence skip).
    SkipCadence,
    /// Inside the cadence window on a path that can hold it for trailing-edge delivery.
    Hold { last: Instant },
    /// Unchanged on an adaptive-rate session (counted as an unchanged skip).
    SkipUnchanged,
}

/// The cadence gate, the dirty-region gate and the cursor exemption of the WGC capture handler.
pub(crate) struct FrameGate {
    /// One frame period at the rate this device asked for. The display may compose faster than
    /// that (see `DISPLAY_COMPOSE_HZ`); everything past the cadence is coalesced, never queued.
    pub(crate) frame_period: Duration,
    /// Off, the default, means `config.fps` is a promise: no frame is skipped for being
    /// unchanged and the keepalive holds the cadence.
    pub(crate) auto_frame_rate: bool,
    /// True when this build exposes `DirtyRegionMode`, so `Frame::dirty_regions()` is meaningful.
    pub(crate) dirty_region_reporting: bool,
    /// `wgc_cadence_counts_staged_only`: the cadence clock advances only on a frame the ring
    /// actually staged, not on one it then dropped for want of a slot.
    pub(crate) cadence_counts_staged_only: bool,
    pub(crate) last_delivered: Option<Instant>,
    pub(crate) unchanged_streak: u32,
    /// The QP governor's frame-rate divisor, applied only where a gated frame can be held.
    pub(crate) divisor: u32,
}

impl FrameGate {
    pub(crate) fn new(
        frame_period: Duration,
        auto_frame_rate: bool,
        dirty_region_reporting: bool,
        cadence_counts_staged_only: bool,
    ) -> Self {
        Self {
            frame_period,
            auto_frame_rate,
            dirty_region_reporting,
            cadence_counts_staged_only,
            last_delivered: None,
            unchanged_streak: 0,
            divisor: 1,
        }
    }

    /// Decide one frame. `timer_delivered` is when the trailing-edge timer last delivered (it
    /// moves the cadence clock too), `trailing` whether a gated frame can be held for it, `ring`
    /// whether an admitted frame goes to a ring, and `has_dirty` the frame's dirty-region answer,
    /// asked only when the dirty-region gate needs it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn admit(
        &mut self,
        capture: Instant,
        cursor_moved: bool,
        timer_delivered: Option<Instant>,
        trailing: bool,
        ring: bool,
        has_dirty: impl FnOnce() -> Option<bool>,
        telemetry: &SessionTelemetry,
    ) -> Admit {
        let last = match (self.last_delivered, timer_delivered) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        // without a trailing edge a gated frame is lost until something else changes, so the
        // divisor widens the window only where the frame can be held
        let div = if trailing { self.divisor.max(1) } else { 1 };
        if let Some(last) = last {
            if (!cursor_moved || div > 1)
                && capture.duration_since(last)
                    < self.frame_period * (CADENCE_NUM + CADENCE_DEN * (div - 1)) / CADENCE_DEN
            {
                if trailing {
                    return Admit::Hold { last };
                }
                telemetry
                    .degradation
                    .frames_skipped_cadence
                    .fetch_add(1, Ordering::Relaxed);
                return Admit::SkipCadence;
            }
        }
        // adaptive-rate sessions only: with `autoFrameRate` off the rate is a promise. The streak
        // cap covers a driver that under-reports dirty regions
        if self.auto_frame_rate && self.dirty_region_reporting && !cursor_moved {
            if let Some(dirty) = has_dirty() {
                if !dirty && self.unchanged_streak < MAX_CONSECUTIVE_UNCHANGED {
                    self.unchanged_streak += 1;
                    telemetry
                        .degradation
                        .frames_skipped_unchanged
                        .fetch_add(1, Ordering::Relaxed);
                    return Admit::SkipUnchanged;
                }
                self.unchanged_streak = 0;
            }
        }
        if !self.cadence_counts_staged_only || !ring {
            self.last_delivered = Some(capture);
        }
        Admit::Stage
    }

    /// After a ring stage of an admitted frame.
    pub(crate) fn staged(&mut self, capture: Instant, staged: bool) {
        if staged && self.cadence_counts_staged_only {
            self.last_delivered = Some(capture);
        }
    }
}

/// Whether the gate holds a frame it gates instead of dropping it: always on the ring's trailing
/// edge, and on the classic path only while the governor divides the frame rate, so at the full
/// rate the classic gate is exactly the one it always was.
pub(crate) fn gate_holds(trailing_edge: bool, classic_hold: bool, divisor: u32) -> bool {
    trailing_edge || (classic_hold && divisor > 1)
}

/// A frame the cadence gate held back, on the ring path with `wgc_trailing_frame`: staged into a
/// free slot now, delivered by [`TrailingEdge`]'s timer if nothing newer arrives first.
#[allow(clippy::too_many_arguments)]
pub(crate) fn hold_trailing_frame(
    ring: Option<&mut AsyncEncodeRing>,
    trailing: Option<&mut TrailingEdge>,
    gate: &FrameGate,
    has_dirty: impl FnOnce() -> Option<bool>,
    raw: &ID3D11Texture2D,
    timing: FrameTiming,
    capture: Instant,
    last: Instant,
    telemetry: &SessionTelemetry,
) -> Result<()> {
    // nothing new in the frame, nothing to trail: keep whatever is already held
    if gate.auto_frame_rate && gate.dirty_region_reporting && has_dirty() == Some(false) {
        telemetry
            .degradation
            .frames_skipped_cadence
            .fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let (Some(ring), Some(trailing)) = (ring, trailing) else {
        telemetry
            .degradation
            .frames_skipped_cadence
            .fetch_add(1, Ordering::Relaxed);
        return Ok(());
    };
    // older held frame first, so its slot is free for this one
    trailing.discard_pending(telemetry);
    let deadline =
        trailing_deadline(last, gate.frame_period) + gate.frame_period * (gate.divisor.max(1) - 1);
    match ring.stage_into(raw, timing, &mut trailing.hold_tx) {
        Ok(true) => {
            if let Ok(msg) = trailing.hold_rx.try_recv() {
                trailing.hold(msg, deadline, capture);
            }
        }
        // no free slot: `stage` counted the drop
        Ok(false) => {}
        // only ever a `RingFailed`: nothing can be staged again, so the capture halts here
        Err(e) => {
            teprintln!("ring capture stage (held frame) failed: {e:#}; stopping capture");
            return Err(e);
        }
    }
    Ok(())
}

/// Whether the pointer has moved, changed shape, or appeared or disappeared since the last frame.
///
/// WGC composites the cursor into the captured surface, but DWM tracks no dirty region for it,
/// so a pointer moving over a still window yields frames that report no dirty region. Skipping
/// those freezes the pointer at the 200 ms keepalive rate. `GetCursorInfo` takes no lock and
/// does not allocate, so it can sit in a WGC callback.
///
/// The cursor is only in this capture while it is over the captured display, so a pointer on
/// the host's own screen must not defeat the skip; leaving is still a change.
struct CursorProbe {
    /// The `HMONITOR` of the display being captured, as an integer so the struct stays `Send`.
    monitor: isize,
    handle: isize,
    x: i32,
    y: i32,
    /// The pointer was over this monitor and showing at the previous frame.
    on_monitor: bool,
    /// `GetCursorInfo` failed; stop asking and let the dirty region decide on its own.
    broken: bool,
}

impl CursorProbe {
    fn new(monitor: isize) -> Self {
        Self {
            monitor,
            handle: 0,
            x: i32::MIN,
            y: i32::MIN,
            on_monitor: false,
            broken: false,
        }
    }

    fn changed(&mut self) -> bool {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::Graphics::Gdi::{MonitorFromPoint, HMONITOR, MONITOR_DEFAULTTONULL};
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, CURSORINFO, CURSOR_SHOWING};

        if self.broken {
            return false;
        }
        let mut info = CURSORINFO {
            cbSize: std::mem::size_of::<CURSORINFO>() as u32,
            ..Default::default()
        };
        if unsafe { GetCursorInfo(&mut info) }.is_err() {
            self.broken = true;
            return false;
        }
        // `flags` is a bitfield: CURSOR_SHOWING (0x1) and, from Windows 8, CURSOR_SUPPRESSED
        // (0x2: touch or pen input, the system is not drawing the pointer). Drawn means the first
        // bit without the second
        const CURSOR_SUPPRESSED: u32 = 0x2;
        let showing = info.flags.0 & CURSOR_SHOWING.0 != 0 && info.flags.0 & CURSOR_SUPPRESSED == 0;
        let pt = POINT {
            x: info.ptScreenPos.x,
            y: info.ptScreenPos.y,
        };
        let mine = HMONITOR(self.monitor as *mut core::ffi::c_void);
        let on_monitor = showing && unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONULL) } == mine;
        let handle = info.hCursor.0 as isize;

        let changed = if on_monitor {
            !self.on_monitor || handle != self.handle || pt.x != self.x || pt.y != self.y
        } else {
            // leaving is the last frame the pointer affects; staying away affects nothing
            self.on_monitor
        };

        self.on_monitor = on_monitor;
        if on_monitor {
            self.handle = handle;
            self.x = pt.x;
            self.y = pt.y;
        }
        changed
    }
}

/// The encoder tier the newest live session actually started, in the compatibility report's
/// words. The report's own probe can only predict the tier from the adapters; once a session
/// has run, this is what it shows.
static STARTED_BACKEND: Mutex<Option<&'static str>> = Mutex::new(None);

fn note_started_backend(path_name: &'static str) {
    *STARTED_BACKEND.lock_recover() = Some(started_backend_label(path_name));
}

pub(crate) fn started_backend() -> Option<&'static str> {
    *STARTED_BACKEND.lock_recover()
}

/// The encoder tier label the compatibility report uses for a pipeline path name.
pub(crate) fn started_backend_label(path_name: &'static str) -> &'static str {
    match path_name {
        "nvenc-zero-copy-ring" | "zero-copy" => "nvenc",
        "cpu-bridge" => "nvenc (cpu bridge)",
        "intel-same-adapter" => "quicksync",
        "intel-cpu-bridge" => "quicksync (cpu bridge)",
        "amd-amf-ring" | "amd-amf-same-adapter" => "amf",
        "amd-amf-cpu-bridge" => "amf (cpu bridge)",
        "software-x264" => "x264",
        other => other,
    }
}

pub(crate) fn is_transient_encode_error(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("device busy")
}

pub(crate) fn live_encoder_config(
    native_w: u32,
    native_h: u32,
    refresh_hz: u32,
    cfg: &Config,
) -> EncoderConfig {
    let fps = if let Some(f) = cfg.fps {
        f.clamp(15, 550)
    } else {
        // no 60 fps floor: an indirect display has no scanout, so its reported refresh is the
        // rate DWM composes at and a higher encoder rate only buys duplicate frames
        let refresh = if refresh_hz == 0 { 60 } else { refresh_hz };
        let max_fps = cfg.max_fps.clamp(15, 550);
        refresh.clamp(15, max_fps)
    };

    let (width, height) = scaled_dims(native_w, native_h, cfg.scale);

    let computed =
        ((width as u64 * height as u64 * fps as u64) / 10).clamp(6_000_000, 30_000_000) as u32;
    let bitrate = cfg
        .max_bitrate_kbps
        .map(|kbps| kbps.saturating_mul(1000))
        .unwrap_or(computed);

    EncoderConfig {
        width,
        height,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: cfg.h264_profile,
        qp: cfg.qp,
        min_qp: cfg.quality_qp,
        intra_refresh: cfg.intra_refresh,
    }
}

pub(crate) fn scaled_dims(native_w: u32, native_h: u32, scale: ScalePercent) -> (u32, u32) {
    if scale.is_native() || native_w == 0 || native_h == 0 {
        return (native_w & !1, native_h & !1);
    }
    let w = scale.apply(native_w).max(2) & !1;
    let h = scale.apply(native_h).max(2) & !1;
    (w, h)
}

fn effective_vendor(cfg: &Config) -> crate::streamer::config::EncoderVendor {
    if cfg.software_encode_now() {
        crate::streamer::config::EncoderVendor::Software
    } else {
        cfg.encoder_vendor
    }
}

pub fn start(cfg: &Config) -> Result<Pipeline> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr_request = Arc::new(AtomicBool::new(false));
    let repair = Arc::new(crate::streamer::repair::RepairState::new());
    let target_bitrate = Arc::new(AtomicU32::new(0));
    let telemetry = SessionTelemetry::new_shared();

    if cfg.synthetic_pattern {
        let frame_duration = Duration::from_nanos(1_000_000_000 / SP_FPS as u64);
        let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
        telemetry.capture_backend.set("synthetic".into());
        let pipeline = Pipeline {
            tx: tx.clone(),
            frame_duration,
            telemetry: Arc::clone(&telemetry),
            idr_request: Arc::clone(&idr_request),
            target_bitrate: Arc::clone(&target_bitrate),
            wake,
            max_bitrate_bps: SP_BITRATE_BPS,
            h264_profile: cfg.h264_profile,
            repair: Arc::clone(&repair),
            frames: None,
        };
        let workload = cfg.workload;
        let counter = cfg.counter;
        let enc = EncoderConfig {
            width: SP_WIDTH,
            height: SP_HEIGHT,
            fps: SP_FPS,
            bitrate_bps: SP_BITRATE_BPS,
            max_bitrate_bps: SP_BITRATE_BPS,
            profile: cfg.h264_profile,
            qp: cfg.qp,
            min_qp: cfg.quality_qp,
            intra_refresh: cfg.intra_refresh,
        };
        let vendor = effective_vendor(cfg);
        std::thread::Builder::new()
            .name("synthetic-encode".to_string())
            .spawn(move || {
                synthetic_pattern_loop(
                    tx,
                    idr_request,
                    repair,
                    target_bitrate,
                    enc,
                    vendor,
                    wake_rx,
                    telemetry,
                    workload,
                    counter,
                )
            })
            .expect("spawn encode thread");
        return Ok(pipeline);
    }

    super::capture::check_dwm_composition()?;

    let (monitor, info) = select_monitor(cfg.monitor)?;
    if cfg.force_dxgi {
        let device_name = monitor
            .device_name()
            .map_err(|e| anyhow!("monitor device name for forced DXGI capture: {e}"))?;
        let (pipeline, control) = start_dxgi_capture(
            cfg,
            &device_name,
            &info,
            tx,
            idr_request,
            repair,
            target_bitrate,
            telemetry,
            None,
        )?;
        control.detach();
        return Ok(pipeline);
    }
    match start_live_capture(
        cfg,
        monitor,
        &info,
        tx.clone(),
        Arc::clone(&idr_request),
        Arc::clone(&repair),
        Arc::clone(&target_bitrate),
        Arc::clone(&telemetry),
        None,
    ) {
        Ok((pipeline, control)) => {
            std::mem::forget(control);
            Ok(pipeline)
        }
        Err(wgc_err) => {
            let device_name = match monitor.device_name() {
                Ok(name) => name,
                Err(e) => {
                    return Err(
                        wgc_err.context(format!("monitor device name for DXGI fallback: {e}"))
                    );
                }
            };
            teprintln!(
                "WGC capture failed for display {}: {wgc_err:#}; \
                 falling back to DXGI Desktop Duplication on {device_name}",
                info.index
            );
            telemetry
                .degradation
                .capture_fallback
                .fetch_add(1, Ordering::Relaxed);
            let (pipeline, control) = start_dxgi_capture(
                cfg,
                &device_name,
                &info,
                tx,
                idr_request,
                repair,
                target_bitrate,
                telemetry,
                None,
            )
            .map_err(|e| e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})")))?;
            control.detach();
            Ok(pipeline)
        }
    }
}

pub struct DxgiControl {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
    /// Cleared by [`detach`](Self::detach): the capture is meant to outlive this handle.
    stop_on_drop: bool,
    /// Duplication from the start holds the render preference for as long as this lives.
    claim: Option<DuplicationClaim>,
}

impl DxgiControl {
    pub(crate) fn new(stop: Arc<AtomicBool>, join: std::thread::JoinHandle<()>) -> Self {
        Self {
            stop,
            join: Some(join),
            stop_on_drop: true,
            claim: None,
        }
    }

    pub(crate) fn hold(&mut self, claim: DuplicationClaim) {
        self.claim = Some(claim);
    }

    /// Let go of the capture without stopping it. The probe harness's `start` hands back only
    /// the `Pipeline` and lets the capture run for the life of the process; a plain `drop`
    /// would stop it (see the `Drop` impl).
    pub(crate) fn detach(mut self) {
        self.stop_on_drop = false;
        self.join.take();
        // the claim lives as long as the capture: the life of the process
        std::mem::forget(self.claim.take());
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// A control dropped without [`DxgiControl::stop`] still stops its capture.
///
/// It signals and does not join: a drop can run on a tokio worker (the `spawn_blocking` result
/// of a join whose handler was cancelled), and joining there parks the worker while the capture
/// winds down. The capture thread sees the flag within one poll and joins its own encode
/// thread; the repeater follows within a tick.
impl Drop for DxgiControl {
    fn drop(&mut self) {
        if self.stop_on_drop {
            self.stop.store(true, Ordering::Relaxed);
        }
        // dropping the handle detaches the thread; it exits on the flag
        self.join.take();
    }
}

enum SessionControl {
    Wgc(CaptureControl<LiveCapture, anyhow::Error>),
    Dxgi(DxgiControl),
    /// Answered before its monitor was ready; the opening, once there is one, is inside.
    Deferred(Arc<DeferredSlot<SessionCapture>>),
}

pub struct SessionCapture {
    pub pipeline: Pipeline,
    control: Option<SessionControl>,
}

impl SessionCapture {
    /// Which capture API this session opened.
    pub fn backend_name(&self) -> &'static str {
        match self.control {
            Some(SessionControl::Wgc(_)) => "wgc",
            Some(SessionControl::Dxgi(_)) => "dxgi",
            Some(SessionControl::Deferred(ref slot)) => slot.backend(),
            None => "none",
        }
    }

    pub fn stop(mut self) {
        match self.control.take() {
            Some(SessionControl::Wgc(control)) => {
                if let Err(e) = control.stop() {
                    teprintln!("pipeline: stopping session capture failed: {e:?}");
                }
            }
            Some(SessionControl::Dxgi(control)) => control.stop(),
            Some(SessionControl::Deferred(slot)) => {
                if let Some(opened) = slot.end() {
                    opened.stop();
                }
            }
            None => {}
        }
    }
}

/// Stops the capture when a `SessionCapture` is dropped without `stop()` (a cancelled join
/// handler drops the `spawn_blocking` result nobody received). Signal only, never join (see
/// `DxgiControl`'s `Drop`): this can run on a tokio worker. For WGC it halts the frame pool and
/// drops the `CaptureControl`, whose own `Drop` ends its thread.
impl Drop for SessionCapture {
    fn drop(&mut self) {
        match self.control.take() {
            Some(SessionControl::Wgc(control)) => {
                control.halt_handle().store(true, Ordering::Relaxed);
                drop(control);
            }
            Some(SessionControl::Dxgi(control)) => drop(control),
            Some(SessionControl::Deferred(slot)) => drop(slot.end()),
            None => {}
        }
    }
}

/// The GDI name to capture for `target`, re-derived from the monitor rather than trusted.
///
/// A `\\.\DISPLAYn` is a slot Windows hands straight back out when the monitor holding it
/// detaches, so a cached name can name the host's own panel or another client's display.
/// Capture goes through the same rule every display write does
/// ([`resolve_owned_display`](crate::windows_utils::virtual_display::resolve_owned_display)):
/// the monitor must be one this host created. Retried `retries` times across the second a mode
/// change can take to settle.
fn resolve_capture_name(
    target: &crate::streamer::session::DisplayTarget,
    retries: u32,
) -> Result<String> {
    resolve_capture_name_with(target, retries, WGC_STALE_MONITOR_BACKOFF, &mut |t| {
        crate::windows_utils::virtual_display::resolve_owned_display(t).map(|r| r.gdi_name)
    })
}

/// [`resolve_capture_name`] with the resolver and the wait between attempts given, so the retry
/// and refusal rules can be tested without a display.
pub(crate) fn resolve_capture_name_with(
    target: &crate::streamer::session::DisplayTarget,
    retries: u32,
    backoff: Duration,
    resolve: &mut dyn FnMut(&crate::streamer::session::DisplayTarget) -> Result<String>,
) -> Result<String> {
    let mut attempt = 0u32;
    loop {
        match resolve(target)
            .map_err(|e| e.context("refusing to capture a display this host does not own"))
        {
            Ok(name) => return Ok(name),
            Err(e) if attempt < retries => {
                attempt += 1;
                teprintln!(
                    "capture: {} does not resolve to a display this host owns yet ({e:#}); \
                     retry {attempt}/{retries}",
                    target.device_name
                );
                std::thread::sleep(backoff);
            }
            Err(e) => return Err(e),
        }
    }
}

pub fn start_on_monitor(cfg: &Config, device_name: &str) -> Result<SessionCapture> {
    start_on_display(
        cfg,
        &crate::streamer::session::DisplayTarget {
            device_path: String::new(),
            device_name: device_name.to_string(),
        },
    )
}

/// The parts of a session's pipeline that outlive any one opening of its capture.
struct CaptureShared {
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,
    target_bitrate: Arc<AtomicU32>,
    telemetry: SharedTelemetry,
    /// One wake channel for every opening, when the session was handed its pipeline before any
    /// opening existed (a deferred capture); `None` gives each opening its own.
    wake: Option<WakePair>,
    /// Duplication from the start: the listing adapter this attempt last asked the driver to
    /// compose on, so the request is made once per attempt, not at every opening.
    composed: ComposeOnce,
    /// A deferred capture's budget, which the compose request must also fit in.
    budget: Option<Arc<ReadyBudget>>,
}

/// The budget of a deferred capture's wait. Time spent waiting out a withheld desktop is not
/// counted: a locked host is not a monitor that never became ready.
pub(crate) struct ReadyBudget {
    started: Instant,
    budget: Duration,
    paused: Mutex<Duration>,
}

impl ReadyBudget {
    pub(crate) fn new(budget: Duration) -> Self {
        Self {
            started: Instant::now(),
            budget,
            paused: Mutex::new(Duration::ZERO),
        }
    }

    pub(crate) fn pause(&self, d: Duration) {
        *self.paused.lock_recover() += d;
    }

    pub(crate) fn left(&self) -> Duration {
        let paused = *self.paused.lock_recover();
        self.budget
            .saturating_sub(self.started.elapsed().saturating_sub(paused))
    }

    pub(crate) fn spent(&self) -> bool {
        self.left().is_zero()
    }
}

/// Remembers the adapter an attempt last asked the driver to compose its display on.
#[derive(Default)]
pub(crate) struct ComposeOnce(Mutex<Option<i64>>);

impl ComposeOnce {
    /// The adapter to ask for now: DXGI lists the output under exactly one adapter and it is not
    /// the one already asked for on this attempt. Recorded whether or not the request succeeds,
    /// so a silent driver costs one request per attempt.
    pub(crate) fn take(&self, listing: &[i64]) -> Option<i64> {
        let [only] = listing[..] else {
            return None;
        };
        let mut last = self.0.lock_recover();
        if *last == Some(only) {
            return None;
        }
        *last = Some(only);
        Some(only)
    }
}

/// Why a duplication start was refused, when the duplicator said it was only not ready. Read
/// from the typed error it returns for the codes its own duplication calls got, never from text.
pub(crate) fn not_ready_kind(e: &anyhow::Error) -> Option<NotReady> {
    e.downcast_ref::<DuplicationNotReady>().map(|n| n.kind)
}

/// How long an unbroken run of `DXGI_ERROR_UNSUPPORTED` is waited on. The run starts at the
/// first refusal, which comes only once DXGI lists the output and the request to compose it
/// there has had its answer or its bound; a composition change takes effect well inside this,
/// and a driver that cannot move composition never will.
#[cfg(not(test))]
pub(crate) const UNSUPPORTED_BOUND: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(crate) const UNSUPPORTED_BOUND: Duration = Duration::from_millis(300);
/// How often a withheld desktop is asked again.
const WITHHELD_POLL: Duration = Duration::from_millis(500);

static DUPLICATION_CLAIMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A session capturing through duplication from the start (`--capture-backend dxgi`) while it
/// opens and for as long as its capture lives; a failed opening drops it at once.
pub(crate) struct DuplicationClaim(());

impl DuplicationClaim {
    pub(crate) fn new() -> Self {
        DUPLICATION_CLAIMS.fetch_add(1, Ordering::Relaxed);
        Self(())
    }
}

impl Drop for DuplicationClaim {
    fn drop(&mut self) {
        DUPLICATION_CLAIMS.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn duplication_claims() -> usize {
    DUPLICATION_CLAIMS.load(Ordering::Relaxed)
}

/// Whether the driver's render preference belongs to duplication: it is this host's capture
/// tier, or a live session captures through duplication from the start. The one rule both
/// requests read; while it holds the encoder-follow request stands down, since duplication
/// delivers only from the adapter DXGI lists the output under.
pub fn duplication_owns_render() -> bool {
    dxgi_is_capture_tier() || duplication_claims() > 0
}

/// A duplication start that failed after WGC did, as text: only duplication from the start
/// waits on a duplication that is not ready, so the typed reason must not survive.
pub(crate) fn fallback_error(wgc_err: &anyhow::Error, e: anyhow::Error) -> anyhow::Error {
    anyhow!("DXGI fallback (after WGC failed: {wgc_err:#}): {e:#}")
}

/// The most a duplication opening spends asking the driver for the composing adapter, the
/// request and the wait for it together.
const COMPOSE_BOUND: Duration = Duration::from_secs(2);

/// Duplication from the start: the monitor is ready once it resolves to an output DXGI lists. A
/// name resolves milliseconds after the attach, the output appears in DXGI only once the shell
/// has taken the monitor in.
pub(crate) fn duplication_ready(
    resolve: &mut dyn FnMut() -> Result<String>,
    listed: &mut dyn FnMut(&str) -> bool,
) -> bool {
    match resolve() {
        Ok(name) => listed(&name),
        Err(_) => false,
    }
}

type WakePair = (
    crossbeam_channel::Sender<()>,
    crossbeam_channel::Receiver<()>,
);

#[derive(Clone, Copy)]
struct CaptureSwitches {
    weak_sender: bool,
    wait_not_ready: bool,
}

/// How long an opened capture may go without delivering a frame before it is opened again.
/// On a still monitor WGC's first frame comes 1.0 to 1.2 s after the session starts (the next
/// composition of that output) and a reopen costs about 4 s, so the bound sits well past that.
/// `SE_FIRST_FRAME_MS` overrides it.
const FIRST_FRAME_BOUND: Duration = Duration::from_millis(3000);
/// Openings after the first, and the pause between two.
const FIRST_FRAME_REOPENS: u32 = 2;
const FIRST_FRAME_FLOOR: Duration = Duration::from_millis(250);

fn first_frame_bound() -> Duration {
    static BOUND: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *BOUND.get_or_init(|| {
        crate::envcfg::var("SE_FIRST_FRAME_MS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(FIRST_FRAME_BOUND, Duration::from_millis)
    })
}

/// Whether the capture handed over a frame after `before` was read, within `bound`.
pub(crate) fn wait_first_frame(arrived: &AtomicU64, before: u64, bound: Duration) -> bool {
    wait_first_frame_while(arrived, before, bound, &|| true)
}

/// [`wait_first_frame`] that also gives up as soon as `still_current` turns false.
fn wait_first_frame_while(
    arrived: &AtomicU64,
    before: u64,
    bound: Duration,
    still_current: &dyn Fn() -> bool,
) -> bool {
    let started = Instant::now();
    loop {
        if arrived.load(Ordering::Relaxed) > before {
            return true;
        }
        if started.elapsed() >= bound || !still_current() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `SE_FORCE_DXGI=1` with `SE_TEST_HOOKS=1`: every session captures through DXGI duplication,
/// which a real join otherwise reaches only when the first capture API fails, so it can be
/// measured. Read once.
fn force_dxgi_hook() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| super::tune::hook_var("SE_FORCE_DXGI").as_deref() == Some("1"))
}

/// Whether DXGI duplication is this host's capture from the start, so a display can be set up
/// for it before it exists: the first capture API is unavailable by probe, or the hook forces
/// duplication. Read once. A session that reaches DXGI as a fallback after the first API failed
/// on an existing display is not this.
pub fn dxgi_is_capture_tier() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        force_dxgi_hook() || !crate::windows_capture::graphics_capture_api::wgc_supported()
    })
}

/// Start capturing the display `target` names.
///
/// With a device path (the session's `LiveDisplay::target()`) the monitor is found by that path
/// whatever `\\.\DISPLAYn` it answers to now; with only a name it must still resolve to a
/// display this host created. A name that has moved to somebody else's monitor is refused, on
/// the first attempt and on every stale-handle retry.
///
/// With `capture_first_frame_reopen` a capture that delivers nothing within
/// [`FIRST_FRAME_BOUND`] is stopped and opened again from WGC, a bounded number of times: a new
/// monitor can hand out a capture item seconds before a capture of it delivers anything. The
/// start never fails for want of a frame: a withheld desktop (locked, the secure desktop) is
/// not reopened, and after the last reopen the last opening is kept.
pub fn start_on_display(
    cfg: &Config,
    target: &crate::streamer::session::DisplayTarget,
) -> Result<SessionCapture> {
    start_on_display_while(cfg, target, &|| true)
}

/// [`start_on_display`] for an attempt that a newer one can overtake: `still_current` is asked at
/// every wait and retry, and a `false` stops the start with [`Superseded`].
pub fn start_on_display_while(
    cfg: &Config,
    target: &crate::streamer::session::DisplayTarget,
    still_current: &dyn Fn() -> bool,
) -> Result<SessionCapture> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let shared = CaptureShared {
        tx,
        idr_request: Arc::new(AtomicBool::new(false)),
        repair: Arc::new(crate::streamer::repair::RepairState::new()),
        target_bitrate: Arc::new(AtomicU32::new(0)),
        telemetry: SessionTelemetry::new_shared(),
        wake: None,
        composed: ComposeOnce::default(),
        budget: None,
    };
    let switches = CaptureSwitches {
        weak_sender: super::tune::candidate("pipeline_weak_sender"),
        wait_not_ready: super::tune::on("capture_wait_not_ready"),
    };
    open_on(cfg, target, &shared, switches, still_current)
}

/// Open the capture on `target` with the first-frame test, on the parts `shared` holds.
fn open_on(
    cfg: &Config,
    target: &crate::streamer::session::DisplayTarget,
    shared: &CaptureShared,
    switches: CaptureSwitches,
    still_current: &dyn Fn() -> bool,
) -> Result<SessionCapture> {
    if !super::tune::on("capture_first_frame_reopen") {
        if !still_current() {
            return Err(anyhow::Error::new(Superseded));
        }
        return open_capture(cfg, target, shared, switches, still_current);
    }
    let rules = FirstFrameRules {
        bound: first_frame_bound(),
        reopens: FIRST_FRAME_REOPENS,
        floor: FIRST_FRAME_FLOOR,
    };
    let t = &shared.telemetry;
    let (capture, first) = open_until_first_frame(
        &rules,
        &t.capture_frames_arrived,
        &t.degradation.capture_first_frame_reopens,
        still_current,
        &mut || open_capture(cfg, target, shared, switches, still_current),
        // a dark DXGI opening holds the output itself, so a second duplication cannot tell a
        // locked desktop from its own hold; it is reopened from WGC and asked then
        &|backend| backend != "dxgi" && desktop_withheld_for(target),
        &super::workarea::generation,
        &target.device_name,
    )?;
    if let Some(backend) = first {
        t.first_frame_backend.set(backend.to_string());
    }
    Ok(capture)
}

/// How long a capture answered before its monitor was ready may wait for it. The shell's slowest
/// commit of a new monitor's work area seen so far was 26 s after attach; this is half as much
/// again. Past it the session is ended, and says why.
pub const CAPTURE_READY_BUDGET: Duration = Duration::from_secs(40);
/// How often the capture item is asked for while waiting, and how often in the two seconds after
/// a work-area broadcast, which is when it usually becomes creatable (0.15 to 1.05 s after,
/// measured). The poll is the gate either way: a capture is never started while it is refused.
const READY_POLL: Duration = Duration::from_millis(100);
const READY_POLL_FAST: Duration = Duration::from_millis(50);
const READY_FAST_FOR: Duration = Duration::from_secs(2);
/// An opening that fails for a reason other than "not ready" (a capture start error, an encoder
/// build refused, a name that did not resolve) is tried this many times in all, this far apart,
/// within the budget, before the session is ended. Each try resolves the display afresh.
const OPEN_TRIES: u32 = 3;
const OPEN_RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Whether `target`'s monitor hands out a capture item now. Resolved by its device path, with
/// the ownership test every display write uses.
fn capture_item_ready(
    target: &crate::streamer::session::DisplayTarget,
) -> Result<(
    String,
    crate::windows_capture::monitor::Monitor,
    super::capture::MonitorInfo,
)> {
    let t0 = Instant::now();
    let name = resolve_capture_name(target, 0)?;
    let t1 = Instant::now();
    let (monitor, info) = select_monitor_by_device_name(&name)?;
    let t2 = Instant::now();
    let item: std::result::Result<crate::windows_capture::settings::GraphicsCaptureItemType, _> =
        monitor.try_into();
    let t3 = Instant::now();
    LAST_ITEM_CALL.with(|c| {
        c.set([
            ms(t1.duration_since(t0)),
            ms(t2.duration_since(t1)),
            ms(t3.duration_since(t2)),
        ])
    });
    item.map_err(|e| anyhow!("{name} refused a capture item: {e}"))?;
    Ok((name, monitor, info))
}

thread_local! {
    /// How the last capture-item ask on this thread split: resolving the display, finding its
    /// monitor, and creating the item (`CreateForMonitor`), in ms. For the opening's timing line.
    static LAST_ITEM_CALL: std::cell::Cell<[f64; 3]> = const { std::cell::Cell::new([0.0; 3]) };
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Where a capture answered before it opened is: waiting for its monitor, running, or ended.
pub(crate) enum Deferred<C> {
    Waiting,
    Running(C),
    Ended,
}

/// Shared by the session (which may end it) and the task that opens it. Whichever comes second
/// does the stopping, exactly once: an end takes out what is running, and an opening that
/// arrives after an end is handed back to be stopped.
pub(crate) struct DeferredSlot<C> {
    state: Mutex<Deferred<C>>,
}

impl<C: Opening> DeferredSlot<C> {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(Deferred::Waiting),
        }
    }

    /// Hand an opened capture over. Given back when the session has already ended.
    pub(crate) fn install(&self, opened: C) -> Option<C> {
        let mut state = self.state.lock_recover();
        if matches!(*state, Deferred::Ended) {
            return Some(opened);
        }
        *state = Deferred::Running(opened);
        None
    }

    /// End it. What was running is taken out for the caller to stop; nothing is, the second time.
    pub(crate) fn end(&self) -> Option<C> {
        match std::mem::replace(&mut *self.state.lock_recover(), Deferred::Ended) {
            Deferred::Running(opened) => Some(opened),
            _ => None,
        }
    }

    pub(crate) fn ended(&self) -> bool {
        matches!(*self.state.lock_recover(), Deferred::Ended)
    }

    pub(crate) fn running(&self) -> bool {
        matches!(*self.state.lock_recover(), Deferred::Running(_))
    }

    fn backend(&self) -> &'static str {
        match &*self.state.lock_recover() {
            Deferred::Running(opened) => opened.backend(),
            _ => "pending",
        }
    }
}

/// How the wait of a deferred capture ended.
#[derive(Debug)]
pub(crate) enum DeferredEnd {
    /// Opened and handed to the session.
    Running,
    /// A newer attempt owns the join.
    Superseded,
    /// The session ended first.
    Ended,
    /// The monitor never handed out a capture item within the budget.
    OutOfBudget,
    /// The capture could not be opened for a reason waiting does not cure.
    Failed(String),
}

/// The waiting side of a deferred capture: polls for the capture item, opens the capture and
/// hands it over, within `budget`. `wait` sleeps up to its argument and says whether a
/// work-area broadcast arrived meanwhile.
///
/// A duplication start that is not ready is not a failed try: one still settling is waited on
/// within the budget, a withheld desktop (locked, or the secure desktop) outside it for as long
/// as it lasts, and `DXGI_ERROR_UNSUPPORTED` that persists for [`UNSUPPORTED_BOUND`] fails the
/// session.
pub(crate) fn run_deferred<C: Opening>(
    slot: &DeferredSlot<C>,
    budget: &ReadyBudget,
    still_current: &dyn Fn() -> bool,
    item_ready: &mut dyn FnMut() -> bool,
    wait: &mut dyn FnMut(Duration) -> bool,
    open: &mut dyn FnMut() -> Result<C>,
) -> DeferredEnd {
    let mut fast_until: Option<Instant> = None;
    let mut failed_tries = 0u32;
    let mut unsupported_since: Option<Instant> = None;
    let mut withheld_since: Option<Instant> = None;
    loop {
        let turn = Instant::now();
        if !still_current() {
            return DeferredEnd::Superseded;
        }
        if slot.ended() {
            return DeferredEnd::Ended;
        }
        if item_ready() {
            match open() {
                Ok(opened) => {
                    if !still_current() {
                        opened.stop();
                        return DeferredEnd::Superseded;
                    }
                    return match slot.install(opened) {
                        None => DeferredEnd::Running,
                        Some(late) => {
                            late.stop();
                            DeferredEnd::Ended
                        }
                    };
                }
                // the opening stands down for either reason; say which
                Err(e) if e.downcast_ref::<Superseded>().is_some() => {
                    return if slot.ended() {
                        DeferredEnd::Ended
                    } else {
                        DeferredEnd::Superseded
                    };
                }
                // refused again between the poll and the opening: still only not ready
                Err(e) if e.downcast_ref::<StaleMonitor>().is_some() => {}
                Err(e) => {
                    let kind = not_ready_kind(&e);
                    if kind != Some(NotReady::Withheld) {
                        if let Some(since) = withheld_since.take() {
                            tprintln!(
                                "capture: the desktop is back after {} ms",
                                since.elapsed().as_millis()
                            );
                        }
                    }
                    if kind != Some(NotReady::Unsupported) {
                        unsupported_since = None;
                    }
                    match kind {
                        Some(NotReady::Settling) => {}
                        // the whole turn is off the budget, the opening that found the desktop
                        // withheld included: it builds devices and asks for a duplication
                        Some(NotReady::Withheld) => {
                            if withheld_since.is_none() {
                                withheld_since = Some(Instant::now());
                                tprintln!(
                                    "capture: the desktop is withheld (locked, or the secure \
                                     desktop); waiting for it, outside the capture budget ({e:#})"
                                );
                            }
                            wait(WITHHELD_POLL);
                            budget.pause(turn.elapsed());
                            continue;
                        }
                        Some(NotReady::Unsupported) => {
                            let since = *unsupported_since.get_or_insert_with(Instant::now);
                            if since.elapsed() >= UNSUPPORTED_BOUND {
                                teprintln!(
                                    "capture: DXGI duplication is unsupported for this display on \
                                     the adapter that lists it: refused for {} ms after the \
                                     output was listed and its composition asked for ({e:#})",
                                    since.elapsed().as_millis()
                                );
                                return DeferredEnd::Failed(format!(
                                    "duplication is unsupported for this display on this \
                                     adapter: {e:#}"
                                ));
                            }
                        }
                        None => {
                            failed_tries += 1;
                            teprintln!(
                                "capture: opening try {failed_tries}/{OPEN_TRIES} failed: {e:#}"
                            );
                            if failed_tries >= OPEN_TRIES || budget.spent() {
                                return DeferredEnd::Failed(format!("{e:#}"));
                            }
                            std::thread::sleep(OPEN_RETRY_BACKOFF);
                            continue;
                        }
                    }
                }
            }
        }
        if budget.spent() {
            return DeferredEnd::OutOfBudget;
        }
        let fast = fast_until.is_some_and(|t| Instant::now() < t);
        if wait(if fast { READY_POLL_FAST } else { READY_POLL }) {
            fast_until = Some(Instant::now() + READY_FAST_FOR);
        }
    }
}

/// Clears the waiting flag when the wait ends, however it ends; a wait that ends in a panic gives
/// up, so the session is ended rather than left saying "pending" with nothing waiting.
pub(crate) struct WaitGuard {
    telemetry: SharedTelemetry,
    give_up: Option<Box<dyn FnOnce(String) + Send>>,
}

impl WaitGuard {
    pub(crate) fn new(telemetry: SharedTelemetry, give_up: Box<dyn FnOnce(String) + Send>) -> Self {
        Self {
            telemetry,
            give_up: Some(give_up),
        }
    }

    pub(crate) fn give_up(&mut self, why: String) {
        if let Some(g) = self.give_up.take() {
            g(why);
        }
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.telemetry
            .capture_waiting
            .store(false, Ordering::Relaxed);
        if std::thread::panicking() {
            self.give_up("the capture wait failed unexpectedly".to_string());
        }
    }
}

/// Start capturing `target` without waiting for its monitor (`answer_before_capture`).
///
/// Returns at once with a session capture the transport can bind to: its frame channel, keyframe
/// and bitrate controls, repair state and telemetry are the ones every opening will use, and its
/// encoder limits come from `native` (the monitor's size and refresh rate). A thread of its own
/// waits for the capture item, opens the capture and hands it over; a newer attempt, or the
/// session ending, stands it down. At [`CAPTURE_READY_BUDGET`], or on a failure waiting cannot
/// cure, `give_up` is called once with the reason and ends the session.
pub fn start_on_display_deferred(
    cfg: &Config,
    target: &crate::streamer::session::DisplayTarget,
    native: (u32, u32, u32),
    still_current: Arc<dyn Fn() -> bool + Send + Sync>,
    give_up: Box<dyn FnOnce(String) + Send>,
) -> Result<SessionCapture> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let shared = CaptureShared {
        tx,
        idr_request: Arc::new(AtomicBool::new(false)),
        repair: Arc::new(crate::streamer::repair::RepairState::new()),
        target_bitrate: Arc::new(AtomicU32::new(0)),
        telemetry: SessionTelemetry::new_shared(),
        wake: Some((wake.clone(), wake_rx)),
        composed: ComposeOnce::default(),
        budget: Some(Arc::new(ReadyBudget::new(CAPTURE_READY_BUDGET))),
    };
    let switches = CaptureSwitches {
        weak_sender: super::tune::candidate("pipeline_weak_sender"),
        wait_not_ready: super::tune::on("capture_wait_not_ready"),
    };
    let config = live_encoder_config(native.0, native.1, native.2, cfg);
    let mut pipeline = Pipeline {
        tx: shared.tx.clone(),
        frame_duration: Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64),
        telemetry: Arc::clone(&shared.telemetry),
        idr_request: Arc::clone(&shared.idr_request),
        target_bitrate: Arc::clone(&shared.target_bitrate),
        wake,
        max_bitrate_bps: config.max_bitrate_bps,
        h264_profile: cfg.h264_profile,
        repair: Arc::clone(&shared.repair),
        frames: None,
    };
    if switches.weak_sender {
        pipeline.release_sender();
    }
    shared
        .telemetry
        .picture_pending
        .store(true, Ordering::Relaxed);
    shared
        .telemetry
        .capture_waiting
        .store(true, Ordering::Relaxed);

    // duplication opens no WGC capture item: the monitor is ready once DXGI lists its output
    let duplication = cfg.force_dxgi || dxgi_is_capture_tier();
    let slot = Arc::new(DeferredSlot::<SessionCapture>::new());
    let thread_slot = Arc::clone(&slot);
    let cfg = cfg.clone();
    let target = target.clone();
    std::thread::Builder::new()
        .name("capture-wait".into())
        .spawn(move || {
            // a panic anywhere below still clears the waiting flag and ends the session
            let mut guard = WaitGuard::new(Arc::clone(&shared.telemetry), give_up);
            let _com = crate::windows_utils::com::ensure_for_this_thread(
                "capture-wait",
                crate::windows_utils::com::Apartment::Mta,
            );
            // also stands the opening down once the session has ended, not only when superseded
            let current = || still_current() && !thread_slot.ended();
            let label = target.device_name.clone();
            let started = Instant::now();
            let mut seen = super::workarea::generation();
            let mut broadcast_at: Option<Instant> = None;
            let budget = Arc::clone(shared.budget.as_ref().expect("a deferred capture's budget"));
            let end = run_deferred(
                &thread_slot,
                &budget,
                &*still_current,
                &mut || {
                    if duplication {
                        duplication_ready(&mut || resolve_capture_name(&target, 0), &mut |name| {
                            !super::dxgi::listing_adapters(name).is_empty()
                        })
                    } else {
                        capture_item_ready(&target).is_ok()
                    }
                },
                &mut |poll| {
                    let (now, at) = super::workarea::wait_after(seen, poll);
                    let came = now != seen;
                    seen = now;
                    if came {
                        broadcast_at = at;
                    }
                    came
                },
                &mut || open_on(&cfg, &target, &shared, switches, &current),
            );
            let t = &shared.telemetry;
            t.capture_waiting.store(false, Ordering::Relaxed);
            let broadcast = broadcast_at.map_or_else(
                || "no work-area broadcast seen".to_string(),
                |at| {
                    format!(
                        "the work-area broadcast came {} ms after the answer",
                        at.saturating_duration_since(started).as_millis()
                    )
                },
            );
            match end {
                DeferredEnd::Running => tprintln!(
                    "capture: {label} opened {} ms after the answer ({broadcast})",
                    started.elapsed().as_millis()
                ),
                DeferredEnd::Superseded | DeferredEnd::Ended => tprintln!(
                    "capture: the wait for {label} stood down ({end:?}) after {} ms",
                    started.elapsed().as_millis()
                ),
                DeferredEnd::OutOfBudget => {
                    t.degradation
                        .capture_never_ready
                        .fetch_add(1, Ordering::Relaxed);
                    guard.give_up(format!(
                        "{label} handed out no capture item within {} s of the answer ({broadcast})",
                        CAPTURE_READY_BUDGET.as_secs()
                    ));
                }
                DeferredEnd::Failed(why) => {
                    guard.give_up(format!("the capture of {label} failed: {why}"))
                }
            }
        })
        .context("spawning the capture wait")?;

    Ok(SessionCapture {
        pipeline,
        control: Some(SessionControl::Deferred(slot)),
    })
}

/// Whether the desktop on the display `target` names is withheld from every capture now (the
/// secure desktop, a disconnected session). The monitor is found by its device path at the time
/// of asking, with the same ownership test every display write uses; unresolved is `false`.
pub(crate) fn desktop_withheld_for(target: &crate::streamer::session::DisplayTarget) -> bool {
    match resolve_capture_name(target, 0) {
        Ok(name) => super::dxgi::desktop_withheld(&name),
        Err(_) => false,
    }
}

/// What the first-frame loop needs from one opening of a capture.
pub(crate) trait Opening {
    fn backend(&self) -> &'static str;
    fn stop(self);
}

impl Opening for SessionCapture {
    fn backend(&self) -> &'static str {
        self.backend_name()
    }
    fn stop(self) {
        SessionCapture::stop(self)
    }
}

pub(crate) struct FirstFrameRules {
    pub bound: Duration,
    pub reopens: u32,
    pub floor: Duration,
}

/// Open a capture until one delivers a frame within `rules.bound`, opening it again at most
/// `rules.reopens` times with `rules.floor` between two; each reopen is counted in `reopened`. The
/// previous opening is stopped (and with it its encoder) before the next is opened. Gives back
/// the last opening either way, with the backend that delivered when one did: while `withheld`
/// says the desktop is locked or on the secure desktop nothing is reopened, and after the last
/// reopen the last opening is kept rather than the start failing. `work_area` counts the shell's
/// work-area broadcasts; one arriving before any frame ends the wait early.
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_until_first_frame<C: Opening>(
    rules: &FirstFrameRules,
    arrived: &AtomicU64,
    reopened: &AtomicU64,
    still_current: &dyn Fn() -> bool,
    open: &mut dyn FnMut() -> Result<C>,
    withheld: &dyn Fn(&'static str) -> bool,
    work_area: &dyn Fn() -> u64,
    label: &str,
) -> Result<(C, Option<&'static str>)> {
    let mut reopens = 0u32;
    loop {
        if !still_current() {
            return Err(anyhow::Error::new(Superseded));
        }
        let before = arrived.load(Ordering::Relaxed);
        let opened_at = Instant::now();
        let shell_before = work_area();
        let capture = open()?;
        let backend = capture.backend();
        // the bound is a ceiling: a work-area commit after this opening started, with no frame
        // yet, is what makes an opening on a fresh monitor never deliver, so reopen then
        let waiting = || still_current() && work_area() == shell_before;
        if wait_first_frame_while(arrived, before, rules.bound, &waiting) {
            tprintln!(
                "capture: first frame of {label} from {backend} {} ms after it opened{}",
                opened_at.elapsed().as_millis(),
                if reopens > 0 {
                    format!(" ({reopens} reopen(s))")
                } else {
                    String::new()
                }
            );
            return Ok((capture, Some(backend)));
        }
        let shell_moved = work_area() != shell_before;
        // before the desktop probe, which builds a device per adapter
        if !still_current() {
            capture.stop();
            return Err(anyhow::Error::new(Superseded));
        }
        if withheld(backend) {
            tprintln!(
                "capture: {backend} on {label} has no frame yet and the desktop is withheld \
                 (locked, or the secure desktop); answering without one"
            );
            return Ok((capture, None));
        }
        if reopens >= rules.reopens {
            teprintln!(
                "capture: {backend} on {label} delivered no frame within {} ms, {reopens} \
                 reopen(s) included; answering with it as it is",
                rules.bound.as_millis()
            );
            return Ok((capture, None));
        }
        reopens += 1;
        reopened.fetch_add(1, Ordering::Relaxed);
        if shell_moved {
            teprintln!(
                "capture: {backend} on {label}: the shell committed the work area {} ms after \
                 the opening, before any frame; opening it again from WGC (reopen {reopens}/{})",
                opened_at.elapsed().as_millis(),
                rules.reopens
            );
        } else {
            teprintln!(
                "capture: {backend} on {label} delivered no frame within {} ms; opening it again \
                 from WGC (reopen {reopens}/{})",
                rules.bound.as_millis(),
                rules.reopens
            );
        }
        capture.stop();
        std::thread::sleep(rules.floor);
    }
}

/// One opening of a session's capture: WGC first, DXGI when WGC cannot be used.
fn open_capture(
    cfg: &Config,
    target: &crate::streamer::session::DisplayTarget,
    shared: &CaptureShared,
    switches: CaptureSwitches,
    still_current: &dyn Fn() -> bool,
) -> Result<SessionCapture> {
    let tx = shared.tx.clone();
    let idr_request = Arc::clone(&shared.idr_request);
    let repair = Arc::clone(&shared.repair);
    let target_bitrate = Arc::clone(&shared.target_bitrate);
    let telemetry = Arc::clone(&shared.telemetry);
    let weak_sender = switches.weak_sender;

    let t_open = Instant::now();
    super::capture::check_dwm_composition()?;
    let dwm_ms = ms(t_open.elapsed());

    // a just-attached monitor refuses a capture item for a second or more, and a WGC start that
    // finds it refused has already built a D3D device and a dispatcher queue: wait for the item
    // first (not for duplication from the start, which opens none)
    let wait_for_item =
        !cfg.force_dxgi && !dxgi_is_capture_tier() && super::tune::on("capture_item_ready_wait");
    let ready = if wait_for_item {
        // counted from the display's last attach or write, so a retry waits only for what is left
        let wait = super::capture::item_wait_remaining(target, CAPTURE_ITEM_WAIT);
        let poll = CAPTURE_ITEM_POLL;
        let t_item = Instant::now();
        let waited =
            wait_for_capture_item(target, wait, poll, still_current, &mut capture_item_ready);
        let [resolve_ms, monitor_ms, create_ms] = LAST_ITEM_CALL.with(|c| c.get());
        tprintln!(
            "capture: {} before the start: dwm {dwm_ms:.0} ms, item {:.0} ms (last ask: resolve \
             {resolve_ms:.0} ms, monitor {monitor_ms:.0} ms, CreateForMonitor {create_ms:.0} ms)",
            target.device_name,
            ms(t_item.elapsed())
        );
        match waited {
            Ok(found) => Some(found),
            Err(e) if e.downcast_ref::<Superseded>().is_some() => return Err(e),
            Err(e) => {
                teprintln!("capture: {e:#}; starting anyway");
                None
            }
        }
    } else {
        None
    };
    let (mut device_name, mut monitor, mut info) = match ready {
        Some(found) => found,
        None => {
            let name = resolve_capture_name(target, WGC_STALE_MONITOR_RETRIES)?;
            let (monitor, info) = select_monitor_by_device_name(&name)?;
            (name, monitor, info)
        }
    };
    if device_name != target.device_name {
        tprintln!(
            "capture: {} now answers to {device_name}; capturing the monitor, not the name",
            target.device_name
        );
    }

    if cfg.force_dxgi || dxgi_is_capture_tier() {
        // the display must be composed where DXGI lists it, which can only be read now that the
        // output exists; asked once per attempt and never past the attempt's budget
        let claim = DuplicationClaim::new();
        if still_current() {
            let listing = super::dxgi::listing_adapters(&device_name);
            let left = shared
                .budget
                .as_ref()
                .map_or(COMPOSE_BOUND, |b| b.left().min(COMPOSE_BOUND));
            if !left.is_zero() {
                if let Some(adapter) = shared.composed.take(&listing) {
                    crate::windows_utils::virtual_display::compose_for_duplication(
                        &device_name,
                        adapter,
                        left,
                    );
                }
            }
        }
        let (pipeline, control) = start_dxgi_capture(
            cfg,
            &device_name,
            &info,
            tx,
            idr_request,
            repair,
            target_bitrate,
            telemetry,
            shared.wake.clone(),
        )?;
        let mut control = control;
        control.hold(claim);
        let mut pipeline = pipeline;
        if weak_sender {
            pipeline.release_sender();
        }
        return Ok(SessionCapture {
            pipeline,
            control: Some(SessionControl::Dxgi(control)),
        });
    }

    let mut stale_retries = 0u32;
    let started = loop {
        if !still_current() {
            return Err(anyhow::Error::new(Superseded));
        }
        let attempt = start_live_capture(
            cfg,
            monitor,
            &info,
            tx.clone(),
            Arc::clone(&idr_request),
            Arc::clone(&repair),
            Arc::clone(&target_bitrate),
            Arc::clone(&telemetry),
            shared.wake.clone(),
        );
        match attempt {
            Err(e)
                if e.downcast_ref::<StaleMonitor>().is_some()
                    && stale_retries < WGC_STALE_MONITOR_RETRIES =>
            {
                stale_retries += 1;
                teprintln!(
                    "WGC could not open {device_name} ({e:#}); re-resolving the display \
                     (retry {stale_retries}/{WGC_STALE_MONITOR_RETRIES})"
                );
                std::thread::sleep(WGC_STALE_MONITOR_BACKOFF);
                // re-derive the name from the monitor too: a mode change is exactly when names
                // move, and the old name may now be somebody else's display
                match resolve_capture_name(target, 0).and_then(|name| {
                    select_monitor_by_device_name(&name).map(|found| (name, found))
                }) {
                    Ok((name, (m, i))) => {
                        if name != device_name {
                            tprintln!(
                                "capture: {} moved from {device_name} to {name}; following the \
                                 monitor",
                                target.device_name
                            );
                        }
                        device_name = name;
                        monitor = m;
                        info = i;
                    }
                    Err(e) => teprintln!("re-resolving {device_name} failed: {e:#}"),
                }
            }
            other => break other,
        }
    };

    match started {
        Ok((mut pipeline, control)) => {
            if weak_sender {
                pipeline.release_sender();
            }
            Ok(SessionCapture {
                pipeline,
                control: Some(SessionControl::Wgc(control)),
            })
        }
        // a just-created display can refuse a capture item for seconds: "not ready", which
        // waiting cures. Duplication opened on a display in that state delivers nothing
        Err(wgc_err)
            if switches.wait_not_ready && wgc_err.downcast_ref::<StaleMonitor>().is_some() =>
        {
            Err(wgc_err.context(format!(
                "{device_name} was still not ready for capture after the waits; not falling back \
                 to DXGI Desktop Duplication for a display that is only not ready yet"
            )))
        }
        Err(wgc_err) => {
            teprintln!(
                "WGC capture failed for {device_name}: {wgc_err:#}; \
                 falling back to DXGI Desktop Duplication. The display already exists, so where \
                 the driver composes it is left as it is; duplication delivers only if that is \
                 the adapter DXGI lists it under"
            );
            telemetry
                .degradation
                .capture_fallback
                .fetch_add(1, Ordering::Relaxed);
            // duplication opens the output by name, and names may have moved during WGC's retries
            let device_name = resolve_capture_name(target, 0)
                .map_err(|e| e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})")))?;
            let (pipeline, control) = start_dxgi_capture(
                cfg,
                &device_name,
                &info,
                tx,
                idr_request,
                repair,
                target_bitrate,
                telemetry,
                shared.wake.clone(),
            )
            .map_err(|e| fallback_error(&wgc_err, e))?;
            let mut pipeline = pipeline;
            if weak_sender {
                pipeline.release_sender();
            }
            Ok(SessionCapture {
                pipeline,
                control: Some(SessionControl::Dxgi(control)),
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_live_capture(
    cfg: &Config,
    monitor: Monitor,
    info: &MonitorInfo,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,
    target_bitrate: Arc<AtomicU32>,
    telemetry: SharedTelemetry,
    wake: Option<WakePair>,
) -> Result<(Pipeline, CaptureControl<LiveCapture, anyhow::Error>)> {
    let config = live_encoder_config(info.width, info.height, info.refresh_hz, cfg);
    let downscale = config.width != info.width || config.height != info.height;
    let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps as u64);

    let features = wgc_features();
    telemetry.capture_backend.set("wgc".into());
    telemetry.capture_features.set(features.summary());
    let (wake, wake_rx) = wake.unwrap_or_else(|| crossbeam_channel::bounded::<()>(1));
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        telemetry: Arc::clone(&telemetry),
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: config.max_bitrate_bps,
        h264_profile: cfg.h264_profile,
        repair: Arc::clone(&repair),
        frames: None,
    };

    // read before `monitor` is moved into the settings (`CursorProbe` needs it)
    let monitor_handle = monitor.as_raw_hmonitor() as isize;

    // each property is applied only where the OS exposes it. `ReportOnly` gives
    // `Frame::dirty_regions()` without asking the compositor to render the regions separately
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(WGC_MIN_UPDATE_INTERVAL),
        DirtyRegionSettings::ReportOnly,
        ColorFormat::Bgra8,
        CaptureFlags {
            config,
            vendor: effective_vendor(cfg),
            native_w: info.width,
            native_h: info.height,
            monitor_handle,
            auto_frame_rate: cfg.auto_frame_rate,
            tx,
            idr_request,
            repair,
            target_bitrate,
            wake_rx,
            telemetry,
        },
    )
    .with_gpu_preference(super::capture_gpu_preference(effective_vendor(cfg)));

    tprintln!(
        "pipeline: starting live monitor capture (WGC): \
         display={}, name={}, gpu={}, native={}x{}, encode={}x{}, downscale={}, fps={}, bitrate_bps={}",
        info.index,
        info.name,
        info.gpu,
        info.width,
        info.height,
        config.width,
        config.height,
        downscale,
        config.fps,
        config.bitrate_bps,
    );
    tprintln!(
        "wgc: feature set {} ({}); optional properties this build lacks are skipped, not fatal",
        features.summary(),
        if features.has_26100_properties() {
            "build 26100+"
        } else {
            "pre-26100"
        },
    );

    let control = LiveCapture::start_free_threaded(settings).map_err(wgc_start_error)?;

    Ok((pipeline, control))
}

/// A WGC start failure as an `anyhow` error, tagged [`StaleMonitor`] when the capture item could
/// not be created from the monitor handle: the one failure worth retrying before DXGI.
pub(crate) fn wgc_start_error(e: GraphicsCaptureApiError<anyhow::Error>) -> anyhow::Error {
    let stale = matches!(e, GraphicsCaptureApiError::ItemConvertFailed);
    let err = anyhow!("starting WGC capture: {e}");
    if stale {
        err.context(StaleMonitor)
    } else {
        err
    }
}

/// `CreateForMonitor` refused the `HMONITOR` this session resolved a moment earlier.
///
/// A join sets the virtual display's mode just before capture starts, and a handle resolved
/// before that can be dead by the time WGC converts it. The display is still there under a
/// fresh handle, and falling straight to DXGI is the worst answer: on a hybrid laptop,
/// duplicating an indirect display returns `ACCESS_LOST` on every acquire (first picture 12.9 s
/// after the submit, two offers).
#[derive(Debug)]
pub(crate) struct StaleMonitor;

impl std::fmt::Display for StaleMonitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the monitor handle went stale before WGC could open it")
    }
}

/// How long a capture start waits for a new monitor's capture item, and how often it asks.
/// A new monitor refuses one for 2.4 to 4.4 s after the attach on some joins.
const CAPTURE_ITEM_WAIT: Duration = Duration::from_secs(6);
const CAPTURE_ITEM_POLL: Duration = Duration::from_millis(50);

/// The attempt that asked for this capture has been overtaken by a newer offer for its join.
#[derive(Debug)]
pub(crate) struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a newer offer took the join over; standing down")
    }
}

impl std::error::Error for Superseded {}

/// Ask `attempt` every `poll` until it succeeds or `wait` has passed, and return the last error
/// then. Each attempt resolves the display again, so a name that moves meanwhile is followed.
/// `still_current` is asked before every try; an attempt that has been overtaken stops at once.
pub(crate) fn wait_for_capture_item<T>(
    target: &crate::streamer::session::DisplayTarget,
    wait: Duration,
    poll: Duration,
    still_current: &dyn Fn() -> bool,
    attempt: &mut dyn FnMut(&crate::streamer::session::DisplayTarget) -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    let mut tries = 0u32;
    loop {
        if !still_current() {
            return Err(anyhow::Error::new(Superseded));
        }
        tries += 1;
        match attempt(target) {
            Ok(found) => {
                if tries > 1 {
                    tprintln!(
                        "capture: {} handed out a capture item after {} ms ({tries} tries)",
                        target.device_name,
                        started.elapsed().as_millis()
                    );
                }
                return Ok(found);
            }
            Err(e) if started.elapsed() + poll <= wait => {
                if tries == 1 {
                    tprintln!(
                        "capture: {} is not capturable yet ({e:#}); waiting up to {} ms",
                        target.device_name,
                        wait.as_millis()
                    );
                }
                std::thread::sleep(poll);
            }
            Err(e) => {
                return Err(e.context(format!(
                    "{} gave no capture item within {} ms ({tries} tries)",
                    target.device_name,
                    wait.as_millis()
                )))
            }
        }
    }
}

/// Retries of a WGC start that failed on a stale monitor handle before DXGI is tried. Each one
/// costs the backoff plus a rebuilt encoder; five cover the second or so a mode change takes to
/// settle on a loaded machine.
const WGC_STALE_MONITOR_RETRIES: u32 = 5;
const WGC_STALE_MONITOR_BACKOFF: Duration = Duration::from_millis(200);

#[allow(clippy::too_many_arguments)]
fn start_dxgi_capture(
    cfg: &Config,
    device_name: &str,
    info: &MonitorInfo,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,
    target_bitrate: Arc<AtomicU32>,
    telemetry: SharedTelemetry,
    wake: Option<WakePair>,
) -> Result<(Pipeline, DxgiControl)> {
    let config = live_encoder_config(info.width, info.height, info.refresh_hz, cfg);
    let downscale = config.width != info.width || config.height != info.height;
    let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);

    telemetry.capture_backend.set("dxgi".into());
    let (wake, wake_rx) = wake.unwrap_or_else(|| crossbeam_channel::bounded::<()>(1));
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        telemetry: Arc::clone(&telemetry),
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: config.max_bitrate_bps,
        h264_profile: cfg.h264_profile,
        repair: Arc::clone(&repair),
        frames: None,
    };

    let composed =
        match crate::windows_utils::virtual_display::composing_adapter_of_name(device_name) {
            Some(Ok(adapter)) => format!(", composed={adapter}"),
            Some(Err(why)) => format!(", composed=unknown ({why})"),
            None => String::new(),
        };
    tprintln!(
        "pipeline: starting DXGI duplication capture: device={}, native={}x{}, encode={}x{}, \
         downscale={}, fps={}, bitrate_bps={}{composed}",
        device_name,
        info.width,
        info.height,
        config.width,
        config.height,
        downscale,
        config.fps,
        config.bitrate_bps,
    );

    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

    let thread_args = DxgiThreadArgs {
        device_name: device_name.to_string(),
        config,
        vendor: effective_vendor(cfg),
        native_w: info.width,
        native_h: info.height,
        frame_duration,
        tx,
        idr_request,
        repair,
        target_bitrate,
        wake_rx,
        stop: Arc::clone(&stop),
        telemetry,
    };
    let join = std::thread::Builder::new()
        .name("dxgi-capture".to_string())
        .spawn(move || dxgi_capture_thread(thread_args, ready_tx))
        .context("spawning dxgi capture thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((pipeline, DxgiControl::new(stop, join))),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(anyhow!("dxgi capture thread exited during setup"))
        }
    }
}

/// Staging slots between the DXGI capture thread and its encode thread.
///
/// Two is enough to overlap one encode with one capture; three buys slack for a jittery encoder
/// without adding a frame of latency, because the consumer always takes the oldest queued slot
/// and the producer drops rather than queueing when none is free.
const DXGI_STAGE_SLOTS: usize = 3;

/// The ring plus the three handles its encode thread needs: the ready queue, the slot-return
/// channel, and its own view of the slot textures.
type DxgiRingParts = (
    DxgiRing,
    crossbeam_channel::Receiver<DxgiStaged>,
    crossbeam_channel::Sender<usize>,
    Vec<ID3D11Texture2D>,
);

struct DxgiStaged {
    slot: usize,
    capture: Instant,
    timing: FrameTiming,
}

/// Decouples DXGI capture from encode, so the achievable rate is `1 / capture` and not
/// `1 / (capture + composite + encode)`: the capture thread only copies the composed frame
/// into a free slot (a GPU-to-GPU copy on the duplication device, which is
/// multithread-protected) and hands the index over.
struct DxgiRing {
    slots: Vec<ID3D11Texture2D>,
    context: ID3D11DeviceContext,
    free_rx: crossbeam_channel::Receiver<usize>,
    ready_tx: crossbeam_channel::Sender<DxgiStaged>,
    dropped: u64,
    dropped_reported: u64,
    telemetry: SharedTelemetry,
}

impl DxgiRing {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        telemetry: SharedTelemetry,
    ) -> Result<DxgiRingParts> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            // the encode side may hand this to a VideoProcessor input view (the scaler) or read
            // it back, so it needs both bind flags a duplication target has
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut slots = Vec::with_capacity(DXGI_STAGE_SLOTS);
        for i in 0..DXGI_STAGE_SLOTS {
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .with_context(|| format!("CreateTexture2D (dxgi stage slot {i})"))?;
            slots.push(tex.context("dxgi stage slot texture was null")?);
        }

        let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(DXGI_STAGE_SLOTS);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<DxgiStaged>(DXGI_STAGE_SLOTS);
        for i in 0..DXGI_STAGE_SLOTS {
            free_tx.send(i).expect("prefill dxgi free slots");
        }
        let encode_slots = slots.clone();
        Ok((
            Self {
                slots,
                context: context.clone(),
                free_rx,
                ready_tx,
                dropped: 0,
                dropped_reported: 0,
                telemetry,
            },
            ready_rx,
            free_tx,
            encode_slots,
        ))
    }

    fn stage(
        &mut self,
        src: &ID3D11Texture2D,
        capture: Instant,
        timing: FrameTiming,
    ) -> Result<()> {
        let Some(slot) = take_free_slot(&self.free_rx)? else {
            // every slot is with the encoder: drop, a fresher frame comes before one is free
            self.dropped += 1;
            self.telemetry
                .degradation
                .frames_dropped_ring
                .fetch_add(1, Ordering::Relaxed);
            if self.dropped - self.dropped_reported >= 120 {
                self.dropped_reported = self.dropped;
                teprintln!(
                    "dxgi ring: {} frames dropped for want of a free slot; the encoder is not \
                     keeping up",
                    self.dropped
                );
            }
            return Ok(());
        };
        // SAFETY: both textures are live, on the same multithread-protected device, and have the
        // same descriptor.
        unsafe {
            self.context.CopyResource(&self.slots[slot], src);
            self.context.Flush();
        }
        if self
            .ready_tx
            .send(DxgiStaged {
                slot,
                capture,
                timing,
            })
            .is_err()
        {
            bail!("dxgi encode thread disconnected");
        }
        Ok(())
    }
}

/// A free stage slot, `None` when every slot is with the encoder, or an error when the encoder
/// thread is gone. The two empty cases must stay apart: an encode thread that died holding every
/// slot would otherwise read as "the encoder is not keeping up" for the rest of the session.
pub(crate) fn take_free_slot(
    free_rx: &crossbeam_channel::Receiver<usize>,
) -> Result<Option<usize>> {
    match free_rx.try_recv() {
        Ok(slot) => Ok(Some(slot)),
        Err(crossbeam_channel::TryRecvError::Empty) => Ok(None),
        Err(crossbeam_channel::TryRecvError::Disconnected) => {
            bail!("dxgi encode thread is gone (its slot channel disconnected)")
        }
    }
}

/// Sets a stop flag when dropped, so a thread that panics still tells its siblings to stop.
struct StopOnExit(Arc<AtomicBool>);

impl Drop for StopOnExit {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_dxgi_encoder(
    core: Arc<Mutex<EncodeCore>>,
    slots: Vec<ID3D11Texture2D>,
    ready_rx: crossbeam_channel::Receiver<DxgiStaged>,
    free_tx: crossbeam_channel::Sender<usize>,
    tx: broadcast::Sender<EncodedFrame>,
    last_frame_at: Arc<AtomicU64>,
    epoch: Instant,
    frame_duration: Duration,
    stop: Arc<AtomicBool>,
    path_name: &'static str,
    telemetry: SharedTelemetry,
) -> std::thread::JoinHandle<()> {
    // `ID3D11Texture2D` is not `Send`, but the device is multithread-protected and the slots are
    // only ever touched by the capture thread's `CopyResource` and this thread's encode, which the
    // free/ready channels already order
    struct SendSlots(Vec<ID3D11Texture2D>);
    unsafe impl Send for SendSlots {}
    let slots = SendSlots(slots);

    let governed = core.lock_recover().governed();
    std::thread::Builder::new()
        .name("dxgi-encode".to_string())
        .spawn(move || {
            let slots = slots;
            // a panic in the encoder skips the `stop.store` at the bottom; this does not
            let _stop_on_exit = StopOnExit(Arc::clone(&stop));
            let _thread_tuning = tuning::tune_current_thread();
            let mut busy_streak: u32 = 0;
            let mut frames_sent: u64 = 0;
            let mut timing_sum_ns: u128 = 0;
            let mut timing_count: u64 = 0;
            let mut timing_max_ns: u128 = 0;

            while !stop.load(Ordering::Relaxed) {
                let staged = match ready_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(s) => s,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                let DxgiStaged {
                    slot,
                    capture,
                    mut timing,
                } = staged;
                let Some(tex) = slots.0.get(slot) else {
                    break;
                };

                let encode_res = {
                    // the repeater shares this lock: a panic on its side must not poison it
                    // and end this session's capture with nothing in the log
                    let mut core = core.lock_recover();
                    timing.mark_encode_submit();
                    core.encode_texture(tex)
                };
                timing.mark_encode_done();
                let returned = free_tx.send(slot).is_ok();

                let au = match encode_res {
                    Ok(au) => au,
                    Err(e)
                        if is_transient_encode_error(&e)
                            && busy_streak < MAX_TRANSIENT_ENCODE_DROPS =>
                    {
                        busy_streak += 1;
                        telemetry
                            .degradation
                            .encoder_transient_errors
                            .fetch_add(1, Ordering::Relaxed);
                        teprintln!(
                            "dxgi encode transiently overloaded ({e:#}); dropping frame \
                             ({busy_streak} in a row)"
                        );
                        continue;
                    }
                    Err(e) => {
                        teprintln!("dxgi encode failed: {e:?}; stopping capture");
                        break;
                    }
                };
                busy_streak = 0;
                if !returned {
                    break;
                }
                // no bytes means the encoder dropped the frame (x264 and Quick Sync filling their
                // pipeline, AMF on a zero-length output). Broadcasting it makes the packetizer
                // ask for a keyframe and refreshes `last_frame_at`, holding the keepalive off
                if au.is_empty() {
                    continue;
                }

                last_frame_at.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let data = Bytes::from(au);
                let record = governed.then(|| data.clone());
                let _ = tx.send(EncodedFrame {
                    data,
                    capture,
                    timing,
                });
                record_sent(&core, record, false);

                frames_sent += 1;
                let dt = capture.elapsed().as_nanos();
                timing_sum_ns += dt;
                timing_count += 1;
                timing_max_ns = timing_max_ns.max(dt);
                if frames_sent.is_multiple_of(60) {
                    let avg_ms = (timing_sum_ns / timing_count.max(1) as u128) as f64 / 1.0e6;
                    let max_ms = timing_max_ns as f64 / 1.0e6;
                    tprintln!(
                        "encode-path latency: path=dxgi+{}, avg_ms={:.2}, max_ms={:.2}, \
                         frames={}, budget_ms={:.2}",
                        path_name,
                        avg_ms,
                        max_ms,
                        frames_sent,
                        frame_duration.as_secs_f64() * 1000.0,
                    );
                    timing_sum_ns = 0;
                    timing_count = 0;
                    timing_max_ns = 0;
                }
            }
            stop.store(true, Ordering::Relaxed);
            tprintln!("dxgi encode thread stopped");
        })
        .expect("spawn dxgi encode thread")
}

struct DxgiThreadArgs {
    device_name: String,
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    frame_duration: Duration,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,
    target_bitrate: Arc<AtomicU32>,
    wake_rx: crossbeam_channel::Receiver<()>,
    stop: Arc<AtomicBool>,
    telemetry: SharedTelemetry,
}

/// Whether the DXGI path builds a `Scaler`.
///
/// `needs_scaler` also asks for one at identity size for every CPU-bridge backend that wants
/// NV12, and `Scaler::with_format` cannot be built without a video device. On an adapter with
/// no video processor (the duplication device is retried without `VIDEO_SUPPORT` for that
/// case) the answer is no scaler: the `TextureReader` reads BGRA back and every CPU-bridge
/// encoder takes BGRA.
pub(crate) fn dxgi_needs_scaler(
    backend: &Backend,
    video_processor: bool,
    want_w: u32,
    want_h: u32,
    native_w: u32,
    native_h: u32,
) -> bool {
    video_processor && needs_scaler(backend, want_w, want_h, native_w, native_h)
}

/// When the DXGI capture loop stages a frame. Two behaviours, chosen once by the
/// `dxgi_cadence_gate` switch:
///
/// * **Gated** (ships on). A fresh frame is staged once three quarters of a period has passed
///   since the last one (see [`CADENCE_NUM`]); an early one stays pending until the full period
///   is up unless a fresher one arrives, so the last change before the screen goes still waits
///   at most a period. "Up" means within a millisecond: the poll timeout is whole milliseconds,
///   and without the tolerance the loop calls `AcquireNextFrame(0)` back to back on a
///   time-critical thread. A fresh poll that carries a pointer move is staged at once
///   (`dxgi_cadence_pointer_exempt`): gating those cost a pointer sweep 113 -> 70-74 painted
///   fps on the WGC path.
/// * **Legacy** (`SE_TUNE_OFF=dxgi_cadence_gate`). The gate never holds anything and the path
///   encodes at the duplication rate, whatever `max_fps` says.
#[derive(Debug, Clone)]
pub(crate) struct DxgiCadence {
    gate: bool,
    period: Duration,
    dirty: bool,
    // gated state
    last_staged: Option<Instant>,
    // legacy state
    next_due: Instant,
    was_idle: bool,
}

/// Idle poll timeout: how long `AcquireNextFrame` waits when nothing is pending.
const DXGI_IDLE_POLL_MS: u32 = 8;
/// A deadline this close counts as reached; see [`DxgiCadence`].
const DXGI_DUE_TOLERANCE: Duration = Duration::from_millis(1);

impl DxgiCadence {
    pub(crate) fn new(gate: bool, period: Duration, now: Instant) -> Self {
        Self {
            gate,
            period,
            dirty: false,
            last_staged: None,
            next_due: now,
            was_idle: true,
        }
    }

    /// The earliest a fresh frame may be staged.
    fn accept_at(&self) -> Option<Instant> {
        self.last_staged
            .map(|t| t + self.period * CADENCE_NUM / CADENCE_DEN)
    }

    /// The latest a pending frame waits.
    fn deadline(&self) -> Option<Instant> {
        self.last_staged.map(|t| t + self.period)
    }

    /// How long the next `AcquireNextFrame` may wait.
    pub(crate) fn poll_timeout_ms(&self, now: Instant) -> u32 {
        if !self.dirty {
            return DXGI_IDLE_POLL_MS;
        }
        let due = if self.gate {
            self.deadline().unwrap_or(now)
        } else {
            self.next_due
        };
        let rem = due.saturating_duration_since(now);
        (rem.as_micros() / 1000) as u32
    }

    /// The duplication reported a new frame.
    pub(crate) fn on_dirty(&mut self, now: Instant) {
        if !self.gate && !self.dirty {
            if self.was_idle {
                self.next_due = now;
            }
            self.was_idle = false;
        }
        self.dirty = true;
    }

    /// Whether to stage now. `fresh` is whether this iteration's poll delivered a new frame, and
    /// `pointer_moved` whether the pointer moved on this display since the previous fresh poll
    /// (always false when the exemption is off). The legacy mode ignores it: it never gates.
    pub(crate) fn due(&self, now: Instant, fresh: bool, pointer_moved: bool) -> bool {
        if !self.dirty {
            return false;
        }
        if !self.gate {
            return now >= self.next_due;
        }
        let (Some(accept_at), Some(deadline)) = (self.accept_at(), self.deadline()) else {
            // nothing staged yet: the first change goes out at once
            return true;
        };
        (fresh && (pointer_moved || now >= accept_at)) || now + DXGI_DUE_TOLERANCE > deadline
    }

    pub(crate) fn staged(&mut self, capture: Instant) {
        self.dirty = false;
        if self.gate {
            self.last_staged = Some(capture);
        } else {
            self.was_idle = true;
            self.next_due = capture + self.period;
        }
    }
}

fn dxgi_capture_thread(args: DxgiThreadArgs, ready_tx: std::sync::mpsc::Sender<Result<()>>) {
    let DxgiThreadArgs {
        device_name,
        config,
        vendor,
        native_w,
        native_h,
        frame_duration,
        tx,
        idr_request,
        // the repair tier is wired into the NVENC zero-copy ring only. DXGI duplication keeps
        // keyframe-only recovery: the slot count stays zero and the transport reads that
        repair: _,
        target_bitrate,
        wake_rx,
        stop,
        telemetry,
    } = args;

    let _stop_on_exit = StopOnExit(Arc::clone(&stop));
    let _thread_tuning = tuning::tune_current_thread();
    let _keep_awake = tuning::KeepAwake::begin();
    let cadence_gate = tune::on("dxgi_cadence_gate");
    let epoch = Instant::now();
    let last_frame_at = Arc::new(AtomicU64::new(0));
    // a removed or reset device takes the duplication, the encoder and the ring with it; each
    // turn of this loop builds all of them again, a few times a minute at most
    let mut rebuilds = RebuildBudget::default();
    let mut first = true;

    'session: loop {
        // this generation's encode thread and repeater; the session's `stop` ends the whole thread
        let gen_stop = Arc::new(AtomicBool::new(false));
        let _gen_stop_on_exit = StopOnExit(Arc::clone(&gen_stop));

        let setup = (|| -> Result<(Duplicator, Arc<Mutex<EncodeCore>>, &'static str)> {
            let dup = Duplicator::new(&device_name, native_w, native_h)?;

            // duplication is adapter-local, so the scaler lives on the adapter composing this
            // display, which may expose no video processor (an indirect-display adapter). Then
            // encode at native and let the client scale rather than fail the session
            let mut config = config;
            let wants_downscale = config.width != native_w || config.height != native_h;
            let video_processor = video_processor_available(dup.device(), dup.context());
            if wants_downscale && !video_processor {
                teprintln!(
                    "dxgi: the adapter composing {device_name} exposes no D3D11 video processor, \
                 so the requested {}x{} downscale cannot be done on the GPU; encoding at the \
                 native {native_w}x{native_h} and leaving the scale to the client",
                    config.width,
                    config.height
                );
                config.width = native_w & !1;
                config.height = native_h & !1;
            }

            let backend = build_backend(
                config,
                vendor,
                native_w,
                native_h,
                dup.device(),
                dup.context(),
            )?;

            let needs_scaler = dxgi_needs_scaler(
                &backend,
                video_processor,
                config.width,
                config.height,
                native_w,
                native_h,
            );
            if !video_processor && backend.is_cpu_bridge() {
                teprintln!(
                    "dxgi: no D3D11 video processor on the adapter composing {device_name}, so {} \
                 reads back BGRA and converts on the CPU instead of taking NV12 from the GPU",
                    backend.name()
                );
            }
            // a video processor that cannot build the pass degrades as on the WGC classic path:
            // a conversion-only pass becomes a BGRA readback, a real downscale fails the session
            let scaler = if needs_scaler {
                let (scale, convert) =
                    scaler_reasons(&backend, config.width, config.height, native_w, native_h);
                classic_scaler(
                    dup.device(),
                    dup.context(),
                    scale,
                    convert,
                    (native_w, native_h),
                    (config.width, config.height),
                    scaler_output_format(&backend),
                )
                .context("building GPU downscaler for --scale")?
            } else {
                None
            };
            let reader = if backend.is_cpu_bridge() && scaler.is_none() {
                Some(TextureReader::new(
                    dup.device(),
                    dup.context(),
                    native_w,
                    native_h,
                )?)
            } else {
                None
            };

            if let Some(dev) = backend.device() {
                tuning::raise_d3d11_gpu_priority(dev);
            }
            tuning::raise_d3d11_gpu_priority(dup.device());
            let path_name = backend.name();
            super::governor::arm(&telemetry, &config);

            let core = Arc::new(Mutex::new(EncodeCore {
                backend,
                scaler,
                reader,
                staging: None,
                held: None,
                target_bitrate: Arc::clone(&target_bitrate),
                idr_request: Arc::clone(&idr_request),
                current_bitrate: config.bitrate_bps,
                bitrate_warned: false,
                have_frame: false,
                frame_index: 0,
                health: CoreHealth::new(Arc::clone(&telemetry)),
            }));
            Ok((dup, core, path_name))
        })();

        let (mut dup, core, path_name) = match setup {
            Ok(state) => state,
            Err(e) if first => {
                let _ = ready_tx.send(Err(e));
                return;
            }
            Err(e) => {
                if retry_dxgi_rebuild(&mut rebuilds, &stop, &e) {
                    continue 'session;
                }
                break 'session;
            }
        };

        let ring = DxgiRing::new(
            dup.device(),
            dup.context(),
            native_w,
            native_h,
            Arc::clone(&telemetry),
        );
        let (mut ring, ready_rx, free_tx, encode_slots) = match ring {
            Ok(parts) => parts,
            Err(e) if first => {
                let _ = ready_tx.send(Err(
                    e.context("building the DXGI capture/encode staging ring")
                ));
                return;
            }
            Err(e) => {
                if retry_dxgi_rebuild(&mut rebuilds, &stop, &e) {
                    continue 'session;
                }
                break 'session;
            }
        };
        let encode_join = spawn_dxgi_encoder(
            Arc::clone(&core),
            encode_slots,
            ready_rx,
            free_tx,
            tx.clone(),
            Arc::clone(&last_frame_at),
            epoch,
            frame_duration,
            Arc::clone(&gen_stop),
            path_name,
            Arc::clone(&telemetry),
        );

        spawn_repeater(
            Arc::clone(&core),
            tx.clone(),
            Arc::clone(&idr_request),
            frame_duration,
            Arc::clone(&last_frame_at),
            epoch,
            Arc::clone(&gen_stop),
            wake_rx.clone(),
            Arc::clone(&telemetry),
        );

        telemetry.encoder.set(path_name.into());
        note_started_backend(path_name);
        telemetry.capture_features.set(dup.api().name().into());
        tprintln!(
            "pipeline: live capture ready -- DXGI duplication ({}) -> {path_name}",
            dup.api().name()
        );
        if first {
            let _ = ready_tx.send(Ok(()));
            first = false;
        }
        let mut lost: Option<anyhow::Error> = None;

        let mut cadence = DxgiCadence::new(cadence_gate, frame_duration, Instant::now());
        // only the gate needs to know about the pointer; the legacy loop stages every dirty poll
        let mut pointer = if cadence_gate && tune::on("dxgi_cadence_pointer_exempt") {
            match select_monitor_by_device_name(&device_name) {
                Ok((monitor, _)) => Some(CursorProbe::new(monitor.as_raw_hmonitor() as isize)),
                Err(e) => {
                    teprintln!(
                    "dxgi: no monitor handle for {device_name} ({e:#}); pointer moves are gated \
                     like any other frame"
                );
                    None
                }
            }
        } else {
            None
        };

        // per duplicator, which a rebuild replaces, so only what moved is added
        let (mut skips_seen, mut waits_seen) = (0u64, 0u64);
        let mut acquired_seen = 0u64;
        loop {
            if stop.load(Ordering::Relaxed) || gen_stop.load(Ordering::Relaxed) {
                break;
            }

            let timeout_ms = cadence.poll_timeout_ms(Instant::now());
            let fresh = match dup.poll(timeout_ms) {
                Ok(PollStatus::Dirty) => {
                    cadence.on_dirty(Instant::now());
                    true
                }
                Ok(PollStatus::Timeout) => false,
                Err(e) => {
                    if is_device_lost(&e) {
                        lost = Some(e);
                    } else {
                        teprintln!("dxgi capture: {e:?}; stopping capture");
                    }
                    break;
                }
            };

            // counted at the acquire, not at `Dirty`: the first surface of a fresh duplication
            // over a still desktop can be classified as nothing new, and it still proves delivery
            let acquired = dup.images_acquired();
            telemetry
                .capture_frames_arrived
                .fetch_add(acquired.saturating_sub(acquired_seen), Ordering::Relaxed);
            acquired_seen = acquired;

            // frames the OS coalesced away while the previous one was held; drained every poll so
            // the counter tracks the capture, not the report cadence
            let coalesced = dup.take_accumulated_drops();
            if coalesced != 0 {
                telemetry
                    .degradation
                    .frames_dropped_capture
                    .fetch_add(coalesced, Ordering::Relaxed);
            }
            let (skips, waits) = (dup.format_skips(), dup.denied_waits());
            telemetry
                .degradation
                .capture_format_skips
                .fetch_add(skips.saturating_sub(skips_seen), Ordering::Relaxed);
            telemetry
                .degradation
                .capture_access_denied_waits
                .fetch_add(waits.saturating_sub(waits_seen), Ordering::Relaxed);
            (skips_seen, waits_seen) = (skips, waits);

            // asked on fresh polls only, so "moved" means since the previous delivered frame
            let pointer_moved = fresh && pointer.as_mut().is_some_and(CursorProbe::changed);

            let now = Instant::now();
            if !cadence.due(now, fresh, pointer_moved) {
                // a held-back frame is still composited now: the duplicator keeps only the latest
                // poll's changed region and its cursor composite redraws only that region, so an
                // uncomposited poll would leave stale pixels on the client
                if fresh && cadence_gate {
                    if let Err(e) = dup.frame() {
                        if is_device_lost(&e) {
                            lost = Some(e);
                        } else {
                            teprintln!("dxgi compose failed: {e:?}; stopping capture");
                        }
                        break;
                    }
                }
                continue;
            }

            let capture = now;
            let mut timing = FrameTiming::entering_capture();
            timing.capture_available_ns = dup.last_present_host_ns();
            let tex = match dup.frame() {
                Ok(t) => t,
                Err(e) => {
                    if is_device_lost(&e) {
                        lost = Some(e);
                    } else {
                        teprintln!("dxgi compose failed: {e:?}; stopping capture");
                    }
                    break;
                }
            };
            match ring.stage(tex, capture, timing) {
                Ok(()) => {}
                Err(e) => {
                    if is_device_lost(&e) {
                        lost = Some(e);
                    } else {
                        teprintln!("dxgi stage failed: {e:?}; stopping capture");
                    }
                    break;
                }
            }
            cadence.staged(capture);
        }

        gen_stop.store(true, Ordering::Relaxed);
        drop(ring);
        let _ = encode_join.join();
        drop(core);
        drop(dup);
        match lost {
            Some(e) if retry_dxgi_rebuild(&mut rebuilds, &stop, &e) => {
                // the new encoder opens with an IDR anyway; this also covers a repeat racing it
                idr_request.store(true, Ordering::Relaxed);
                continue 'session;
            }
            _ => break 'session,
        }
    }

    stop.store(true, Ordering::Relaxed);
    tprintln!("dxgi capture stopped");
}

/// A duplication error that means the device under it is gone, not that capture cannot work.
fn is_device_lost(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<super::dxgi::DeviceLost>())
}

/// After a lost device, or a rebuild that failed: wait a moment for the driver to come back and
/// say whether to build the DXGI session again. False once the session is stopping or the
/// rebuild budget is spent.
fn retry_dxgi_rebuild(
    budget: &mut RebuildBudget,
    stop: &AtomicBool,
    cause: &anyhow::Error,
) -> bool {
    if stop.load(Ordering::Relaxed) {
        return false;
    }
    if !budget.allow(Instant::now()) {
        teprintln!(
            "dxgi capture: {cause:#}; {REBUILD_LIMIT} rebuilds in {} s is the limit, stopping \
             capture",
            REBUILD_WINDOW.as_secs()
        );
        return false;
    }
    teprintln!("dxgi capture: {cause:#}; building the duplication and the encoder again");
    // a device reset takes a moment to settle; a rebuild straight into it fails again
    std::thread::sleep(DXGI_REBUILD_SETTLE);
    !stop.load(Ordering::Relaxed)
}

const DXGI_REBUILD_SETTLE: Duration = Duration::from_millis(500);

/// What [`recv_frame_within`] got.
pub(crate) enum ProbeRecv {
    Frame(Box<EncodedFrame>),
    /// The receiver fell behind and skipped this many frames.
    Lagged(u64),
    TimedOut,
}

/// How often [`recv_frame_within`] looks at the channel while it waits.
const PROBE_RECV_POLL: Duration = Duration::from_millis(2);

/// Wait at most `timeout` for the next frame.
///
/// `blocking_recv` can only return on a frame or a closed channel, and this channel cannot close
/// while the probe holds the `Pipeline` (it holds a sender), so an encode loop that never
/// started (`--encoder amd` with no AMD GPU and no fallback) would hang the probe forever. A
/// probe is not a real-time path, so a short poll is fine here.
pub(crate) fn recv_frame_within(
    rx: &mut broadcast::Receiver<EncodedFrame>,
    timeout: Duration,
) -> Result<ProbeRecv> {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(frame) => return Ok(ProbeRecv::Frame(Box::new(frame))),
            Err(broadcast::error::TryRecvError::Lagged(n)) => return Ok(ProbeRecv::Lagged(n)),
            Err(broadcast::error::TryRecvError::Closed) => {
                bail!("the encoder stopped (its frame channel closed)")
            }
            Err(broadcast::error::TryRecvError::Empty) => {}
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(ProbeRecv::TimedOut);
        }
        std::thread::sleep(PROBE_RECV_POLL.min(deadline - now));
    }
}

/// How long a probe waits for any one frame before deciding the encoder is not producing.
const PROBE_FRAME_TIMEOUT: Duration = Duration::from_secs(5);

pub fn probe_bitrate(cfg: &Config) -> Result<()> {
    let mut cfg = cfg.clone();
    cfg.synthetic_pattern = true;
    let pipeline = start(&cfg)?;
    let mut rx = pipeline.tx.subscribe();

    let next_frame = |rx: &mut broadcast::Receiver<EncodedFrame>| -> Result<()> {
        match recv_frame_within(rx, PROBE_FRAME_TIMEOUT)? {
            ProbeRecv::Frame(_) | ProbeRecv::Lagged(_) => Ok(()),
            ProbeRecv::TimedOut => bail!(
                "probe-bitrate: no encoded frame within {} s; the synthetic encoder did not \
                 start or has stopped (see the log above)",
                PROBE_FRAME_TIMEOUT.as_secs()
            ),
        }
    };
    next_frame(&mut rx)?;

    let targets = [4_000_000u32, 2_000_000, 1_000_000, 3_000_000, 6_000_000];
    for (i, &t) in targets.iter().enumerate() {
        tprintln!("probe-bitrate: injecting synthetic target (step={i}, target_bps={t})");
        pipeline.set_target_bitrate(t);
        for _ in 0..10 {
            next_frame(&mut rx)?;
        }
    }

    tprintln!("probe-bitrate complete: cross-thread bitrate update exercised");
    Ok(())
}

pub fn probe_live(cfg: &Config, path: &str) -> Result<()> {
    use std::io::Write;

    const FRAMES: u64 = 150;

    let mut cfg = cfg.clone();
    cfg.synthetic_pattern = false;

    // `--workload` normally implies `--synthetic-pattern`, which bypasses capture. Here the same
    // frames go onto the real display, so capture, scaler and encoder see repeatable content
    let _workload_window = match select_monitor(cfg.monitor) {
        Ok((monitor, _)) => monitor
            .device_name()
            .ok()
            .and_then(|name| super::capture::monitor_rect(&name))
            .and_then(|(left, top, width, height)| {
                match super::workload_window::WorkloadWindow::spawn(
                    left,
                    top,
                    width,
                    height,
                    cfg.max_fps.clamp(1, 240),
                    cfg.workload,
                    cfg.counter,
                ) {
                    Ok(window) => {
                        tprintln!(
                            "probe: painting the '{}' workload onto {width}x{height} at ({left},{top})",
                            cfg.workload.name()
                        );
                        Some(window)
                    }
                    Err(e) => {
                        teprintln!(
                            "probe: could not put the workload on the display ({e:#}); \
                             capturing whatever is on it instead"
                        );
                        None
                    }
                }
            }),
        Err(e) => {
            teprintln!("probe: no monitor to paint the workload onto ({e:#})");
            None
        }
    };

    let pipeline = start(&cfg)?;
    // the telemetry registry is fed only by the HTTP server; publish the probe's own here
    crate::streamer::telemetry::publish("probe-live", Arc::clone(&pipeline.telemetry));
    let mut rx = pipeline.tx.subscribe();
    pipeline.request_idr();

    let mut file = std::fs::File::create(path)?;
    let mut written = 0u64;
    let mut total = 0usize;
    let mut started = false;
    // a fixed 150 frames is 2.5 s at 60 fps, which is startup transients and little else
    let deadline =
        (cfg.probe_secs > 0).then(|| Instant::now() + Duration::from_secs(cfg.probe_secs));

    while deadline.map_or(written < FRAMES, |d| Instant::now() < d) {
        // never wait past the run's own deadline, and never forever: a capture that stops
        // producing cannot close this channel while the probe holds the pipeline
        let wait = deadline.map_or(PROBE_FRAME_TIMEOUT, |d| {
            d.saturating_duration_since(Instant::now())
                .min(PROBE_FRAME_TIMEOUT)
        });
        match recv_frame_within(&mut rx, wait) {
            Ok(ProbeRecv::Frame(frame)) => {
                let timing = frame.timing;
                let au = frame.data;
                if !started {
                    if is_keyframe(&au) {
                        started = true;
                    } else {
                        continue;
                    }
                }
                // no transport here to fold the frame's stamps into the histograms
                pipeline.telemetry.record_frame(&timing);
                file.write_all(&au)?;
                total += au.len();
                written += 1;
            }
            Ok(ProbeRecv::Lagged(_)) => continue,
            Ok(ProbeRecv::TimedOut) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break;
                }
                bail!(
                    "capture produced nothing for {} s after {written} frames (capture or \
                     encoder stopped?)",
                    PROBE_FRAME_TIMEOUT.as_secs()
                )
            }
            Err(e) => {
                return Err(e.context(format!("capture stopped after {written} frames")));
            }
        }
    }

    tprintln!("probe-live complete: path={path}, frames={written}, total_bytes={total}");
    Ok(())
}

fn is_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        let nal_type = if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            i += 3;
            au[i] & 0x1f
        } else if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
            i += 4;
            au[i] & 0x1f
        } else {
            i += 1;
            continue;
        };
        if nal_type == 5 || nal_type == 7 {
            return true;
        }
        i += 1;
    }
    false
}

struct CaptureFlags {
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    /// `HMONITOR` of the captured display, for [`CursorProbe`].
    monitor_handle: isize,
    /// The device's `autoFrameRate`. Off (the default) makes `config.fps` a promise rather than
    /// a ceiling: nothing skips a frame for being unchanged and the keepalive holds the cadence.
    auto_frame_rate: bool,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,

    target_bitrate: Arc<AtomicU32>,
    wake_rx: crossbeam_channel::Receiver<()>,
    telemetry: SharedTelemetry,
}

pub(crate) enum EncodePath {
    ZeroCopy {
        igpu_context: ID3D11DeviceContext,
        shared_igpu: ID3D11Texture2D,
        igpu_mutex: IDXGIKeyedMutex,
    },
    CpuBridge,
}

impl EncodePath {
    fn name(&self) -> &'static str {
        match self {
            EncodePath::ZeroCopy { .. } => "zero-copy",
            EncodePath::CpuBridge => "cpu-bridge",
        }
    }
}

impl super::governor::QpEncoder for Backend {
    fn set_qp(&mut self, qp: u8) -> Result<bool> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.set_qp(qp),
            Backend::Intel { encoder } | Backend::IntelCpu { encoder } => encoder.set_qp(qp),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => encoder.set_qp(qp),
            Backend::X264 { encoder } => encoder.set_qp(qp),
        }
    }

    fn set_qp_costly(&mut self, qp: u8) -> Result<bool> {
        match self {
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => encoder.set_qp_reinit(qp),
            _ => Ok(false),
        }
    }

    fn qp_top(&self) -> u8 {
        match self {
            Backend::X264 { encoder } => encoder.qp_range().map_or(51, |(_, top)| top),
            _ => 51,
        }
    }
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum Backend {
    Nvenc { encoder: Encoder, path: EncodePath },
    Intel { encoder: IntelEncoder },
    IntelCpu { encoder: IntelEncoder },
    Amf { encoder: AmfEncoder },
    AmfCpu { encoder: AmfEncoder },
    X264 { encoder: X264Encoder },
}

impl Backend {
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Backend::Nvenc { path, .. } => path.name(),
            Backend::Intel { .. } => "intel-same-adapter",
            Backend::IntelCpu { .. } => "intel-cpu-bridge",
            Backend::Amf { .. } => "amd-amf-same-adapter",
            Backend::AmfCpu { .. } => "amd-amf-cpu-bridge",
            Backend::X264 { .. } => "software-x264",
        }
    }

    fn device(&self) -> Option<&ID3D11Device> {
        match self {
            Backend::Nvenc { encoder, .. } => Some(encoder.device()),
            Backend::Intel { encoder } => Some(encoder.device()),
            Backend::IntelCpu { encoder } => Some(encoder.device()),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => Some(encoder.device()),
            Backend::X264 { .. } => None,
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.set_bitrate(bps),
            Backend::Intel { encoder } => encoder.set_bitrate(bps),
            Backend::IntelCpu { encoder } => encoder.set_bitrate(bps),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => encoder.set_bitrate(bps),
            Backend::X264 { encoder } => encoder.set_bitrate(bps),
        }
    }

    pub(crate) fn wants_prescale(&self) -> bool {
        !matches!(self, Backend::Intel { .. })
    }

    /// What this backend wants a CPU-bridge readback to be in.
    ///
    /// The scale and the colour conversion are one `VideoProcessorBlt`, so NV12 costs the video
    /// engine nothing extra and takes 62% off the readback, the memcpy and the upload. x264
    /// deinterleaves it in SIMD (450 us median per frame against 1655 us from BGRA at 640x360,
    /// `x264_nv12_input_matches_the_bgra_path`), the Intel bridge skips its VPP pass, and AMF
    /// avoids its internal GPU colour converter, which competes with the foreground application
    /// (untested on real AMD hardware).
    ///
    /// NVENC is deliberately not a candidate: `NV_ENC_BUFFER_FORMAT_ARGB` is byte-identical to
    /// the captured texture and its internal conversion beats a pre-pass. `Backend::Amf` (the
    /// GPU path) is not a bridge and takes the blt straight into its own ring texture.
    pub(crate) fn cpu_bridge_format(&self) -> ReadbackFormat {
        match self {
            Backend::X264 { .. } | Backend::IntelCpu { .. } | Backend::AmfCpu { .. } => {
                ReadbackFormat::Nv12
            }
            Backend::Nvenc { .. } | Backend::Intel { .. } | Backend::Amf { .. } => {
                ReadbackFormat::Bgra8
            }
        }
    }

    pub(crate) fn is_cpu_bridge(&self) -> bool {
        matches!(
            self,
            Backend::Nvenc {
                path: EncodePath::CpuBridge,
                ..
            } | Backend::IntelCpu { .. }
                | Backend::AmfCpu { .. }
                | Backend::X264 { .. }
        )
    }

    pub(crate) fn encode_gpu(
        &mut self,
        raw: &ID3D11Texture2D,
        scaler: Option<&mut Scaler>,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        match self {
            Backend::Intel { encoder } => {
                debug_assert!(
                    scaler.is_none(),
                    "Intel fuses scaling in VPP; no prescaler expected"
                );
                encoder.encode_texture(raw, force_idr)
            }
            Backend::Nvenc {
                encoder,
                path:
                    EncodePath::ZeroCopy {
                        igpu_context,
                        shared_igpu,
                        igpu_mutex,
                    },
            } => {
                // a timeout or an abandoned mutex is not ownership: return before the shared
                // texture is touched and without a `ReleaseSync` on a mutex not held
                let outcome = keyed::acquire(igpu_mutex, KEY_WRITER, KEY_TIMEOUT_MS)
                    .context("iGPU keyed mutex AcquireSync(writer)")?;
                require_owned(
                    outcome,
                    "iGPU keyed mutex AcquireSync(writer)",
                    KEY_TIMEOUT_MS,
                )?;
                unsafe {
                    let staged = match scaler {
                        Some(s) => s.scale_into(raw, &*shared_igpu),
                        None => {
                            igpu_context.CopyResource(&*shared_igpu, raw);
                            Ok(())
                        }
                    };
                    igpu_context.Flush();
                    let released = igpu_mutex.ReleaseSync(KEY_ENCODER);
                    staged.context("staging frame into shared NVENC input")?;
                    released.context("iGPU keyed mutex ReleaseSync(encoder)")?;
                }
                encoder.encode_input(force_idr)
            }
            // AMF stages into its own texture ring, so the prescaler (when there is one) blits
            // straight into the slot the encoder is about to wrap
            Backend::Amf { encoder } => encoder.encode_texture(raw, scaler, force_idr),
            _ => bail!("encode_gpu called on a CPU-bridge backend"),
        }
    }

    /// Hand the readback to the encoder, in the format the reader actually produced.
    ///
    /// `format` is not `self.cpu_bridge_format()`: that is what the backend asks the scaler for,
    /// and only the scaler path can honour it. The two no-scaler routes (the staging readback of
    /// a captured frame and `TextureReader`) read the source texture's own format, which is
    /// BGRA. Guessing here would hand an encoder 1.5 bytes a pixel to read as 4.
    pub(crate) fn encode_cpu(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        format: ReadbackFormat,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        match (self, format) {
            (
                Backend::Nvenc {
                    encoder,
                    path: EncodePath::CpuBridge,
                },
                ReadbackFormat::Bgra8,
            ) => encoder.encode_bgra_padded(data, row_pitch, force_idr),
            (Backend::IntelCpu { encoder }, ReadbackFormat::Bgra8) => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            (Backend::IntelCpu { encoder }, ReadbackFormat::Nv12) => {
                encoder.encode_nv12_padded(data, row_pitch, force_idr)
            }
            (Backend::AmfCpu { encoder }, ReadbackFormat::Bgra8) => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            (Backend::AmfCpu { encoder }, ReadbackFormat::Nv12) => {
                encoder.encode_nv12_padded(data, row_pitch, force_idr)
            }
            (Backend::X264 { encoder }, ReadbackFormat::Bgra8) => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            (Backend::X264 { encoder }, ReadbackFormat::Nv12) => {
                encoder.encode_nv12_padded(data, row_pitch, force_idr)
            }
            (backend, format) => bail!(
                "encode_cpu: {} cannot take a {} readback",
                backend.name(),
                format.name()
            ),
        }
    }

    fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.encode_repeat(force_idr),
            Backend::Intel { encoder } => encoder.encode_repeat(force_idr),
            Backend::IntelCpu { encoder } => encoder.encode_repeat(force_idr),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => {
                encoder.encode_repeat(force_idr)
            }
            Backend::X264 { encoder } => encoder.encode_repeat(force_idr),
        }
    }
}

pub(crate) struct EncodeCore {
    pub(crate) backend: Backend,
    pub(crate) scaler: Option<Scaler>,
    pub(crate) reader: Option<TextureReader>,
    /// Reused readback texture for the WGC classic CPU-bridge path. Rebuilt only when the
    /// captured content size or format changes, which is a virtual-display mode change.
    pub(crate) staging: Option<StagingTexture>,
    /// WGC classic path with a governor: the frame its gate held back while the frame rate is
    /// divided, encoded by the repeater at its deadline unless a newer frame comes first.
    pub(crate) held: Option<HeldCopy>,
    pub(crate) target_bitrate: Arc<AtomicU32>,
    pub(crate) idr_request: Arc<AtomicBool>,
    pub(crate) current_bitrate: u32,
    pub(crate) bitrate_warned: bool,
    pub(crate) have_frame: bool,
    pub(crate) frame_index: u64,
    pub(crate) health: CoreHealth,
}

/// A copy of a frame the classic gate held back. It lives in the encode core because the copy
/// and the encode both use the capture device's immediate context, which the core's lock
/// already serializes between the capture callback and the repeater.
pub(crate) struct HeldCopy {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    tex: ID3D11Texture2D,
    desc: D3D11_TEXTURE2D_DESC,
    /// The size the core's `TextureReader` was built for, when the held copy built it.
    reader_for: Option<(u32, u32)>,
    pending: bool,
    capture: Instant,
}

/// What the WGC classic callback and its repeater share about the held copy.
#[derive(Default)]
pub(crate) struct ClassicHold {
    /// When the held copy is due, in nanoseconds after the session epoch; 0 when none is.
    due_ns: AtomicU64,
    /// Callback entry of the newest held copy the repeater delivered; 0 when none.
    delivered_ns: AtomicU64,
}

impl EncodeCore {
    /// Copy `src` to be encoded later, replacing any copy still waiting. `true` when one was.
    pub(crate) fn hold_copy(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
        capture: Instant,
    ) -> Result<bool> {
        let mut want = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut want) };
        let fits = self.held.as_ref().is_some_and(|h| {
            h.desc.Width == want.Width
                && h.desc.Height == want.Height
                && h.desc.Format == want.Format
        });
        if !fits {
            let desc = D3D11_TEXTURE2D_DESC {
                MipLevels: 1,
                ArraySize: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: want.BindFlags
                    | (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                ..want
            };
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .context("creating the held-frame copy")?;
            self.held = Some(HeldCopy {
                device: device.clone(),
                context: context.clone(),
                tex: tex.context("the held-frame copy was null")?,
                desc,
                reader_for: None,
                pending: false,
                capture,
            });
        }
        let h = self
            .held
            .as_mut()
            .ok_or_else(|| anyhow!("held-frame copy missing"))?;
        unsafe { h.context.CopyResource(&h.tex, src) };
        let replaced = std::mem::replace(&mut h.pending, true);
        h.capture = capture;
        Ok(replaced)
    }

    /// A newer frame supersedes the held copy. `true` when one was waiting.
    pub(crate) fn drop_held(&mut self) -> bool {
        self.held
            .as_mut()
            .is_some_and(|h| std::mem::replace(&mut h.pending, false))
    }

    /// Encode the held copy, if one is waiting: its access unit and capture time.
    pub(crate) fn encode_held(&mut self) -> Option<Result<(Vec<u8>, Instant)>> {
        let h = self.held.as_mut().filter(|h| h.pending)?;
        h.pending = false;
        let (tex, capture, size) = (h.tex.clone(), h.capture, (h.desc.Width, h.desc.Height));
        // a CPU bridge with no scaler reads a texture through the core's reader; the classic
        // path never built one, its own frames read through `staging`
        if self.backend.is_cpu_bridge() && self.scaler.is_none() && h.reader_for != Some(size) {
            match TextureReader::new(&h.device, &h.context, size.0, size.1) {
                Ok(r) => {
                    self.reader = Some(r);
                    h.reader_for = Some(size);
                }
                Err(e) => return Some(Err(e)),
            }
        }
        Some(self.encode_texture(&tex).map(|au| (au, capture)))
    }

    fn take_force_idr(&mut self) -> bool {
        if self.frame_index == 0 {
            return true;
        }
        let asked = self.idr_request.swap(false, Ordering::Relaxed);
        match self.health.qp.as_mut() {
            Some(g) => g.admit_keyframe(asked),
            None => asked,
        }
    }

    fn apply_pending_bitrate(&mut self) {
        apply_pending_bitrate(
            &mut self.backend,
            &self.target_bitrate,
            &mut self.current_bitrate,
            &mut self.bitrate_warned,
            &mut self.health.bitrate_retry,
        );
        if let Some(g) = self.health.qp.as_mut() {
            g.before_submit(&mut self.backend);
        }
    }

    /// The bookkeeping every encode shares once the encoder has answered: a forced keyframe is
    /// counted or handed back (see [`settle_forced_idr`]), and a success advances the frame.
    fn finish(&mut self, result: Result<Vec<u8>>, force_idr: bool) -> Result<Vec<u8>> {
        settle_forced_idr(
            &result,
            force_idr,
            &self.idr_request,
            &self.health.telemetry.degradation.idrs_sent,
        );
        let au = result?;
        self.have_frame = true;
        self.frame_index += 1;
        Ok(au)
    }

    /// After an access unit has been sent: what the QP governor learns from it.
    fn emitted(&mut self, au: &[u8], repeat: bool) {
        if let Some(g) = self.health.qp.as_mut() {
            g.emitted(au, repeat);
        }
    }

    /// Whether a QP governor runs on this core. Asked once, where the path is built, so a path
    /// without one never takes the lock again after a send.
    fn governed(&self) -> bool {
        self.health.qp.is_some()
    }

    fn encode_captured(&mut self, frame: &mut Frame) -> Result<Vec<u8>> {
        let force_idr = self.take_force_idr();
        if let Some(g) = self.health.qp.as_mut() {
            g.keyframe_next(force_idr);
        }
        self.apply_pending_bitrate();
        let result = self.encode_captured_with(frame, force_idr);
        self.finish(result, force_idr)
    }

    fn encode_captured_with(&mut self, frame: &mut Frame, force_idr: bool) -> Result<Vec<u8>> {
        let mapped_readback = self.health.mapped_readback;
        let x264_no_copy = self.health.x264_no_copy;
        let Self {
            backend,
            scaler,
            staging,
            ..
        } = self;

        let au = if backend.is_cpu_bridge() {
            if let Some(scaler) = scaler {
                let format = scaler.format();
                scaler.scale(frame.as_raw_texture())?;
                if mapped_readback {
                    scaler.read_back_with(|data, row_pitch| {
                        backend.encode_cpu(data, row_pitch, format, force_idr)
                    })?
                } else {
                    let (data, row_pitch) = scaler.read_back()?;
                    encode_scaled(backend, data, row_pitch, format, force_idr, x264_no_copy)?
                }
            } else {
                let desc = *frame.desc();
                let stale = staging.as_ref().is_none_or(|s| {
                    let d = s.desc();
                    d.Width != desc.Width || d.Height != desc.Height || d.Format != desc.Format
                });
                if stale {
                    *staging = Some(
                        StagingTexture::new(frame.device(), desc.Width, desc.Height, desc.Format)
                            .context("creating the reusable CPU-bridge readback texture")?,
                    );
                }
                let staging = staging
                    .as_mut()
                    .ok_or_else(|| anyhow!("cpu-bridge readback texture missing"))?;
                let mut fb = frame.buffer_with(staging)?;
                let row_pitch = fb.row_pitch();
                // no scaler, so no video processor: this is the captured texture's own format
                backend.encode_cpu(
                    fb.as_raw_buffer(),
                    row_pitch,
                    ReadbackFormat::Bgra8,
                    force_idr,
                )?
            }
        } else {
            backend.encode_gpu(frame.as_raw_texture(), scaler.as_mut(), force_idr)?
        };
        Ok(au)
    }

    pub(crate) fn encode_texture(&mut self, tex: &ID3D11Texture2D) -> Result<Vec<u8>> {
        let force_idr = self.take_force_idr();
        if let Some(g) = self.health.qp.as_mut() {
            g.keyframe_next(force_idr);
        }
        self.apply_pending_bitrate();
        let result = self.encode_texture_with(tex, force_idr);
        self.finish(result, force_idr)
    }

    fn encode_texture_with(&mut self, tex: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<u8>> {
        let mapped_readback = self.health.mapped_readback;
        let x264_no_copy = self.health.x264_no_copy;
        let Self {
            backend,
            scaler,
            reader,
            ..
        } = self;

        let au = if backend.is_cpu_bridge() {
            if let Some(scaler) = scaler {
                let format = scaler.format();
                scaler.scale(tex)?;
                if mapped_readback {
                    scaler.read_back_with(|data, row_pitch| {
                        backend.encode_cpu(data, row_pitch, format, force_idr)
                    })?
                } else {
                    let (data, row_pitch) = scaler.read_back()?;
                    encode_scaled(backend, data, row_pitch, format, force_idr, x264_no_copy)?
                }
            } else {
                let reader = reader
                    .as_mut()
                    .ok_or_else(|| anyhow!("cpu-bridge texture encode without a TextureReader"))?;
                if mapped_readback {
                    reader.read_back_with(tex, |data, row_pitch| {
                        backend.encode_cpu(data, row_pitch, ReadbackFormat::Bgra8, force_idr)
                    })?
                } else {
                    let (data, row_pitch) = reader.read_back(tex)?;
                    backend.encode_cpu(data, row_pitch, ReadbackFormat::Bgra8, force_idr)?
                }
            }
        } else {
            backend.encode_gpu(tex, scaler.as_mut(), force_idr)?
        };
        Ok(au)
    }

    /// Re-encode the last picture for the idle repeater.
    ///
    /// `Ok(None)` means "nothing to send this tick": no picture yet, or a previous repeat failed
    /// and the backoff has not run out. With `repeat_error_backoff` on, a repeat that keeps
    /// failing (a lost device, a wedged encoder) is retried at 200 ms doubling to 5 s, and only
    /// the 1st, 2nd, 4th... failure reaches the repeater, which logs it.
    fn encode_repeat(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.have_frame {
            return Ok(None);
        }
        if !self.health.repeat_retry.ready_now() {
            return Ok(None);
        }
        let force_idr = self.take_force_idr();
        if let Some(g) = self.health.qp.as_mut() {
            g.keyframe_next(force_idr);
        }
        self.apply_pending_bitrate();
        let from_scaler = if self.health.x264_no_copy {
            repeat_from_scaler(&mut self.backend, self.scaler.as_ref(), force_idr)
        } else {
            None
        };
        let result = match from_scaler {
            Some(result) => result,
            None => self.backend.encode_repeat(force_idr),
        };
        settle_forced_idr(
            &result,
            force_idr,
            &self.idr_request,
            &self.health.telemetry.degradation.idrs_sent,
        );
        match result {
            Ok(au) => {
                self.health.repeat_retry.succeeded();
                self.frame_index += 1;
                Ok(Some(au))
            }
            Err(e) => {
                if self.health.repeat_retry.failed(Instant::now()) {
                    Err(e.context(format!(
                        "repeat attempt {}",
                        self.health.repeat_retry.failures()
                    )))
                } else {
                    Ok(None)
                }
            }
        }
    }
}

/// Locks taken after a send to hand the QP governor its record.
#[cfg(test)]
pub(crate) static POST_SEND_LOCKS: AtomicU64 = AtomicU64::new(0);

/// After a send on a classic path: the governor's record, if the path has a governor (`record`
/// is `None` otherwise, and then the encode lock is not touched).
pub(crate) fn record_sent(core: &Mutex<EncodeCore>, record: Option<Bytes>, repeat: bool) {
    if let Some(au) = record {
        #[cfg(test)]
        POST_SEND_LOCKS.fetch_add(1, Ordering::Relaxed);
        core.lock_recover().emitted(&au, repeat);
    }
}

/// Rebuilds of the classic backend allowed in [`REBUILD_WINDOW`] before a failure ends the
/// capture: a device that keeps failing is not worth a new session on every frame.
const REBUILD_LIMIT: usize = 3;
const REBUILD_WINDOW: Duration = Duration::from_secs(60);

/// A sliding window of recent rebuilds.
#[derive(Debug, Default)]
pub(crate) struct RebuildBudget {
    recent: std::collections::VecDeque<Instant>,
}

impl RebuildBudget {
    /// Records a rebuild at `now` and says whether it is allowed.
    pub(crate) fn allow(&mut self, now: Instant) -> bool {
        while self
            .recent
            .front()
            .is_some_and(|t| now.duration_since(*t) >= REBUILD_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= REBUILD_LIMIT {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}

struct BackendRebuild {
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    budget: RebuildBudget,
}

struct LiveCapture {
    core: Option<Arc<Mutex<EncodeCore>>>,
    /// The classic core has a QP governor (decided once, at construction).
    governed: bool,
    ring: Option<AsyncEncodeRing>,
    tx: broadcast::Sender<EncodedFrame>,
    epoch: Instant,
    last_frame_at: Arc<AtomicU64>,
    path_name: &'static str,
    frames_sent: u64,
    busy_streak: u32,
    stop: Arc<AtomicBool>,
    frame_pool_buffers: u32,
    /// Frames the frame pool discarded while draining to the newest. Folded into telemetry from
    /// the callback, the only place that can see it move.
    stale_frames: Arc<AtomicU32>,
    stale_frames_reported: u32,
    gate: FrameGate,
    cursor: CursorProbe,
    /// True when this session draws the pointer itself and WGC was therefore asked not to. Read
    /// once by [`GraphicsCaptureApiHandler::cursor_capture_override`], before the session starts.
    cursor_composited: bool,
    /// Ring path only, `wgc_trailing_frame`: delivers the newest frame the cadence gate held back
    /// once the burst it belonged to has ended. See [`TrailingEdge`].
    trailing: Option<TrailingEdge>,
    /// Classic path with a governor: the held copy's deadline, shared with the repeater that
    /// encodes it. The gate holds a frame only while the frame rate is divided.
    classic_hold: Option<Arc<ClassicHold>>,
    hold_failed: bool,
    /// The display's size when the pipeline was built, and the first frame's size. A later frame
    /// of another size means the display changed mode: `CopyResource` into a slot of the old size
    /// is a call the runtime silently drops, so the client would keep getting stale pixels.
    native_w: u32,
    native_h: u32,
    frame_size: Option<(u32, u32)>,
    /// `wgc_drop_resized_frames`: such a frame is dropped instead of being staged or encoded, so
    /// the client keeps the last correct picture until the display is back at its size or the
    /// pipeline is rebuilt.
    drop_resized_frames: bool,
    size_change_reported: bool,
    /// Set by `on_frame_arrived` on the classic path while a frame is on its way to the encoder,
    /// so the idle repeater does not start a keepalive encode under it (`classic_repeater_yields`).
    arriving: Option<Arc<AtomicBool>>,
    /// `wgc_free_threaded_pool`: callbacks arrive on WinRT worker threads, each tuned on first use.
    free_threaded: bool,
    /// The capture has ended for good (see `mark_dead`); nothing more is encoded or repeated.
    dead: bool,
    /// Classic path: what it takes to build the backend again when the encoder says only a new
    /// session can clear its failure (a Quick Sync session whose keyed mutex or device state is
    /// lost).
    rebuild: Option<BackendRebuild>,
    telemetry: SharedTelemetry,
}

impl Drop for LiveCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut ring) = self.ring.take() {
            ring.stop();
        }
    }
}

fn build_zero_copy(
    config: EncoderConfig,
    igpu_device: &ID3D11Device,
    igpu_context: &ID3D11DeviceContext,
) -> Result<(Encoder, EncodePath)> {
    let encoder = Encoder::new_shared(config)?;
    let handle = encoder
        .shared_handle()
        .ok_or_else(|| anyhow!("shared encoder produced no handle"))?;
    let device1: ID3D11Device1 = igpu_device.cast().context("iGPU device as ID3D11Device1")?;
    let shared_igpu: ID3D11Texture2D =
        unsafe { device1.OpenSharedResource1(handle) }.context("OpenSharedResource1 on iGPU")?;
    let igpu_mutex: IDXGIKeyedMutex = shared_igpu
        .cast()
        .context("opened shared texture as IDXGIKeyedMutex")?;
    Ok((
        encoder,
        EncodePath::ZeroCopy {
            igpu_context: igpu_context.clone(),
            shared_igpu,
            igpu_mutex,
        },
    ))
}

pub(crate) fn build_zero_copy_ring(
    config: EncoderConfig,
    igpu_device: &ID3D11Device,
    ring: usize,
) -> Result<(Encoder, Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>)> {
    let encoder = Encoder::new_shared_ring(config, ring)?;
    let handles = encoder.shared_handles();
    if handles.len() != ring {
        bail!(
            "ring encoder produced {} handles, expected {}",
            handles.len(),
            ring
        );
    }
    let device1: ID3D11Device1 = igpu_device.cast().context("iGPU device as ID3D11Device1")?;
    let mut pairs: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)> = Vec::with_capacity(ring);
    for handle in handles {
        let shared_igpu: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(handle) }
            .context("OpenSharedResource1 on iGPU (ring)")?;
        let igpu_mutex: IDXGIKeyedMutex = shared_igpu
            .cast()
            .context("opened shared ring texture as IDXGIKeyedMutex")?;
        pairs.push((shared_igpu, igpu_mutex));
    }
    Ok((encoder, pairs))
}

/// The AMD half of the async ring.
///
/// Nothing is shared and nothing is opened: the encoder builds its input textures on the
/// capture device, which makes `CreateSurfaceFromDX11Native` a pointer wrap rather than a hidden
/// cross-device copy, so the ring just borrows them. There is no keyed mutex: the free/ready
/// channels are the whole ownership protocol.
///
/// Long-term references are off by default (`amf_ring_ltr_off`, ships on), as on NVENC:
/// `amd/sps.rs` rewrites the VUI with `max_dec_frame_buffering = max_num_ref_frames` copied from
/// AMF's own SPS, and whether AMF counts its LTRs in that field is unknown. If it does not, a
/// client decoder sized from that SPS evicts the picture a repair predicts from and paints
/// green with no decode error. With the switch off, `MAX_LTR_FRAMES_SUPPORTED` are requested,
/// the encoder clamps to what the adapter reports and reads back what AMF granted, so
/// `ltr_slots()` is the truth and may still be 0.
pub(crate) fn build_amf_ring(
    config: EncoderConfig,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    ring: usize,
) -> Result<(AmfEncoder, Vec<ID3D11Texture2D>)> {
    let ltr_frames = if super::tune::on("amf_ring_ltr_off") {
        0
    } else {
        super::amd::encoder::MAX_LTR_FRAMES_SUPPORTED
    };
    let encoder = AmfEncoder::new_on_device_with_ltr(config, device, context, ltr_frames, ring)?;
    let textures = encoder.ring_textures();
    if textures.len() != ring {
        bail!(
            "AMF ring encoder produced {} textures, expected {ring}",
            textures.len()
        );
    }
    Ok((encoder, textures))
}

// declared here so the ring's tests can reach the encode thread's private state
#[cfg(test)]
#[path = "test/ring.rs"]
mod ring_tests;

#[cfg(test)]
#[path = "test/session_teardown.rs"]
mod session_teardown_tests;

struct ReadyMsg {
    slot: usize,
    force_idr: bool,
    capture: Instant,
    timing: FrameTiming,
}

/// A slot the encode thread could not acquire (GPU hang, TDR, driver fault) is wedged at
/// `KEY_ENCODER` and nobody will ever release it. The producer must know, or it pulls the slot
/// out of the free pool and blocks the capture callback on a mutex with no owner. `K` is at most
/// a handful, so a bitmask is enough and can be read from the capture thread with one load.
#[derive(Default)]
struct PoisonedSlots(AtomicU32);

impl PoisonedSlots {
    fn mark(&self, slot: usize) {
        if slot < 32 {
            self.0.fetch_or(1 << slot, Ordering::Relaxed);
        }
    }

    fn contains(&self, slot: usize) -> bool {
        slot < 32 && self.0.load(Ordering::Relaxed) & (1 << slot) != 0
    }

    fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed).count_ones()
    }
}

/// Slots the encode thread handed back to the free pool without encoding them: an older queued
/// frame skipped for a newer one (`ring_latest_wins`). On the shared NVENC ring such a slot's
/// keyed mutex still sits at `KEY_ENCODER`, where the stage left it, so the next stage must
/// acquire that key instead of `KEY_WRITER` or it times out on a slot nobody holds. The bit is
/// set before the slot is sent to the free channel and cleared by the stage that takes it, and
/// the channel orders the two.
#[derive(Default)]
struct UnencodedSlots(AtomicU32);

impl UnencodedSlots {
    fn mark(&self, slot: usize) {
        if slot < 32 {
            self.0.fetch_or(1 << slot, Ordering::Relaxed);
        }
    }

    /// Clears the bit and says whether it was set.
    fn take(&self, slot: usize) -> bool {
        slot < 32 && self.0.fetch_and(!(1 << slot), Ordering::Relaxed) & (1 << slot) != 0
    }
}

/// The async ring cannot go on: its encode thread has stopped (a dead encoder, a removed device,
/// too few usable slots). [`AsyncEncodeRing::stage`] returns this, and only this, as an `Err`;
/// every per-frame problem it can recover from is an `Ok(false)` drop. The capture handler should
/// return it from `on_frame_arrived` so WGC halts and the session is torn down and rebuilt, the
/// way the classic path's own fatal encode error does.
#[derive(Debug)]
pub(crate) struct RingFailed(&'static str);

impl std::fmt::Display for RingFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "async encode ring failed: {}", self.0)
    }
}

impl std::error::Error for RingFailed {}

/// The `Instant` whose host-clock reading is `ns`, so a stamp the capture callback took as host
/// nanoseconds (`FrameTiming::capture_cb_entry_ns`) can ride an `EncodedFrame` as its capture
/// instant. Exact: `host_instant_to_ns(instant_at_host_ns(ns)) == ns` for any `ns` in the past.
/// Zero (no stamp) or a value in the future gives "now".
fn instant_at_host_ns(ns: u64) -> Instant {
    let now = Instant::now();
    let now_ns = crate::streamer::audio::host_instant_to_ns(now);
    if ns == 0 || ns > now_ns {
        return now;
    }
    now - Duration::from_nanos(now_ns - ns)
}

/// Whether an Annex-B access unit is an IDR. Stops at the first slice NAL, so it reads the
/// parameter sets and SEI of a keyframe and a handful of bytes of anything else, unlike
/// [`is_keyframe`], which walks a whole P frame looking for a type it will not find.
fn access_unit_is_idr(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 3 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            match au[i + 3] & 0x1f {
                5 => return true,
                1..=4 => return false,
                _ => i += 4,
            }
        } else {
            i += 1;
        }
    }
    false
}

/// The encoder half of the async ring.
///
/// Two engines, one protocol. NVENC's ring textures are shared across adapters and reached
/// through a keyed mutex, because its encoder lives on the dGPU while capture happens on the
/// iGPU. AMF's live on the capture device itself, so there is no handle to open and no mutex to
/// hold, and slot ownership is entirely the ring's free/ready channels'. Everything downstream
/// of the submit is identical: the encode thread, the keepalive, the bitrate driver and the
/// `RepairState` protocol are written once.
// one per session, moved once into the encode thread: boxing would only put an indirection on
// the per-frame submit (`Backend` is allowed for the same reason)
#[allow(clippy::large_enum_variant)]
pub(crate) enum RingEncoder {
    Nvenc(Encoder),
    Amf(AmfEncoder),
}

impl RingEncoder {
    fn name(&self) -> &'static str {
        match self {
            RingEncoder::Nvenc(_) => "nvenc-zero-copy-ring",
            RingEncoder::Amf(_) => "amd-amf-ring",
        }
    }

    fn device(&self) -> &ID3D11Device {
        match self {
            RingEncoder::Nvenc(e) => e.device(),
            RingEncoder::Amf(e) => e.device(),
        }
    }

    fn attach_idr_flag(&mut self, flag: Arc<AtomicBool>) {
        match self {
            RingEncoder::Nvenc(e) => e.attach_idr_flag(flag),
            RingEncoder::Amf(e) => e.attach_idr_flag(flag),
        }
    }

    fn request_idr(&mut self) {
        match self {
            RingEncoder::Nvenc(e) => e.request_idr(),
            RingEncoder::Amf(e) => e.request_idr(),
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        match self {
            RingEncoder::Nvenc(e) => e.set_bitrate(bps),
            RingEncoder::Amf(e) => e.set_bitrate(bps),
        }
    }

    fn set_qp(&mut self, qp: u8) -> Result<bool> {
        match self {
            RingEncoder::Nvenc(e) => e.set_qp(qp),
            RingEncoder::Amf(e) => e.set_qp(qp),
        }
    }

    fn set_qp_costly(&mut self, qp: u8) -> Result<bool> {
        match self {
            RingEncoder::Nvenc(_) => Ok(false),
            RingEncoder::Amf(e) => e.set_qp_reinit(qp),
        }
    }

    fn ltr_slots(&self) -> u32 {
        match self {
            RingEncoder::Nvenc(e) => e.ltr_slots(),
            RingEncoder::Amf(e) => e.ltr_slots(),
        }
    }

    fn next_frame_index(&self) -> u64 {
        match self {
            RingEncoder::Nvenc(e) => e.next_frame_index(),
            RingEncoder::Amf(e) => e.next_frame_index(),
        }
    }

    fn mark_ltr(&mut self) -> Option<u32> {
        match self {
            RingEncoder::Nvenc(e) => e.mark_ltr(),
            RingEncoder::Amf(e) => e.mark_ltr(),
        }
    }

    fn use_ltr(&mut self, bitmap: u32) -> bool {
        match self {
            RingEncoder::Nvenc(e) => e.use_ltr(bitmap),
            RingEncoder::Amf(e) => e.use_ltr(bitmap),
        }
    }

    /// Drop a frame the client never received from the reference set.
    ///
    /// AMF has no `nvEncInvalidateRefFrames` analogue (it bars long-term references outside the
    /// forced bitfield from prediction instead), so this is `Unsupported` there and
    /// [`RingEngine::invalidates_references`] says so. The encode thread must never ask AMF: its
    /// `Unsupported` does not mean "this encoder cannot repair".
    fn invalidate_reference(&mut self, frame_index: u64) -> RefInvalidation {
        match self {
            RingEncoder::Nvenc(e) => e.invalidate_reference(frame_index),
            RingEncoder::Amf(_) => RefInvalidation::Unsupported,
        }
    }

    fn encode_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>, SlotEncodeError> {
        match self {
            RingEncoder::Nvenc(e) => e.encode_input_slot(slot, force_idr),
            // no keyed mutex to wedge, so an AMF slot can never be poisoned
            RingEncoder::Amf(e) => e
                .encode_slot(slot, force_idr)
                .map_err(SlotEncodeError::Encode),
        }
    }

    fn encode_repeat_slot(&mut self, slot: usize) -> Result<Vec<u8>> {
        match self {
            RingEncoder::Nvenc(e) => e.encode_repeat_slot(slot),
            RingEncoder::Amf(e) => e.encode_repeat_slot(slot),
        }
    }
}

impl<E: RingEngine> super::governor::QpEncoder for E {
    fn set_qp(&mut self, qp: u8) -> Result<bool> {
        RingEngine::set_qp(self, qp)
    }
    fn set_qp_costly(&mut self, qp: u8) -> Result<bool> {
        RingEngine::set_qp_costly(self, qp)
    }
}

/// What the ring's encode thread needs from an encoder. [`RingEncoder`] is the only production
/// implementation; the trait exists so the thread's decisions can be exercised without a GPU.
pub(crate) trait RingEngine: Send + 'static {
    fn name(&self) -> &'static str;
    fn attach_idr_flag(&mut self, flag: Arc<AtomicBool>);
    /// Whether this engine wants the NVIDIA clock hold (`nvidia::clock_hold`) while it lives.
    fn holds_gpu_clock(&self) -> bool;
    fn request_idr(&mut self);
    fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied>;
    fn set_qp(&mut self, _qp: u8) -> Result<bool> {
        Ok(false)
    }
    fn set_qp_costly(&mut self, _qp: u8) -> Result<bool> {
        Ok(false)
    }
    fn ltr_slots(&self) -> u32;
    fn next_frame_index(&self) -> u64;
    fn mark_ltr(&mut self) -> Option<u32>;
    fn use_ltr(&mut self, bitmap: u32) -> bool;
    /// Whether [`Self::invalidate_reference`] is how this engine drops a lost picture. False
    /// means the repair is the forced long-term bitfield alone, and a lost frame is no reason to
    /// stop repairing.
    fn invalidates_references(&self) -> bool;
    fn invalidate_reference(&mut self, frame_index: u64) -> RefInvalidation;
    fn encode_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>, SlotEncodeError>;
    fn encode_repeat_slot(&mut self, slot: usize) -> Result<Vec<u8>>;
    /// The encoder's D3D11 device has been removed (TDR, driver update or restart, an unplugged
    /// adapter). Asked only after an encode failed.
    fn device_lost(&self) -> bool;
}

impl RingEngine for RingEncoder {
    fn name(&self) -> &'static str {
        RingEncoder::name(self)
    }
    fn attach_idr_flag(&mut self, flag: Arc<AtomicBool>) {
        RingEncoder::attach_idr_flag(self, flag)
    }
    fn holds_gpu_clock(&self) -> bool {
        matches!(self, RingEncoder::Nvenc(_))
    }
    fn request_idr(&mut self) {
        RingEncoder::request_idr(self)
    }
    fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        RingEncoder::set_bitrate(self, bps)
    }
    fn set_qp(&mut self, qp: u8) -> Result<bool> {
        RingEncoder::set_qp(self, qp)
    }
    fn set_qp_costly(&mut self, qp: u8) -> Result<bool> {
        RingEncoder::set_qp_costly(self, qp)
    }
    fn ltr_slots(&self) -> u32 {
        RingEncoder::ltr_slots(self)
    }
    fn next_frame_index(&self) -> u64 {
        RingEncoder::next_frame_index(self)
    }
    fn mark_ltr(&mut self) -> Option<u32> {
        RingEncoder::mark_ltr(self)
    }
    fn use_ltr(&mut self, bitmap: u32) -> bool {
        RingEncoder::use_ltr(self, bitmap)
    }
    fn invalidates_references(&self) -> bool {
        matches!(self, RingEncoder::Nvenc(_))
    }
    fn invalidate_reference(&mut self, frame_index: u64) -> RefInvalidation {
        RingEncoder::invalidate_reference(self, frame_index)
    }
    fn encode_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>, SlotEncodeError> {
        RingEncoder::encode_slot(self, slot, force_idr)
    }
    fn encode_repeat_slot(&mut self, slot: usize) -> Result<Vec<u8>> {
        RingEncoder::encode_repeat_slot(self, slot)
    }
    fn device_lost(&self) -> bool {
        // SAFETY: a plain query on a live device the encoder owns.
        unsafe { RingEncoder::device(self).GetDeviceRemovedReason() }.is_err()
    }
}

/// The staging half of the async ring: where the capture callback puts each frame.
pub(crate) enum RingTextures {
    /// NVENC. Cross-adapter shared textures, each guarded by a keyed mutex the encode thread
    /// takes at `KEY_ENCODER` and gives back at `KEY_WRITER`.
    Shared {
        textures: Vec<ID3D11Texture2D>,
        mutexes: Vec<IDXGIKeyedMutex>,
    },
    /// AMF. The encoder's own textures, on the capture device. One `CopyResource` (or
    /// `VideoProcessorBlt`) and the `Flush` AMD's own fix for AMF issue #92 calls for; the
    /// free/ready channels are the only thing that says who owns a slot.
    Local { textures: Vec<ID3D11Texture2D> },
}

impl RingTextures {
    fn len(&self) -> usize {
        self.slots().len()
    }

    fn slots(&self) -> &[ID3D11Texture2D] {
        match self {
            RingTextures::Shared { textures, .. } | RingTextures::Local { textures } => textures,
        }
    }

    /// The per-slot keys, or an empty slice for a ring that has none.
    fn keys(&self) -> &[IDXGIKeyedMutex] {
        match self {
            RingTextures::Shared { mutexes, .. } => mutexes,
            RingTextures::Local { .. } => &[],
        }
    }
}

/// The ring's A/B switches (`tune.rs`), read once per session in [`AsyncEncodeRing::new`].
#[derive(Clone, Copy, Debug)]
struct RingSwitches {
    /// `ring_latest_wins` (candidate, ships OFF). Under overload the encode thread releases the
    /// slot it holds for the keepalive before it encodes rather than after, so the capture side
    /// has two free slots while the encoder works instead of one; and when it picks up a frame it
    /// first drains the ready queue to the newest one, handing the older slots back unencoded.
    /// Off, an arriving frame is dropped whenever no slot is free and the older queued frame is
    /// encoded next, so the client paints frames up to one encode time older than they need be.
    latest_wins: bool,
    /// `ring_wake_yields_to_frame` (ships ON). A wake or idle tick that finds a captured frame
    /// already queued does nothing, so the frame is encoded next and takes any pending IDR itself
    /// (both encoders read the shared IDR flag at submit). Off, `select!` picks at random and can
    /// re-encode the held, older picture as a keyframe ahead of the fresher one.
    yield_to_frame: bool,
    /// `ring_capture_stamp` (ships ON). The stamp contract: a captured frame carries the instant
    /// its WGC callback was entered, a cursor frame the instant the pointer was read, and a
    /// repeat its own submit instant (`max(submit, emit - E)`, which is the submit on an encoder
    /// that holds no frame). Off, a captured frame is stamped after the stage and a keepalive
    /// after its own encode.
    stamp_contract: bool,
    /// `ring_give_up_on_error_streak` (ships on). `MAX_TRANSIENT_ENCODE_DROPS` failed encodes in
    /// a row end the ring, as a removed device always does, and the WGC handler then halts the
    /// capture on the stage's `RingFailed`. Off, the ring keeps retrying (logging at powers of
    /// two).
    give_up_on_streak: bool,
}

/// Holds a D3D11 device's multithread critical section (`ID3D11Multithread::Enter`) for a scope.
/// The section is recursive, and the same one `SetMultithreadProtected` serialises single calls
/// with, so a run of calls inside it cannot be interleaved with another thread's calls on the same
/// device.
struct DeviceLock<'a>(&'a windows::Win32::Graphics::Direct3D11::ID3D11Multithread);

impl<'a> DeviceLock<'a> {
    fn enter(mt: &'a windows::Win32::Graphics::Direct3D11::ID3D11Multithread) -> Self {
        // SAFETY: `mt` is a live interface on a device this ring holds a reference to.
        unsafe { mt.Enter() };
        Self(mt)
    }
}

impl Drop for DeviceLock<'_> {
    fn drop(&mut self) {
        // SAFETY: paired with the `Enter` in `DeviceLock::enter`, on the same thread.
        unsafe { self.0.Leave() };
    }
}

pub(crate) struct AsyncEncodeRing {
    textures: RingTextures,
    igpu_context: ID3D11DeviceContext,
    /// The capture device's multithread lock, for the AMF (`Local`) ring with a host-composited
    /// pointer (`SE_CURSOR_OVERLAY=1`) only. There the encode thread's `SubmitInput` runs AMF's
    /// own converter on the very immediate context the stage copies and draws on, and protected
    /// mode serialises single calls, not a set-state-then-draw run. The NVENC ring stages on a
    /// device nothing else uses.
    local_lock: Option<windows::Win32::Graphics::Direct3D11::ID3D11Multithread>,
    scaler: Option<Scaler>,
    /// Draws the pointer onto each staged slot, when the host is compositing it rather than WGC.
    /// Shared with the encode thread, which redraws it on the slot it holds when the pointer
    /// moves and no frame arrives (see [`cursor_overlay`]).
    cursor: Option<Arc<Mutex<CursorOverlay>>>,
    free_slots: crossbeam_channel::Sender<usize>,
    free_recv: crossbeam_channel::Receiver<usize>,
    ready: crossbeam_channel::Sender<ReadyMsg>,
    encode_join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    /// Set by the encode thread when it gives up for any reason other than `stop`.
    failed: Arc<AtomicBool>,
    idr_request: Arc<AtomicBool>,
    poisoned: Arc<PoisonedSlots>,
    unencoded: Arc<UnencodedSlots>,
    slots: usize,
    frame_index: u64,
    dropped: u64,
    dropped_reported: u64,
    stage_timeout_ms: u32,
    switches: RingSwitches,
    telemetry: SharedTelemetry,
    /// Holds the NVIDIA GPU out of its idle clocks while this NVENC ring lives
    /// (`nvidia::clock_hold`). Dropped with the ring.
    _clock_hold: Option<super::nvidia::clock_hold::Guard>,
}

impl AsyncEncodeRing {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new<E: RingEngine>(
        encoder: E,
        textures: RingTextures,
        igpu_context: ID3D11DeviceContext,
        scaler: Option<Scaler>,
        cursor: Option<Arc<Mutex<CursorOverlay>>>,
        tx: broadcast::Sender<EncodedFrame>,
        target_bitrate: Arc<AtomicU32>,
        initial_bitrate: u32,
        // unread on the ring path; kept so the caller's argument list does not change
        _last_frame_at: Arc<AtomicU64>,
        idr_request: Arc<AtomicBool>,
        repair: Arc<crate::streamer::repair::RepairState>,
        wake_rx: crossbeam_channel::Receiver<()>,
        frame_duration: Duration,
        _epoch: Instant,
        telemetry: SharedTelemetry,
    ) -> Self {
        let k = textures.len();
        // the keepalive re-encodes the held slot without going through `stage()`, the other
        // reader of `idr_request`; sharing the atomic lets a PLI on a static desktop get its IDR
        let mut encoder = encoder;
        encoder.attach_idr_flag(Arc::clone(&idr_request));
        let thread_name = format!("{}-encode", encoder.name());
        let clock_hold = if encoder.holds_gpu_clock() {
            super::nvidia::clock_hold::acquire()
        } else {
            None
        };

        let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(k);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<ReadyMsg>(k);
        for slot in 0..k {
            free_tx.send(slot).expect("prefill free_slots");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let failed = Arc::new(AtomicBool::new(false));
        let poisoned = Arc::new(PoisonedSlots::default());
        let unencoded = Arc::new(UnencodedSlots::default());
        let switches = RingSwitches {
            latest_wins: super::tune::candidate("ring_latest_wins"),
            yield_to_frame: super::tune::on("ring_wake_yields_to_frame"),
            stamp_contract: super::tune::on("ring_capture_stamp"),
            give_up_on_streak: super::tune::on("ring_give_up_on_error_streak"),
        };
        // only a host-composited pointer makes the stage a set-state-then-draw run; without it
        // every stage call is a single call that protected mode already serialises
        let local_lock = match &textures {
            RingTextures::Local { .. } if cursor.is_some() => igpu_context
                .cast::<windows::Win32::Graphics::Direct3D11::ID3D11Multithread>()
                .ok(),
            RingTextures::Local { .. } | RingTextures::Shared { .. } => None,
        };

        let encode_join = {
            let args = RingEncodeArgs {
                encoder,
                cursor: cursor.clone(),
                ready_rx,
                free_tx: free_tx.clone(),
                tx,
                target_bitrate,
                initial_bitrate,
                idr_request: Arc::clone(&idr_request),
                repair: Arc::clone(&repair),
                wake_rx,
                frame_duration,
                stop: Arc::clone(&stop),
                failed: Arc::clone(&failed),
                poisoned: Arc::clone(&poisoned),
                unencoded: Arc::clone(&unencoded),
                slots: k,
                telemetry: Arc::clone(&telemetry),
                switches,
            };
            std::thread::Builder::new()
                .name(thread_name)
                .spawn(move || ring_encode_thread(args))
                .expect("spawn ring encode thread")
        };

        // a capture-side acquire only waits for a slot the encoder already released. Not
        // `KEY_TIMEOUT_MS` (1 s of blocked WGC callback): past two periods the frame is lost
        let stage_timeout_ms = (frame_duration.as_millis() as u32)
            .saturating_mul(2)
            .clamp(5, 50);

        Self {
            textures,
            igpu_context,
            local_lock,
            scaler,
            cursor,
            free_slots: free_tx,
            free_recv: free_rx,
            ready: ready_tx,
            encode_join: Some(encode_join),
            stop,
            failed,
            idr_request,
            poisoned,
            unencoded,
            slots: k,
            frame_index: 0,
            dropped: 0,
            dropped_reported: 0,
            stage_timeout_ms,
            switches,
            telemetry,
            _clock_hold: clock_hold,
        }
    }

    /// Pulls a usable free slot, discarding any the encode thread has quarantined. A poisoned
    /// slot is never returned to the pool, so this drains at most once per poisoning.
    fn take_free_slot(&mut self) -> Option<usize> {
        while let Ok(slot) = self.free_recv.try_recv() {
            if !self.poisoned.contains(slot) {
                return Some(slot);
            }
        }
        None
    }

    /// Slots not quarantined. One is always held by the encode thread for the keepalive, so a
    /// ring with fewer than two can never stage another frame.
    fn live_slots(&self) -> u32 {
        (self.slots as u32).saturating_sub(self.poisoned.count())
    }

    fn note_drop(&mut self) {
        self.dropped += 1;
        self.telemetry
            .degradation
            .frames_dropped_ring
            .fetch_add(1, Ordering::Relaxed);
        // rate-limited: every logbus line is a UI IPC event
        if self.dropped - self.dropped_reported >= 120 {
            self.dropped_reported = self.dropped;
            let live = self.live_slots();
            teprintln!(
                "ring: {} frames dropped for want of a free slot (live slots {live}/{}); \
                 the encoder is not keeping up",
                self.dropped,
                self.slots
            );
        }
    }

    /// A slot whose keyed mutex this stage does not own, or could not hand on: quarantined for
    /// the rest of the session, and the frame dropped.
    fn quarantine(&mut self, slot: usize, why: std::fmt::Arguments<'_>) {
        self.poisoned.mark(slot);
        self.telemetry
            .degradation
            .ring_slot_quarantines
            .fetch_add(1, Ordering::Relaxed);
        self.note_drop();
        teprintln!(
            "ring stage: slot {slot} quarantined ({why}); {} of {} slots left",
            self.live_slots(),
            self.slots
        );
    }

    /// Stage one captured frame into a free slot and queue it for the encode thread.
    ///
    /// `Ok(true)`: queued. `Ok(false)`: this frame was dropped (no free slot, or its slot could
    /// not be acquired and was quarantined) and the next one may go through. `Err` is always a
    /// [`RingFailed`]: the ring can never stage again, and the capture should stop.
    pub(crate) fn stage(&mut self, raw: &ID3D11Texture2D, mut timing: FrameTiming) -> Result<bool> {
        if self.failed.load(Ordering::Relaxed) {
            return Err(RingFailed("the encode thread has stopped").into());
        }
        if self.live_slots() < 2 {
            return Err(RingFailed("fewer than two usable slots are left").into());
        }
        let Some(slot) = self.take_free_slot() else {
            // every slot is still in the encoder: the one place the ring discards captured pixels
            self.note_drop();
            return Ok(false);
        };
        let unencoded = self.unencoded.take(slot);

        // one lock for the whole GPU sequence below: D3D11 serialises single context calls, not a
        // set-state-then-draw run, and the encode thread draws the pointer on its own slot through
        // the same immediate context. Cloned out first so the guard borrows a local, not `self`,
        // which the drop paths below still need mutably
        let overlay = self.cursor.clone();
        let mut cursor = overlay.as_ref().map(|c| c.lock_recover());

        match &self.textures {
            RingTextures::Shared { textures, mutexes } => {
                let mutex = &mutexes[slot];
                let dst = &textures[slot];

                // a slot the encode thread skipped for a newer frame came back without the
                // encoder ever taking it, so its mutex is still at the key this stage released
                let key = if unencoded { KEY_ENCODER } else { KEY_WRITER };
                match super::keyed::acquire(mutex, key, self.stage_timeout_ms) {
                    Ok(super::keyed::Acquired::Owned) => {}
                    // not owned: neither the texture nor `ReleaseSync` may be touched. The
                    // encoder released a free slot before it was handed out, so a timeout here
                    // is a GPU or driver hang: quarantine the slot, or the next frame blocks on it
                    outcome => {
                        drop(cursor);
                        self.quarantine(
                            slot,
                            format_args!("AcquireSync(key {key}) returned {outcome:?}"),
                        );
                        return Ok(false);
                    }
                }

                let staged = unsafe {
                    let r = match self.scaler.as_mut() {
                        Some(s) => s.scale_into(raw, dst),
                        None => {
                            self.igpu_context.CopyResource(dst, raw);
                            Ok(())
                        }
                    };
                    // after the scale, so the pointer is drawn at destination resolution and is
                    // not resampled; before the flush, so it costs no extra submit
                    if let Some(c) = cursor.as_mut() {
                        c.compose_fresh(slot);
                    }
                    self.igpu_context.Flush();
                    r
                };
                let released = unsafe { mutex.ReleaseSync(KEY_ENCODER) };

                if let Err(e) = staged {
                    teprintln!(
                        "ring stage copy failed (slot={slot}): {e:?}; encoding stale slot to recycle"
                    );
                }
                if let Err(e) = released {
                    // the slot is still held at `KEY_WRITER` by this device and the encoder will
                    // never get it: back in the free pool it would wedge the next capture callback
                    drop(cursor);
                    self.quarantine(slot, format_args!("ReleaseSync(encoder) failed: {e:?}"));
                    return Ok(false);
                }
            }
            // same-adapter: no shared handle, no keyed mutex, nothing that can wedge. The
            // `Flush` stays: it is AMD's own fix for flicker and growing latency on a
            // Desktop Duplication device (AMF issue #92). Do not remove it without a
            // torn-frame soak
            RingTextures::Local { textures } => {
                let dst = &textures[slot];
                let device_lock = self.local_lock.as_ref().map(DeviceLock::enter);
                let staged = unsafe {
                    let r = match self.scaler.as_mut() {
                        Some(s) => s.scale_into(raw, dst),
                        None => {
                            self.igpu_context.CopyResource(dst, raw);
                            Ok(())
                        }
                    };
                    if let Some(c) = cursor.as_mut() {
                        c.compose_fresh(slot);
                    }
                    self.igpu_context.Flush();
                    r
                };
                drop(device_lock);
                if let Err(e) = staged {
                    teprintln!(
                        "ring stage copy failed (slot={slot}): {e:?}; encoding stale slot to recycle"
                    );
                }
            }
        }
        drop(cursor);

        timing.mark_scale_done();
        // counted in `idrs_sent` by the encode thread, from what the encoder actually emitted
        let force_idr = self.frame_index == 0 || self.idr_request.swap(false, Ordering::Relaxed);
        self.frame_index += 1;
        // the RTP timestamp and the repair map's key. The callback's entry, not now: the stage
        // above (cursor lock, acquire, copy, flush) is host latency the client must see
        let capture = if self.switches.stamp_contract {
            instant_at_host_ns(timing.capture_cb_entry_ns)
        } else {
            Instant::now()
        };
        let msg = ReadyMsg {
            slot,
            force_idr,
            capture,
            timing,
        };
        if self.ready.send(msg).is_err() {
            let _ = self.free_slots.send(slot);
            return Err(RingFailed("the encode thread is gone").into());
        }
        Ok(true)
    }

    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.encode_join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AsyncEncodeRing {
    fn drop(&mut self) {
        self.stop();
    }
}

struct RingEncodeArgs<E> {
    encoder: E,
    cursor: Option<Arc<Mutex<CursorOverlay>>>,
    ready_rx: crossbeam_channel::Receiver<ReadyMsg>,
    free_tx: crossbeam_channel::Sender<usize>,
    tx: broadcast::Sender<EncodedFrame>,
    target_bitrate: Arc<AtomicU32>,
    initial_bitrate: u32,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,
    wake_rx: crossbeam_channel::Receiver<()>,
    frame_duration: Duration,
    stop: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    poisoned: Arc<PoisonedSlots>,
    unencoded: Arc<UnencodedSlots>,
    slots: usize,
    telemetry: SharedTelemetry,
    switches: RingSwitches,
}

/// What woke the ring encode thread.
enum RingEvent {
    Frame(ReadyMsg),
    /// `Pipeline::request_idr` or `set_target_bitrate` poked the wake channel.
    Wake,
    /// Nothing arrived within the tick.
    Idle,
    ReadyClosed,
    WakeClosed,
}

/// What the encode thread does after one event.
#[derive(Debug, PartialEq, Eq)]
enum RingStep {
    Continue,
    /// The ring is being torn down normally (its free channel is gone).
    Stop,
    /// The ring cannot go on. The thread stops and the stage starts returning [`RingFailed`].
    Fail(&'static str),
}

/// The encode thread's state and every decision it makes, kept apart from the thread's channel
/// loop so they can be driven one event at a time.
struct RingWorker<E> {
    encoder: E,
    cursor: Option<Arc<Mutex<CursorOverlay>>>,
    ready_rx: crossbeam_channel::Receiver<ReadyMsg>,
    free_tx: crossbeam_channel::Sender<usize>,
    tx: broadcast::Sender<EncodedFrame>,
    target_bitrate: Arc<AtomicU32>,
    current_bitrate: u32,
    bitrate_warned: bool,
    idr_request: Arc<AtomicBool>,
    repair: Arc<crate::streamer::repair::RepairState>,
    poisoned: Arc<PoisonedSlots>,
    unencoded: Arc<UnencodedSlots>,
    slots: usize,
    telemetry: SharedTelemetry,
    switches: RingSwitches,
    /// How long the stream may go without emitting anything.
    keepalive: Duration,
    /// How often the thread wakes when nothing arrives.
    tick: Duration,
    /// Reused, because the encode thread does not allocate per frame.
    lost_frames: Vec<u64>,
    held_slot: Option<usize>,
    /// Paced to the link with more than a frame period waiting at the sender: the newest frame,
    /// staged and not encoded, sent once the backlog has left.
    deferred: Option<ReadyMsg>,
    last_emit: Instant,
    /// Repair work taken from `RepairState` and not yet put on a picture. An idle tick reads the
    /// state too, and a mark it takes there must not be lost because the tick did not emit.
    pending: crate::streamer::repair::RepairAction,
    /// False once the encoder has said it cannot repair at all; from then on nothing is put on a
    /// picture, even what was already pending.
    ltr_active: bool,
    /// Consecutive failed encodes, frames and keepalives together.
    error_streak: u32,
    qp: Option<super::governor::QpControl>,
}

impl<E: RingEngine> RingWorker<E> {
    fn new(args: RingEncodeArgs<E>) -> Self {
        let RingEncodeArgs {
            encoder,
            cursor,
            ready_rx,
            free_tx,
            tx,
            target_bitrate,
            initial_bitrate,
            idr_request,
            repair,
            wake_rx: _,
            frame_duration,
            stop: _,
            failed: _,
            poisoned,
            unencoded,
            slots,
            telemetry,
            switches,
        } = args;

        // zero slots leaves `RepairState::can_repair` false for the session, so the transport
        // keeps asking for keyframes
        repair.set_ltr_slots(encoder.ltr_slots());
        if encoder.ltr_slots() > 0 {
            tprintln!(
                "{}: {} long-term reference slot(s); a lost frame can be repaired without an IDR",
                encoder.name(),
                encoder.ltr_slots()
            );
        }
        // 200 ms unless the host draws the pointer: then the idle tick is the only clock the
        // cursor has (5 Hz at 200 ms), so the keepalive is the frame period. With WGC drawing
        // it (the default) a pointer move is itself a captured frame. Not the full rate always:
        // on a static desktop that is 47 painted fps against 5, with nine times the NVENC
        // submissions, keyed-mutex traffic and socket writes contending with capture
        let keepalive = if host_composites_cursor() {
            frame_duration.clamp(Duration::from_millis(4), Duration::from_millis(200))
        } else {
            Duration::from_millis(200)
        };
        // tick at the frame period so a pending IDR is honoured within one frame on a still desktop
        let tick = frame_duration.clamp(Duration::from_millis(4), keepalive);
        let ltr_active = encoder.ltr_slots() > 0;
        let qp = super::governor::QpControl::from_telemetry(&telemetry);

        Self {
            qp,
            encoder,
            cursor,
            ready_rx,
            free_tx,
            tx,
            target_bitrate,
            current_bitrate: initial_bitrate,
            bitrate_warned: false,
            idr_request,
            repair,
            poisoned,
            unencoded,
            slots,
            telemetry,
            switches,
            keepalive,
            tick,
            lost_frames: Vec::with_capacity(8),
            held_slot: None,
            deferred: None,
            last_emit: Instant::now(),
            pending: Default::default(),
            ltr_active,
            error_streak: 0,
        }
    }

    fn live_slots(&self) -> u32 {
        (self.slots as u32).saturating_sub(self.poisoned.count())
    }

    /// A frame that waited at the sender was superseded before it was sent: skipped there, its
    /// slot back to the stage unencoded. `true` when the stage has gone.
    fn skip_deferred(&mut self, slot: usize) -> bool {
        self.telemetry.link.note_skip();
        self.unencoded.mark(slot);
        self.free_tx.send(slot).is_err()
    }

    /// Hold `slot` for the keepalive and hand the previously held one back to the stage.
    fn hold(&mut self, slot: usize) -> RingStep {
        if let Some(prev) = self.held_slot.replace(slot) {
            if self.free_tx.send(prev).is_err() {
                return RingStep::Stop;
            }
        }
        RingStep::Continue
    }

    fn apply_bitrate(&mut self) {
        apply_pending_bitrate_atomic(
            &mut self.encoder,
            &self.target_bitrate,
            &mut self.current_bitrate,
            &mut self.bitrate_warned,
        );
    }

    /// Everything the transport learned since the last look: lost frames are dropped from the
    /// reference set now, and the action for the next picture is added to `pending`.
    ///
    /// The order matters: drop what the client never received *before* deciding what to predict
    /// from, or the repair frame can reference a picture that is on its way out of the DPB.
    fn collect_repair(&mut self) {
        self.repair.take_lost(&mut self.lost_frames);
        if self.ltr_active && self.encoder.invalidates_references() {
            for i in 0..self.lost_frames.len() {
                if self.encoder.invalidate_reference(self.lost_frames[i])
                    == RefInvalidation::Unsupported
                {
                    // the engine cannot do this at all; stop the transport asking
                    self.repair.set_ltr_slots(0);
                    self.ltr_active = false;
                    break;
                }
            }
        }
        self.lost_frames.clear();
        let action = self.repair.take_action();
        if self.ltr_active {
            self.pending.mark_ltr |= action.mark_ltr;
            if action.use_ltr_bitmap != 0 {
                self.pending.use_ltr_bitmap = action.use_ltr_bitmap;
            }
        } else {
            // no slots, so nothing pending may name one
            self.pending = Default::default();
        }
    }

    /// Put the pending repair on the picture about to be submitted.
    fn apply_pending_repair(&mut self) {
        let action = std::mem::take(&mut self.pending);
        if !self.ltr_active {
            return;
        }
        if action.use_ltr_bitmap != 0 && !self.encoder.use_ltr(action.use_ltr_bitmap) {
            // the encoder no longer holds any slot the repair names (AMF forgets them on every
            // IDR and on a QP re-init), so the loss is answered with a keyframe after all
            self.encoder.request_idr();
        }
        if action.mark_ltr {
            if let Some(slot) = self.encoder.mark_ltr() {
                self.repair
                    .note_marked(slot, self.encoder.next_frame_index());
            }
        }
    }

    /// Counted from the bytes, so every IDR is counted wherever it was decided: the stage, the
    /// keepalive, or the shared flag the encoder reads at submit.
    fn note_emitted(&self, au: &[u8], index: u64) {
        if access_unit_is_idr(au) {
            self.telemetry
                .degradation
                .idrs_sent
                .fetch_add(1, Ordering::Relaxed);
            self.repair.note_idr(index);
        }
        self.telemetry
            .frames_encoded
            .fetch_add(1, Ordering::Relaxed);
    }

    /// An encode (a captured frame or a keepalive) failed.
    ///
    /// The picture may already be in the encoder's reference set while the client never gets it
    /// (NVENC fails in `complete_next` after `nvEncEncodePicture` accepted it, AMF after
    /// `SubmitInput`), so the next picture is a keyframe; the RTP stream has no gap for the
    /// client to notice otherwise. A removed device ends the ring; so do
    /// `MAX_TRANSIENT_ENCODE_DROPS` failures in a row (the classic path's bound) when
    /// `ring_give_up_on_error_streak` is on. Otherwise the ring keeps retrying, and the log is
    /// rate-limited to powers of two.
    fn encode_failed(&mut self, what: &str, slot: usize, e: &anyhow::Error) -> RingStep {
        self.error_streak += 1;
        self.telemetry
            .degradation
            .encoder_transient_errors
            .fetch_add(1, Ordering::Relaxed);
        self.encoder.request_idr();
        let device_lost = self.encoder.device_lost();
        if self.error_streak.is_power_of_two() || device_lost {
            teprintln!(
                "{}: ring {what} encode failed (slot={slot}, {} in a row{}): {e:?}",
                self.encoder.name(),
                self.error_streak,
                if device_lost {
                    ", encoder device removed"
                } else {
                    ""
                }
            );
        }
        if device_lost {
            return RingStep::Fail("the encoder's device was removed");
        }
        if self.switches.give_up_on_streak && self.error_streak >= MAX_TRANSIENT_ENCODE_DROPS {
            return RingStep::Fail("the encoder failed every frame for too long");
        }
        RingStep::Continue
    }

    /// A captured frame from the stage.
    fn on_frame(&mut self, mut msg: ReadyMsg) -> RingStep {
        if self.poisoned.contains(msg.slot) {
            // the producer no longer hands these out, so this can only be a message already in
            // flight when the slot was quarantined
            self.telemetry
                .degradation
                .frames_dropped_ring
                .fetch_add(1, Ordering::Relaxed);
            return RingStep::Continue;
        }
        if self.switches.latest_wins {
            // encode the newest frame waiting, not the oldest, as the client resynchronises to
            // the freshest picture. An older frame goes back unencoded, keeping its IDR demand
            while let Ok(newer) = self.ready_rx.try_recv() {
                self.telemetry
                    .degradation
                    .frames_dropped_ring
                    .fetch_add(1, Ordering::Relaxed);
                if self.poisoned.contains(newer.slot) {
                    continue;
                }
                let older = std::mem::replace(&mut msg, newer);
                msg.force_idr |= older.force_idr;
                self.unencoded.mark(older.slot);
                if self.free_tx.send(older.slot).is_err() {
                    return RingStep::Stop;
                }
            }
        }
        // one load while not paced
        if self.qp.is_some() {
            if self.telemetry.link.sender_busy() {
                if let Some(older) = self.deferred.replace(msg) {
                    if let Some(newest) = self.deferred.as_mut() {
                        newest.force_idr |= older.force_idr;
                    }
                    if self.skip_deferred(older.slot) {
                        return RingStep::Stop;
                    }
                }
                return RingStep::Continue;
            }
            if let Some(older) = self.deferred.take() {
                msg.force_idr |= older.force_idr;
                if self.skip_deferred(older.slot) {
                    return RingStep::Stop;
                }
            }
        }
        let mut timing = msg.timing;
        timing.mark_encode_submit();

        self.apply_bitrate();
        self.collect_repair();
        self.apply_pending_repair();
        // the QP for this picture, once repair has had its say on what kind of picture it is
        if let Some(g) = self.qp.as_mut() {
            if self.encoder.next_frame_index() != 0 {
                msg.force_idr = g.admit_keyframe(msg.force_idr);
            }
            g.keyframe_next(msg.force_idr || self.idr_request.load(Ordering::Relaxed));
            g.before_submit(&mut self.encoder);
        }
        let submitted_index = self.encoder.next_frame_index();
        if self.switches.latest_wins {
            // the held slot goes back before the encode rather than after, so the capture side
            // has a second free slot meanwhile. Nothing else touches it during an encode
            if let Some(prev) = self.held_slot.take() {
                if self.free_tx.send(prev).is_err() {
                    return RingStep::Stop;
                }
            }
        }
        match self.encoder.encode_slot(msg.slot, msg.force_idr) {
            Ok(au) => {
                self.error_streak = 0;
                timing.mark_encode_done();
                // a zero-length access unit (AMF only) is a dropped frame: the slot is recycled,
                // and nothing is recorded that the transport could then acknowledge
                if !au.is_empty() {
                    self.repair.note_encoded(
                        crate::streamer::audio::host_instant_to_ns(msg.capture),
                        submitted_index,
                    );
                    self.note_emitted(&au, submitted_index);
                    let data = Bytes::from(au);
                    let record = self.qp.is_some().then(|| data.clone());
                    let _ = self.tx.send(EncodedFrame {
                        data,
                        capture: msg.capture,
                        timing,
                    });
                    if let (Some(g), Some(au)) = (self.qp.as_mut(), record) {
                        g.emitted(&au, false);
                    }
                    self.last_emit = Instant::now();
                }
                self.hold(msg.slot)
            }
            Err(SlotEncodeError::Poisoned(e)) => {
                // counted as an encoder reset too, for readers of that counter
                self.telemetry
                    .degradation
                    .encoder_resets
                    .fetch_add(1, Ordering::Relaxed);
                self.telemetry
                    .degradation
                    .ring_slot_quarantines
                    .fetch_add(1, Ordering::Relaxed);
                teprintln!(
                    "ring encode: slot {} poisoned (AcquireSync failed - GPU hang or driver fault): {e:?}",
                    msg.slot
                );
                self.poisoned.mark(msg.slot);
                if self.held_slot.is_some_and(|s| self.poisoned.contains(s)) {
                    self.held_slot = None;
                }
                // a ring with one usable slot can never stage again: the encode thread holds it.
                // Anything more keeps going, held slot or not: the next frame provides one
                if self.live_slots() < 2 {
                    return RingStep::Fail("fewer than two usable slots are left");
                }
                RingStep::Continue
            }
            Err(SlotEncodeError::Encode(e)) => match self.encode_failed("frame", msg.slot, &e) {
                RingStep::Continue => self.hold(msg.slot),
                stop => stop,
            },
        }
    }

    /// A wake (an IDR or repair request, a bitrate change) or an idle tick.
    fn on_idle(&mut self, now: Instant) -> RingStep {
        if self.switches.yield_to_frame && !self.ready_rx.is_empty() {
            // a fresher captured frame is waiting; it takes any IDR and repair itself
            return RingStep::Continue;
        }
        if self.qp.is_some() {
            // a frame waiting at the sender goes first, once the backlog has left; until then
            // nothing else does either
            if self.telemetry.link.sender_busy() {
                return RingStep::Continue;
            }
            if let Some(msg) = self.deferred.take() {
                return self.on_frame(msg);
            }
        }
        let Some(slot) = self.held_slot else {
            return RingStep::Continue;
        };
        if self.poisoned.contains(slot) {
            self.held_slot = None;
            return RingStep::Continue;
        }
        // the pointer moved and no frame arrived to carry it: on an indirect display the
        // compositor produces none for pointer-only motion. Redraw it on the held slot and
        // send that as a real frame (a P-frame covering a 32 px square, a few hundred bytes)
        let pointer_read_at = Instant::now();
        let cursor_moved = self
            .cursor
            .as_ref()
            .is_some_and(|c| c.lock_recover().recompose(slot));
        // a repair taken instead of a keyframe must reach a picture even on a still desktop:
        // until it does the client stays broken and every later PLI is taken as another repair
        self.collect_repair();
        let repair_due = self.pending.use_ltr_bitmap != 0;
        // read, then claim only if this tick will emit: a `swap` before the early return below
        // loses the keyframe request, and a client that never gets its keyframe gives up on
        // WebCodecs and demotes itself to a `<video>` element
        // a keyframe request the governor held is carried by the first tick it may go on
        if !self.idr_request.load(Ordering::Relaxed)
            && !cursor_moved
            && !repair_due
            && now.saturating_duration_since(self.last_emit) < self.keepalive
            && !self.qp.as_ref().is_some_and(|g| g.keyframe_due())
        {
            return RingStep::Continue;
        }
        let mut idr_pending = self.idr_request.swap(false, Ordering::Relaxed);
        self.apply_bitrate();
        self.apply_pending_repair();
        if let Some(g) = self.qp.as_mut() {
            idr_pending = g.admit_keyframe(idr_pending);
            g.keyframe_next(idr_pending);
            g.before_submit(&mut self.encoder);
        }
        let step = self.keepalive(slot, idr_pending, cursor_moved, pointer_read_at);
        self.last_emit = Instant::now();
        step
    }

    /// A re-encode of the held slot, emitted when no new frame has arrived.
    ///
    /// This is the only place a keyframe request can land on an idle desktop: both ring encoders
    /// are configured with an infinite GOP and IDR period, and `AsyncEncodeRing::stage`, the only
    /// other reader of `idr_request`, runs only when WGC delivers a new frame.
    ///
    /// `cursor_frame` says the slot's pixels changed since the last emit (the pointer was redrawn
    /// on it), so this is a real frame, not a repeat. It is counted separately and keeps its
    /// capture stamps: the span from reading the pointer to the client painting it is the
    /// cursor latency.
    fn keepalive(
        &mut self,
        slot: usize,
        force_idr: bool,
        cursor_frame: bool,
        pointer_read_at: Instant,
    ) -> RingStep {
        if force_idr {
            self.encoder.request_idr();
        }
        // stamped around the encode, not after it, or every keepalive records a ~0 us encode
        // and a cursor frame reads as captured after it was encoded
        let mut timing = if cursor_frame {
            FrameTiming::entering_capture()
        } else {
            FrameTiming::repeating()
        };
        if cursor_frame && self.switches.stamp_contract {
            timing.capture_cb_entry_ns =
                crate::streamer::audio::host_instant_to_ns(pointer_read_at);
        }
        timing.mark_encode_submit();
        let submitted_at = Instant::now();
        let submitted_index = self.encoder.next_frame_index();
        match self.encoder.encode_repeat_slot(slot) {
            // no bytes means a dropped frame (AMF only); never packetize a payloadless access unit
            Ok(au) if au.is_empty() => {
                self.error_streak = 0;
                RingStep::Continue
            }
            Ok(au) => {
                self.error_streak = 0;
                timing.mark_encode_done();
                let d = &self.telemetry.degradation;
                if cursor_frame {
                    d.cursor_frames.fetch_add(1, Ordering::Relaxed);
                } else {
                    d.frames_repeated.fetch_add(1, Ordering::Relaxed);
                }
                // a cursor frame was captured when the pointer was read; a repeat is stamped at
                // its own submit, so a frame staged while it encoded is not stamped behind it
                let capture = match (self.switches.stamp_contract, cursor_frame) {
                    (true, true) => pointer_read_at,
                    (true, false) => submitted_at,
                    (false, _) => Instant::now(),
                };
                // this picture consumed an encoder frame index like any other, and the next P
                // frame may predict from it, so its loss has to be repairable too
                self.repair.note_encoded(
                    crate::streamer::audio::host_instant_to_ns(capture),
                    submitted_index,
                );
                self.note_emitted(&au, submitted_index);
                let data = Bytes::from(au);
                let record = self.qp.is_some().then(|| data.clone());
                let _ = self.tx.send(EncodedFrame {
                    data,
                    capture,
                    timing,
                });
                if let (Some(g), Some(au)) = (self.qp.as_mut(), record) {
                    g.emitted(&au, !cursor_frame);
                }
                RingStep::Continue
            }
            Err(e) => self.encode_failed("keepalive", slot, &e),
        }
    }
}

fn ring_encode_thread<E: RingEngine>(args: RingEncodeArgs<E>) {
    let _thread_tuning = tuning::tune_current_thread();
    let stop = Arc::clone(&args.stop);
    let failed = Arc::clone(&args.failed);
    let ready_rx = args.ready_rx.clone();
    let wake_rx = args.wake_rx.clone();
    let mut worker = RingWorker::new(args);
    let encoder_name = worker.encoder.name();
    let tick = worker.tick;
    let mut wake_open = true;

    let failure = loop {
        if stop.load(Ordering::Relaxed) {
            break None;
        }

        let event = if wake_open {
            crossbeam_channel::select! {
                recv(ready_rx) -> msg => match msg {
                    Ok(m) => RingEvent::Frame(m),
                    Err(_) => RingEvent::ReadyClosed,
                },
                recv(wake_rx) -> r => if r.is_ok() { RingEvent::Wake } else { RingEvent::WakeClosed },
                default(tick) => RingEvent::Idle,
            }
        } else {
            match ready_rx.recv_timeout(tick) {
                Ok(m) => RingEvent::Frame(m),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => RingEvent::Idle,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => RingEvent::ReadyClosed,
            }
        };

        let step = match event {
            RingEvent::ReadyClosed => break None,
            RingEvent::WakeClosed => {
                wake_open = false;
                continue;
            }
            RingEvent::Wake | RingEvent::Idle => {
                while wake_rx.try_recv().is_ok() {}
                worker.on_idle(Instant::now())
            }
            RingEvent::Frame(msg) => worker.on_frame(msg),
        };
        match step {
            RingStep::Continue => {}
            RingStep::Stop => break None,
            RingStep::Fail(reason) => break Some(reason),
        }
    };
    if let Some(reason) = failure {
        failed.store(true, Ordering::Relaxed);
        worker
            .telemetry
            .degradation
            .encoder_resets
            .fetch_add(1, Ordering::Relaxed);
        teprintln!(
            "{encoder_name}: ring encode thread giving up: {reason}. No further frame is encoded; \
             every later stage returns RingFailed"
        );
    }
    tprintln!("{encoder_name} encode thread stopped");
}

fn apply_pending_bitrate_atomic<E: RingEngine>(
    encoder: &mut E,
    target_bitrate: &AtomicU32,
    current: &mut u32,
    warned: &mut bool,
) {
    let pending = target_bitrate.swap(0, Ordering::Relaxed);
    if pending == 0 || pending == *current {
        return;
    }
    match encoder.set_bitrate(pending) {
        Ok(BitrateApplied::Applied) => {
            tprintln!("adapting bitrate: {} -> {pending} bps", *current);
            *current = pending;
        }
        Ok(BitrateApplied::IgnoredConstantQp) => {
            warn_constant_qp_once(warned);
            *current = pending;
        }
        Ok(BitrateApplied::Unsupported) => {
            warn_bitrate_fixed_once(warned);
            *current = pending;
        }
        Ok(BitrateApplied::Clamped { applied_bps }) => {
            tprintln!(
                "adapting bitrate: {} -> {pending} bps, held to {applied_bps} bps by the encoder",
                *current
            );
            *current = pending;
        }
        Err(e) => {
            teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; will retry");
            restore_pending_bitrate(target_bitrate, pending);
        }
    }
}

/// Whether this device can run a `VideoProcessorBlt` at all.
///
/// `Scaler::new` casts the device to `ID3D11VideoDevice` and the context to `ID3D11VideoContext`;
/// both casts fail on a device created without `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` or on an
/// adapter with no video engine. Asking first turns a failed session into a degraded one.
fn video_processor_available(device: &ID3D11Device, context: &ID3D11DeviceContext) -> bool {
    device.cast::<ID3D11VideoDevice>().is_ok() && context.cast::<ID3D11VideoContext>().is_ok()
}

/// Whether to run a `VideoProcessorBlt` before handing the frame to the encoder.
///
/// Two independent reasons:
///
/// 1. **Scaling.** The display is not the size the client asked for. `Backend::Intel` never
///    needs it because its VPP fuses the scale into the encode.
/// 2. **Converting.** A CPU-bridge backend that wants NV12 gets a scaler even at identity size:
///    the readback is what costs, and NV12 halves it. At 1920x1080 identity, median per frame
///    (`identity_blt_to_nv12_against_a_bgra_readback`):
///
///    ```text
///    bgra8  blt 11 us  readback 2394 us  total 2405 us  8,294,400 bytes
///    nv12   blt  6 us  readback 1170 us  total 1176 us  3,110,400 bytes
///    ```
fn needs_scaler(backend: &Backend, want_w: u32, want_h: u32, native_w: u32, native_h: u32) -> bool {
    let (scale, convert) = scaler_reasons(backend, want_w, want_h, native_w, native_h);
    scale || convert
}

/// The two reasons of [`needs_scaler`], kept apart: only the first is one a session cannot do
/// without. A conversion-only pass that cannot be built (no `ID3D11VideoDevice`: a VM, the Basic
/// Display adapter, an indirect-display adapter with no video engine) is a slower readback, not a
/// failed session: every CPU-bridge encoder still takes BGRA.
fn scaler_reasons(
    backend: &Backend,
    want_w: u32,
    want_h: u32,
    native_w: u32,
    native_h: u32,
) -> (bool, bool) {
    let scale = backend.wants_prescale() && (want_w != native_w || want_h != native_h);
    let convert = backend.is_cpu_bridge() && backend.cpu_bridge_format() != ReadbackFormat::Bgra8;
    (scale, convert)
}

/// The classic WGC path's scaler.
///
/// x264, the tier for hosts with no hardware encoder, always wants the NV12 conversion pass, and
/// such a host is likely to have no video processor either. When the pass was wanted only to
/// convert, the frame is read back as BGRA instead of failing the session.
fn classic_scaler(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    scale: bool,
    convert: bool,
    native: (u32, u32),
    want: (u32, u32),
    format: ReadbackFormat,
) -> Result<Option<Scaler>> {
    scaler_or_degrade(scale, convert, format, || {
        Scaler::with_format(device, context, native.0, native.1, want.0, want.1, format)
    })
}

/// The decision inside [`classic_scaler`], with the build injected so it can be tested on a
/// machine whose devices all have a video processor: no pass wanted is no scaler; a pass that
/// cannot be built is a BGRA readback when it was only a conversion, and an error when it had to
/// scale.
fn scaler_or_degrade<S>(
    scale: bool,
    convert: bool,
    format: ReadbackFormat,
    build: impl FnOnce() -> Result<S>,
) -> Result<Option<S>> {
    if !scale && !convert {
        return Ok(None);
    }
    match build() {
        Ok(s) => Ok(Some(s)),
        Err(e) if !scale => {
            teprintln!(
                "pipeline: no video processor for the {} conversion pass ({e:#}); reading the \
                 captured BGRA back instead (a larger readback, and the encoder converts)",
                format.name()
            );
            Ok(None)
        }
        Err(e) => Err(e.context("building GPU downscaler for --scale")),
    }
}

/// The scaler's destination format for a given backend.
///
/// Only a CPU-bridge backend reads the scaled surface back, so only it has an opinion. A GPU-path
/// backend receives the blt straight into its own input texture (`scale_into`), and that
/// texture's format is the encoder's business.
fn scaler_output_format(backend: &Backend) -> ReadbackFormat {
    if backend.is_cpu_bridge() {
        backend.cpu_bridge_format()
    } else {
        ReadbackFormat::Bgra8
    }
}

/// One rung of the encoder fallback ladder.
///
/// Named separately from [`Backend`] because a rung is a choice, not a constructed encoder, so
/// the ladder can be asserted on a machine with no GPU at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Nvenc,
    Intel,
    Amf,
    X264,
}

impl BackendKind {
    pub fn name(self) -> &'static str {
        match self {
            BackendKind::Nvenc => "NVENC",
            BackendKind::Intel => "Intel Quick Sync",
            BackendKind::Amf => "AMD AMF",
            BackendKind::X264 => "software x264",
        }
    }
}

/// The encoders [`build_backend`] tries, in order, for a requested vendor.
///
/// This is the order the vendors are attempted in, not the number of attempts: each rung has its
/// own fallback (Quick Sync and AMF try the same-adapter path before their own-device CPU bridge,
/// NVENC tries zero-copy before its CPU bridge).
///
/// `Auto` goes NVIDIA, then AMD, then Intel: a discrete GPU's encoder before the integrated
/// one, and software only when no hardware encoder opens.
pub fn encoder_ladder(vendor: crate::streamer::config::EncoderVendor) -> &'static [BackendKind] {
    use crate::streamer::config::EncoderVendor;

    match vendor {
        EncoderVendor::Software => &[BackendKind::X264],
        EncoderVendor::Nvidia => &[BackendKind::Nvenc, BackendKind::X264],
        EncoderVendor::Intel => &[BackendKind::Intel, BackendKind::X264],
        EncoderVendor::Amd => &[BackendKind::Amf, BackendKind::X264],
        EncoderVendor::Auto => &[
            BackendKind::Nvenc,
            BackendKind::Amf,
            BackendKind::Intel,
            BackendKind::X264,
        ],
    }
}

/// Which half of a rung an attempt covers.
///
/// The ladder is walked twice: every vendor's GPU path (NVENC zero-copy, Quick Sync or AMF on
/// the capture device), then every vendor's CPU bridge. With `encoder_ladder_gpu_paths_first`
/// off a rung tries its GPU path and then its own bridge, which is [`RungTier::Both`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RungTier {
    Both,
    Gpu,
    Bridge,
}

/// The attempts [`build_backend`] makes, in order.
///
/// With `gpu_paths_first` no readback path is tried while a path without one is still untried.
/// Without it this is [`encoder_ladder`] rung by rung, each rung owning its own fallback: that
/// order puts a vendor's CPU bridge (a BGRA readback of ~2.4 ms at 1080p plus an upload, every
/// frame) ahead of the next vendor encoding the captured texture with no readback at all, which
/// is what a hybrid laptop reaches when the cross-adapter zero-copy open fails. x264 has no GPU
/// path.
pub(crate) fn ladder_attempts(
    vendor: crate::streamer::config::EncoderVendor,
    gpu_paths_first: bool,
) -> Vec<(BackendKind, RungTier)> {
    let ladder = encoder_ladder(vendor);
    if !gpu_paths_first {
        return ladder.iter().map(|&k| (k, RungTier::Both)).collect();
    }
    let gpu = ladder
        .iter()
        .filter(|&&k| k != BackendKind::X264)
        .map(|&k| (k, RungTier::Gpu));
    let bridge = ladder.iter().map(|&k| (k, RungTier::Bridge));
    gpu.chain(bridge).collect()
}

/// The NVENC driver refused a session because the system already holds as many as it allows.
///
/// Matched on the text `nvidia::encoder::open_session` attaches to exactly this refusal. Every
/// further NVENC attempt in the same join opens another session against the same cap, so it can
/// only fail again.
fn is_nvenc_session_cap(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("concurrent-session limit")
}

/// An NVIDIA display adapter that is not a software rasteriser exists on this machine.
///
/// Decides whether a failed NVENC ring attempt is a fallback worth counting: on an Intel-only or
/// AMD-only host it is the expected outcome, and counting it would hide a real fallback behind
/// `capture_fallback >= 1` on every session. If the adapters cannot be enumerated the answer is
/// `true`.
fn nvidia_adapter_present() -> bool {
    crate::windows_capture::d3d11::enumerate_adapters(None)
        .map(|adapters| {
            adapters
                .iter()
                .any(|(_, info)| info.vendor_id == super::NVIDIA_PCI_VENDOR_ID && !info.software)
        })
        .unwrap_or(true)
}

pub(crate) fn build_backend(
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
) -> Result<Backend> {
    build_backend_with(config, vendor, native_w, native_h, device, context, false)
}

/// [`build_backend`], told whether this join has already met the NVENC session cap.
fn build_backend_with(
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    nvenc_capped_already: bool,
) -> Result<Backend> {
    let gpu_paths_first = super::tune::on("encoder_ladder_gpu_paths_first");
    let intel_retry = super::tune::candidate("intel_same_adapter_retry");
    let nvenc_capped = std::cell::Cell::new(nvenc_capped_already);

    let nvenc_gpu = || -> Result<Backend> {
        let (encoder, path) = build_zero_copy(config, device, context)?;
        tprintln!(
            "pipeline: live capture ready -- NVENC ZERO-COPY cross-adapter GPU path ({}x{}@{})",
            config.width,
            config.height,
            config.fps
        );
        Ok(Backend::Nvenc { encoder, path })
    };
    let nvenc_bridge = || -> Result<Backend> {
        let encoder = Encoder::new(config)?;
        tprintln!(
            "pipeline: live capture ready -- NVENC CPU-bridge fallback ({}x{}@{})",
            config.width,
            config.height,
            config.fps
        );
        Ok(Backend::Nvenc {
            encoder,
            path: EncodePath::CpuBridge,
        })
    };
    let try_nvenc = |tier: RungTier| -> Result<Backend> {
        if nvenc_capped.get() {
            bail!(
                "NVENC not tried again: this join already met the driver's concurrent-session \
                 limit"
            );
        }
        let gpu = match tier {
            RungTier::Bridge => return nvenc_bridge(),
            RungTier::Gpu | RungTier::Both => nvenc_gpu(),
        };
        match gpu {
            Ok(backend) => Ok(backend),
            Err(e) if is_nvenc_session_cap(&e) => {
                nvenc_capped.set(true);
                Err(e)
            }
            Err(e) if tier == RungTier::Both => {
                teprintln!(
                    "NVENC zero-copy path unavailable ({e:?}); falling back to CPU bridge (higher latency)"
                );
                nvenc_bridge()
            }
            Err(e) => Err(e),
        }
    };

    let intel_gpu = || -> Result<Backend> {
        let opened = match IntelEncoder::new_on_device(config, native_w, native_h, device, context)
        {
            Err(e) if intel_retry => {
                teprintln!(
                    "Intel Quick Sync same-adapter path failed ({e:?}); trying it once more \
                     before the CPU bridge"
                );
                IntelEncoder::new_on_device(config, native_w, native_h, device, context)
            }
            other => other,
        };
        let encoder = opened?;
        tprintln!(
            "pipeline: live capture ready -- INTEL Quick Sync same-adapter path ({}x{}@{})",
            config.width,
            config.height,
            config.fps
        );
        Ok(Backend::Intel { encoder })
    };
    let intel_bridge = || -> Result<Backend> {
        let encoder = IntelEncoder::new(config)?;
        tprintln!(
            "pipeline: live capture ready -- INTEL Quick Sync CPU-bridge path ({}x{}@{})",
            config.width,
            config.height,
            config.fps
        );
        Ok(Backend::IntelCpu { encoder })
    };
    let try_intel = |tier: RungTier| -> Result<Backend> {
        match tier {
            RungTier::Gpu => intel_gpu(),
            RungTier::Bridge => intel_bridge(),
            RungTier::Both => match intel_gpu() {
                Ok(backend) => Ok(backend),
                Err(e) => {
                    teprintln!(
                        "Intel Quick Sync same-adapter path unavailable ({e:?}); trying own-device CPU bridge"
                    );
                    intel_bridge().context(
                        "Intel Quick Sync unavailable (same-adapter and CPU-bridge both failed)",
                    )
                }
            },
        }
    };

    let amf_gpu = || -> Result<Backend> {
        let encoder = AmfEncoder::new_on_device(config, device, context)?;
        tprintln!(
            "pipeline: live capture ready -- AMD AMF same-adapter GPU path ({}x{}@{})",
            config.width,
            config.height,
            config.fps
        );
        Ok(Backend::Amf { encoder })
    };
    let amf_bridge = || -> Result<Backend> {
        // the bridge is fed by the NV12 scaler, so an NV12 input ring skips the second Init the
        // first frame would pay; on a BGRA readback (no video processor) it re-initializes
        let encoder = AmfEncoder::new_with_input_format(
            config,
            windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12,
        )?;
        tprintln!(
            "pipeline: live capture ready -- AMD AMF CPU-bridge path ({}x{}@{})",
            config.width,
            config.height,
            config.fps
        );
        Ok(Backend::AmfCpu { encoder })
    };
    let try_amf = |tier: RungTier| -> Result<Backend> {
        match tier {
            RungTier::Gpu => amf_gpu(),
            RungTier::Bridge => amf_bridge(),
            RungTier::Both => match amf_gpu() {
                Ok(backend) => Ok(backend),
                Err(e) => {
                    teprintln!(
                        "AMD AMF same-adapter path unavailable ({e:?}); trying own-device CPU bridge"
                    );
                    amf_bridge()
                        .context("AMD AMF unavailable (same-adapter and CPU-bridge both failed)")
                }
            },
        }
    };

    let try_x264 = || -> Result<Backend> {
        match X264Encoder::new(config) {
            Ok(encoder) => {
                tprintln!(
                    "pipeline: live capture ready -- SOFTWARE x264 CPU path ({}x{}@{})",
                    config.width,
                    config.height,
                    config.fps
                );
                Ok(Backend::X264 { encoder })
            }
            Err(e) => Err(e.context("software x264 fallback unavailable")),
        }
    };

    let tier_name = |kind: BackendKind, tier: RungTier| -> String {
        match tier {
            RungTier::Both => kind.name().to_string(),
            RungTier::Gpu => format!("{} (GPU path)", kind.name()),
            RungTier::Bridge => format!("{} (CPU bridge)", kind.name()),
        }
    };
    let ladder = ladder_attempts(vendor, gpu_paths_first);
    let mut failure: Option<anyhow::Error> = None;
    for (i, (kind, tier)) in ladder.iter().copied().enumerate() {
        if let Some(previous) = failure.as_ref() {
            let (prev_kind, prev_tier) = ladder[i - 1];
            teprintln!(
                "{} unavailable ({previous:?}); trying {}",
                tier_name(prev_kind, prev_tier),
                tier_name(kind, tier)
            );
        }
        let attempt = match kind {
            BackendKind::Nvenc => try_nvenc(tier),
            BackendKind::Intel => try_intel(tier),
            BackendKind::Amf => try_amf(tier),
            BackendKind::X264 => try_x264(),
        };
        match attempt {
            Ok(backend) => return Ok(backend),
            Err(e) => {
                failure = Some(match failure {
                    Some(previous) => {
                        previous.context(format!("{} also failed: {e:?}", tier_name(kind, tier)))
                    }
                    None => e,
                });
            }
        }
    }
    Err(failure.expect("encoder_ladder is never empty"))
}

/// How many idle-repeat failures in a row are logged one by one before the log goes quiet (it
/// then reports every [`REPEAT_ERROR_LOG_EVERY`]th).
const REPEAT_ERROR_LOG_EVERY: u32 = 50;

/// The classic path's keepalive. The classic path is exactly the non-NVENC-ring backends (Quick
/// Sync, AMF, x264, and NVENC only when its ring could not be built).
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_repeater(
    core: Arc<Mutex<EncodeCore>>,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    frame_duration: Duration,
    last_frame_at: Arc<AtomicU64>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    wake_rx: crossbeam_channel::Receiver<()>,
    telemetry: SharedTelemetry,
) {
    spawn_named_repeater(
        core,
        tx,
        idr_request,
        frame_duration,
        last_frame_at,
        epoch,
        stop,
        wake_rx,
        telemetry,
        "classic",
        None,
        None,
    );
}

/// How long until what waits at the sender has left at the pace.
fn backlog_left(telemetry: &SessionTelemetry) -> Duration {
    let now = crate::streamer::audio::host_now_ns();
    Duration::from_nanos(telemetry.link.busy_until_ns().saturating_sub(now))
}

/// The classic path's held copy, once its deadline has passed and no newer frame is on its way to
/// the encoder: encoded and sent as a captured frame. `true` when one was sent.
#[allow(clippy::too_many_arguments)]
fn deliver_held(
    core: &Mutex<EncodeCore>,
    held: &ClassicHold,
    tx: &broadcast::Sender<EncodedFrame>,
    telemetry: &SessionTelemetry,
    last_frame_at: &AtomicU64,
    epoch: Instant,
    arriving: &dyn Fn() -> bool,
    failures: &mut u32,
) -> bool {
    let due = held.due_ns.load(Ordering::Relaxed);
    let now = epoch.elapsed().as_nanos() as u64;
    if due == 0 || now < due || arriving() {
        return false;
    }
    if telemetry.link.sender_busy() {
        let later = now + backlog_left(telemetry).as_nanos() as u64;
        let _ =
            held.due_ns
                .compare_exchange(due, later.max(1), Ordering::Relaxed, Ordering::Relaxed);
        return false;
    }
    let (au, capture, mut timing) = {
        let Ok(mut core) = core.lock() else {
            return false;
        };
        if arriving() {
            return false;
        }
        // zeroed under the lock the callback sets it under, so a newer deadline is never lost
        held.due_ns.store(0, Ordering::Relaxed);
        let mut timing = FrameTiming::entering_capture();
        timing.mark_encode_submit();
        match core.encode_held() {
            Some(Ok((au, capture))) if !au.is_empty() => {
                timing.capture_cb_entry_ns = crate::streamer::audio::host_instant_to_ns(capture);
                (au, capture, timing)
            }
            Some(Err(e)) => {
                *failures = failures.saturating_add(1);
                telemetry
                    .degradation
                    .encoder_transient_errors
                    .fetch_add(1, Ordering::Relaxed);
                if failures.is_power_of_two() {
                    teprintln!("held frame encode failed ({failures} so far): {e:#}");
                }
                return false;
            }
            _ => return false,
        }
    };
    *failures = 0;
    timing.mark_encode_done();
    last_frame_at.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
    held.delivered_ns.store(
        (capture.saturating_duration_since(epoch).as_nanos() as u64).max(1),
        Ordering::Relaxed,
    );
    telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
    let data = Bytes::from(au);
    let record = data.clone();
    let _ = tx.send(EncodedFrame {
        data,
        capture,
        timing,
    });
    record_sent(core, Some(record), false);
    true
}

/// [`spawn_repeater`] with the backend's name for the thread and, on the WGC classic path, the
/// flag a captured frame raises on its way to the encoder and the held copy's deadline.
#[allow(clippy::too_many_arguments)]
fn spawn_named_repeater(
    core: Arc<Mutex<EncodeCore>>,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    frame_duration: Duration,
    last_frame_at: Arc<AtomicU64>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    wake_rx: crossbeam_channel::Receiver<()>,
    telemetry: SharedTelemetry,
    path_name: &'static str,
    arriving: Option<Arc<AtomicBool>>,
    held: Option<Arc<ClassicHold>>,
) {
    let tick = frame_duration.max(Duration::from_millis(8));
    let idle_after_ms = (frame_duration.as_millis() as u64 * 2).max(34);
    let keepalive_ms = 200u64;
    // `classic_repeater_yields`: a captured frame already on its way to the encoder wins, since
    // a repeat holds the encode lock for a whole encode (4-7 ms on Quick Sync). Checked before
    // the lock is taken and again once it is held
    //
    // `repeater_error_backoff`: a failed repeat is retried at the keepalive cadence, not on the
    // next tick. This thread serves the DXGI path too, so the switch covers both callers
    let error_backoff = super::tune::on("repeater_error_backoff");
    let governed = core.lock_recover().governed();
    std::thread::Builder::new()
        .name(format!("{path_name}-repeat"))
        .spawn(move || {
            let frame_arriving = || arriving.as_ref().is_some_and(|a| a.load(Ordering::Relaxed));
            let _thread_tuning = tuning::tune_timer_thread();
            let mut last_emit = Instant::now();
            let mut failures = 0u32;
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // a held copy due before the next tick is woken for
                let wait = held
                    .as_ref()
                    .map(|h| h.due_ns.load(Ordering::Relaxed))
                    .filter(|due| *due != 0)
                    .map_or(tick, |due| {
                        let now = epoch.elapsed().as_nanos() as u64;
                        Duration::from_nanos(due.saturating_sub(now)).min(tick)
                    });
                match wake_rx.recv_timeout(wait) {
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(wait);
                    }
                }
                while wake_rx.try_recv().is_ok() {}
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Some(h) = held.as_ref() {
                    if deliver_held(
                        &core,
                        h,
                        &tx,
                        &telemetry,
                        &last_frame_at,
                        epoch,
                        &frame_arriving,
                        &mut failures,
                    ) {
                        last_emit = Instant::now();
                        continue;
                    }
                }
                let now_ms = epoch.elapsed().as_millis() as u64;
                let last = last_frame_at.load(Ordering::Relaxed);
                if last != 0 && now_ms.saturating_sub(last) < idle_after_ms {
                    continue;
                }
                let idr_pending = idr_request.load(Ordering::Relaxed);
                let keepalive_due = last_emit.elapsed().as_millis() as u64 >= keepalive_ms;
                if !idr_pending && !keepalive_due {
                    continue;
                }
                // nothing goes behind a backlog that is waiting at the sender
                if held.is_some() && telemetry.link.sender_busy() {
                    continue;
                }
                if frame_arriving() {
                    continue;
                }
                let capture = Instant::now();
                let mut timing = FrameTiming::repeating();
                timing.mark_encode_submit();
                let au = {
                    let mut core = match core.lock() {
                        Ok(g) => g,
                        Err(_) => {
                            teprintln!(
                                "idle repeater stopping: the encode core mutex is poisoned (a \
                                 capture callback panicked while encoding)"
                            );
                            break;
                        }
                    };
                    if frame_arriving() {
                        continue;
                    }
                    match core.encode_repeat() {
                        // an empty access unit is a dropped frame, see `on_frame_arrived`
                        Ok(Some(au)) if !au.is_empty() => au,
                        Ok(Some(_)) | Ok(None) => continue,
                        Err(e) => {
                            // log-limited so a persistent failure cannot flood the log bus;
                            // `repeater_error_backoff` also retries at the keepalive cadence
                            failures = failures.saturating_add(1);
                            telemetry
                                .degradation
                                .encoder_transient_errors
                                .fetch_add(1, Ordering::Relaxed);
                            if failures <= 3 || failures.is_multiple_of(REPEAT_ERROR_LOG_EVERY) {
                                teprintln!(
                                    "idle repeat encode failed ({failures} in a row): {e:?}"
                                );
                            }
                            if error_backoff {
                                last_emit = Instant::now();
                            }
                            continue;
                        }
                    }
                };
                failures = 0;
                timing.mark_encode_done();
                telemetry
                    .degradation
                    .frames_repeated
                    .fetch_add(1, Ordering::Relaxed);
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let data = Bytes::from(au);
                let record = governed.then(|| data.clone());
                let _ = tx.send(EncodedFrame {
                    data,
                    capture,
                    timing,
                });
                record_sent(&core, record, true);
                last_emit = Instant::now();
            }
            tprintln!("idle repeater stopped");
        })
        .expect("spawn repeater thread");
}

impl GraphicsCaptureApiHandler for LiveCapture {
    type Flags = CaptureFlags;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureFlags {
            config,
            vendor,
            native_w,
            native_h,
            monitor_handle,
            auto_frame_rate,
            tx,
            idr_request,
            repair,
            target_bitrate,
            wake_rx,
            telemetry,
        } = ctx.flags;
        super::governor::arm(&telemetry, &config);

        // MMCSS, `TIME_CRITICAL`, P-core confinement and keep-awake are per thread, and the
        // handler is dropped on whichever thread stops the session, where
        // `AvRevertMmThreadCharacteristics` fails by design: park them in a thread-local here
        park_on_capture_thread(tuning::tune_current_thread(), tuning::KeepAwake::begin());
        let features = wgc_features();
        let free_threaded =
            features.free_threaded_pool && super::tune::candidate("wgc_free_threaded_pool");
        // on the NVENC ring the capture device is a different GPU from the encoder's, and it is
        // where the stage copy or scale and its `Flush` run inside the callback
        if super::tune::candidate("wgc_capture_gpu_priority") {
            tuning::raise_d3d11_gpu_priority(&ctx.device);
        }

        let epoch = Instant::now();
        let last_frame_at = Arc::new(AtomicU64::new(0));
        let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);
        let stop = Arc::new(AtomicBool::new(false));

        use crate::streamer::config::EncoderVendor;
        const RING: usize = 3;
        // AMF's ring is tried for `Amd`, and for `Auto` only when the capture device is itself
        // AMD: on a hybrid box that is the iGPU, and the ladder's order (NVENC, AMF, Quick Sync)
        // applies
        let capture_is_amd = super::device_vendor(&ctx.device) == super::Vendor::Amd;
        let try_nvenc_ring = matches!(vendor, EncoderVendor::Nvidia | EncoderVendor::Auto);
        let try_amf_ring = matches!(vendor, EncoderVendor::Amd)
            || (matches!(vendor, EncoderVendor::Auto) && capture_is_amd);

        // the ring encoder builds its own input textures, so a `--scale` prescaler blits
        // straight into the slot rather than through a staging texture
        let ring_scaler = || -> Result<Option<Scaler>> {
            if config.width == native_w && config.height == native_h {
                return Ok(None);
            }
            Scaler::new(
                &ctx.device,
                &ctx.device_context,
                native_w,
                native_h,
                config.width,
                config.height,
            )
            .map(Some)
            .map_err(|e| e.context("building GPU downscaler for --scale"))
        };

        let mut nvenc_capped = false;
        let ring_built: Option<(RingEncoder, RingTextures)> = if try_nvenc_ring {
            match build_zero_copy_ring(config, &ctx.device, RING) {
                Ok((encoder, pairs)) => {
                    let (textures, mutexes): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
                    Some((
                        RingEncoder::Nvenc(encoder),
                        RingTextures::Shared { textures, mutexes },
                    ))
                }
                Err(e) => {
                    nvenc_capped = is_nvenc_session_cap(&e);
                    // a degradation only where NVENC could have worked: on a host with no NVIDIA
                    // adapter, `Auto` reaching for the ring first is the ladder's order
                    if nvidia_adapter_present() {
                        telemetry
                            .degradation
                            .capture_fallback
                            .fetch_add(1, Ordering::Relaxed);
                        teprintln!(
                            "NVENC zero-copy ring unavailable ({e:?}); falling back to classic backend path"
                        );
                    } else {
                        tprintln!(
                            "NVENC ring not available on this host (no NVIDIA adapter): {e:#}"
                        );
                    }
                    None
                }
            }
        } else {
            None
        };

        let ring_built = match ring_built {
            Some(built) => Some(built),
            None if try_amf_ring => {
                match build_amf_ring(config, &ctx.device, &ctx.device_context, RING) {
                    Ok((encoder, textures)) => {
                        Some((RingEncoder::Amf(encoder), RingTextures::Local { textures }))
                    }
                    Err(e) => {
                        telemetry
                            .degradation
                            .capture_fallback
                            .fetch_add(1, Ordering::Relaxed);
                        teprintln!(
                            "AMD AMF ring unavailable ({e:?}); falling back to classic backend path"
                        );
                        None
                    }
                }
            }
            None => None,
        };

        if let Some((encoder, textures)) = ring_built {
            tuning::raise_d3d11_gpu_priority(encoder.device());
            let scaler = ring_scaler()?;
            let path_name = encoder.name();

            // the overlay needs a render-target view on the encoder's own input textures, which
            // only the ring has, so the tier is probed here by building it; if it does not build,
            // WGC keeps compositing the pointer (stale, but never missing). `cursor_toggle`
            // first: without `IsCursorCaptureEnabled` (below Windows 10 2004) `WithoutCursor` is
            // skipped, not failed, and WGC's pointer would be drawn under a second one
            let cursor = if !host_composites_cursor() {
                tprintln!(
                    "cursor: composited by WGC. A pointer move is a change the compositor tracks, \
                     so it produces a frame by itself: measured 85 fps for a pointer sweep on an \
                     otherwise static indirect display, against 60 with the host drawing it"
                );
                None
            } else if !features.cursor_toggle {
                tprintln!(
                    "cursor: left to WGC. This build has no IsCursorCaptureEnabled, so its \
                     pointer cannot be turned off and drawing another would double it"
                );
                None
            } else {
                match CursorOverlay::new(
                    &ctx.device,
                    &ctx.device_context,
                    textures.slots(),
                    textures.keys(),
                    monitor_handle,
                    (native_w, native_h),
                    (config.width, config.height),
                    Arc::clone(&telemetry),
                ) {
                    Ok(overlay) => {
                        tprintln!(
                            "cursor: composited by the host on {} ring slots; WGC asked for the \
                             desktop without a pointer. {}",
                            textures.len(),
                            overlay.describe()
                        );
                        Some(Arc::new(Mutex::new(overlay)))
                    }
                    Err(e) => {
                        teprintln!(
                            "cursor: host compositing unavailable ({e:#}); leaving the pointer to \
                             WGC, which does not redraw it on a static indirect display"
                        );
                        None
                    }
                }
            };
            let cursor_composited = cursor.is_some();

            super::report_adapter_pairing(&ctx.adapter, Some(encoder.device()), monitor_handle);
            telemetry.encoder.set(path_name.into());
            note_started_backend(path_name);
            let trailing_idr = Arc::clone(&idr_request);
            let ring = AsyncEncodeRing::new(
                encoder,
                textures,
                ctx.device_context.clone(),
                scaler,
                cursor,
                tx.clone(),
                Arc::clone(&target_bitrate),
                config.bitrate_bps,
                Arc::clone(&last_frame_at),
                Arc::clone(&idr_request),
                Arc::clone(&repair),
                wake_rx,
                frame_duration,
                epoch,
                Arc::clone(&telemetry),
            );

            tprintln!(
                "pipeline: live capture ready -- {path_name} RING (K={RING}) decoupled path ({}x{}@{})",
                config.width,
                config.height,
                config.fps
            );

            let trailing = super::tune::on("wgc_trailing_frame")
                .then(|| TrailingEdge::start(&ring, trailing_idr, epoch));

            return Ok(Self {
                core: None,
                governed: false,
                ring: Some(ring),
                tx,
                epoch,
                last_frame_at,
                path_name,
                frames_sent: 0,
                busy_streak: 0,
                stop,
                frame_pool_buffers: FRAME_POOL_BUFFERS_RING,
                stale_frames: Arc::new(AtomicU32::new(0)),
                stale_frames_reported: 0,
                gate: FrameGate::new(
                    frame_duration,
                    auto_frame_rate,
                    features.dirty_region,
                    super::tune::on("wgc_cadence_counts_staged_only"),
                ),
                cursor: CursorProbe::new(monitor_handle),
                cursor_composited,
                trailing,
                classic_hold: None,
                hold_failed: false,
                native_w,
                native_h,
                frame_size: None,
                drop_resized_frames: super::tune::on("wgc_drop_resized_frames"),
                size_change_reported: false,
                arriving: None,
                free_threaded,
                dead: false,
                rebuild: None,
                telemetry,
            });
        }

        let backend = build_backend_with(
            config,
            vendor,
            native_w,
            native_h,
            &ctx.device,
            &ctx.device_context,
            nvenc_capped,
        )?;
        let rebuild = Some(BackendRebuild {
            config,
            vendor,
            native_w,
            native_h,
            device: ctx.device.clone(),
            context: ctx.device_context.clone(),
            budget: RebuildBudget::default(),
        });

        let (scale, convert) =
            scaler_reasons(&backend, config.width, config.height, native_w, native_h);
        let scaler = classic_scaler(
            &ctx.device,
            &ctx.device_context,
            scale,
            convert,
            (native_w, native_h),
            (config.width, config.height),
            scaler_output_format(&backend),
        )?;
        let readback_degraded = convert && scaler.is_none();

        if let Some(dev) = backend.device() {
            tuning::raise_d3d11_gpu_priority(dev);
        }

        super::report_adapter_pairing(&ctx.adapter, backend.device(), monitor_handle);
        let path_name = backend.name();
        if readback_degraded {
            telemetry
                .encoder
                .set(format!("{path_name} (bgra readback: no video processor)"));
        } else {
            telemetry.encoder.set(path_name.into());
        }
        note_started_backend(path_name);
        let core = Arc::new(Mutex::new(EncodeCore {
            backend,
            scaler,
            reader: None,
            staging: None,
            held: None,
            target_bitrate: Arc::clone(&target_bitrate),
            idr_request: Arc::clone(&idr_request),
            current_bitrate: config.bitrate_bps,
            bitrate_warned: false,
            have_frame: false,
            frame_index: 0,
            health: CoreHealth::new(Arc::clone(&telemetry)),
        }));

        let arriving =
            super::tune::on("classic_repeater_yields").then(|| Arc::new(AtomicBool::new(false)));
        let governed = core.lock_recover().governed();
        let classic_hold = governed.then(|| Arc::new(ClassicHold::default()));
        spawn_named_repeater(
            Arc::clone(&core),
            tx.clone(),
            idr_request,
            frame_duration,
            Arc::clone(&last_frame_at),
            epoch,
            Arc::clone(&stop),
            wake_rx,
            Arc::clone(&telemetry),
            path_name,
            arriving.clone(),
            classic_hold.clone(),
        );

        Ok(Self {
            governed,
            core: Some(core),
            ring: None,
            tx,
            epoch,
            last_frame_at,
            path_name,
            frames_sent: 0,
            busy_streak: 0,
            stop,
            frame_pool_buffers: FRAME_POOL_BUFFERS_INLINE,
            stale_frames: Arc::new(AtomicU32::new(0)),
            stale_frames_reported: 0,
            gate: FrameGate::new(
                frame_duration,
                auto_frame_rate,
                features.dirty_region,
                super::tune::on("wgc_cadence_counts_staged_only"),
            ),
            cursor: CursorProbe::new(monitor_handle),
            // the classic path has no slot to draw a pointer onto (it encodes the captured
            // texture in place), so WGC keeps compositing it
            cursor_composited: false,
            trailing: None,
            classic_hold,
            hold_failed: false,
            native_w,
            native_h,
            frame_size: None,
            drop_resized_frames: super::tune::on("wgc_drop_resized_frames"),
            size_change_reported: false,
            arriving,
            free_threaded,
            dead: false,
            rebuild,
            telemetry,
        })
    }

    fn frame_pool_buffers(&self) -> u32 {
        self.frame_pool_buffers
    }

    fn frame_pool_free_threaded(&self) -> bool {
        self.free_threaded
    }

    fn on_frame_pool_error(
        &mut self,
        error: &windows::core::Error,
        consecutive: u32,
    ) -> Result<(), Self::Error> {
        if consecutive <= 3 || consecutive.is_multiple_of(REPEAT_ERROR_LOG_EVERY) {
            teprintln!(
                "wgc: the frame pool signalled a frame it could not hand over ({consecutive} in a \
                 row): {error}"
            );
        }
        if consecutive >= MAX_CONSECUTIVE_POOL_ERRORS {
            let why = format!("the frame pool failed {consecutive} frames in a row ({error})");
            self.mark_dead(&why);
            bail!("{why}");
        }
        Ok(())
    }

    fn stale_frame_counter(&self) -> Option<Arc<AtomicU32>> {
        Some(Arc::clone(&self.stale_frames))
    }

    fn cursor_capture_override(&self) -> Option<CursorCaptureSettings> {
        self.cursor_composited
            .then_some(CursorCaptureSettings::WithoutCursor)
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        if self.dead {
            bail!("the capture has already stopped");
        }
        self.telemetry
            .capture_frames_arrived
            .fetch_add(1, Ordering::Relaxed);
        if self.free_threaded {
            tune_callback_thread();
        }
        if let Some(arriving) = self.arriving.as_ref() {
            arriving.store(true, Ordering::Relaxed);
        }
        let delivered = self.deliver(frame);
        if let Some(arriving) = self.arriving.as_ref() {
            arriving.store(false, Ordering::Relaxed);
        }
        // `Err` halts the capture, but the handler outlives that inside the capture control:
        // without this its repeater and ring would keep emitting repeats of the last picture
        if let Err(e) = delivered.as_ref() {
            self.mark_dead(&format!("{e:#}"));
        }
        delivered
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        teprintln!("pipeline: capture item closed (display disconnected?)");
        self.telemetry
            .degradation
            .capture_lost
            .fetch_add(1, Ordering::Relaxed);
        self.mark_dead("the captured display went away");
        Ok(())
    }
}

impl LiveCapture {
    /// Build the classic backend again after an encoder failure that only a new session can
    /// clear, and drop the frame that failed. True when the new backend is in place; false when
    /// there is nothing to rebuild with, the budget is spent, the build failed, or the ladder
    /// landed on a different backend than the scaler was built for.
    fn rebuild_backend(&mut self, cause: &anyhow::Error) -> bool {
        let (Some(core), Some(rebuild)) = (self.core.as_ref(), self.rebuild.as_mut()) else {
            return false;
        };
        if !rebuild.budget.allow(Instant::now()) {
            teprintln!(
                "pipeline: {} needs a rebuild again ({cause:#}); {REBUILD_LIMIT} rebuilds in {} s \
                 is the limit, so the capture stops",
                self.path_name,
                REBUILD_WINDOW.as_secs()
            );
            return false;
        }
        let Ok(mut core) = core.lock() else {
            return false;
        };
        let built = build_backend(
            rebuild.config,
            rebuild.vendor,
            rebuild.native_w,
            rebuild.native_h,
            &rebuild.device,
            &rebuild.context,
        );
        match built {
            Ok(backend) if backend.name() == core.backend.name() => {
                teprintln!(
                    "pipeline: {} rebuilt after an encoder failure ({cause:#}); the next frame is \
                     a keyframe",
                    self.path_name
                );
                core.backend = backend;
                // a new session starts with an IDR at the configured rate; the pending bitrate
                // target is applied again on the next frame
                core.frame_index = 0;
                core.have_frame = false;
                core.current_bitrate = rebuild.config.bitrate_bps;
                if let Some(g) = core.health.qp.as_mut() {
                    g.encoder_replaced();
                }
                true
            }
            Ok(other) => {
                teprintln!(
                    "pipeline: {} rebuild landed on {} instead; the capture stops",
                    self.path_name,
                    other.name()
                );
                false
            }
            Err(e) => {
                teprintln!("pipeline: {} rebuild failed: {e:#}", self.path_name);
                false
            }
        }
    }

    /// End this capture for good: stop the repeater and the ring (so no keepalive keeps repeating
    /// a picture that will never change again), drop any held trailing frame, and say so once
    /// in the log and in `status --json` (`capture_backend`).
    ///
    /// The fork halts the capture thread on the same paths, so `CaptureControl::is_finished` is
    /// then true; nothing above this layer polls it yet.
    fn mark_dead(&mut self, why: &str) {
        if self.dead {
            return;
        }
        self.dead = true;
        teprintln!(
            "pipeline: {} capture stopped for good: {why}. No further frames or keepalive \
             repeats are sent on this session",
            self.path_name
        );
        self.telemetry
            .capture_backend
            .set(format!("wgc (stopped: {why})"));
        self.telemetry
            .degradation
            .capture_stopped
            .fetch_add(1, Ordering::Relaxed);
        self.stop.store(true, Ordering::Relaxed);
        self.trailing = None;
        if let Some(mut ring) = self.ring.take() {
            ring.stop();
        }
    }

    /// When the trailing-edge timer last delivered a held frame; the gate measures from the
    /// newer of that and its own last delivery.
    fn timer_delivered_at(&self) -> Option<Instant> {
        if let Some(h) = self.classic_hold.as_ref() {
            return match h.delivered_ns.load(Ordering::Relaxed) {
                0 => None,
                ns => Some(self.epoch + Duration::from_nanos(ns)),
            };
        }
        self.trailing
            .as_ref()
            .and_then(|t| t.delivered_at(self.epoch))
    }

    /// The ring cannot make progress any more: say so, so the capture halts once instead of
    /// staging, failing and logging on every frame for the rest of the session.
    fn check_ring_alive(&mut self) -> Result<()> {
        let Some(ring) = self.ring.as_ref() else {
            return Ok(());
        };
        let finished = ring.encode_join.as_ref().is_none_or(|j| j.is_finished());
        if let Some(why) = ring_dead_verdict(finished, ring.poisoned.count(), ring.slots) {
            self.telemetry
                .degradation
                .encoder_resets
                .fetch_add(1, Ordering::Relaxed);
            bail!("{} ring is dead: {why}", self.path_name);
        }
        Ok(())
    }

    /// A frame the cadence gate held back; see [`hold_trailing_frame`].
    fn hold_trailing(
        &mut self,
        frame: &mut Frame,
        timing: FrameTiming,
        capture: Instant,
        last: Instant,
    ) -> Result<()> {
        hold_trailing_frame(
            self.ring.as_mut(),
            self.trailing.as_mut(),
            &self.gate,
            || frame.has_dirty_regions().ok(),
            frame.as_raw_texture(),
            timing,
            capture,
            last,
            &self.telemetry,
        )?;
        self.check_ring_alive()
    }

    /// A frame the classic gate held back while the frame rate is divided: copied, and encoded by
    /// the repeater at the divided cadence's deadline unless a newer frame arrives first. Without
    /// it the last picture of a burst would never reach the client.
    fn hold_classic(
        &mut self,
        frame: &mut Frame,
        capture: Instant,
        deadline: Instant,
        backlog: bool,
    ) -> Result<()> {
        let skipped = |t: &SessionTelemetry| {
            t.degradation
                .frames_skipped_cadence
                .fetch_add(1, Ordering::Relaxed);
        };
        if self.gate.auto_frame_rate
            && self.gate.dirty_region_reporting
            && frame.has_dirty_regions().ok() == Some(false)
        {
            skipped(&self.telemetry);
            return Ok(());
        }
        let (Some(core), Some(h)) = (self.core.as_ref(), self.classic_hold.as_ref()) else {
            skipped(&self.telemetry);
            return Ok(());
        };
        let held = {
            let mut core = core
                .lock()
                .map_err(|_| anyhow!("encode core mutex poisoned; stopping capture"))?;
            let held = core.hold_copy(
                frame.device(),
                frame.device_context(),
                frame.as_raw_texture(),
                capture,
            );
            if held.is_ok() {
                let due = deadline.saturating_duration_since(self.epoch).as_nanos() as u64;
                h.due_ns.store(due.max(1), Ordering::Relaxed);
            }
            held
        };
        match held {
            Ok(false) => {}
            Ok(true) => {
                skipped(&self.telemetry);
                if backlog {
                    self.telemetry.link.note_skip();
                }
            }
            Err(e) => {
                // dropped as before the copy existed; a device that cannot make one says so once
                skipped(&self.telemetry);
                if !self.hold_failed {
                    self.hold_failed = true;
                    teprintln!("wgc: the held-frame copy failed ({e:#}); gated frames are dropped");
                }
            }
        }
        Ok(())
    }

    fn deliver(&mut self, frame: &mut Frame) -> Result<()> {
        let capture = Instant::now();
        // no OS frame time: WGC's `SystemRelativeTime` is the target present time, 3-14 ms after
        // this callback (see `clock.rs`), so capture-relative stages start at callback entry
        let mut timing = FrameTiming::entering_capture();

        // a display that changed mode under this pipeline from outside the app (Windows
        // Settings, an OS re-apply after a topology reset). A ring copy of a different size is
        // dropped by the runtime without an error, so such frames are dropped here until the
        // size is back; ending the capture would make a transient a dead session
        let size = (frame.width(), frame.height());
        match self.frame_size {
            None => {
                self.frame_size = Some(size);
                if size != (self.native_w, self.native_h) {
                    teprintln!(
                        "wgc: first frame is {}x{} but the pipeline was sized for {}x{}",
                        size.0,
                        size.1,
                        self.native_w,
                        self.native_h
                    );
                }
            }
            Some(first) if first != size => {
                if !self.size_change_reported {
                    self.size_change_reported = true;
                    teprintln!(
                        "wgc: the captured display changed size {}x{} -> {}x{} under a running \
                         pipeline sized for the first; {}",
                        first.0,
                        first.1,
                        size.0,
                        size.1,
                        if self.drop_resized_frames {
                            "its frames are dropped until it is back or the pipeline is rebuilt"
                        } else {
                            "encoding them anyway (wgc_drop_resized_frames is off)"
                        }
                    );
                }
                if self.drop_resized_frames {
                    return Ok(());
                }
            }
            Some(_) => {
                if self.size_change_reported {
                    self.size_change_reported = false;
                    tprintln!(
                        "wgc: the captured display is back at {}x{}; frames flow again",
                        size.0,
                        size.1
                    );
                }
            }
        }

        // the compositor reports no dirty region for the pointer it composites, so it is asked
        // about separately and before either gate below, which would otherwise throw away the
        // frame carrying its new position. Not asked when the host draws the pointer itself
        let cursor_moved = !self.cursor_composited && self.cursor.changed();

        // frames past this device's cadence are coalesced, never queued. WGC delivers on change
        // only, so the last frame of a burst is held for `TrailingEdge` (ring path only: at the
        // full rate the classic path has no slot to hold it in and drops it)
        //
        // a cursor-moved frame is exempt, bounded only by the rate the display composes at
        // (`DISPLAY_COMPOSE_HZ`): a pointer sweep paints 113 fps exempt, 74 with a
        // two-thirds-period window, 70 with a half-period one, 59 fully gated. A window narrower
        // than DWM's compose interval drops every frame that arrives a jitter early
        //
        // `can_hold`: only where a gated frame can be delivered later does the gate widen its
        // window for the QP governor's divisor, and the governor cut the frame rate. The classic
        // path holds a copy for its repeater, and only while the rate is divided: at the full
        // rate its gate is the one it always had
        let can_hold = self.trailing.is_some() || self.classic_hold.is_some();
        super::governor::set_frame_hold(&self.telemetry, can_hold, self.trailing.is_some());
        self.gate.divisor = super::governor::frame_divisor(&self.telemetry);
        let gate_holds = gate_holds(
            self.trailing.is_some(),
            self.classic_hold.is_some(),
            self.gate.divisor,
        );
        let admit = self.gate.admit(
            capture,
            cursor_moved,
            self.timer_delivered_at(),
            gate_holds,
            self.ring.is_some(),
            || frame.has_dirty_regions().ok(),
            &self.telemetry,
        );
        match admit {
            Admit::Stage => {}
            Admit::Hold { last } if self.classic_hold.is_some() => {
                let deadline = trailing_deadline(last, self.gate.frame_period)
                    + self.gate.frame_period * (self.gate.divisor.max(1) - 1);
                return self.hold_classic(frame, capture, deadline, false);
            }
            Admit::Hold { last } => return self.hold_trailing(frame, timing, capture, last),
            Admit::SkipCadence | Admit::SkipUnchanged => return Ok(()),
        }

        let stale = self.stale_frames.load(Ordering::Relaxed);
        if stale != self.stale_frames_reported {
            self.telemetry.degradation.frames_dropped_stale.fetch_add(
                (stale - self.stale_frames_reported) as u64,
                Ordering::Relaxed,
            );
            self.stale_frames_reported = stale;
        }

        if let Some(ring) = self.ring.as_mut() {
            // this frame supersedes any frame the gate is still holding, and takes its slot
            if let Some(trailing) = self.trailing.as_ref() {
                trailing.discard_pending(&self.telemetry);
            }
            match ring.stage(frame.as_raw_texture(), timing) {
                Ok(staged) => self.gate.staged(capture, staged),
                // only ever a `RingFailed`: the ring has ended itself
                Err(e) => {
                    teprintln!("ring capture stage failed: {e:#}; stopping capture");
                    return Err(e);
                }
            }
            return self.check_ring_alive();
        }

        // no panics past this point: this is inside a WinRT `TypedEventHandler`, so an unwind
        // would cross `extern "system"` frames. Returning `Err` halts the capture cleanly
        // paced to the link with more than a frame period waiting at the sender: this frame waits
        // too, as a copy the repeater sends once the backlog has left (one load while not paced)
        if self.classic_hold.is_some() && self.telemetry.link.sender_busy() {
            let deadline = Instant::now() + backlog_left(&self.telemetry);
            return self.hold_classic(frame, capture, deadline, true);
        }
        let core = self
            .core
            .as_ref()
            .ok_or_else(|| anyhow!("classic capture path has no EncodeCore"))?;
        let encode_res = {
            let mut core = core
                .lock()
                .map_err(|_| anyhow!("encode core mutex poisoned; stopping capture"))?;
            // this frame supersedes the held copy
            // (a copy is only ever waiting with a deadline set, so the full rate pays one load)
            if let Some(h) = self
                .classic_hold
                .as_ref()
                .filter(|h| h.due_ns.load(Ordering::Relaxed) != 0)
            {
                h.due_ns.store(0, Ordering::Relaxed);
                if core.drop_held() {
                    self.telemetry
                        .degradation
                        .frames_skipped_cadence
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
            timing.mark_encode_submit();
            core.encode_captured(frame)
        };
        timing.mark_encode_done();
        let au = match encode_res {
            Ok(au) => au,
            Err(e)
                if is_transient_encode_error(&e)
                    && self.busy_streak < MAX_TRANSIENT_ENCODE_DROPS =>
            {
                self.busy_streak += 1;
                self.telemetry
                    .degradation
                    .encoder_transient_errors
                    .fetch_add(1, Ordering::Relaxed);
                teprintln!(
                    "encode transiently overloaded ({e:#}); dropping frame ({} in a row)",
                    self.busy_streak
                );
                return Ok(());
            }
            Err(e) => {
                self.telemetry
                    .degradation
                    .encoder_resets
                    .fetch_add(1, Ordering::Relaxed);
                if super::intel::encoder::needs_rebuild(&e) && self.rebuild_backend(&e) {
                    return Ok(());
                }
                return Err(e);
            }
        };
        self.busy_streak = 0;
        // no bytes means the encoder dropped the frame (AMF on a zero-length output buffer, see
        // `amd::encoder::classify_query_output`); never packetize a payloadless access unit
        if au.is_empty() {
            return Ok(());
        }

        self.last_frame_at
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        self.telemetry
            .frames_encoded
            .fetch_add(1, Ordering::Relaxed);
        let data = Bytes::from(au);
        let record = self.governed.then(|| data.clone());
        let _ = self.tx.send(EncodedFrame {
            data,
            capture,
            timing,
        });
        record_sent(core, record, false);
        self.frames_sent += 1;
        Ok(())
    }
}

/// A `Backend` on the encoder's **own** D3D11 device, for a path that has no capture device.
///
/// The synthetic-pattern probe holds a CPU frame buffer and no captured texture, so every rung
/// here is an own-device constructor and every encode goes through `Backend::encode_cpu`. It
/// walks [`encoder_ladder`] rather than repeating the order, so the probe cannot drift from what
/// a live session would pick.
fn build_own_device_backend(
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
) -> Result<Backend> {
    let ladder = encoder_ladder(vendor);
    let mut failure: Option<anyhow::Error> = None;
    for (i, kind) in ladder.iter().copied().enumerate() {
        if let Some(previous) = failure.as_ref() {
            teprintln!(
                "synthetic encode: {} unavailable ({previous:?}); trying {}",
                ladder[i - 1].name(),
                kind.name()
            );
        }
        let attempt = match kind {
            BackendKind::Nvenc => Encoder::new(config).map(|encoder| Backend::Nvenc {
                encoder,
                path: EncodePath::CpuBridge,
            }),
            BackendKind::Intel => {
                IntelEncoder::new(config).map(|e| Backend::IntelCpu { encoder: e })
            }
            BackendKind::Amf => AmfEncoder::new(config).map(|e| Backend::AmfCpu { encoder: e }),
            BackendKind::X264 => X264Encoder::new(config).map(|e| Backend::X264 { encoder: e }),
        };
        match attempt {
            Ok(backend) => return Ok(backend),
            Err(e) => {
                failure = Some(match failure {
                    Some(previous) => {
                        previous.context(format!("{} also failed: {e:?}", kind.name()))
                    }
                    None => e,
                });
            }
        }
    }
    Err(failure.unwrap_or_else(|| anyhow!("the encoder ladder for {vendor:?} is empty")))
}

#[allow(clippy::too_many_arguments)]
fn synthetic_pattern_loop(
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    // no client on the synthetic path, so no receipts: the slot count stays zero
    _repair: Arc<crate::streamer::repair::RepairState>,

    target_bitrate: Arc<AtomicU32>,
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    wake_rx: crossbeam_channel::Receiver<()>,
    telemetry: SharedTelemetry,
    workload: crate::streamer::synthetic::Workload,
    counter: bool,
) {
    let _thread_tuning = tuning::tune_current_thread();
    let counter_epoch = Instant::now();

    let mut backend = match build_own_device_backend(config, vendor) {
        Ok(b) => b,
        Err(e) => {
            teprintln!(
                "encode thread: no encoder for --encoder {vendor:?} ({e:?}); pipeline stopped"
            );
            return;
        }
    };
    if let Some(device) = backend.device() {
        tuning::raise_d3d11_gpu_priority(device);
    }
    telemetry.encoder.set(backend.name().into());
    let mut current_bitrate = config.bitrate_bps;
    let mut bitrate_warned = false;
    let mut bitrate_retry = RetryBackoff::new(
        tune::on("bitrate_retry_backoff"),
        BITRATE_RETRY_FIRST,
        BITRATE_RETRY_MAX,
    );
    // set once somebody has subscribed. The loop has no stop flag of its own; it ends when the
    // last subscriber has gone or every `Pipeline` handle has been dropped
    let mut subscribed = false;

    let frame_interval = Duration::from_nanos(1_000_000_000 / SP_FPS as u64);
    let mut frame_buf = vec![0u8; (SP_WIDTH * SP_HEIGHT * 4) as usize];
    let mut frame_index: u32 = 0;
    let mut next_deadline = Instant::now();

    tprintln!(
        "pipeline: synthetic pattern encode loop started ({SP_WIDTH}x{SP_HEIGHT}@{SP_FPS}, \
         encoder={}, workload={}, counter={counter})",
        backend.name(),
        workload.name()
    );

    loop {
        let receivers = tx.receiver_count();
        if receivers > 0 {
            subscribed = true;
        } else if subscribed {
            tprintln!("pipeline: synthetic pattern encode loop stopped (no subscribers left)");
            return;
        }

        // the same applier the live paths use, so a refused target is put back and retried
        apply_pending_bitrate(
            &mut backend,
            &target_bitrate,
            &mut current_bitrate,
            &mut bitrate_warned,
            &mut bitrate_retry,
        );

        let force_idr = frame_index == 0 || idr_request.swap(false, Ordering::Relaxed);
        // stamped before the picture is generated: this is the probe's "capture", so the encode
        // is inside every capture-relative number the synthetic path produces
        let capture = Instant::now();
        let mut timing = FrameTiming::entering_capture();
        crate::streamer::synthetic::fill(
            &mut frame_buf,
            SP_WIDTH,
            SP_HEIGHT,
            frame_index,
            workload,
        );
        if counter {
            crate::streamer::synthetic::draw_counter(
                &mut frame_buf,
                SP_WIDTH,
                SP_HEIGHT,
                frame_index,
                counter_epoch.elapsed().as_millis() as u64,
            );
        }

        timing.mark_encode_submit();
        // every own-device backend takes the frame the same way: a full-width BGRA upload
        match backend.encode_cpu(&frame_buf, SP_WIDTH * 4, ReadbackFormat::Bgra8, force_idr) {
            Ok(au) if au.is_empty() => {}
            Ok(au) => {
                timing.mark_encode_done();
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture,
                    timing,
                });
            }
            Err(e) => {
                teprintln!("encode failed (frame={frame_index}): {e:?}; pipeline stopped");
                return;
            }
        }

        frame_index = frame_index.wrapping_add(1);

        next_deadline += frame_interval;
        let now = Instant::now();
        if next_deadline > now {
            match wake_rx.recv_timeout(next_deadline - now) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    // every `Pipeline` is gone, so nothing can subscribe or steer this loop again
                    tprintln!("pipeline: synthetic pattern encode loop stopped (pipeline dropped)");
                    return;
                }
            }
            while wake_rx.try_recv().is_ok() {}
        } else {
            next_deadline = now;
        }
    }
}

/// Consecutive frame-pool failures (`ContentSize`, `Surface`, the DXGI interface, `Recreate`)
/// after which the capture is declared dead. Half a second at 60 fps: a transient failure clears
/// long before, and a removed device or a `Recreate` that cannot succeed never does.
const MAX_CONSECUTIVE_POOL_ERRORS: u32 = 30;

/// Keeps the capture thread's per-thread tuning until that thread exits.
///
/// Thread-local destructors run on the thread being torn down, which is the only thread
/// `AvRevertMmThreadCharacteristics` accepts and the only one whose execution state the
/// keep-awake set.
fn park_on_capture_thread(tuning: tuning::ThreadTuning, keep_awake: tuning::KeepAwake) {
    use std::cell::RefCell;
    thread_local! {
        static CAPTURE_THREAD_STATE: RefCell<Option<(tuning::ThreadTuning, tuning::KeepAwake)>> =
            const { RefCell::new(None) };
    }
    CAPTURE_THREAD_STATE.with(|slot| *slot.borrow_mut() = Some((tuning, keep_awake)));
}

/// `wgc_free_threaded_pool`: tune whichever WinRT worker thread delivered this frame, once.
///
/// With the default pool every callback runs on the capture thread, tuned in `LiveCapture::new`.
/// A free-threaded pool raises `FrameArrived` on threads this process does not own, so each is
/// tuned the first time it shows up and keeps that tuning (MMCSS, `TIME_CRITICAL`, P-cores) until
/// it exits, which for a pool thread can be the life of the process. That cost is one reason
/// this is a candidate rather than the default.
fn tune_callback_thread() {
    use std::cell::RefCell;
    thread_local! {
        static CALLBACK_TUNING: RefCell<Option<tuning::ThreadTuning>> = const { RefCell::new(None) };
    }
    CALLBACK_TUNING.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(tuning::tune_current_thread());
        }
    });
}

/// Why a ring can no longer make progress, if it cannot.
///
/// The encode thread has stopped, or at most one slot is left unpoisoned. One live slot is not
/// enough: the encoder holds its last slot until a newer one arrives, so nothing can ever be
/// staged again and every captured frame is dropped for want of a slot.
fn ring_dead_verdict(
    encode_thread_finished: bool,
    poisoned: u32,
    slots: usize,
) -> Option<&'static str> {
    if encode_thread_finished {
        Some("its encode thread has stopped")
    } else if poisoned as usize + 1 >= slots {
        Some("no more than one slot is left unpoisoned, so nothing can be staged")
    } else {
        None
    }
}

/// When a frame the cadence gate held is delivered if nothing newer has arrived: five quarters of
/// a frame period after the last delivery.
///
/// Later than the next on-cadence frame, so in a continuing burst that frame arrives first and
/// supersedes it: a 120 Hz compose for a 60 Hz client delivers at 0, holds the frame at 8.3 ms,
/// and the frame at 16.7 ms (past the gate's 12.5 ms) replaces it before 20.8 ms. Early enough
/// that the end of a burst is on screen within about one period of where a periodic sampler
/// would have put it.
fn trailing_deadline(last_delivered: Instant, frame_period: Duration) -> Instant {
    last_delivered + frame_period * TRAILING_NUM / TRAILING_DEN
}

const TRAILING_NUM: u32 = 5;
const TRAILING_DEN: u32 = 4;

impl AsyncEncodeRing {
    /// [`AsyncEncodeRing::stage`], with the staged message sent to `hold` instead of to the encode
    /// thread.
    ///
    /// The ready sender is swapped for the duration of the call so the copy, the keyed mutex, the
    /// poisoning and the IDR bookkeeping all stay in `stage` alone. A merge that changes how
    /// `stage` hands its message over has to keep it going through `self.ready`.
    fn stage_into(
        &mut self,
        raw: &ID3D11Texture2D,
        timing: FrameTiming,
        hold: &mut crossbeam_channel::Sender<ReadyMsg>,
    ) -> Result<bool> {
        std::mem::swap(&mut self.ready, hold);
        let staged = self.stage(raw, timing);
        std::mem::swap(&mut self.ready, hold);
        staged
    }
}

struct HeldFrame {
    msg: ReadyMsg,
    deadline: Instant,
    /// Callback entry of the frame, which is what the cadence gate measures between.
    arrived: Instant,
    /// The timer could not hand it to the encode thread. It stays here for the capture thread to
    /// give back (see [`SlotReturn`]); the timer never retries it.
    undeliverable: bool,
}

/// Hands a staged slot that will never be encoded back to the ring's free pool.
///
/// `stage` leaves a shared slot's keyed mutex released at `KEY_ENCODER`, and only the encode
/// thread ever moves it back to `KEY_WRITER`. A slot that skips the encoder has to be moved back
/// here, or the next `stage` into it waits out its timeout on a key nobody will ever release and
/// poisons the slot; two of those and the ring is dead. Called on the capture thread only: the
/// acquire/release pair is work on the capture device's immediate context, so it takes the
/// cursor-overlay lock exactly as `stage` does. A `Local` (AMF) ring has no keys and only needs
/// the slot sent back.
struct SlotReturn {
    keys: Vec<IDXGIKeyedMutex>,
    cursor: Option<Arc<Mutex<CursorOverlay>>>,
    poisoned: Arc<PoisonedSlots>,
    free: crossbeam_channel::Sender<usize>,
}

impl SlotReturn {
    fn of(ring: &AsyncEncodeRing) -> Self {
        Self {
            keys: ring.textures.keys().to_vec(),
            cursor: ring.cursor.clone(),
            poisoned: Arc::clone(&ring.poisoned),
            free: ring.free_slots.clone(),
        }
    }

    fn give_back(&self, slot: usize) {
        if let Some(mutex) = self.keys.get(slot) {
            let _overlay = self.cursor.as_ref().map(|c| c.lock_recover());
            // the slot was released at `KEY_ENCODER` by `stage` and never reached the encoder,
            // so the key is free and a zero timeout is enough
            let restored = match super::keyed::acquire(mutex, KEY_ENCODER, 0) {
                Ok(super::keyed::Acquired::Owned) => unsafe { mutex.ReleaseSync(KEY_WRITER) }
                    .map_err(|e| format!("ReleaseSync(writer) failed: {e:?}")),
                Ok(other) => Err(format!("AcquireSync(encoder) returned {other:?}")),
                Err(e) => Err(format!("AcquireSync(encoder) failed: {e:?}")),
            };
            if let Err(why) = restored {
                // not handed back: a slot whose key is in an unknown state would wedge the next
                // stage. `check_ring_alive` sees the quarantine on the next frame
                self.poisoned.mark(slot);
                teprintln!(
                    "ring: unencoded slot {slot} could not be handed back ({why}); slot quarantined"
                );
                return;
            }
        }
        let _ = self.free.send(slot);
    }
}

struct TrailingShared {
    pending: Mutex<Option<HeldFrame>>,
    /// Callback entry of the newest frame the timer delivered, as nanoseconds after the session
    /// epoch; zero when it has delivered none.
    delivered_ns: AtomicU64,
    /// Held frames the timer delivered.
    delivered: AtomicU64,
}

/// Trailing-edge delivery for the WGC cadence gate (`wgc_trailing_frame`).
///
/// The capture thread stages a gated frame into a free slot and parks it here; a timer thread
/// sends it to the encode thread at [`trailing_deadline`] unless the capture thread takes it back
/// first because a newer frame arrived. Both sides take the one lock: the timer for "take and
/// send", the capture thread for "take", so a held frame either reached the encoder before the
/// newer one was staged or never reaches it at all, and the encoder can never be handed an older
/// picture after a newer one. The capture thread's critical section is a pointer swap; the timer
/// is never in the lock for longer than a non-blocking channel send.
pub(crate) struct TrailingEdge {
    shared: Arc<TrailingShared>,
    notify: crossbeam_channel::Sender<()>,
    hold_tx: crossbeam_channel::Sender<ReadyMsg>,
    hold_rx: crossbeam_channel::Receiver<ReadyMsg>,
    slots: SlotReturn,
    idr_request: Arc<AtomicBool>,
}

impl TrailingEdge {
    pub(crate) fn start(
        ring: &AsyncEncodeRing,
        idr_request: Arc<AtomicBool>,
        epoch: Instant,
    ) -> Self {
        Self::spawn(ring.ready.clone(), SlotReturn::of(ring), idr_request, epoch)
    }

    fn spawn(
        ready_tx: crossbeam_channel::Sender<ReadyMsg>,
        slots: SlotReturn,
        idr_request: Arc<AtomicBool>,
        epoch: Instant,
    ) -> Self {
        let shared = Arc::new(TrailingShared {
            pending: Mutex::new(None),
            delivered_ns: AtomicU64::new(0),
            delivered: AtomicU64::new(0),
        });
        let (notify, notify_rx) = crossbeam_channel::bounded::<()>(1);
        let (hold_tx, hold_rx) = crossbeam_channel::bounded::<ReadyMsg>(1);
        let thread_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("wgc-trailing".to_string())
            .spawn(move || trailing_edge_thread(thread_shared, notify_rx, ready_tx, epoch));
        if let Err(e) = spawned {
            // without the timer a held frame is only ever superseded; log that, keep the session
            teprintln!("pipeline: trailing-frame timer did not start ({e}); held frames wait for the next frame");
        }
        Self {
            shared,
            notify,
            hold_tx,
            hold_rx,
            slots,
            idr_request,
        }
    }

    /// Park a staged frame until `deadline`, replacing nothing: the caller discards any older one
    /// first so its slot is free.
    fn hold(&self, msg: ReadyMsg, deadline: Instant, arrived: Instant) {
        let previous = self.shared.pending.lock_recover().replace(HeldFrame {
            msg,
            deadline,
            arrived,
            undeliverable: false,
        });
        if let Some(previous) = previous {
            self.release(previous.msg);
        }
        let _ = self.notify.try_send(());
    }

    /// Take back the held frame, if the timer has not sent it yet, and return its slot. Counted
    /// as a cadence skip: it is the frame the gate coalesced away.
    pub(crate) fn discard_pending(&self, telemetry: &SessionTelemetry) {
        let taken = self.shared.pending.lock_recover().take();
        if let Some(held) = taken {
            self.release(held.msg);
            telemetry
                .degradation
                .frames_skipped_cadence
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    fn release(&self, msg: ReadyMsg) {
        // `stage` consumed the keyframe request into this message; a frame that is never encoded
        // must hand it back or the request is lost
        if msg.force_idr {
            self.idr_request.store(true, Ordering::Relaxed);
        }
        self.slots.give_back(msg.slot);
    }

    pub(crate) fn delivered_count(&self) -> u64 {
        self.shared.delivered.load(Ordering::Relaxed)
    }

    /// Callback entry of the newest frame the timer delivered.
    pub(crate) fn delivered_at(&self, epoch: Instant) -> Option<Instant> {
        match self.shared.delivered_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(epoch + Duration::from_nanos(ns)),
        }
    }
}

fn trailing_edge_thread(
    shared: Arc<TrailingShared>,
    notify_rx: crossbeam_channel::Receiver<()>,
    ready_tx: crossbeam_channel::Sender<ReadyMsg>,
    epoch: Instant,
) {
    let _thread_tuning = tuning::tune_timer_thread();
    let mut first_reported = false;
    loop {
        let deadline = shared
            .pending
            .lock_recover()
            .as_ref()
            .filter(|h| !h.undeliverable)
            .map(|h| h.deadline);
        let woke = match deadline {
            None => notify_rx.recv().map_err(|_| ()),
            Some(deadline) => {
                let wait = deadline.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    Err(())
                } else {
                    match notify_rx.recv_timeout(wait) {
                        Ok(()) => Ok(()),
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => Err(()),
                        Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                    }
                }
            }
        };
        match (woke, deadline) {
            // a new frame was held, or the held one was taken back: look again
            (Ok(()), _) => continue,
            // the capture side has gone (its `notify` sender dropped with the handler)
            (Err(()), None) => break,
            (Err(()), Some(_)) => {}
        }
        let mut pending = shared.pending.lock_recover();
        if !pending
            .as_ref()
            .is_some_and(|h| !h.undeliverable && Instant::now() >= h.deadline)
        {
            continue;
        }
        let Some(held) = pending.take() else { continue };
        let arrived_ns = held.arrived.saturating_duration_since(epoch).as_nanos() as u64;
        let (held_deadline, held_arrived) = (held.deadline, held.arrived);
        // sent with the lock held, so the capture thread cannot stage a newer frame between this
        // frame leaving `pending` and it reaching the encoder
        match ready_tx.try_send(held.msg) {
            Ok(()) => {
                shared
                    .delivered_ns
                    .store(arrived_ns.max(1), Ordering::Relaxed);
                shared.delivered.fetch_add(1, Ordering::Relaxed);
                drop(pending);
                if !first_reported {
                    first_reported = true;
                    tprintln!(
                        "cadence: delivered a held trailing frame {} ms after it arrived (first \
                         of this session)",
                        held.arrived.elapsed().as_millis()
                    );
                }
            }
            Err(e) => {
                // `ready` holds as many messages as the ring has slots, so it cannot be full:
                // the encode thread is gone. The slot's key still says `KEY_ENCODER` and only
                // the capture thread may move it back (`SlotReturn`), on its next frame
                let disconnected = e.is_disconnected();
                *pending = Some(HeldFrame {
                    msg: e.into_inner(),
                    deadline: held_deadline,
                    arrived: held_arrived,
                    undeliverable: true,
                });
                drop(pending);
                if disconnected {
                    break;
                }
            }
        }
    }
    // a frame still held when the session ends goes back with the ring; nothing to release
}

#[cfg(test)]
mod wgc_handler_tests {
    use super::*;
    use crate::streamer::config::EncoderVendor;

    /// With the governor switched off, a classic core (armed like a live one) has none, and nothing
    /// after a send touches its lock.
    #[test]
    fn a_core_without_a_governor_is_not_locked_again_after_a_send() {
        let config = EncoderConfig {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 6_000_000,
            profile: H264Profile::High,
            qp: Some(18),
            min_qp: None,
            intra_refresh: false,
        };
        let encoder = match X264Encoder::new(config) {
            Ok(e) => e,
            Err(e) => return crate::test_support::skip("classic core without a governor", e),
        };
        let telemetry = SessionTelemetry::new_shared();
        super::super::governor::arm(&telemetry, &config);
        let mut health = CoreHealth::new(Arc::clone(&telemetry));
        // the link governor ships on: this is the core a session gets with it switched off
        health.qp = None;
        let core = Mutex::new(EncodeCore {
            backend: Backend::X264 { encoder },
            scaler: None,
            reader: None,
            staging: None,
            held: None,
            target_bitrate: Arc::new(AtomicU32::new(0)),
            idr_request: Arc::new(AtomicBool::new(false)),
            current_bitrate: config.bitrate_bps,
            bitrate_warned: false,
            have_frame: false,
            frame_index: 0,
            health,
        });
        let governed = core.lock_recover().governed();
        assert!(!governed, "a governor was built with every switch off");
        let before = POST_SEND_LOCKS.load(Ordering::Relaxed);
        let au = Bytes::from_static(&[0, 0, 0, 1, 0x41, 0x9a]);
        for _ in 0..100 {
            record_sent(&core, governed.then(|| au.clone()), false);
        }
        assert_eq!(POST_SEND_LOCKS.load(Ordering::Relaxed), before);
    }

    #[test]
    fn without_the_switch_the_ladder_is_unchanged() {
        for vendor in [
            EncoderVendor::Auto,
            EncoderVendor::Nvidia,
            EncoderVendor::Intel,
            EncoderVendor::Amd,
            EncoderVendor::Software,
        ] {
            let attempts = ladder_attempts(vendor, false);
            let kinds: Vec<_> = attempts.iter().map(|&(k, _)| k).collect();
            assert_eq!(kinds, encoder_ladder(vendor).to_vec(), "{vendor:?}");
            assert!(attempts.iter().all(|&(_, t)| t == RungTier::Both));
        }
    }

    #[test]
    fn gpu_paths_first_tries_every_readback_free_path_before_any_bridge() {
        use BackendKind::*;
        use RungTier::*;
        assert_eq!(
            ladder_attempts(EncoderVendor::Auto, true),
            vec![
                (Nvenc, Gpu),
                (Amf, Gpu),
                (Intel, Gpu),
                (Nvenc, Bridge),
                (Amf, Bridge),
                (Intel, Bridge),
                (X264, Bridge),
            ]
        );
        // a single-vendor ladder keeps its order: GPU path, own bridge, x264
        assert_eq!(
            ladder_attempts(EncoderVendor::Intel, true),
            vec![(Intel, Gpu), (Intel, Bridge), (X264, Bridge)]
        );
        assert_eq!(
            ladder_attempts(EncoderVendor::Software, true),
            vec![(X264, Bridge)]
        );
    }

    #[test]
    fn the_session_cap_is_recognised_and_nothing_else_is() {
        let cap = anyhow!(
            "nvEncOpenEncodeSessionEx failed: NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY (the driver's \
             concurrent-session limit: this system already holds as many NVENC sessions as it \
             allows, so this client encodes on Quick Sync or x264 instead)"
        )
        .context("opening the zero-copy ring");
        assert!(is_nvenc_session_cap(&cap));
        assert!(!is_nvenc_session_cap(&anyhow!(
            "nvEncOpenEncodeSessionEx failed: NV_ENC_ERR_INVALID_VERSION"
        )));
    }

    #[test]
    fn a_ring_that_cannot_stage_is_dead() {
        assert_eq!(ring_dead_verdict(false, 0, 3), None);
        assert_eq!(ring_dead_verdict(false, 1, 3), None);
        assert!(ring_dead_verdict(false, 2, 3).is_some());
        assert!(ring_dead_verdict(false, 3, 3).is_some());
        assert!(ring_dead_verdict(true, 0, 3).is_some());
    }

    #[test]
    fn a_held_frame_waits_past_the_next_on_cadence_frame() {
        let period = Duration::from_micros(16_667);
        let last = Instant::now();
        let deadline = trailing_deadline(last, period);
        // the gate admits a frame 3/4 of a period after the last delivery; the held frame must
        // still be pending then, or a steady burst would deliver both
        assert!(deadline > last + period * CADENCE_NUM / CADENCE_DEN);
        assert!(deadline > last + period);
        assert!(deadline <= last + period * 2);
    }

    fn held_msg(slot: usize, force_idr: bool) -> ReadyMsg {
        ReadyMsg {
            slot,
            force_idr,
            capture: Instant::now(),
            timing: FrameTiming::entering_capture(),
        }
    }

    struct Rig {
        edge: TrailingEdge,
        ready_rx: crossbeam_channel::Receiver<ReadyMsg>,
        free_rx: crossbeam_channel::Receiver<usize>,
        idr: Arc<AtomicBool>,
        telemetry: SharedTelemetry,
        epoch: Instant,
    }

    /// A rig over a keyless (AMF-shaped) ring: slot ownership is the channels' alone.
    fn rig() -> Rig {
        rig_with_keys(Vec::new())
    }

    fn rig_with_keys(keys: Vec<IDXGIKeyedMutex>) -> Rig {
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<ReadyMsg>(3);
        let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(3);
        let idr = Arc::new(AtomicBool::new(false));
        let telemetry = SessionTelemetry::new_shared();
        let epoch = Instant::now();
        let slots = SlotReturn {
            keys,
            cursor: None,
            poisoned: Arc::new(PoisonedSlots::default()),
            free: free_tx,
        };
        let edge = TrailingEdge::spawn(ready_tx, slots, Arc::clone(&idr), epoch);
        Rig {
            edge,
            ready_rx,
            free_rx,
            idr,
            telemetry,
            epoch,
        }
    }

    /// Two shared keyed-mutex textures on a WARP device, the shape of an NVENC ring's slots seen
    /// from the capture side. `None` when this machine cannot make them.
    fn warp_keyed_slots(test: &str) -> Option<Vec<IDXGIKeyedMutex>> {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
            D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
        };

        let mut device = None;
        let created = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        };
        let (Ok(()), Some(device)) = (created, device) else {
            crate::test_support::skip_because(test, "no WARP device could be created");
            return None;
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 64,
            Height: 64,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
        };
        let mut keys = Vec::new();
        for _ in 0..2 {
            let mut texture = None;
            let made = unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) };
            let key = match (made, texture) {
                (Ok(()), Some(t)) => t.cast::<IDXGIKeyedMutex>().ok(),
                _ => None,
            };
            let Some(key) = key else {
                crate::test_support::skip_because(
                    test,
                    "WARP would not create a shared keyed-mutex texture",
                );
                return None;
            };
            keys.push(key);
        }
        Some(keys)
    }

    /// What `AsyncEncodeRing::stage` does to a shared slot's key: take it as the writer and hand
    /// it to the encoder.
    fn stage_key(key: &IDXGIKeyedMutex) {
        assert_eq!(
            super::super::keyed::acquire(key, KEY_WRITER, 0).expect("acquire writer"),
            super::super::keyed::Acquired::Owned,
            "a free slot is takeable by the writer"
        );
        unsafe { key.ReleaseSync(KEY_ENCODER) }.expect("release to the encoder");
    }

    /// A held frame that is superseded, or replaced by a newer held frame, must not go back to
    /// the pool with its key still at `KEY_ENCODER`, or the next stage into that slot times out
    /// and poisons it. Through real keyed mutexes, every slot that comes back must be takeable
    /// by the writer again at once.
    #[test]
    fn a_slot_that_skips_the_encoder_comes_back_writable() {
        const TEST: &str = "a_slot_that_skips_the_encoder_comes_back_writable";
        let Some(keys) = warp_keyed_slots(TEST) else {
            return;
        };
        let r = rig_with_keys(keys.clone());
        let now = Instant::now();
        let far = now + Duration::from_secs(60);

        // held, then replaced by a newer held frame: slot 0 comes back
        stage_key(&keys[0]);
        r.edge.hold(held_msg(0, false), far, now);
        stage_key(&keys[1]);
        r.edge.hold(held_msg(1, false), far, now);
        assert_eq!(r.free_rx.try_recv().ok(), Some(0));
        // held, then superseded by an on-cadence frame: slot 1 comes back
        r.edge.discard_pending(&r.telemetry);
        assert_eq!(r.free_rx.try_recv().ok(), Some(1));

        // re-stage both, as the next frames would: a key left at `KEY_ENCODER` times out here
        stage_key(&keys[0]);
        stage_key(&keys[1]);
        assert_eq!(r.edge.slots.poisoned.count(), 0);
        assert!(
            r.ready_rx.try_recv().is_err(),
            "nothing reached the encoder"
        );
    }

    /// With the encode thread gone the timer cannot deliver. It must not free the slot itself (it
    /// cannot restore the key from its own thread); the capture thread gives it back on its next
    /// frame.
    #[test]
    fn an_undeliverable_frame_is_given_back_by_the_capture_thread() {
        let r = rig();
        let Rig {
            edge,
            ready_rx,
            free_rx,
            telemetry,
            ..
        } = r;
        drop(ready_rx);
        let now = Instant::now();
        edge.hold(held_msg(2, true), now + Duration::from_millis(5), now);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            free_rx.try_recv().is_err(),
            "the timer must not free a slot whose key it cannot restore"
        );
        edge.discard_pending(&telemetry);
        assert_eq!(free_rx.try_recv().ok(), Some(2));
        assert_eq!(
            telemetry
                .degradation
                .frames_skipped_cadence
                .load(Ordering::Relaxed),
            1
        );
    }

    /// The last frame of a burst that ends inside a cadence window is held and reaches the
    /// encoder once its deadline passes.
    #[test]
    fn the_last_frame_of_a_burst_is_delivered() {
        let r = rig();
        let arrived = Instant::now();
        r.edge.hold(
            held_msg(2, false),
            arrived + Duration::from_millis(5),
            arrived,
        );
        let got = r
            .ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the held frame reached the encoder");
        assert_eq!(got.slot, 2);
        assert!(
            r.free_rx.try_recv().is_err(),
            "a delivered slot is not freed"
        );
        let delivered = r.edge.delivered_at(r.epoch).expect("delivery recorded");
        let slack = Duration::from_millis(1);
        assert!(delivered + slack >= arrived && delivered <= arrived + slack);
    }

    /// A newer frame supersedes the held one: it never reaches the encoder, its slot comes back,
    /// and a keyframe request it consumed is handed back.
    #[test]
    fn a_superseded_frame_is_never_encoded_and_its_slot_and_keyframe_come_back() {
        let r = rig();
        let now = Instant::now();
        r.edge
            .hold(held_msg(1, true), now + Duration::from_millis(200), now);
        r.edge.discard_pending(&r.telemetry);
        assert_eq!(r.free_rx.try_recv().ok(), Some(1));
        assert!(
            r.idr.load(Ordering::Relaxed),
            "the keyframe request is restored"
        );
        assert_eq!(
            r.telemetry
                .degradation
                .frames_skipped_cadence
                .load(Ordering::Relaxed),
            1
        );
        assert!(
            r.ready_rx.recv_timeout(Duration::from_millis(400)).is_err(),
            "a taken-back frame must never be sent"
        );
        assert!(r.edge.delivered_at(r.epoch).is_none());
    }

    /// Holding a second frame replaces the first, which goes back to the free pool.
    #[test]
    fn only_the_newest_held_frame_survives() {
        let r = rig();
        let now = Instant::now();
        r.edge
            .hold(held_msg(0, false), now + Duration::from_millis(300), now);
        r.edge
            .hold(held_msg(1, false), now + Duration::from_millis(5), now);
        assert_eq!(r.free_rx.try_recv().ok(), Some(0));
        let got = r
            .ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("the newer frame is delivered");
        assert_eq!(got.slot, 1);
        assert!(r.ready_rx.recv_timeout(Duration::from_millis(100)).is_err());
    }

    /// The degrade decision without a device: it runs on every machine, including ones whose
    /// devices all have a video processor (where the WARP test below skips).
    #[test]
    fn a_failed_conversion_pass_degrades_and_a_failed_downscale_does_not() {
        let nv12 = ReadbackFormat::Nv12;
        let unbuildable = || -> Result<u32> { Err(anyhow!("no ID3D11VideoDevice")) };
        // conversion only: degrade to the BGRA readback
        assert!(matches!(
            scaler_or_degrade(false, true, nv12, unbuildable),
            Ok(None)
        ));
        // a real downscale cannot be done without it
        assert!(scaler_or_degrade(true, true, nv12, unbuildable).is_err());
        assert!(scaler_or_degrade(true, false, nv12, unbuildable).is_err());
        // nothing wanted: nothing built
        let mut built = false;
        assert!(matches!(
            scaler_or_degrade(false, false, nv12, || {
                built = true;
                Ok(1u32)
            }),
            Ok(None)
        ));
        assert!(!built);
        // built: used
        assert!(matches!(
            scaler_or_degrade(false, true, nv12, || Ok(7u32)),
            Ok(Some(7))
        ));
    }

    /// x264 on a device with no video processor: a conversion-only pass degrades to a BGRA
    /// readback instead of failing the session, while a real downscale still fails.
    #[test]
    fn a_missing_video_processor_only_fails_a_session_that_must_scale() {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
        };

        let mut device = None;
        let mut context = None;
        let created = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        let (Ok(()), Some(device), Some(context)) = (created, device, context) else {
            crate::test_support::skip_because(
                "a_missing_video_processor_only_fails_a_session_that_must_scale",
                "no WARP device could be created",
            );
            return;
        };
        if video_processor_available(&device, &context) {
            crate::test_support::skip_because(
                "a_missing_video_processor_only_fails_a_session_that_must_scale",
                "this WARP device has a video processor, so the degraded case cannot arise",
            );
            return;
        }
        let converted = classic_scaler(
            &device,
            &context,
            false,
            true,
            (640, 360),
            (640, 360),
            ReadbackFormat::Nv12,
        );
        assert!(
            matches!(converted, Ok(None)),
            "conversion-only must degrade to a BGRA readback, got {:?}",
            converted.as_ref().map(|s| s.is_some())
        );
        let scaled = classic_scaler(
            &device,
            &context,
            true,
            true,
            (640, 360),
            (320, 180),
            ReadbackFormat::Nv12,
        );
        assert!(
            scaled.is_err(),
            "a downscale with no video processor cannot be done"
        );
    }

    fn x264_core(qp: u8, governed: bool) -> Result<(Mutex<EncodeCore>, SharedTelemetry)> {
        let config = EncoderConfig {
            width: 640,
            height: 360,
            fps: 60,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 6_000_000,
            profile: H264Profile::High,
            qp: Some(qp),
            min_qp: None,
            intra_refresh: false,
        };
        let encoder = X264Encoder::new(config)?;
        let telemetry = SessionTelemetry::new_shared();
        super::super::governor::arm(&telemetry, &config);
        let mut health = CoreHealth::new(Arc::clone(&telemetry));
        health.qp = if governed {
            super::super::governor::QpControl::build(&telemetry, true, true, true)
                .map(|c| c.with_link_pace(true))
        } else {
            None
        };
        let core = Mutex::new(EncodeCore {
            backend: Backend::X264 { encoder },
            scaler: None,
            reader: None,
            staging: None,
            held: None,
            target_bitrate: Arc::new(AtomicU32::new(0)),
            idr_request: Arc::new(AtomicBool::new(false)),
            current_bitrate: config.bitrate_bps,
            bitrate_warned: false,
            have_frame: false,
            frame_index: 0,
            health,
        });
        Ok((core, telemetry))
    }

    /// A governor that has seen no congestion takes every keyframe request the way a core without
    /// one does: the same submits become keyframes, including the session's first.
    #[test]
    fn a_governor_with_no_congestion_takes_every_keyframe_request_as_it_came() {
        let (bare, gov) = match (x264_core(18, false), x264_core(18, true)) {
            (Ok((a, _)), Ok((b, t))) => (a, (b, t)),
            (Err(e), _) | (_, Err(e)) => {
                return crate::test_support::skip("keyframe requests under an idle governor", e)
            }
        };
        let (gov, telemetry) = gov;
        let (mut bare, mut gov) = (bare.lock_recover(), gov.lock_recover());
        assert!(gov.governed() && !bare.governed());
        let mut x = 0x9E37_79B9u32;
        for k in 0..400u64 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let asked = x.is_multiple_of(5);
            if x.is_multiple_of(17) {
                telemetry
                    .degradation
                    .client_keyframe_requests
                    .fetch_add(1, Ordering::Relaxed);
            }
            bare.idr_request.store(asked, Ordering::Relaxed);
            gov.idr_request.store(asked, Ordering::Relaxed);
            let (a, b) = (bare.take_force_idr(), gov.take_force_idr());
            assert_eq!(a, b, "submit {k}");
            assert_eq!(a, k == 0 || asked, "submit {k}");
            bare.frame_index += 1;
            gov.frame_index += 1;
        }
        assert_eq!(telemetry.link.frame_divisor(), 1);
    }

    #[test]
    fn the_classic_gate_holds_only_while_the_rate_is_divided() {
        assert!(!gate_holds(false, false, 1));
        assert!(!gate_holds(false, false, 4), "no copy to hold it in");
        assert!(
            !gate_holds(false, true, 1),
            "the full rate keeps the gate it had"
        );
        assert!(gate_holds(false, true, 2));
        assert!(
            gate_holds(true, false, 1),
            "the ring's trailing edge always holds"
        );
        // and at the full rate a classic gate with a governor decides exactly as one without
        let t = SessionTelemetry::default();
        let period = Duration::from_micros(16_667);
        let t0 = Instant::now();
        for divisor in [1u32, 2, 4] {
            let mut g = FrameGate::new(period, false, false, false);
            g.divisor = divisor;
            let holds = gate_holds(false, true, divisor);
            let at = |ms: u64| t0 + Duration::from_millis(ms);
            assert_eq!(
                g.admit(at(0), false, None, holds, false, || None, &t),
                Admit::Stage
            );
            let next = g.admit(at(5), false, None, holds, false, || None, &t);
            if divisor == 1 {
                assert_eq!(next, Admit::SkipCadence);
            } else {
                assert!(matches!(next, Admit::Hold { .. }), "{next:?}");
            }
        }
    }

    /// The classic path's held copy: copied from a texture on the capture device, encoded once by
    /// the repeater, and superseded by a newer frame.
    #[test]
    fn a_held_classic_frame_is_encoded_once_and_a_newer_one_supersedes_it() {
        use windows::Win32::Foundation::HMODULE;
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
        use windows::Win32::Graphics::Direct3D11::{
            D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
        };
        let core = match x264_core(18, true) {
            Ok((c, _)) => c,
            Err(e) => return crate::test_support::skip("held classic frame", e),
        };
        let mut device = None;
        let mut context = None;
        let created = unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        let (Ok(()), Some(device), Some(context)) = (created, device, context) else {
            return crate::test_support::skip_because(
                "held classic frame",
                "no WARP device could be created",
            );
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 640,
            Height: 360,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut src: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut src)) }.unwrap();
        let src = src.unwrap();
        let mut core = core.lock_recover();
        let now = Instant::now();
        assert!(core.encode_held().is_none(), "nothing held yet");
        assert!(!core.hold_copy(&device, &context, &src, now).unwrap());
        assert!(
            core.hold_copy(&device, &context, &src, now).unwrap(),
            "replaced"
        );
        let (au, at) = core.encode_held().expect("held").unwrap();
        assert!(
            au.starts_with(&[0, 0, 0, 1]) && au.len() > 16,
            "{} bytes",
            au.len()
        );
        assert_eq!(at, now);
        assert!(core.encode_held().is_none(), "encoded once");
        assert!(!core.hold_copy(&device, &context, &src, now).unwrap());
        assert!(core.drop_held(), "a newer frame supersedes it");
        assert!(core.encode_held().is_none());
        assert!(!core.drop_held());
    }
}
