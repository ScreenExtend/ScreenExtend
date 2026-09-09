use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use tokio::sync::broadcast;

use crate::streamer::config::{Config, H264Profile};
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
    _activity: super::activity::LatencyActivity,
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

fn spawn_pipeline(
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
        // the acknowledgement direction plus `ForceLTRRefresh` for `request_repair`, written up in
        // `findings/major-vt-ltr-frames-would-replace-idr-on-every-pli.md`.
        //
        // The state still has to exist regardless: the transport writes feedback receipts into it
        // unconditionally, and `Encoder::supports_ltr()` already reports per session whether this
        // Mac's encoder advertises the property at all.
        repair: Arc::clone(&repair),
    };

    let encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;
    // The tier that actually started, not the one that was asked for: a session that fell back to
    // the software encoder reports "videotoolbox-software" here and in the compatibility report.
    telemetry.encoder.set(encoder.backend_name().into());

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
) -> Option<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("videotoolbox-drain".to_string())
        .spawn(move || {
            super::qos::pin_current_thread_user_interactive();
            super::qos::raise_current_thread_precedence();
            super::qos::pin_current_thread_encode_affinity();
            // One buffer for the life of the thread. `to_annexb` clears it first, so the only
            // per-frame allocation left is the `Bytes` the transport takes ownership of.
            let mut au: Vec<u8> = Vec::with_capacity(256 * 1024);
            while let Ok(mut sample) = output.recv() {
                let (capture, mut timing) = match sample.tag.take() {
                    Some(t) => (t.capture, t.timing),
                    None => (Instant::now(), FrameTiming::entering_capture()),
                };
                timing.mark_encode_done();
                sample.to_annexb(&mut au);
                if au.is_empty() {
                    continue;
                }
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
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

/// Tear the encoder down and stand a new one up.
///
/// The old session is released *before* the replacement is asked for. The hardware encoder is a
/// machine-wide, instance-limited resource, so creating the new one first is exactly how a
/// recoverable error turns into a permanent software fallback.
fn reset_encoder(
    encoder: &mut Option<Encoder>,
    drain: &mut Option<std::thread::JoinHandle<()>>,
    cfg: EncoderConfig,
    tx: &broadcast::Sender<EncodedFrame>,
    telemetry: &SharedTelemetry,
) -> bool {
    *encoder = None;
    if let Some(handle) = drain.take() {
        let _ = handle.join();
    }
    match Encoder::new(cfg) {
        Ok(e) => {
            telemetry.encoder.set(e.backend_name().into());
            let Some(handle) = spawn_drain(e.output(), tx.clone(), Arc::clone(telemetry)) else {
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
    } = l;

    // One scheduling scheme per thread. This one is a deadline thread and deliberately takes no
    // QoS class: a QoS class is how the kernel *manages* a thread's priority and a time-constraint
    // reservation is how that management is taken away, so setting both only means the last
    // writer wins and nothing in the log says which.
    super::qos::pin_current_thread_time_constraint(frame_duration.as_nanos() as u64);
    super::qos::pin_current_thread_encode_affinity();
    let _keep_awake = super::power::KeepAwake::begin();

    let mut drain = spawn_drain(encoder.output(), tx.clone(), Arc::clone(&telemetry));
    if drain.is_none() {
        return;
    }
    let mut encoder = Some(encoder);

    let mut last_frame: Option<Arc<super::frame::Frame>> = None;
    let mut bitrate_warned = false;
    let mut last_emit = Instant::now();
    let started = Instant::now();
    let mut seen_capture = false;
    let mut submit_errors = 0u32;
    let mut resets = 0u32;
    let mut last_output_drops = 0u64;
    let mut backpressure_skips = 0u64;
    let mut last_emitted = 0u64;
    let mut last_output_at = Instant::now();
    let mut max_in_flight_seen = 0usize;
    let mut stall_reported = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Some(enc) = encoder.as_mut() else {
            break;
        };

        source.wait(IDLE_POLL);
        let mut force_idr = idr_request.swap(false, Ordering::Relaxed);

        // A discarded access unit breaks the reference chain for everything after it. The only
        // repair is a fresh IDR, and the drop count is the only place the condition is visible.
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
        let cap = if stalled {
            HARD_MAX_FRAMES_IN_FLIGHT
        } else {
            MAX_FRAMES_IN_FLIGHT
        };
        let at_capacity = in_flight >= cap;
        if at_capacity {
            backpressure_skips += 1;
        }
        let fresh = if at_capacity {
            None
        } else {
            source.try_take_latest()
        };

        let mut submitted = None;
        if let Some(frame) = fresh {
            // The wake channel counts tokens; a token left behind by a frame that has already
            // been taken makes the next wait return instantly and the loop spin.
            source.drain_wake();
            if let Some(pixbuf) = frame.pixel_buffer() {
                let mut timing = FrameTiming::entering_capture();
                timing.capture_available_ns = frame.os_time_ns;
                submitted = Some(submit_one(
                    enc,
                    frame.captured_at,
                    timing,
                    pixbuf,
                    force_idr,
                ));
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
                    submitted = Some(submit_one(
                        enc,
                        frame.captured_at,
                        FrameTiming::repeating(),
                        pixbuf,
                        force_idr,
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
                    ));
                }
            }
        }

        match submitted {
            Some(true) => {
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
                        || !reset_encoder(&mut encoder, &mut drain, cfg, &tx, &telemetry)
                    {
                        teprintln!(
                            "[vt] giving up after {resets} encoder reset(s); ending the stream so \
                             clients are told rather than left on a frozen picture"
                        );
                        break;
                    }
                    submit_errors = 0;
                    idr_request.store(true, Ordering::Relaxed);
                }
            }
            None => {}
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
         {backpressure_skips} capture(s) skipped for back-pressure"
    );
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
) -> bool {
    timing.mark_encode_submit();
    match encoder.submit(
        pixbuf,
        force_idr,
        FrameTag {
            capture: captured_at,
            timing,
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
    super::ensure_screen_recording_access();
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
    let activity = super::activity::begin_latency_critical_activity();
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
    super::ensure_screen_recording_access();
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
    let activity = super::activity::begin_latency_critical_activity();
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

pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<()> {
    let (w, h) = if portrait {
        (height, width)
    } else {
        (width, height)
    };
    let hidpi = current_hidpi(device_name);
    reconfigure_display(device_name, w, h, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_orientation(device_name: &str, portrait: bool) -> Result<()> {
    let (w, h, refresh, hidpi) = backing_dims(device_name)?;
    let is_portrait_now = h > w;
    if is_portrait_now == portrait {
        return Ok(());
    }
    reconfigure_display(device_name, h, w, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_scale(device_name: &str, percent: u32) -> Result<()> {
    let hidpi = if percent >= HIDPI_THRESHOLD_PERCENT {
        1
    } else {
        0
    };
    crate::macos_utils::virtual_display::set_display_hidpi(device_name, hidpi)
        .map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_topology_extend() -> bool {
    false
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
        }
    }

    #[test]
    #[ignore]
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
    #[ignore]
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
}
