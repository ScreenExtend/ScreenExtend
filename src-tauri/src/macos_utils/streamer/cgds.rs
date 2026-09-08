#![allow(deprecated)]

use std::ffi::{c_char, c_void};
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2_core_foundation::{
    kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFBoolean, CFDictionary,
    CFNumber, CFRetained, CFString,
};
use objc2_core_graphics::{CGDisplayStream, CGDisplayStreamFrameStatus, CGDisplayStreamUpdate};
use objc2_core_video::{CVPixelBuffer, CVPixelBufferCreateWithIOSurface};
use objc2_io_surface::IOSurfaceRef;

use crate::streamer::telemetry::SharedTelemetry;

use super::frame::{Backing, Frame, FrameSink, HeldSurface};
use super::gpu::Gpu;
use super::mach::mach_now;
use super::{CaptureBackend, CaptureConfig, CaptureError, DisplayId, PIXEL_FORMAT_BGRA};

type FrameHandler = RcBlock<
    dyn Fn(CGDisplayStreamFrameStatus, u64, *mut IOSurfaceRef, *const CGDisplayStreamUpdate),
>;

// ---------------------------------------------------------------------------
// CGDisplayStream, resolved at runtime rather than linked against.
//
// The whole API is *obsoleted* — not merely deprecated — in the macOS 15 SDK, and this is the
// fallback tier, reached only where ScreenCaptureKit is absent. A link-time reference to a symbol
// Apple has announced the end of is a bet that the loader will keep finding it, and on the day it
// does not, dyld fails the *entire binary* before `main` and every other feature goes with it.
// That is the rule this codebase already applies to ScreenCaptureKit and to the newer VideoToolbox
// keys, applied in the other direction: to an API on its way out rather than one not yet in.
//
// `nm -u target/release/ScreenExtend | grep -i displaystream` must print nothing.
// ---------------------------------------------------------------------------

type CreateFn = unsafe extern "C" fn(
    display: u32,
    output_width: usize,
    output_height: usize,
    pixel_format: i32,
    properties: *const CFDictionary,
    queue: *mut c_void,
    handler: *mut c_void,
) -> *mut CGDisplayStream;
type StartFn = unsafe extern "C" fn(*mut CGDisplayStream) -> i32;
type StopFn = unsafe extern "C" fn(*mut CGDisplayStream) -> i32;
type DropCountFn = unsafe extern "C" fn(*const CGDisplayStreamUpdate) -> usize;
type RectsFn = unsafe extern "C" fn(*const CGDisplayStreamUpdate, i32, *mut usize) -> *const c_void;

struct CgDisplayStreamApi {
    create: CreateFn,
    start: StartFn,
    stop: StopFn,
    drop_count: DropCountFn,
    rects: RectsFn,
    key_queue_depth: &'static CFString,
    key_show_cursor: &'static CFString,
    key_preserve_aspect_ratio: &'static CFString,
    key_ycbcr_matrix: &'static CFString,
    ycbcr_709: &'static CFString,
}

// Function pointers and immutable framework string constants. `CFString` is not `Sync` because a
// CFString can be mutable in general; these five are process-lifetime constants inside
// CoreGraphics that nothing writes to.
unsafe impl Send for CgDisplayStreamApi {}
unsafe impl Sync for CgDisplayStreamApi {}

/// `kCGDisplayStreamUpdateDirtyRects`. The rect-type enumeration is stable ABI, and inlining it
/// keeps the whole API behind a single runtime lookup.
const UPDATE_DIRTY_RECTS: i32 = 2;

/// `kCGErrorSuccess`.
const CG_ERROR_SUCCESS: i32 = 0;

fn dlsym_ptr(symbol: &[u8]) -> Option<*mut c_void> {
    debug_assert_eq!(symbol.last(), Some(&0), "dlsym needs a NUL-terminated name");
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr() as *const c_char) };
    (!p.is_null()).then_some(p)
}

/// Resolve a CoreFoundation string constant exported as `const CFStringRef NAME`.
fn cf_constant(symbol: &[u8]) -> Option<&'static CFString> {
    let p = dlsym_ptr(symbol)?;
    unsafe { (*(p as *const *const CFString)).as_ref() }
}

