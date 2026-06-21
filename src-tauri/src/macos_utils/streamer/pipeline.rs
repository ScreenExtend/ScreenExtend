//! The consumer-facing interface, identical in shape to
//! `windows_utils::streamer::pipeline`, re-exported by
//! `crate::streamer::pipeline` on macOS. This is what keeps `crate::streamer`
//! platform-neutral (PRD §0.1): the orchestration, transport, and WebRTC layers
//! call exactly these symbols regardless of OS.
//!
//! The encode loop ([`run_encode_loop`]) is the M3 integration glue (PRD
//! §14.1): it owns the [`FrameSource`] consumer and the VideoToolbox
//! [`Encoder`], pulls the newest captured frame, encodes it zero-copy, and
//! broadcasts the Annex B access unit. The glue is real safe Rust; the GPU
//! core, capture backends, and encoder it drives are the M1/M2 FFI seams.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use tokio::sync::broadcast;

use crate::streamer::config::{Config, H264Profile};

use super::config::{EncoderConfig, live_encoder_config};
use super::display;
use super::encoder::Encoder;
use super::frame::{FrameSource, frame_channel};
use super::gpu::Gpu;
use super::{CaptureBackend, CaptureConfig, DisplayId, PIXEL_FORMAT_420F, start_capture};

/// A compressed, transport-ready frame. Mirrors the Windows `EncodedFrame`:
/// same fields, same `Clone`, so `crate::streamer::webrtc_session` consumes it
/// unchanged.
#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
}

// 2 (was 3): tokio rounds up to a power of two, so 3 was really a 4-slot channel
// — up to 3 encoded frames could sit buffered behind a stalled writer (= tail
// latency). 2 caps the in-transport backlog at one frame while still tolerating
// the normal one-frame producer/consumer overlap. NOT 1: at capacity 1 an
// ordinary single-frame overlap overwrites the unread slot → RecvError::Lagged →
// the writer's lag handler requests an IDR every time → an IDR storm that
// saturates the weak Intel encoder. The Lagged→IDR resync then fires only on a
// genuine ≥2-frame stall, which is the intended drop-don't-buffer trip point.
const BROADCAST_CAPACITY: usize = 2;

/// Handle on a running encode pipeline. Field-for-field compatible with the
/// Windows `Pipeline` (the fields `webrtc_session` reads: `tx`,
/// `frame_duration`, `max_bitrate_bps`, `h264_profile`).
#[derive(Clone)]
pub struct Pipeline {
    pub tx: broadcast::Sender<EncodedFrame>,
    pub frame_duration: Duration,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    /// Wakes the encode loop the instant a control event lands (2.2), instead of
    /// letting it sit in its idle park for up to `idle` ms. Best-effort 1-cap.
    wake: crossbeam_channel::Sender<()>,
    pub max_bitrate_bps: u32,
    pub h264_profile: H264Profile,
}

impl Pipeline {
    pub fn request_idr(&self) {
        self.idr_request.store(true, Ordering::Relaxed);
        // Fire the loop now so a PLI is serviced in µs, not after the idle tick.
        let _ = self.wake.try_send(());
    }

    pub fn set_target_bitrate(&self, bps: u32) {
        self.target_bitrate.store(bps, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }
}

/// Owns a running capture+encode session and tears it down on [`stop`].
/// Mirrors the Windows `SessionCapture`.
pub struct SessionCapture {
    pub pipeline: Pipeline,
    control: Option<Box<dyn CaptureBackend>>,
    stop: Arc<AtomicBool>,
    /// The encode thread. Joined on teardown so the `Encoder` (and its
    /// `VTCompressionSession`) is actually dropped+invalidated before `stop`
    /// returns — otherwise a session created right after would race the still-
    /// alive HW encoder and fail with -12915.
    encode_thread: Option<std::thread::JoinHandle<()>>,
}

impl SessionCapture {
    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Idempotent teardown shared by `stop()` and `Drop`. Signals the encode loop,
    /// stops the capture backend, then **waits** for the encode thread to finish
    /// flushing, dropping, and invalidating the encoder so the hardware H.264
    /// encoder is fully released before we return. This is what lets the next
    /// session's `VTCompressionSessionCreate` succeed instead of racing the old
    /// session and hitting kVTVideoEncoderNotAvailableNowErr (-12915).
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
        // Safety net: if a `SessionCapture` (or its stopper closure) is dropped
        // without `stop()` being called, still tear down — otherwise the encode
        // loop would run forever and leak the HW encoder session.
        self.shutdown();
    }
}

