//! CGDisplayStream backend (fallback, macOS 10.15–12.2) — PRD §6.
//!
//! On this OS range CGDisplayStream is the genuine direct-to-WindowServer path
//! (PRD §3.4). It delivers an `IOSurface` per frame on a dispatch queue; we
//! retain it and wrap it once in a `CVPixelBuffer` (zero-copy — PRD §14.3), then
//! publish into the latest-frame sink. No Metal/CPU touch (invariants #1, #2).

// CGDisplayStream is deprecated in favor of ScreenCaptureKit, but on 10.15–12.2
// it is the genuine direct path and SCK does not exist — we use it deliberately.
#![allow(deprecated)]

use std::ffi::c_void;
use std::ptr;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2_core_foundation::{
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, kCFTypeDictionaryKeyCallBacks,
    kCFTypeDictionaryValueCallBacks,
};
use objc2_core_graphics::{
    CGDisplayStream, CGDisplayStreamFrameStatus, CGDisplayStreamUpdate, CGError,
    kCGDisplayStreamMinimumFrameTime, kCGDisplayStreamPreserveAspectRatio,
    kCGDisplayStreamQueueDepth, kCGDisplayStreamShowCursor,
};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferCreateWithIOSurface};
use objc2_io_surface::IOSurfaceRef;

use super::frame::{Backing, Frame, FrameSink};
use super::gpu::Gpu;
use super::mach::mach_now;
use super::{CaptureBackend, CaptureConfig, CaptureError, DisplayId};

/// The block CGDisplayStream copies and calls per frame. `dyn Fn` with exactly
/// the FFI handler arity (PRD §6.2).
type FrameHandler =
    RcBlock<dyn Fn(CGDisplayStreamFrameStatus, u64, *mut IOSurfaceRef, *const CGDisplayStreamUpdate)>;

pub struct CgDisplayStreamBackend {
    display_id: DisplayId,
    cfg: CaptureConfig,
    sink: Arc<FrameSink>,
    /// Count of `FrameComplete` frames published — read by the latency probe.
    frames: Arc<AtomicU64>,
    // Kept alive for the session: the stream, its delivery queue, and the block.
    stream: Option<CFRetained<CGDisplayStream>>,
    _queue: Option<DispatchRetained<DispatchQueue>>,
    _handler: Option<FrameHandler>,
}

// The retained CG/dispatch objects are only created/dropped here and are safe
// to release from any thread; the frame path is lock-free through the sink.
unsafe impl Send for CgDisplayStreamBackend {}

impl CgDisplayStreamBackend {
    pub fn new(
        display_id: DisplayId,
        _gpu: Arc<Gpu>,
        sink: Arc<FrameSink>,
        cfg: CaptureConfig,
    ) -> Result<Self, CaptureError> {
        Ok(Self {
            display_id,
            cfg,
            sink,
            frames: Arc::new(AtomicU64::new(0)),
            stream: None,
            _queue: None,
            _handler: None,
        })
    }

