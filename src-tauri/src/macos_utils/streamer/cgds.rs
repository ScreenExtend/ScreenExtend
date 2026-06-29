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
    kCGDisplayStreamYCbCrMatrix, kCGDisplayStreamYCbCrMatrix_ITU_R_709_2,
};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferCreateWithIOSurface};
use objc2_io_surface::IOSurfaceRef;

use super::frame::{Backing, Frame, FrameSink};
use super::gpu::Gpu;
use super::mach::mach_now;
use super::{CaptureBackend, CaptureConfig, CaptureError, DisplayId};

type FrameHandler =
    RcBlock<dyn Fn(CGDisplayStreamFrameStatus, u64, *mut IOSurfaceRef, *const CGDisplayStreamUpdate)>;

pub struct CgDisplayStreamBackend {
    display_id: DisplayId,
    cfg: CaptureConfig,
    sink: Arc<FrameSink>,
    frames: Arc<AtomicU64>,
    stream: Option<CFRetained<CGDisplayStream>>,
    _queue: Option<DispatchRetained<DispatchQueue>>,
    _handler: Option<FrameHandler>,
}

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

    pub fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
}

fn make_cg_properties(fps: f64) -> CFRetained<CFDictionary> {
    let min_frame_time = CFNumber::new_f64(1.0 / fps.max(1.0));
    let queue_depth = CFNumber::new_isize(2);
    let show_cursor = CFBoolean::new(true);
    let preserve_ar = CFBoolean::new(false);

    unsafe {
        let keys: [*const c_void; 5] = [
            (kCGDisplayStreamMinimumFrameTime as *const CFString).cast(),
            (kCGDisplayStreamQueueDepth as *const CFString).cast(),
            (kCGDisplayStreamShowCursor as *const CFString).cast(),
            (kCGDisplayStreamPreserveAspectRatio as *const CFString).cast(),
            (kCGDisplayStreamYCbCrMatrix as *const CFString).cast(),
        ];
        let values: [*const c_void; 5] = [
            (&*min_frame_time as *const CFNumber).cast(),
            (&*queue_depth as *const CFNumber).cast(),
            (show_cursor as *const CFBoolean).cast(),
            (preserve_ar as *const CFBoolean).cast(),
            (kCGDisplayStreamYCbCrMatrix_ITU_R_709_2 as *const CFString).cast(),
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

fn handle_frame(
    sink: &FrameSink,
    frames: &AtomicU64,
    width: usize,
    height: usize,
    status: CGDisplayStreamFrameStatus,
    surface: *mut IOSurfaceRef,
) {
    if status != CGDisplayStreamFrameStatus::FrameComplete {
        return;
    }
    let Some(surface_ptr) = NonNull::new(surface) else {
        return;
    };

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

        pipeline::set_display_mode(&name, 1920, 1080, 60, false).expect("set 1920x1080");
        let g = display::display_geometry(id);
        println!("after set 1920x1080: geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!((g.0, g.1), (1920, 1080), "resolution change must take effect");
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@1920x1080: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after resolution change");

        pipeline::set_display_mode(&name, 1280, 800, 60, true).expect("set portrait");
        let g = display::display_geometry(id);
        println!("after portrait 800x1280: geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!((g.0, g.1), (800, 1280), "orientation swap must take effect");
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@portrait: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after orientation swap");

        pipeline::set_display_scale(&name, 200).expect("set scale 200%");
        let g = display::display_geometry(id);
        println!("after scale 200%: geometry(logical)={}x{}@{}", g.0, g.1, g.2);
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@hidpi: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after scale change");

        vd.remove_display(id);
        println!("removed id={id}");
    }

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