/// Build the latency-critical pipeline for `enc`, spawn the encode loop fed by
/// `source`, and return the `Pipeline` handle. Shared by [`start`] and
/// [`start_on_monitor`]. (Mirrors the Windows `start_live_capture` shape.)
fn spawn_pipeline(
    enc: EncoderConfig,
    source: FrameSource,
) -> Result<(Pipeline, Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr_request = Arc::new(AtomicBool::new(false));
    let target_bitrate = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let frame_duration = Duration::from_nanos(1_000_000_000 / enc.fps.max(1) as u64);

    // Share the frame channel's wake sender so control events (IDR/bitrate) can
    // also fire the encode loop's park immediately (2.2).
    let wake = source.wake_sender();
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: enc.max_bitrate_bps,
        h264_profile: enc.profile,
    };

    let encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;

    let stop_thread = Arc::clone(&stop);
    let encode_thread = std::thread::Builder::new()
        .name("videotoolbox-encode".to_string())
        .spawn(move || {
            run_encode_loop(
                encoder,
                source,
                tx,
                idr_request,
                target_bitrate,
                stop_thread,
                frame_duration,
            )
        })
        .context("spawn encode thread")?;

    Ok((pipeline, stop, encode_thread))
}

/// The encode feed loop (PRD §14.1). Submits the newest captured frame to the
/// encoder (non-blocking) and applies pending bitrate/IDR requests; a sibling
/// drain thread turns the encoder's async output into broadcast `EncodedFrame`s.
/// Stops when capture drops the frame source.
///
/// Why a dedicated thread rather than encoding inside the capture callback (2.1):
/// the only cost of the split is one scheduler hop (~tens of µs at UserInteractive
/// QoS, and control events no longer wait for it — see 2.2). Encoding in the
/// callback would instead stall the capture handler — delaying the IOSurface's
/// return to WindowServer's pool (invariant #2) — and forfeit the clean
/// drop-latest producer/consumer split. The hop is not worth those costs, so the
/// split stays.
///
/// LTR loss recovery (12+): `Encoder::ltr_enabled` turns on long-term references,
/// but PLI recovery here is a full IDR — which is the *correct* endpoint for this
/// product. Upgrading to a ForceLTRRefresh against an *acknowledged* LTR (the
/// IDR-free recovery) requires the receiver to ack per-frame LTR tokens, and a
/// standard browser WHEP receiver has no such mechanism in WebRTC (loss recovery
/// there is NACK/PLI/FIR). So LTR-refresh is only reachable with a custom receiver
/// that carries the token on RTP and acks it back through the shared
/// `webrtc_session`; the encoder side is ready for that day, and until then
/// PLI→IDR is both correct and standard.
fn run_encode_loop(
    mut encoder: Encoder,
    source: FrameSource,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    frame_duration: Duration,
) {
    // Latency hardening (PRD §7): pin this thread to the top QoS (P-core bias),
    // add a real-time time-constraint contract sized to the frame period (2.5),
    // and disable App Nap / timer coalescing for the whole session. Held until
    // the loop ends.
    super::qos::pin_current_thread_user_interactive();
    super::qos::pin_current_thread_time_constraint(frame_duration.as_nanos() as u64);
    let _activity = super::activity::begin_latency_critical_activity();

    // Per-submit capture-time FIFO: the producer (this loop) pushes one stamp per
    // `encode_frame`; the drain pops one per emitted access unit. Output is
    // strictly in order (no B-frames), so front == oldest.
    let pending_ts: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));

    let drain_ts = Arc::clone(&pending_ts);
    let output = encoder.output();
    let drain = std::thread::Builder::new()
        .name("videotoolbox-drain".to_string())
        .spawn(move || {
            super::qos::pin_current_thread_user_interactive();
            // AVCC→Annex B conversion the callback no longer does (2.3): converting
            // here frees the VideoToolbox callback sooner. Build each access unit
            // straight into a right-sized `Vec` and hand it to `Bytes` with no copy
            // (`Bytes::from(Vec)` is O(1)) — avoiding the per-frame full-AU memcpy a
            // reused scratch + `copy_from_slice` would incur (material on keyframes).
            // `to_annexb` reserves exactly what it needs, so the buffer grows once.
            while let Ok(sample) = output.recv() {
                let capture = drain_ts.lock().unwrap().pop_front().unwrap_or_else(Instant::now);
                let mut au: Vec<u8> = Vec::new();
                sample.to_annexb(&mut au);
                if au.is_empty() {
                    continue;
                }
                let _ = tx.send(EncodedFrame { data: Bytes::from(au), capture });
            }
        })
        .expect("spawn encode-drain thread");

    // Wake-driven: block on the frame wake channel so a real change is picked up
    // within microseconds (no polling delay). On macOS 10.15 VideoToolbox holds
    // one frame inside the session despite `MaxFrameDelayCount=0`, so a lone
    // change would sit in the encoder until the *next* submit pushed it out. We
    // defeat that ~1-frame hold with a flush copy (re-submitting the same buffer
    // pushes the held frame out now), but *adaptively*: only when no newer frame is
    // already waiting. Under sustained motion the next real frame flushes the
    // previous one for free, so an unconditional flush copy there would just double
    // the encoder + wire load for nothing — which is what saturated the weak Intel
    // iGPU encoder at 1080p. So the copy fires only for the trailing frame (motion
    // just stopped) or when there is headroom, where killing the hold matters and
    // the re-encode of an identical image (a few-byte skip-coded P-frame) is cheap.
    // Forwarded flush copies extend the P-frame reference chain, so they are never
    // dropped downstream. (On macOS 11+/Apple Silicon `EnableLowLatencyRateControl`
    // gives true one-in/one-out and removes the need for any flush copy.)
    //
    // `idle` only governs how often we wake while the screen is *static* — long
    // enough to stay quiet, short enough that a PLI (IDR request) is serviced
    // promptly even with nothing else happening. A real frame always wakes us
    // immediately via the publish channel, regardless of this timeout.
    let idle = Duration::from_millis(4);
    let keepalive = Duration::from_millis(200);
    let mut last_frame: Option<Arc<super::frame::Frame>> = None;
    let mut last_emit = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Park until a frame is published or the idle tick elapses.
        source.wait(idle);
        let force_idr = idr_request.swap(false, Ordering::Relaxed);
        // Apply a pending BWE bitrate AFTER the park, not before it. A bitrate
        // update fires the same wake channel `source.wait` blocks on, so reading
        // it here lets the cut take effect on the frame we're about to submit
        // this iteration — one capture-frame sooner than reading it at the top of
        // the next loop. The atomic holds the latest value until swapped, so this
        // loses no update. set_bitrate pushes nothing into the capture FIFO, so
        // the 1:1 submit/pop pairing is untouched.
        let pending = target_bitrate.swap(0, Ordering::Relaxed);
        if pending != 0 {
            if let Err(e) = encoder.set_bitrate(pending) {
                teprintln!("set_bitrate failed (target_bps={pending}): {e:?}");
            }
        }

        if let Some(frame) = source.try_take_latest() {
            if let Some(pixbuf) = frame.pixel_buffer() {
                if !submit_one(&mut encoder, &pending_ts, frame.captured_at, pixbuf, force_idr) {
                    teprintln!("encode submit failed; stopping encode loop");
                    break;
                }
                // Adaptive flush-copy (10.15 one-frame-hold defeat, 1.4): re-submit
                // to push this frame out of VideoToolbox's hold ONLY when nothing
                // newer is already queued. If a newer frame is waiting, the next
                // real submit flushes this one for free, so a copy here would be a
                // pure-waste second encode — the 2x load that saturated the iGPU
                // encoder under sustained 1080p motion. When caught up (trailing
                // frame / headroom) the copy is cheap and kills the ~1-frame hold.
                if !encoder.low_latency() && !source.peek_has_frame() {
                    let _ = submit_one(&mut encoder, &pending_ts, frame.captured_at, pixbuf, false);
                }
                last_emit = Instant::now();
            }
            last_frame = Some(frame);
        } else if force_idr || last_emit.elapsed() >= keepalive {
            // Static screen: service a pending IDR right away (PLI recovery), or
            // emit a slow keepalive so a freshly-attached/recovering decoder has a
            // recent frame. No successor is coming, so flush the re-encode out now.
            if let Some(frame) = last_frame.as_ref() {
                if let Some(pixbuf) = frame.pixel_buffer() {
                    let captured = frame.captured_at;
                    if !submit_one(&mut encoder, &pending_ts, captured, pixbuf, force_idr) {
                        break;
                    }
                    if !encoder.low_latency() {
                        let _ = submit_one(&mut encoder, &pending_ts, captured, pixbuf, false);
                    }
                    last_emit = Instant::now();
                }
            }
        }
    }

    let _ = encoder.flush();
    drop(encoder); // closes the output channel → drain thread exits
    let _ = drain.join();
}