    /// Number of complete frames delivered so far (probe instrumentation).
    pub fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

/// Build the CGDisplayStream property dictionary (PRD §6.1): cap the frame rate,
/// keep the queue shallow (depth 2 — the dominant latency lever), composite the
/// cursor into the frame, no aspect-ratio letterboxing. Values are heterogeneous
/// (number + boolean), so we use the untyped CFDictionary create with CFType
/// callbacks.
fn make_cg_properties(fps: f64) -> CFRetained<CFDictionary> {
    let min_frame_time = CFNumber::new_f64(1.0 / fps.max(1.0));
    let queue_depth = CFNumber::new_isize(2);
    // Have WindowServer paint the hardware cursor straight into the captured
    // IOSurface before it hands it to us. This is the lowest-latency way to show
    // the cursor on macOS — the compositing happens on the window-server side
    // with zero extra passes or textures on our path (contrast the Windows DXGI
    // route, where Desktop Duplication delivers cursorless frames and we blend
    // the cursor ourselves). Mirrors WGC's `CursorCaptureSettings::WithCursor`.
    let show_cursor = CFBoolean::new(true);
    let preserve_ar = CFBoolean::new(false);

    // SAFETY: keys are the framework's static CFString property keys; values are
    // CF objects living until after the create call. CFDictionaryCreate retains
    // them. Pointers are passed as the untyped void* arrays the C API expects.
    unsafe {
        let keys: [*const c_void; 4] = [
            (kCGDisplayStreamMinimumFrameTime as *const CFString).cast(),
            (kCGDisplayStreamQueueDepth as *const CFString).cast(),
            (kCGDisplayStreamShowCursor as *const CFString).cast(),
            (kCGDisplayStreamPreserveAspectRatio as *const CFString).cast(),
        ];
        let values: [*const c_void; 4] = [
            (&*min_frame_time as *const CFNumber).cast(),
            (&*queue_depth as *const CFNumber).cast(),
            (show_cursor as *const CFBoolean).cast(),
            (preserve_ar as *const CFBoolean).cast(),
        ];
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .expect("CFDictionaryCreate for CGDisplayStream properties")
    }
}

/// The hot path (PRD §6.2/§6.3): forward only `FrameComplete` frames, retain the
/// surface, wrap it zero-copy in a `CVPixelBuffer`, publish the newest.
fn handle_frame(
    sink: &FrameSink,
    frames: &AtomicU64,
    width: usize,
    height: usize,
    status: CGDisplayStreamFrameStatus,
    surface: *mut IOSurfaceRef,
) {
    if status != CGDisplayStreamFrameStatus::FrameComplete {
        return; // Idle/Blank/Stopped carry no new pixels.
    }
    let Some(surface_ptr) = NonNull::new(surface) else {
        return;
    };

    // SAFETY: `surface` is valid for the callback's duration; we retain to keep
    // it alive in the Frame. CVPixelBufferCreateWithIOSurface just references
    // the surface (no copy); the out-pointer receives a +1 pixel buffer.
    unsafe {
        let surface_ret: CFRetained<IOSurfaceRef> = CFRetained::retain(surface_ptr);
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = CVPixelBufferCreateWithIOSurface(None, &surface_ret, None, NonNull::from(&mut out));
        let Some(out) = NonNull::new(out) else {
            return;
        };
        if r != 0 {
            return;
        }
        let pixbuf = CFRetained::from_raw(out);
        frames.fetch_add(1, Ordering::Relaxed);
        sink.publish(Frame {
            width,
            height,
            arrived_mach: mach_now(),
            captured_at: Instant::now(),
            backing: Backing::PixelBuffer { pixbuf, _surface: surface_ret },
        });
    }
}

impl CaptureBackend for CgDisplayStreamBackend {
    fn start(&mut self) -> Result<(), CaptureError> {
        let props = make_cg_properties(self.cfg.fps.max(1) as f64);

        // Dedicated serial queue for frame delivery, isolated from UI work
        // (PRD §7), targeted at the global USER_INTERACTIVE queue so WindowServer's
        // per-frame hand-off and our zero-copy IOSurface→CVPixelBuffer wrap run on
        // the performance cores ahead of normal work — this trims wakeup jitter on
        // the very first hop of the latency path, matching the QoS the encode and
        // drain threads already pin themselves to. We set the QoS via the target
        // queue rather than `set_qos_class_floor`, which hard-traps on 10.15.
        let ui_target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInteractive,
        ));
        let queue = DispatchQueue::new_with_target("com.screenextend.cgds", None, Some(&ui_target));

        let sink = self.sink.clone();
        let frames = self.frames.clone();
        let (w, h) = (self.cfg.width, self.cfg.height);
        let handler: FrameHandler = RcBlock::new(
            move |status: CGDisplayStreamFrameStatus,
                  _display_time: u64,
                  surface: *mut IOSurfaceRef,
                  _update: *const CGDisplayStreamUpdate| {
                handle_frame(&sink, &frames, w, h, status, surface);
            },
        );

        // SAFETY: all args validated; the stream copies the block and retains
        // the queue. 420f/BGRA FourCC passed as i32 (PRD §4.1).
        let stream = unsafe {
            CGDisplayStream::with_dispatch_queue(
                self.display_id,
                self.cfg.width,
                self.cfg.height,
                self.cfg.pixel_format as i32,
                Some(&props),
                &queue,
                RcBlock::as_ptr(&handler),
            )
        }
        .ok_or(CaptureError::StreamCreateFailed)?;

        // SAFETY: starting a freshly-created stream.
        let err = unsafe { CGDisplayStream::start(Some(&stream)) };
        if err != CGError::Success {
            teprintln!("[cgds] CGDisplayStreamStart failed: CGError {}", err.0);
            return Err(CaptureError::StartFailed);
        }