fn cg_api() -> Option<&'static CgDisplayStreamApi> {
    static API: OnceLock<Option<CgDisplayStreamApi>> = OnceLock::new();
    API.get_or_init(|| {
        let api = CgDisplayStreamApi {
            create: unsafe {
                std::mem::transmute::<*mut c_void, CreateFn>(dlsym_ptr(
                    b"CGDisplayStreamCreateWithDispatchQueue\0",
                )?)
            },
            start: unsafe {
                std::mem::transmute::<*mut c_void, StartFn>(dlsym_ptr(b"CGDisplayStreamStart\0")?)
            },
            stop: unsafe {
                std::mem::transmute::<*mut c_void, StopFn>(dlsym_ptr(b"CGDisplayStreamStop\0")?)
            },
            drop_count: unsafe {
                std::mem::transmute::<*mut c_void, DropCountFn>(dlsym_ptr(
                    b"CGDisplayStreamUpdateGetDropCount\0",
                )?)
            },
            rects: unsafe {
                std::mem::transmute::<*mut c_void, RectsFn>(dlsym_ptr(
                    b"CGDisplayStreamUpdateGetRects\0",
                )?)
            },
            key_queue_depth: cf_constant(b"kCGDisplayStreamQueueDepth\0")?,
            key_show_cursor: cf_constant(b"kCGDisplayStreamShowCursor\0")?,
            key_preserve_aspect_ratio: cf_constant(b"kCGDisplayStreamPreserveAspectRatio\0")?,
            key_ycbcr_matrix: cf_constant(b"kCGDisplayStreamYCbCrMatrix\0")?,
            ycbcr_709: cf_constant(b"kCGDisplayStreamYCbCrMatrix_ITU_R_709_2\0")?,
        };
        Some(api)
    })
    .as_ref()
}

pub struct CgDisplayStreamBackend {
    display_id: DisplayId,
    cfg: CaptureConfig,
    sink: Arc<FrameSink>,
    frames: Arc<AtomicU64>,
    drops: Arc<AtomicU64>,
    wrap_ns: Arc<AtomicU64>,
    telemetry: SharedTelemetry,
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
        telemetry: SharedTelemetry,
    ) -> Result<Self, CaptureError> {
        Ok(Self {
            display_id,
            cfg,
            sink,
            frames: Arc::new(AtomicU64::new(0)),
            drops: Arc::new(AtomicU64::new(0)),
            wrap_ns: Arc::new(AtomicU64::new(0)),
            telemetry,
            stream: None,
            _queue: None,
            _handler: None,
        })
    }

    pub fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    /// Frames WindowServer produced and threw away because we were not keeping up, summed over
    /// the session. Apple documents `CGDisplayStreamUpdateGetDropCount` as *"primarily useful
    /// for performance measurement to determine if the client is keeping up with all
    /// WindowServer updates"* — it is the only capture-side back-pressure signal this API has.
    pub fn frames_dropped(&self) -> u64 {
        self.drops.load(Ordering::Relaxed)
    }

    /// Mean nanoseconds spent inside the delivery callback wrapping the surface and publishing
    /// it — the whole per-frame allocation cost this backend pays, measured rather than argued
    /// about. Two monotonic clock reads and one relaxed add per frame; no allocation.
    fn mean_wrap_ns(&self) -> Option<u64> {
        let frames = self.frames.load(Ordering::Relaxed);
        (frames > 0).then(|| self.wrap_ns.load(Ordering::Relaxed) / frames)
    }
}

/// Surfaces the stream keeps in flight.
///
/// Apple's own default is 3. Ours was 2, while the pipeline held two at once — the frame in the
/// latest-wins slot and the one pinned as `last_frame` for the keepalive — plus whatever
/// VideoToolbox retains, so the stream had nothing free to composite into. OBS uses 5 on the
/// same API; measured here, 2 collapses a 58 fps capture to one frame in six seconds the moment
/// an encoder joins the holding pattern.
const QUEUE_DEPTH: isize = 5;