/// Submit one frame's `pixbuf` for encoding, recording its capture timestamp in
/// the FIFO so the drain can pair the emitted access unit back to when the pixels
/// were captured — one stamp per submit, kept in step with the encoder's
/// one-AU-per-submit output (no B-frames → strictly in order). Returns `false`
/// only on a fatal encode error, which stops the loop.
///
/// The macOS 10.15 flush copy (re-submitting the same buffer to push it out of
/// VideoToolbox's one-frame hold) is driven *adaptively* by the encode loop —
/// which simply calls this again with the same buffer when, and only when, no
/// newer frame is already waiting. See the loop's adaptive flush-copy note.
fn submit_one(
    encoder: &mut Encoder,
    pending_ts: &Mutex<VecDeque<Instant>>,
    captured_at: Instant,
    pixbuf: &objc2_core_video::CVImageBuffer,
    force_idr: bool,
) -> bool {
    pending_ts.lock().unwrap().push_back(captured_at);
    if let Err(e) = encoder.submit(pixbuf, force_idr) {
        teprintln!("encode submit failed: {e:?}");
        pending_ts.lock().unwrap().pop_back();
        return false;
    }
    true
}

/// Start capturing the configured monitor (PRD §8 wiring). On macOS the monitor
/// is selected by `CGDirectDisplayID` derived from `cfg.monitor`.
pub fn start(cfg: &Config) -> Result<Pipeline> {
    // Trigger the Screen-Recording (TCC) prompt on first run if needed (5.5).
    super::ensure_screen_recording_access();
    let display = select_display(cfg.monitor)?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let enc = live_encoder_config(native_w, native_h, refresh, cfg);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();

    // Capture at the display refresh (not the encode fps) so a change is
    // delivered on the next composite, never throttled below the panel's rate.
    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let backend = start_capture(display, gpu, sink, cap_cfg).context("starting capture backend")?;

    let (pipeline, _stop, _encode_thread) = spawn_pipeline(enc, source)?;
    // `start` returns the handle only; keep the backend + encode thread alive
    // for the process lifetime (detach the join handle).
    std::mem::forget(backend);
    Ok(pipeline)
}