        tprintln!(
            "[cgds] capture started: display={} {}x{} @ {}fps (queueDepth=2)",
            self.display_id,
            self.cfg.width,
            self.cfg.height,
            self.cfg.fps
        );
        self.stream = Some(stream);
        self._queue = Some(queue);
        self._handler = Some(handler);
        Ok(())
    }

    fn stop(&mut self) {
        if let Some(stream) = &self.stream {
            // SAFETY: stopping a running stream; safe from any thread.
            let _ = unsafe { CGDisplayStream::stop(Some(stream)) };
        }
        self.stream = None;
        self._queue = None;
        self._handler = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos_utils::streamer::display;
    use crate::macos_utils::streamer::frame::frame_channel;
    use crate::macos_utils::streamer::mach::mach_age_ms;
    use crate::macos_utils::streamer::{CaptureConfig, PIXEL_FORMAT_420F};
    use std::time::{Duration, Instant};

    /// End-to-end check of the production path: create a virtual display (which
    /// now activates the real mode), capture it, then exercise the in-place
    /// device-setting setters (resolution + orientation) and re-capture after
    /// each change — mirroring what the server does on a join / settings edit.
    ///
    /// Run: `cargo test --lib cgds::tests::probe_virtual_display -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_virtual_display() {
        use crate::macos_utils::virtual_display::MacosVirtualDisplay;
        use crate::macos_utils::streamer::pipeline;

        fn try_capture(id: u32, w: u32, h: u32) -> (bool, u64) {
            let gpu = Gpu::new().unwrap();
            let (sink, _source) = frame_channel();
            let cfg = CaptureConfig {
                width: w as usize,
                height: h as usize,
                fps: 60,
                pixel_format: PIXEL_FORMAT_420F,
            };
            let mut backend = CgDisplayStreamBackend::new(id, gpu, sink, cfg).expect("backend");
            match backend.start() {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(400));
                    let n = backend.frames_captured();
                    backend.stop();
                    (true, n)
                }
                Err(_) => (false, 0),
            }
        }

        let vd = MacosVirtualDisplay::new_shared().expect("vd controller");

        let id = vd
            .create_display("ScreenExtend Probe".to_string(), 1280, 800, 60)
            .expect("create virtual display");
        let name = id.to_string();
        let g = display::display_geometry(id);
        println!("created id={id}, geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!((g.0, g.1), (1280, 800), "virtual display must boot at its real mode");
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@create: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work right after create");

        // In-place resolution change (device settings edit).
        pipeline::set_display_mode(&name, 1920, 1080, 60, false).expect("set 1920x1080");
        let g = display::display_geometry(id);
        println!("after set 1920x1080: geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!((g.0, g.1), (1920, 1080), "resolution change must take effect");
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@1920x1080: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after resolution change");

        // Portrait (orientation swap).
        pipeline::set_display_mode(&name, 1280, 800, 60, true).expect("set portrait");
        let g = display::display_geometry(id);
        println!("after portrait 800x1280: geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!((g.0, g.1), (800, 1280), "orientation swap must take effect");
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@portrait: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after orientation swap");

        // HiDPI / scale change (the macOS analogue of the Windows DPI setter):
        // ≥150% enables 2× Retina, so the logical resolution halves.
        pipeline::set_display_scale(&name, 200).expect("set scale 200%");
        let g = display::display_geometry(id);
        println!("after scale 200%: geometry(logical)={}x{}@{}", g.0, g.1, g.2);
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@hidpi: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after scale change");

        vd.remove_display(id);
        println!("removed id={id}");
    }

    /// Live CGDisplayStream probe (M1 acceptance). Captures the main display for
    /// ~2 s and reports delivered frames + capture→consumer latency.
    ///
    /// Ignored by default (needs a real display + Screen Recording permission).
    /// Run with: `cargo test --lib cgds::tests::probe_main_display -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_main_display() {
        let display_id = display::select_display(0).expect("a main display");
        let (w, h, refresh) = display::display_geometry(display_id);
        let fps = if refresh == 0 { 60 } else { refresh };
        println!("display {display_id}: {w}x{h} @ {fps}fps");

        let gpu = Gpu::new().unwrap();
        let (sink, source) = frame_channel();
        let cfg = CaptureConfig {
            width: w as usize,
            height: h as usize,
            fps: fps as i32,
            pixel_format: PIXEL_FORMAT_420F,
        };
        let mut backend =
            CgDisplayStreamBackend::new(display_id, gpu, sink, cfg).expect("backend");
        if let Err(e) = backend.start() {
            eprintln!(
                "capture did NOT start: {e:?}. On macOS 10.15 a null stream almost \
                 always means Screen Recording permission (TCC) is not granted to this \
                 binary. Grant it in System Preferences > Security & Privacy > Screen \
                 Recording, then rerun."
            );
            return;
        }

        // Poll the latest frame for 2s, sampling capture->grab latency.
        let mut samples: Vec<f64> = Vec::new();
        let mut wrapped_ok = 0usize;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Some(frame) = source.try_take_latest() {
                samples.push(mach_age_ms(frame.arrived_mach));
                if frame.pixel_buffer().is_some() {
                    wrapped_ok += 1;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        backend.stop();

        let delivered = backend.frames_captured();
        println!(
            "delivered={delivered} grabbed={} pixbuf_wrapped={wrapped_ok}",
            samples.len()
        );
        if !samples.is_empty() {
            samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = samples.iter().sum::<f64>() / samples.len() as f64;
            let p99_idx = ((samples.len() as f64 * 0.99) as usize).min(samples.len() - 1);
            let p99 = samples[p99_idx];
            println!("latency ms: mean={mean:.3} p99={p99:.3} max={:.3}", samples.last().unwrap());
        }

        assert!(delivered > 0, "no frames delivered (check Screen Recording permission / TCC)");
        assert!(wrapped_ok > 0, "IOSurface -> CVPixelBuffer wrap failed");
    }
}