fn make_cg_properties(api: &CgDisplayStreamApi) -> Option<CFRetained<CFDictionary>> {
    let queue_depth = CFNumber::new_isize(QUEUE_DEPTH);
    let show_cursor = CFBoolean::new(true);
    let preserve_ar = CFBoolean::new(false);

    // `kCGDisplayStreamMinimumFrameTime` is deliberately absent. Its documented default is
    // zero, so setting it added a throttle rather than relaxing one: after a delivery the
    // stream refuses to deliver again for that long, which delays every update that lands
    // mid-interval by up to a frame period (mean ~8 ms at 60 fps). OBS does not set it either.
    unsafe {
        let keys: [*const c_void; 4] = [
            (api.key_queue_depth as *const CFString).cast(),
            (api.key_show_cursor as *const CFString).cast(),
            (api.key_preserve_aspect_ratio as *const CFString).cast(),
            (api.key_ycbcr_matrix as *const CFString).cast(),
        ];
        let values: [*const c_void; 4] = [
            (&*queue_depth as *const CFNumber).cast(),
            (show_cursor as *const CFBoolean).cast(),
            (preserve_ar as *const CFBoolean).cast(),
            (api.ycbcr_709 as *const CFString).cast(),
        ];
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
}

struct HandlerCtx {
    api: &'static CgDisplayStreamApi,
    sink: Arc<FrameSink>,
    frames: Arc<AtomicU64>,
    drops: Arc<AtomicU64>,
    wrap_ns: Arc<AtomicU64>,
    telemetry: SharedTelemetry,
    published_one: AtomicBool,
    width: usize,
    height: usize,
}

fn handle_frame(
    ctx: &HandlerCtx,
    status: CGDisplayStreamFrameStatus,
    surface: *mut IOSurfaceRef,
    update: *const CGDisplayStreamUpdate,
    display_time: u64,
) {
    let (sink, frames, width, height) = (&ctx.sink, &ctx.frames, ctx.width, ctx.height);

    if let Some(update_ref) = unsafe { update.as_ref() } {
        // WindowServer counts what it had to throw away because our surfaces were still busy.
        // Three relaxed atomics on the delivery queue; no allocation, no logging.
        let dropped = unsafe { (ctx.api.drop_count)(update_ref) } as u64;
        if dropped != 0 {
            ctx.drops.fetch_add(dropped, Ordering::Relaxed);
            ctx.telemetry
                .degradation
                .frames_dropped_ring
                .fetch_add(dropped, Ordering::Relaxed);
        }
    }

    if status != CGDisplayStreamFrameStatus::FrameComplete {
        return;
    }

    // Skip a frame whose dirty-rect list is empty — nothing changed, so re-encoding it buys
    // nothing. Except the *first* one: a client that joins a still display (a freshly created
    // virtual display is exactly that, and it stays still until the user drags a window onto
    // it) would otherwise wait for the first change to see anything at all, and the 200 ms
    // keepalive cannot rescue it because the pipeline has no previous frame to repeat.
    if ctx.published_one.load(Ordering::Relaxed) {
        if let Some(update_ref) = unsafe { update.as_ref() } {
            let mut rect_count: usize = 0;
            unsafe { (ctx.api.rects)(update_ref, UPDATE_DIRTY_RECTS, &mut rect_count) };
            if rect_count == 0 {
                return;
            }
        }
    }

    let Some(surface_ptr) = NonNull::new(surface) else {
        return;
    };

    let wrap_started = mach_now();
    unsafe {
        let surface_ret: CFRetained<IOSurfaceRef> = CFRetained::retain(surface_ptr);
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = CVPixelBufferCreateWithIOSurface(None, &surface_ret, None, NonNull::from(&mut out));
        let Some(out) = NonNull::new(out) else {
            return;
        };
        // CoreVideo can return non-zero *and* hand back a +1 buffer; taking ownership first and
        // dropping it is what stops that path leaking one CVPixelBuffer per bad frame.
        let pixbuf = CFRetained::from_raw(out);
        if r != 0 {
            return;
        }
        frames.fetch_add(1, Ordering::Relaxed);
        sink.publish(Frame {
            width,
            height,
            arrived_mach: mach_now(),
            captured_at: Instant::now(),
            os_time_ns: super::clock::mach_ticks_to_host_ns(display_time),
            backing: Backing::PixelBuffer {
                pixbuf,
                _surface: HeldSurface::hold(surface_ret),
            },
        });
        // Only now is the gate armed, so a failed wrap does not consume the one free pass.
        ctx.published_one.store(true, Ordering::Relaxed);
        ctx.wrap_ns.fetch_add(
            super::mach::mach_delta_ns(wrap_started, mach_now()),
            Ordering::Relaxed,
        );
    }
}

impl CaptureBackend for CgDisplayStreamBackend {
    fn name(&self) -> &'static str {
        "cgdisplaystream"
    }

    fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        let Some(api) = cg_api() else {
            teprintln!(
                "[cgds] CGDisplayStream is not present on this system; there is no capture \
                 backend below ScreenCaptureKit"
            );
            return Err(CaptureError::StreamCreateFailed);
        };
        let props = make_cg_properties(api).ok_or(CaptureError::StreamCreateFailed)?;

        let ui_target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInteractive,
        ));
        let queue = DispatchQueue::new_with_target("com.screenextend.cgds", None, Some(&ui_target));

        let ctx = HandlerCtx {
            api,
            sink: self.sink.clone(),
            frames: self.frames.clone(),
            drops: self.drops.clone(),
            wrap_ns: self.wrap_ns.clone(),
            telemetry: Arc::clone(&self.telemetry),
            published_one: AtomicBool::new(false),
            width: self.cfg.width,
            height: self.cfg.height,
        };
        let handler: FrameHandler = RcBlock::new(
            move |status: CGDisplayStreamFrameStatus,
                  display_time: u64,
                  surface: *mut IOSurfaceRef,
                  update: *const CGDisplayStreamUpdate| {
                handle_frame(&ctx, status, surface, update, display_time);
            },
        );

        let stream = unsafe {
            (api.create)(
                self.display_id,
                self.cfg.width,
                self.cfg.height,
                self.cfg.pixel_format as i32,
                &*props,
                (&*queue as *const DispatchQueue).cast_mut().cast(),
                RcBlock::as_ptr(&handler).cast(),
            )
        };
        let stream = NonNull::new(stream).ok_or(CaptureError::StreamCreateFailed)?;
        // The create call is a `Create`, so the stream arrives +1 and this takes that ownership.
        let stream: CFRetained<CGDisplayStream> = unsafe { CFRetained::from_raw(stream) };

        let err = unsafe { (api.start)((&*stream as *const CGDisplayStream).cast_mut()) };
        if err != CG_ERROR_SUCCESS {
            teprintln!("[cgds] CGDisplayStreamStart failed: CGError {err}");
            return Err(CaptureError::StartFailed);
        }

        tprintln!(
            "[cgds] capture started: display={} {}x{} @ {}fps (queueDepth={QUEUE_DEPTH}, no minimumFrameTime)",
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
        if let (Some(api), Some(stream)) = (cg_api(), &self.stream) {
            let _ = unsafe { (api.stop)((&**stream as *const CGDisplayStream).cast_mut()) };
            tprintln!(
                "[cgds] capture stopped: {} frames published, {} dropped by WindowServer, \
                 {} ns mean in the delivery callback",
                self.frames_captured(),
                self.frames_dropped(),
                self.mean_wrap_ns().unwrap_or(0)
            );
        }
        self.stream = None;
        self._queue = None;
        self._handler = None;
    }
}