/// Start capture for a specific virtual display, returning a stoppable session.
/// The `device_name` is the platform display identifier the server tracks
/// (mirrors the Windows `start_on_monitor`).
pub fn start_on_monitor(cfg: &Config, device_name: &str) -> Result<SessionCapture> {
    super::ensure_screen_recording_access();
    let display = display_by_name(device_name)
        .with_context(|| format!("resolving display {device_name}"))?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let enc = live_encoder_config(native_w, native_h, refresh, cfg);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();

    // Capture at the display refresh (not the encode fps) so a change is
    // delivered on the next composite, never throttled below the panel's rate.
    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let backend =
        start_capture(display, gpu, sink, cap_cfg).context("starting capture backend")?;

    let (pipeline, stop, encode_thread) = spawn_pipeline(enc, source)?;
    Ok(SessionCapture {
        pipeline,
        control: Some(backend),
        stop,
        encode_thread: Some(encode_thread),
    })
}

/// Encode synthetic frames at one bitrate, reconfigure live to a second, and
/// report the achieved rate of each phase — the macOS twin of the Windows
/// `probe_bitrate` (4.1). Self-contained: no capture / Screen-Recording needed.
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
        intra_refresh: cfg.intra_refresh,
    };
    let mut encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;
    let out = encoder.output();

    // Two phases of synthetic frames; measure bytes emitted in each.
    let phase = |encoder: &mut Encoder, out: &crossbeam_channel::Receiver<super::encoder::CompressedSample>, tick0: usize| -> usize {
        let mut buf: Vec<u8> = Vec::new();
        let mut bytes = 0usize;
        for i in 0..120usize {
            if let Some(pb) = make_synthetic_bgra(w as usize, h as usize, tick0 + i) {
                let _ = encoder.submit(&pb, i == 0);
                if !encoder.low_latency() {
                    let _ = encoder.submit(&pb, false);
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

    // 120 frames at 60fps = 2.0s per phase.
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

/// Capture + encode live frames to an Annex B file for a manual sanity check
/// (4.1) — the macOS twin of the Windows `probe_live`. Exercises the real
/// capture→encoder→Annex B path; needs a display + Screen-Recording permission.
pub fn probe_live(cfg: &Config, path: &str) -> Result<()> {
    use std::io::Write;

    let display = select_display(cfg.monitor)?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let enc = live_encoder_config(native_w, native_h, refresh, cfg);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();
    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let mut backend =
        start_capture(display, gpu, sink, cap_cfg).context("starting capture backend")?;
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
                encoder.submit(pixbuf, first).context("submit")?;
                first = false;
                if !encoder.low_latency() {
                    let _ = encoder.submit(pixbuf, false);
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

/// A synthetic BGRA `CVPixelBuffer` with a per-tick gradient (gives the encoder
/// motion → real P-frames). Used by [`probe_bitrate`]; no capture required.
fn make_synthetic_bgra(w: usize, h: usize, tick: usize) -> Option<CFRetained<CVPixelBuffer>> {
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    const BGRA: u32 = 0x4247_5241; // kCVPixelFormatType_32BGRA
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    // SAFETY: standard CVPixelBufferCreate; out-pointer receives a +1 buffer.
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
    // SAFETY: lock for CPU write, fill, unlock — single-threaded probe use.
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

// ---------------------------------------------------------------------------
// Display enumeration (CoreGraphics) + virtual-display control surface.
//
// Enumeration is implemented in `super::display`. The `set_display_*` mutators
// change a *live* virtual display's mode/orientation/scale in place — the macOS
// analogue of the Windows `ChangeDisplaySettingsExW` / DisplayConfig path —
// by re-applying `CGVirtualDisplaySettings` and switching the active CoreGraphics
// mode (see `crate::macos_utils::virtual_display::reconfigure_display`). The
// `device_name` is the `CGDirectDisplayID` rendered as a decimal string.
// ---------------------------------------------------------------------------

use crate::macos_utils::virtual_display::reconfigure_display;

/// HiDPI (2× Retina) kicks in at this display-scale percentage and above. macOS
/// scaling is effectively binary (1×/2×), so we map the Windows-style percent
/// onto the nearest of the two — the closest analogue of the DisplayConfig DPI
/// setter.
const HIDPI_THRESHOLD_PERCENT: u32 = 150;

fn select_display(monitor: u32) -> Result<DisplayId> {
    display::select_display(monitor).context("no active display to capture")
}

fn display_by_name(device_name: &str) -> Result<DisplayId> {
    display::display_by_name(device_name)
        .with_context(|| format!("display {device_name} is not active"))
}

/// `(native_width, native_height, refresh_hz)` for a `CGDirectDisplayID`.
fn display_geometry(display: DisplayId) -> (u32, u32, u32) {
    display::display_geometry(display)
}

/// Active display identifiers (analogue of the Windows device names).
pub fn monitor_device_names() -> Vec<String> {
    display::device_names()
}

/// Native pixel size of a display, by identifier.
pub fn monitor_dimensions(device_name: &str) -> Option<(u32, u32)> {
    display::dimensions_by_name(device_name)
}

/// Map a current HiDPI flag back to the active mode's pixel size. `display_geometry`
/// reports the mode's *logical* points, which for an active HiDPI mode is half the
/// pixels — so to re-apply settings we need the pixel size the descriptor expects.
fn pixel_dims(device_name: &str) -> Result<(u32, u32, u32)> {
    let id = display::display_by_name(device_name)
        .with_context(|| format!("display {device_name} is not active"))?;
    let (w, h, refresh) = display::display_geometry(id);
    if w == 0 || h == 0 {
        anyhow::bail!("display {device_name} reports zero geometry");
    }
    Ok((w, h, refresh))
}

pub fn set_display_resolution(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
) -> Result<()> {
    reconfigure_display(device_name, width, height, refresh, 0).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<()> {
    let (w, h) = if portrait { (height, width) } else { (width, height) };
    reconfigure_display(device_name, w, h, refresh, 0).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_orientation(device_name: &str, portrait: bool) -> Result<()> {
    let (w, h, refresh) = pixel_dims(device_name)?;
    let is_portrait_now = h > w;
    if is_portrait_now == portrait {
        return Ok(());
    }
    // Swap to the requested orientation.
    reconfigure_display(device_name, h, w, refresh, 0).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_scale(device_name: &str, percent: u32) -> Result<()> {
    let (w, h, refresh) = pixel_dims(device_name)?;
    let hidpi = if percent >= HIDPI_THRESHOLD_PERCENT { 1 } else { 0 };
    reconfigure_display(device_name, w, h, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))
}

/// Extend the desktop onto newly-attached virtual displays (Windows root
/// `set_display_topology_extend` analogue). No-op on macOS, where attached
/// displays extend by default.
pub fn set_display_topology_extend() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::config::H264Profile;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::cgds::CgDisplayStreamBackend;
    use super::super::config::EncoderConfig;
    use super::super::encoder::Encoder;
    use super::super::mach::{mach_age_ms, mach_now};
    use super::super::{CaptureBackend, CaptureConfig, PIXEL_FORMAT_420F};

    /// M3 acceptance: the real zero-copy capture→encode path end-to-end.
    /// CGDisplayStream (420f IOSurface) → CVPixelBuffer → VideoToolbox H.264,
    /// measuring capture→encoded latency (PRD §9, M3 gate <12ms).
    ///
    /// Needs the GUI session + Screen Recording permission; run via
    /// `sudo launchctl asuser <uid> <bin> pipeline::tests::end_to_end_latency
    /// --ignored --nocapture`. CGDisplayStream is change-driven, so run some
    /// screen activity alongside it to get a useful sample count.
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
        let mut backend = CgDisplayStreamBackend::new(display, gpu, sink, cap_cfg).unwrap();
        backend.start().expect("capture starts");

        let enc_cfg = EncoderConfig {
            width: (w & !1),
            height: (h & !1),
            fps,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 12_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            intra_refresh: false,
        };
        let mut encoder = Encoder::new(enc_cfg).expect("encoder");
        let out = encoder.output();

        // FIFO of (arrived_mach, is_new): feed pushes per submit, drain pops per
        // AU (in-order, no B-frames). Latency is measured only for `is_new`
        // submits — the real screen changes; re-submitted keepalives are skipped.
        let ts: Arc<Mutex<VecDeque<(u64, bool)>>> = Arc::new(Mutex::new(VecDeque::new()));
        let drain_ts = Arc::clone(&ts);
        let drain = std::thread::spawn(move || {
            let mut lat: Vec<f64> = Vec::new();
            let mut bytes = 0usize;
            let mut keyframes = 0usize;
            let mut new_frames = 0usize;
            let mut buf: Vec<u8> = Vec::new();
            while let Ok(cs) = out.recv() {
                let tag = drain_ts.lock().unwrap().pop_front();
                let is_keyframe = cs.to_annexb(&mut buf);
                if buf.is_empty() {
                    continue;
                }
                if let Some((a, is_new)) = tag {
                    if is_new {
                        lat.push(mach_age_ms(a));
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

        // Feed loop. The 10.15 HW encoder buffers a frame until the next one
        // pushes it through (MaxFrameDelayCount=0 is not truly honored). So when
        // no new frame arrives shortly after a real one, we re-submit the last
        // frame — a tiny duplicate P-frame (NOT an IDR) that flushes the pending
        // real frame out at low latency.
        // Bounded keepalive burst: after each real change, fire a few duplicate
        // P-frames at a tight cadence to flush the HW encoder's pipeline (it
        // holds frames despite MaxFrameDelayCount=0), then go quiet. Low latency
        // for real changes without continuously encoding a static screen.
        // Wake-driven feed loop, mirroring the production `run_encode_loop`.
        super::super::qos::pin_current_thread_user_interactive();
        let idle = Duration::from_millis(1);
        let keepalive_budget = 4u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let mut first = true;
        let mut submitted = 0usize;
        let mut last_frame: Option<std::sync::Arc<super::super::frame::Frame>> = None;
        let mut budget = 0u32;
        while std::time::Instant::now() < deadline {
            source.wait(idle);
            if let Some(frame) = source.try_take_latest() {
                if let Some(pixbuf) = frame.pixel_buffer() {
                    ts.lock().unwrap().push_back((frame.arrived_mach, true));
                    encoder.submit(pixbuf, first).expect("submit");
                    first = false;
                    submitted += 1;
                    last_frame = Some(frame);
                    budget = keepalive_budget; // refill the flush burst
                }
            } else if budget > 0 {
                if let Some(lf) = last_frame.as_ref() {
                    if let Some(pixbuf) = lf.pixel_buffer() {
                        ts.lock().unwrap().push_back((lf.arrived_mach, false));
                        encoder.submit(pixbuf, false).ok();
                    }
                    budget -= 1;
                }
            }
        }
        encoder.flush().ok();
        backend.stop();
        drop(encoder); // close output → drain ends
        let (mut lat, bytes, keyframes, new_frames) = drain.join().unwrap();

        println!(
            "submitted={submitted} new_frames={new_frames} keyframes={keyframes} bytes={bytes}"
        );
        assert!(submitted > 0, "no frames captured (idle screen? add activity)");
        assert!(!lat.is_empty(), "no encoded frames");
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = lat.iter().sum::<f64>() / lat.len() as f64;
        let p50 = lat[lat.len() / 2];
        let p99 = lat[((lat.len() as f64 * 0.99) as usize).min(lat.len() - 1)];
        println!(
            "capture→encoded latency ms (new frames): mean={mean:.3} p50={p50:.3} p99={p99:.3} max={:.3}",
            lat.last().unwrap()
        );
    }

    /// Smoke test of the production `spawn_pipeline` / `run_encode_loop` path:
    /// real capture → encoder → broadcast `EncodedFrame`s, confirming the encode
    /// thread, drain thread, keepalive, and stop flag all work together.
    ///
    /// Run via `sudo launchctl asuser <uid> <bin>
    /// pipeline::tests::production_pipeline_smoke --ignored --nocapture` with
    /// screen activity.
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
        let mut backend = CgDisplayStreamBackend::new(display, gpu, sink, cap_cfg).unwrap();
        backend.start().expect("capture starts");

        let enc = EncoderConfig {
            width: w & !1,
            height: h & !1,
            fps,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 12_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            intra_refresh: false,
        };
        let (pipeline, stop, _encode_thread) = spawn_pipeline(enc, source).expect("pipeline");
        let mut rx = pipeline.tx.subscribe();
        pipeline.request_idr();

        let mut count = 0usize;
        let mut bytes = 0usize;
        let mut lat: Vec<f64> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ef) => {
                    count += 1;
                    bytes += ef.data.len();
                    lat.push(ef.capture.elapsed().as_secs_f64() * 1000.0);
                }
                Err(_) => std::thread::sleep(Duration::from_micros(500)),
            }
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        backend.stop();

        println!("production pipeline: broadcast {count} EncodedFrames, {bytes} bytes");
        if !lat.is_empty() {
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = lat.iter().sum::<f64>() / lat.len() as f64;
            println!(
                "submit→broadcast latency ms: mean={mean:.3} p50={:.3} max={:.3}",
                lat[lat.len() / 2],
                lat.last().unwrap()
            );
        }
        assert!(count > 0, "production pipeline produced no encoded frames");
    }
}