pub fn probe_screen_recording() -> bool {
    use std::sync::mpsc;
    use std::time::Duration;

    let Some(display_id) = super::display::select_display(0) else {
        return false;
    };

    let Some(api) = cg_api() else {
        return false;
    };
    let Some(props) = make_cg_properties(api) else {
        return false;
    };
    let ui_target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
        DispatchQoS::UserInteractive,
    ));
    let queue =
        DispatchQueue::new_with_target("com.screenextend.tcc-probe", None, Some(&ui_target));

    let (tx, rx) = mpsc::channel::<()>();
    let handler: FrameHandler = RcBlock::new(
        move |status: CGDisplayStreamFrameStatus,
              _display_time: u64,
              surface: *mut IOSurfaceRef,
              _update: *const CGDisplayStreamUpdate| {
            if status == CGDisplayStreamFrameStatus::FrameComplete && !surface.is_null() {
                let _ = tx.send(());
            }
        },
    );

    let stream = unsafe {
        (api.create)(
            display_id,
            64,
            64,
            PIXEL_FORMAT_BGRA as i32,
            &*props,
            (&*queue as *const DispatchQueue).cast_mut().cast(),
            RcBlock::as_ptr(&handler).cast(),
        )
    };
    let Some(stream) = NonNull::new(stream) else {
        return false;
    };
    let stream: CFRetained<CGDisplayStream> = unsafe { CFRetained::from_raw(stream) };
    let raw = (&*stream as *const CGDisplayStream).cast_mut();
    if unsafe { (api.start)(raw) } != CG_ERROR_SUCCESS {
        return false;
    }
    let got_frame = rx.recv_timeout(Duration::from_millis(700)).is_ok();
    let _ = unsafe { (api.stop)(raw) };
    got_frame
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos_utils::streamer::display;
    use crate::macos_utils::streamer::frame::frame_channel;
    use crate::macos_utils::streamer::mach::mach_age_ms;
    use crate::macos_utils::streamer::{CaptureConfig, PIXEL_FORMAT_420F};
    use crate::streamer::telemetry::SessionTelemetry;
    use std::time::{Duration, Instant};

    fn test_telemetry() -> SharedTelemetry {
        SessionTelemetry::new_shared()
    }

    /// The W5 measurement instrument: delivered frame rate, WindowServer drop count, and the
    /// `displayTime → our callback` delivery latency, on the main display with animated content.
    /// Every capture-side change in `docs/OPTIMIZATIONS.md` is A/B'd with this.
    ///
    /// The main display, not a virtual one: a freshly created `CGVirtualDisplay` has no windows
    /// and the cursor cannot be warped onto it, so it produces literally zero dirty rects and
    /// measures nothing (that emptiness is its own finding — see `probe_virtual_display`).
    #[test]
    #[ignore]
    fn capture_rate() {
        let id = display::select_display(0).expect("a main display");
        let (gw, gh, _) = display::display_geometry(id);
        run_capture_rate(id, gw, gh, "native");
    }

    fn run_capture_rate(id: u32, w: u32, h: u32, label: &str) {
        const SECONDS: u64 = 5;

        let (gw, gh, refresh) = display::display_geometry(id);
        println!("display {id}: {gw}x{gh} @ {refresh}fps, capturing {w}x{h} ({label})");

        let _cursor = display::CursorAnimation::start(id, gw, gh);

        let gpu = Gpu::new().unwrap();
        let (sink, source) = frame_channel();
        let cfg = CaptureConfig {
            width: w as usize,
            height: h as usize,
            fps: if refresh == 0 { 60 } else { refresh } as i32,
            pixel_format: PIXEL_FORMAT_420F,
        };
        let mut backend =
            CgDisplayStreamBackend::new(id, gpu, sink, cfg, test_telemetry()).expect("backend");
        backend.start().expect("capture starts");

        // Drain like the encode loop does, so the surface-holding pattern under test is the
        // production one: take the latest frame, hold it until the next arrives.
        let mut delivery_ms: Vec<f64> = Vec::new();
        let mut taken = 0usize;
        let mut no_os_stamp = 0usize;
        let mut held: Option<Arc<crate::macos_utils::streamer::frame::Frame>> = None;
        let deadline = Instant::now() + Duration::from_secs(SECONDS);
        while Instant::now() < deadline {
            source.wait(Duration::from_millis(4));
            if let Some(frame) = source.try_take_latest() {
                taken += 1;
                let arrived = super::super::clock::mach_ticks_to_host_ns(frame.arrived_mach);
                if frame.os_time_ns == 0 {
                    no_os_stamp += 1;
                } else {
                    // Signed: CGDisplayStream's `displayTime` is *"when the frame was to be
                    // displayed by the WindowServer"*, which can be ahead of our callback.
                    delivery_ms.push((arrived as i64 - frame.os_time_ns as i64) as f64 / 1e6);
                }
                held = Some(frame);
            }
        }
        if no_os_stamp > 0 {
            println!("frames with no displayTime: {no_os_stamp}/{taken}");
        }
        drop(held);
        let delivered = backend.frames_captured();
        let dropped = backend.frames_dropped();
        let wrap_ns = backend.mean_wrap_ns().unwrap_or(0);
        backend.stop();

        println!(
            "[{label}] delivered={delivered} ({:.1} fps) taken={taken} \
             windowserver_drops={dropped} callback_mean={wrap_ns}ns",
            delivered as f64 / SECONDS as f64
        );
        if delivery_ms.is_empty() {
            println!("[{label}] displayTime->callback: no OS timestamps");
        } else {
            delivery_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let p = |q: f64| {
                delivery_ms[((delivery_ms.len() as f64 * q) as usize).min(delivery_ms.len() - 1)]
            };
            println!(
                "[{label}] displayTime->callback ms: p50={:.2} p90={:.2} p99={:.2} max={:.2} (n={})",
                p(0.5),
                p(0.9),
                p(0.99),
                delivery_ms.last().unwrap(),
                delivery_ms.len()
            );
        }
        assert!(delivered > 0, "no frames delivered");
    }

    /// Does WindowServer's own downscale cost delivery latency?
    ///
    /// All scaling is delegated to it: the stream is created at the encode size and no
    /// `SourceRect`, `DestinationRect` or scaling-quality key is set, so a client asking for half
    /// resolution makes the compositor resample every frame before we see it. Run this next to
    /// `capture_rate` (which captures at native size) and compare the two `displayTime→callback`
    /// distributions. If they match, the scaler is free and `gpu.rs` has nothing to do.
    #[test]
    #[ignore]
    fn capture_rate_downscaled() {
        let id = display::select_display(0).expect("a main display");
        let (gw, gh, _) = display::display_geometry(id);
        run_capture_rate(id, (gw / 2) & !1, (gh / 2) & !1, "half scale");
    }

    #[test]
    #[ignore]
    fn probe_virtual_display() {
        use crate::macos_utils::streamer::pipeline;
        use crate::macos_utils::virtual_display::MacosVirtualDisplay;

        fn try_capture(id: u32, w: u32, h: u32) -> (bool, u64) {
            let gpu = Gpu::new().unwrap();
            let (sink, _source) = frame_channel();
            let cfg = CaptureConfig {
                width: w as usize,
                height: h as usize,
                fps: 60,
                pixel_format: PIXEL_FORMAT_420F,
            };
            let mut backend =
                CgDisplayStreamBackend::new(id, gpu, sink, cfg, test_telemetry()).expect("backend");
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
        assert_eq!(
            (g.0, g.1),
            (1280, 800),
            "virtual display must boot at its real mode"
        );
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@create: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work right after create");

        pipeline::set_display_mode(&name, 1920, 1080, 60, false).expect("set 1920x1080");
        let g = display::display_geometry(id);
        println!("after set 1920x1080: geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!(
            (g.0, g.1),
            (1920, 1080),
            "resolution change must take effect"
        );
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@1920x1080: ok={ok} frames={frames}");
        assert!(
            ok && frames > 0,
            "capture must work after resolution change"
        );

        pipeline::set_display_mode(&name, 1280, 800, 60, true).expect("set portrait");
        let g = display::display_geometry(id);
        println!("after portrait 800x1280: geometry={}x{}@{}", g.0, g.1, g.2);
        assert_eq!((g.0, g.1), (800, 1280), "orientation swap must take effect");
        let (ok, frames) = try_capture(id, g.0, g.1);
        println!("  capture@portrait: ok={ok} frames={frames}");
        assert!(ok && frames > 0, "capture must work after orientation swap");

        pipeline::set_display_scale(&name, 200).expect("set scale 200%");
        let g = display::display_geometry(id);
        println!(
            "after scale 200%: geometry(logical)={}x{}@{}",
            g.0, g.1, g.2
        );
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
        let mut backend = CgDisplayStreamBackend::new(display_id, gpu, sink, cfg, test_telemetry())
            .expect("backend");
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
            println!(
                "latency ms: mean={mean:.3} p99={p99:.3} max={:.3}",
                samples.last().unwrap()
            );
        }

        assert!(
            delivered > 0,
            "no frames delivered (check Screen Recording permission / TCC)"
        );
        assert!(wrapped_ok > 0, "IOSurface -> CVPixelBuffer wrap failed");
    }
}
