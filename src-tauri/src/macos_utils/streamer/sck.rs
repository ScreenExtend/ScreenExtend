//! ScreenCaptureKit backend (primary, macOS 12.3+).
//!
//! Dyld safety is load-bearing: SCK does not exist on 10.15, and any link-time reference to it
//! (`objc2-screen-capture-kit`, an `extern_class!`, a 12.3+ `extern static`) stops the whole
//! binary loading there. Runtime interop only: [`AnyClass::get`], [`msg_send!`], a
//! [`ClassBuilder`] delegate, and `dlsym` for the string constants ([`sck_optional_cfstring`]).

use std::ffi::{c_void, CStr};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString};
use objc2_core_media::CMSampleBuffer;
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferCreateWithIOSurface, CVPixelBufferGetIOSurface,
};
use objc2_foundation::NSString;

use crate::streamer::telemetry::SharedTelemetry;

use super::frame::{Backing, Frame, FrameSink, HeldSurface};
use super::gpu::Gpu;
use super::mach::mach_now;
use super::{CaptureBackend, CaptureConfig, CaptureError, DisplayId};

/// `SCFrameStatusComplete`, the only status that carries new pixels. Inlined (stable ABI value)
/// so the 12.3+ enum symbol is never referenced.
const SC_FRAME_STATUS_COMPLETE: i64 = 0;

/// `SCFrameStatusStarted`: the first frame after `startCapture`, carrying the whole display.
/// Must be forwarded, or a client joining a still screen waits for the first thing to move.
const SC_FRAME_STATUS_STARTED: i64 = 4;

/// `SCStreamOutputType.screen` (= 0), inlined like `SC_FRAME_STATUS_COMPLETE`.
const SC_OUTPUT_TYPE_SCREEN: isize = 0;

/// `CFNumberType.kCFNumberSInt64Type` (= 4), the type tag for reading the frame status out of
/// its `CFNumber`. Stable CoreFoundation ABI.
const K_CF_NUMBER_S_INT64_TYPE: isize = 4;

/// Runtime name of the delegate class. Unique to this app so it cannot collide with a
/// framework class.
const DELEGATE_CLASS_NAME: &CStr = c"ScreenExtendSckOutput";
/// The ivar (a single boxed-context pointer) the delegate carries.
const CTX_IVAR_NAME: &CStr = c"ctx";

/// Resolve an SCK CFString constant by its C symbol name, or `None` when the
/// running framework does not export it (always `None` on < 12.3).
fn sck_optional_cfstring(symbol: &CStr) -> Option<&'static CFString> {
    // SAFETY: when present the symbol is `const CFStringRef NAME`, so dlsym returns `&NAME`
    // (a `*const CFStringRef`): deref once. A framework static, hence 'static.
    unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, symbol.as_ptr());
        if p.is_null() {
            return None;
        }
        (*(p as *const *const CFString)).as_ref()
    }
}

/// Heap context handed to the runtime-built delegate through its ivar. Holds
/// everything the hot path needs; reclaimed when the backend stops.
struct OutputCtx {
    sink: Arc<FrameSink>,
    frames: Arc<AtomicU64>,
    width: usize,
    height: usize,
    /// Resolved once at start (not per frame): the SCK attachment keys. `None`
    /// only on an OS without SCK, where this code never runs.
    status_key: Option<&'static CFString>,
    dirty_rects_key: Option<&'static CFString>,
    /// Cleared until one frame has actually been published. The dirty-rect skip is right for a
    /// screen nobody is looking at and wrong for a client that has never seen a pixel.
    published_one: AtomicBool,
}

pub struct SckBackend {
    display_id: DisplayId,
    #[allow(dead_code)]
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
    /// Count of complete frames published, read by the latency probe.
    frames: Arc<AtomicU64>,
    #[allow(dead_code)]
    telemetry: SharedTelemetry,
    // kept alive for the session:
    stream: Option<Retained<AnyObject>>,
    delegate: Option<Retained<AnyObject>>,
    _content_filter: Option<Retained<AnyObject>>,
    _queue: Option<DispatchRetained<DispatchQueue>>,
    /// Raw boxed `OutputCtx` stored in the delegate's ivar; reclaimed on stop.
    ctx: *mut OutputCtx,
}

// the retained objc2/dispatch objects are safe to release from any thread, and the raw `ctx`
// pointer is owned solely by this struct (set on start, freed on stop)
unsafe impl Send for SckBackend {}

impl SckBackend {
    pub fn new(
        display_id: DisplayId,
        gpu: Arc<Gpu>,
        sink: Arc<FrameSink>,
        cfg: CaptureConfig,
        telemetry: SharedTelemetry,
    ) -> Result<Self, CaptureError> {
        Ok(Self {
            display_id,
            gpu,
            sink,
            cfg,
            frames: Arc::new(AtomicU64::new(0)),
            telemetry,
            stream: None,
            delegate: None,
            _content_filter: None,
            _queue: None,
            ctx: ptr::null_mut(),
        })
    }
}

/// Look a class up at runtime; an error where it does not exist (10.15).
fn sck_class(name: &CStr) -> Result<&'static AnyClass, CaptureError> {
    AnyClass::get(name).ok_or(CaptureError::StreamCreateFailed)
}

/// Take ownership of a +1 object without another retain. `None` if null.
///
/// # Safety
/// `obj` must be +1-owned (`alloc`/`init`/`new`). An autoreleased factory result is not: retain
/// that explicitly instead.
unsafe fn retain_new(obj: *mut AnyObject) -> Option<Retained<AnyObject>> {
    let ptr = NonNull::new(obj)?;
    unsafe { Retained::from_raw(ptr.as_ptr()) }
}

/// The `SCStreamOutput` delegate class, registered once. Built with `ClassBuilder` so the typed
/// protocol (a 12.3+ symbol) is never referenced: SCK dispatches via `respondsToSelector:`, so
/// the method existing is what matters.
fn delegate_class() -> &'static AnyClass {
    use std::sync::OnceLock;
    static CLS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLS.get_or_init(|| {
        // already registered (or a name clash): reuse it
        if let Some(existing) = AnyClass::get(DELEGATE_CLASS_NAME) {
            return existing as *const AnyClass as usize;
        }
        let superclass = class_nsobject();
        let mut builder = ClassBuilder::new(DELEGATE_CLASS_NAME, superclass)
            .expect("ClassBuilder for SCK output delegate");

        builder.add_ivar::<*mut c_void>(CTX_IVAR_NAME);

        // SAFETY: the IMP's signature matches `-stream:didOutputSampleBuffer:ofType:` (id, SEL,
        // id, id, NSInteger). The receiver is `*mut AnyObject`, not `&AnyObject`:
        // `MethodImplementation` needs a function type that is not higher-ranked over a lifetime.
        unsafe {
            builder.add_method(
                sel!(stream:didOutputSampleBuffer:ofType:),
                did_output_sample_buffer
                    as extern "C-unwind" fn(
                        *mut AnyObject,
                        Sel,
                        *mut AnyObject,
                        *mut AnyObject,
                        isize,
                    ),
            );
        }

        // best effort: SCK only needs `respondsToSelector:`, and the protocol is not
        // registered on 10.15
        if let Some(proto) = objc2::runtime::AnyProtocol::get(c"SCStreamOutput") {
            builder.add_protocol(proto);
        }

        builder.register() as *const AnyClass as usize
    });
    // SAFETY: the stored pointer is a `&'static AnyClass` reinterpreted as usize.
    unsafe { &*(ptr as *const AnyClass) }
}

/// `NSObject` fetched at runtime, not through `objc2::class!`, to keep the module link-free.
fn class_nsobject() -> &'static AnyClass {
    AnyClass::get(c"NSObject").expect("NSObject is always present")
}

impl CaptureBackend for SckBackend {
    fn name(&self) -> &'static str {
        "screencapturekit"
    }

    fn frames_captured(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }

    fn start(&mut self) -> Result<(), CaptureError> {
        // re-check that SCK is present: on 10.15 these fail and nothing touches the framework
        let cls_content = sck_class(c"SCShareableContent")?;
        let cls_filter = sck_class(c"SCContentFilter")?;
        let cls_config = sck_class(c"SCStreamConfiguration")?;
        let cls_stream = sck_class(c"SCStream")?;

        let display = resolve_display(cls_content, self.display_id)?;

        // alloc/init through raw pointers, wrapping the +1 result in `Retained::from_raw`:
        // `msg_send!`'s typed retain handling needs a concrete `Message` type that these
        // runtime-only classes do not have
        let arr_cls = sck_class(c"NSArray")?; // NSArray is universal; fetched at runtime
        let empty_excludes: *mut AnyObject = unsafe { msg_send![arr_cls, array] };
        let filter = unsafe {
            let alloc: *mut AnyObject = msg_send![cls_filter, alloc];
            let f: *mut AnyObject = msg_send![
                alloc,
                initWithDisplay: &*display,
                excludingWindows: empty_excludes,
            ];
            retain_new(f).ok_or(CaptureError::StreamCreateFailed)?
        };

        let config = unsafe {
            let alloc: *mut AnyObject = msg_send![cls_config, alloc];
            let c: *mut AnyObject = msg_send![alloc, init];
            retain_new(c).ok_or(CaptureError::StreamCreateFailed)?
        };
        configure_stream(&config, &self.cfg);

        let status_key = sck_optional_cfstring(c"SCStreamFrameInfoStatus");
        let dirty_rects_key = sck_optional_cfstring(c"SCStreamFrameInfoDirtyRects");
        if status_key.is_none() {
            // without the status key incomplete frames cannot be filtered out
            teprintln!("[sck] SCStreamFrameInfoStatus unavailable; cannot start SCK capture");
            return Err(CaptureError::StreamCreateFailed);
        }
        let ctx = Box::into_raw(Box::new(OutputCtx {
            sink: self.sink.clone(),
            frames: self.frames.clone(),
            width: self.cfg.width,
            height: self.cfg.height,
            status_key,
            dirty_rects_key,
            published_one: AtomicBool::new(false),
        }));

        let delegate = unsafe {
            let d: *mut AnyObject = msg_send![delegate_class(), new];
            let d = retain_new(d).ok_or(CaptureError::StreamCreateFailed)?;
            if !set_ctx_ivar(&d, ctx) {
                drop(Box::from_raw(ctx));
                teprintln!("[sck] the output delegate has no ctx ivar; cannot start SCK capture");
                return Err(CaptureError::StreamCreateFailed);
            }
            d
        };

        // the output delegate doubles as the SCStreamDelegate
        let stream = unsafe {
            let alloc: *mut AnyObject = msg_send![cls_stream, alloc];
            let s: *mut AnyObject = msg_send![
                alloc,
                initWithFilter: &*filter,
                configuration: &*config,
                delegate: &*delegate,
            ];
            retain_new(s).ok_or(CaptureError::StreamCreateFailed)?
        };

        // serial delivery queue at UserInteractive, same as cgds
        let ui_target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInteractive,
        ));
        let queue = DispatchQueue::new_with_target("com.screenextend.sck", None, Some(&ui_target));

        let mut add_err: *mut AnyObject = ptr::null_mut();
        let added: bool = unsafe {
            let r: Bool = msg_send![
                &*stream,
                addStreamOutput: &*delegate,
                type: SC_OUTPUT_TYPE_SCREEN,
                sampleHandlerQueue: &*queue,
                error: &mut add_err,
            ];
            r.as_bool()
        };
        if !added {
            drop(unsafe { Box::from_raw(ctx) });
            teprintln!("[sck] addStreamOutput:type:sampleHandlerQueue:error: failed");
            return Err(CaptureError::StreamCreateFailed);
        }

        let (tx, rx) = mpsc::channel::<Option<String>>();
        let handler = RcBlock::new(move |error: *mut AnyObject| {
            let msg = if error.is_null() {
                None
            } else {
                Some(nserror_description(error))
            };
            let _ = tx.send(msg);
        });
        unsafe {
            let _: () = msg_send![
                &*stream,
                startCaptureWithCompletionHandler: RcBlock::as_ptr(&handler),
            ];
        }
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(None) => {}
            Ok(Some(detail)) => {
                drop(unsafe { Box::from_raw(ctx) });
                teprintln!("[sck] startCapture failed: {detail}");
                return Err(CaptureError::StartFailed);
            }
            Err(_) => {
                drop(unsafe { Box::from_raw(ctx) });
                teprintln!("[sck] startCapture timed out");
                return Err(CaptureError::StartFailed);
            }
        }

        tprintln!(
            "[sck] capture started: display={} {}x{} @ {}fps \
             (queueDepth={}, minimumFrameInterval={}/fps)",
            self.display_id,
            self.cfg.width,
            self.cfg.height,
            self.cfg.fps,
            sck_knobs().queue_depth.unwrap_or(QUEUE_DEPTH),
            sck_knobs().min_interval_factor.unwrap_or(0.9),
        );

        self.ctx = ctx;
        self.stream = Some(stream);
        self.delegate = Some(delegate);
        self._content_filter = Some(filter);
        self._queue = Some(queue);
        Ok(())
    }

    fn stop(&mut self) {
        // Order matters. Null the delegate's context first: a handler already enqueued on the
        // delivery queue then returns instead of dereferencing a box about to be freed. Neither
        // releasing the queue nor `stopCapture` is a barrier against an in-flight handler.
        if let Some(delegate) = &self.delegate {
            // SAFETY: `delegate` is an instance of our own class, which declares the ivar.
            unsafe { set_ctx_ivar(delegate, ptr::null_mut()) };
            // return ignored: the queue barrier below is what makes the free safe
        }
        if let Some(stream) = &self.stream {
            // wait for the stop so the delegate is not called again before its context is freed
            let (tx, rx) = mpsc::channel::<()>();
            let handler = RcBlock::new(move |_error: *mut AnyObject| {
                let _ = tx.send(());
            });
            // SAFETY: stopping a running stream; safe from any thread.
            unsafe {
                let _: () = msg_send![
                    &**stream,
                    stopCaptureWithCompletionHandler: RcBlock::as_ptr(&handler),
                ];
            }
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
        // drain the serial delivery queue: everything enqueued before this block has run when
        // exec_sync returns, so nothing is inside the handler when the context is freed
        if let Some(queue) = &self._queue {
            queue.exec_sync(|| {});
        }
        // release the stream and delegate (detaches the output) before freeing the context
        self.stream = None;
        self.delegate = None;
        self._content_filter = None;
        self._queue = None;
        if !self.ctx.is_null() {
            // SAFETY: ctx was created via Box::into_raw in start(); the stream and
            // its delegate are now released, so no callback can still read it.
            drop(unsafe { Box::from_raw(self.ctx) });
            self.ctx = ptr::null_mut();
        }
    }
}

impl Drop for SckBackend {
    fn drop(&mut self) {
        // free the context even if stop() was never called
        if self.stream.is_some() || !self.ctx.is_null() {
            self.stop();
        }
    }
}

/// Store the boxed `OutputCtx` pointer into the delegate's ivar.
///
/// # Safety
/// `delegate` must be an instance of [`delegate_class`] (which declares the
/// `ctx` ivar of type `*mut c_void`).
unsafe fn set_ctx_ivar(delegate: &AnyObject, ctx: *mut OutputCtx) -> bool {
    let Some(ivar) = delegate.class().instance_variable(CTX_IVAR_NAME) else {
        return false;
    };
    // SAFETY: the ivar was declared as `*mut c_void` on this exact class.
    let slot = unsafe { ivar.load_ptr::<*mut c_void>(delegate) };
    unsafe { *slot = ctx.cast::<c_void>() };
    true
}

/// Read the boxed `OutputCtx` pointer back out of the delegate's ivar.
///
/// # Safety
/// `delegate` must be an instance of [`delegate_class`].
unsafe fn get_ctx_ivar(delegate: &AnyObject) -> *mut OutputCtx {
    let Some(ivar) = delegate.class().instance_variable(CTX_IVAR_NAME) else {
        return ptr::null_mut();
    };
    // SAFETY: declared as `*mut c_void` on this class.
    let slot = unsafe { ivar.load_ptr::<*mut c_void>(delegate) };
    unsafe { (*slot).cast::<OutputCtx>() }
}

/// Resolve the `SCDisplay` whose `displayID == display_id`, bridging the async
/// `getShareableContentWithCompletionHandler:` to a synchronous result.
fn resolve_display(
    cls_content: &AnyClass,
    display_id: DisplayId,
) -> Result<Retained<AnyObject>, CaptureError> {
    // the retained SCDisplay crosses the channel as a usize so the value is Send
    let (tx, rx) = mpsc::channel::<Option<usize>>();
    let handler = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
        if !error.is_null() || content.is_null() {
            let _ = tx.send(None);
            return;
        }
        // SAFETY: `content` is a valid (+0, autoreleased) SCShareableContent for
        // the block's duration.
        let content_ref = unsafe { &*content };
        let found = unsafe { find_display(content_ref, display_id) };
        let _ = tx.send(found.map(|d| {
            // leak a +1 retain across the channel; reclaimed by `from_raw` below
            Retained::into_raw(d) as usize
        }));
    });

    // SAFETY: standard class method; the block is copied by SCK and invoked once.
    unsafe {
        let _: () = msg_send![
            cls_content,
            getShareableContentWithCompletionHandler: RcBlock::as_ptr(&handler),
        ];
    }

    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(raw)) => {
            let ptr = NonNull::new(raw as *mut AnyObject).ok_or(CaptureError::DisplayNotFound)?;
            // SAFETY: re-wrap the +1 retain leaked in the block.
            unsafe { Retained::from_raw(ptr.as_ptr()) }.ok_or(CaptureError::DisplayNotFound)
        }
        Ok(None) => Err(CaptureError::DisplayNotFound),
        Err(_) => Err(CaptureError::ShareableContent),
    }
}

/// Iterate `content.displays` and return the `SCDisplay` whose `displayID`
/// matches, retained.
///
/// # Safety
/// `content` must be a valid `SCShareableContent`.
unsafe fn find_display(content: &AnyObject, display_id: DisplayId) -> Option<Retained<AnyObject>> {
    let displays: Retained<AnyObject> = unsafe { msg_send![content, displays] };
    let count: usize = unsafe { msg_send![&*displays, count] };
    for i in 0..count {
        let display: *mut AnyObject = unsafe { msg_send![&*displays, objectAtIndex: i] };
        if display.is_null() {
            continue;
        }
        // `-[SCDisplay displayID]` returns a CGDirectDisplayID (u32)
        let id: u32 = unsafe { msg_send![&*display, displayID] };
        if id == display_id {
            // SAFETY: retain the +0 element to hand it back across the bridge.
            return unsafe { Retained::retain(display) };
        }
    }
    None
}

/// Surfaces ScreenCaptureKit keeps in flight. Depth trades memory against stalling, not latency,
/// and this pipeline holds surfaces from the queue (the latest-wins slot plus the encode loop's
/// keepalive frame). 8 is Apple's own documented default (it was 3 in the 12.3 SDK).
const QUEUE_DEPTH: isize = 8;

/// Capture A/B switches, read once from the environment (the bench `live` stage sets them; a
/// shipped host never does). Unset means the default:
///
/// - `SE_SCK_QUEUE_DEPTH=<n>`: `queueDepth` (default [`QUEUE_DEPTH`]).
/// - `SE_SCK_MIN_INTERVAL=<factor>`: `minimumFrameInterval` as a multiple of the frame period
///   (default 0.9; `0` sets `kCMTimeZero`, i.e. no throttle; `1.0` is exactly 1/fps).
/// - `SE_SCK_PIXEL_FORMAT=420f|420v`: the delivered pixel format (default: the pipeline's).
/// - `SE_SCK_SHOWS_CURSOR=0|1`: `showsCursor` (default 1).
#[derive(Clone, Debug, Default)]
struct SckKnobs {
    queue_depth: Option<isize>,
    min_interval_factor: Option<f64>,
    pixel_format: Option<u32>,
    shows_cursor: Option<bool>,
    rejected: Vec<String>,
}

fn sck_knobs() -> &'static SckKnobs {
    static KNOBS: std::sync::OnceLock<SckKnobs> = std::sync::OnceLock::new();
    KNOBS.get_or_init(|| {
        let mut k = SckKnobs::default();
        let var = |n: &str| crate::envcfg::var(n).ok().filter(|v| !v.trim().is_empty());
        if let Some(v) = var("SE_SCK_QUEUE_DEPTH") {
            match v.trim().parse::<isize>() {
                Ok(n) if (1..=64).contains(&n) => k.queue_depth = Some(n),
                _ => k.rejected.push(format!("SE_SCK_QUEUE_DEPTH={v}")),
            }
        }
        if let Some(v) = var("SE_SCK_MIN_INTERVAL") {
            match v.trim().parse::<f64>() {
                Ok(f) if (0.0..=4.0).contains(&f) => k.min_interval_factor = Some(f),
                _ => k.rejected.push(format!("SE_SCK_MIN_INTERVAL={v}")),
            }
        }
        if let Some(v) = var("SE_SCK_PIXEL_FORMAT") {
            match v.trim() {
                "420f" => k.pixel_format = Some(0x3432_3066),
                "420v" => k.pixel_format = Some(0x3432_3076),
                _ => k.rejected.push(format!("SE_SCK_PIXEL_FORMAT={v}")),
            }
        }
        if let Some(v) = var("SE_SCK_SHOWS_CURSOR") {
            match v.trim() {
                "1" | "true" | "on" => k.shows_cursor = Some(true),
                "0" | "false" | "off" => k.shows_cursor = Some(false),
                _ => k.rejected.push(format!("SE_SCK_SHOWS_CURSOR={v}")),
            }
        }
        for r in &k.rejected {
            teprintln!("[sck] ignoring {r} (out of range or unrecognised)");
        }
        k
    })
}

/// What the last `configure_stream` set, for a measurement to record.
static APPLIED: std::sync::Mutex<Option<serde_json::Value>> = std::sync::Mutex::new(None);

/// What the most recent SCK stream was configured with, and which values came from an
/// `SE_SCK_*` switch. `None` until a stream has been configured.
pub fn applied_knobs() -> Option<serde_json::Value> {
    APPLIED.lock().ok().and_then(|g| g.clone())
}

fn configure_stream(config: &AnyObject, cfg: &CaptureConfig) {
    let fps = cfg.fps.max(1);
    // 0.9 of a frame period: exactly 1/fps beats against the compositor's cadence (OpenDisplay saw
    // ~51 fps from a 60 fps request, and OBS ships the same 10 % shortening). The property is a
    // floor on delivery spacing, so the shorter interval costs nothing.
    let knobs = sck_knobs();
    let interval = match knobs.min_interval_factor {
        None => objc2_core_media::CMTime {
            value: 9,
            timescale: fps.saturating_mul(10),
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        },
        // kCMTimeZero: deliver as fast as the compositor produces
        Some(f) if f <= 0.0 => objc2_core_media::CMTime {
            value: 0,
            timescale: 1,
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        },
        Some(f) => objc2_core_media::CMTime {
            value: (f * 1000.0).round() as i64,
            timescale: fps.saturating_mul(1000),
            flags: objc2_core_media::CMTimeFlags::Valid,
            epoch: 0,
        },
    };
    let queue_depth = knobs.queue_depth.unwrap_or(QUEUE_DEPTH);
    let pixel_format = knobs.pixel_format.unwrap_or(cfg.pixel_format);
    // on by default: WindowServer composites the cursor into the frame, as with CGDisplayStream
    let shows_cursor = knobs.shows_cursor.unwrap_or(true);
    // SAFETY: each selector is a public `SCStreamConfiguration` property setter
    // with the documented argument type. `width`/`height`/`queueDepth` take
    // NSInteger (isize); `pixelFormat` takes an OSType (u32); the bools take BOOL.
    unsafe {
        let _: () = msg_send![config, setWidth: cfg.width as isize];
        let _: () = msg_send![config, setHeight: cfg.height as isize];
        let _: () = msg_send![config, setMinimumFrameInterval: interval];
        let _: () = msg_send![config, setPixelFormat: pixel_format];
        let _: () = msg_send![config, setQueueDepth: queue_depth];
        let _: () = msg_send![config, setShowsCursor: shows_cursor];
        let _: () = msg_send![config, setCapturesAudio: false];
    }
    if let Ok(mut g) = APPLIED.lock() {
        *g = Some(serde_json::json!({
            "queue_depth": queue_depth,
            "queue_depth_from_env": knobs.queue_depth.is_some(),
            "minimum_frame_interval_s": if interval.value == 0 { 0.0 } else {
                interval.value as f64 / interval.timescale.max(1) as f64
            },
            "minimum_frame_interval_factor": knobs.min_interval_factor.unwrap_or(0.9),
            "minimum_frame_interval_from_env": knobs.min_interval_factor.is_some(),
            "pixel_format": String::from_utf8_lossy(&pixel_format.to_be_bytes()),
            "pixel_format_from_env": knobs.pixel_format.is_some(),
            "shows_cursor": shows_cursor,
            "shows_cursor_from_env": knobs.shows_cursor.is_some(),
            "rejected": knobs.rejected,
            "fps": fps,
        }));
    }
    // same colour handling as the CGDisplayStream backend: both tiers feed one encoder, and
    // primaries that depend on which capture tier started would be a silent colour shift
    set_optional_constant(
        config,
        sel!(setColorMatrix:),
        c"kCVImageBufferYCbCrMatrix_ITU_R_709_2",
    );
    set_optional_constant(config, sel!(setColorSpaceName:), c"kCGColorSpaceSRGB");
}

/// Set a property whose value is a framework CFString constant, when both halves exist.
///
/// Both are probed: `respondsToSelector:` for the setter, because these arrived after 12.3, and
/// `dlsym` for the value, because referencing the constant would be a link-time dependency.
fn set_optional_constant(config: &AnyObject, selector: Sel, symbol: &CStr) {
    let responds: Bool = unsafe { msg_send![config, respondsToSelector: selector] };
    if !responds.as_bool() {
        return;
    }
    let Some(value) = sck_optional_cfstring(symbol) else {
        return;
    };
    let raw = (value as *const CFString).cast::<AnyObject>();
    // SAFETY: both setters take a CFStringRef, which is toll-free bridged to the object pointer
    // `performSelector:withObject:` passes.
    unsafe {
        let _: *mut AnyObject = msg_send![config, performSelector: selector, withObject: raw];
    }
}

/// The hot path: C-ABI IMP for `-[ScreenExtendSckOutput stream:didOutputSampleBuffer:ofType:]`.
///
/// `extern "C-unwind"` so a Rust panic is not UB crossing the ObjC boundary.
extern "C-unwind" fn did_output_sample_buffer(
    this: *mut AnyObject,
    _cmd: Sel,
    _stream: *mut AnyObject,
    sample: *mut AnyObject,
    of_type: isize,
) {
    if of_type != SC_OUTPUT_TYPE_SCREEN {
        return; // ignore audio / microphone
    }
    let Some(this) = NonNull::new(this) else {
        return;
    };
    // SAFETY: `this` is a live instance of our delegate class for the call.
    let this = unsafe { this.as_ref() };
    let Some(sample) = NonNull::new(sample) else {
        return;
    };
    // SAFETY: `this` is an instance of our delegate class with the ctx ivar.
    let ctx_ptr = unsafe { get_ctx_ivar(this) };
    if ctx_ptr.is_null() {
        return;
    }
    // SAFETY: ctx lives until stop() frees it, after the stream is released; the
    // delivery queue is drained before that.
    let ctx = unsafe { &*ctx_ptr };

    let sample = sample.cast::<CMSampleBuffer>();
    // SAFETY: valid CMSampleBuffer for the callback's duration.
    let sample_ref = unsafe { sample.as_ref() };

    if !frame_should_publish(sample_ref, ctx) {
        return;
    }

    publish_sample(sample_ref, ctx);
}

/// Decide from the `SCStreamFrameInfo` attachment whether to forward the frame: only
/// `SCFrameStatusComplete` or `Started`, and not when the dirty-rects attachment is present but
/// empty (nothing changed).
fn frame_should_publish(sample: &CMSampleBuffer, ctx: &OutputCtx) -> bool {
    let attachments: CFRetained<CFArray> = match unsafe { sample.sample_attachments_array(false) } {
        Some(a) if a.count() > 0 => a,
        _ => return false,
    };
    let dict_ptr = unsafe { attachments.value_at_index(0) } as *const CFDictionary;
    let Some(dict) = (unsafe { dict_ptr.as_ref() }) else {
        return false;
    };

    let Some(status_key) = ctx.status_key else {
        return false;
    };
    let status_val =
        unsafe { CFDictionaryGetValue(dict, status_key as *const CFString as *const c_void) };
    if status_val.is_null() {
        return false;
    }
    let mut status: i64 = -1;
    let ok = unsafe {
        CFNumberGetValue(
            status_val as *const CFNumber,
            K_CF_NUMBER_S_INT64_TYPE,
            (&mut status as *mut i64).cast::<c_void>(),
        )
    };
    if ok == 0 || (status != SC_FRAME_STATUS_COMPLETE && status != SC_FRAME_STATUS_STARTED) {
        return false;
    }

    // the first frame and any `Started` frame are published whatever the dirty rects say: a
    // client that has seen nothing needs one, and nothing moves on a fresh virtual display
    if status == SC_FRAME_STATUS_STARTED || !ctx.published_one.load(Ordering::Relaxed) {
        return true;
    }

    // dirty-rect skip: an attachment that is present and empty means nothing changed. An absent
    // key (older SCK) or a non-empty array publishes
    if let Some(dirty_key) = ctx.dirty_rects_key {
        let dirty_val =
            unsafe { CFDictionaryGetValue(dict, dirty_key as *const CFString as *const c_void) };
        if !dirty_val.is_null() {
            let dirty_arr = dirty_val as *const CFArray;
            if let Some(arr) = unsafe { dirty_arr.as_ref() } {
                if arr.count() == 0 {
                    return false;
                }
            }
        }
    }
    true
}

/// Wrap the sample's `IOSurface` in a fresh zero-copy `CVPixelBuffer` and publish a [`Frame`],
/// the same hand-off as [`super::cgds::handle_frame`].
fn publish_sample(sample: &CMSampleBuffer, ctx: &OutputCtx) {
    // SAFETY: returns the sample's backing CVImageBuffer (+1 retained by the
    // binding). NULL if the sample carries no image buffer.
    let Some(image_buffer): Option<CFRetained<CVImageBuffer>> = (unsafe { sample.image_buffer() })
    else {
        return;
    };
    // a screen CVImageBuffer is a CVPixelBuffer
    let pixbuf_ptr = (&*image_buffer) as *const CVImageBuffer as *const CVPixelBuffer;
    let pixel_buffer = unsafe { &*pixbuf_ptr };

    // SAFETY: the pixel buffer's IOSurface is valid for the callback duration; we
    // retain it (binding returns +1) so it stays alive inside the Frame.
    let Some(surface) = (unsafe { CVPixelBufferGetIOSurface(Some(pixel_buffer)) }) else {
        return;
    };

    // a fresh CVPixelBuffer over the surface (zero-copy) rather than reusing `image_buffer`, so
    // the Frame owns its own surface/pixbuf pair, released back to the pool on drop
    // SAFETY: surface is a live IOSurfaceRef; the out-pointer receives +1.
    unsafe {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = CVPixelBufferCreateWithIOSurface(None, &surface, None, NonNull::from(&mut out));
        let Some(out) = NonNull::new(out) else {
            return;
        };
        // take ownership before checking the status: CoreVideo can return non-zero and still
        // hand back a +1 buffer, which would leak
        let pixbuf = CFRetained::from_raw(out);
        if r != 0 {
            return;
        }
        ctx.frames.fetch_add(1, Ordering::Relaxed);
        let arrived_mach = mach_now();
        ctx.sink.publish(Frame {
            width: ctx.width,
            height: ctx.height,
            arrived_mach,
            captured_at: Instant::now(),
            os_time_ns: super::clock::os_frame_time_or(
                super::clock::mach_ticks_to_host_ns(arrived_mach),
                sample_host_ns(sample),
            ),
            os_time_raw_ns: sample_host_ns(sample),
            backing: Backing::PixelBuffer {
                pixbuf,
                _surface: HeldSurface::hold(surface),
            },
        });
        ctx.published_one.store(true, Ordering::Relaxed);
    }
}

/// The frame's own presentation time, on the clock the rest of the streamer measures against.
///
/// ScreenCaptureKit stamps every sample from `CMClockGetHostTimeClock` (mach time as a `CMTime`
/// in seconds), the same clock as CGDisplayStream's `displayTime`. Without it the SCK path could
/// only measure from its callback, hiding the time between the compositor and us.
fn sample_host_ns(sample: &CMSampleBuffer) -> u64 {
    // SAFETY: CoreMedia, present on the 10.15 floor; valid sample for the callback's duration.
    let pts = unsafe { sample.presentation_time_stamp() };
    if pts.timescale <= 0 || (pts.flags.0 & objc2_core_media::CMTimeFlags::Valid.0) == 0 {
        return 0;
    }
    let ns = (pts.value as i128 * 1_000_000_000 / pts.timescale as i128).max(0) as u64;
    super::clock::host_ns_to_shared_ns(ns)
}

/// `-[NSError localizedDescription]` as a Rust string (for diagnostics).
fn nserror_description(error: *mut AnyObject) -> String {
    if error.is_null() {
        return "unknown error".to_string();
    }
    // SAFETY: `error` is a valid NSError for the call.
    let desc: *mut NSString = unsafe { msg_send![&*error, localizedDescription] };
    if desc.is_null() {
        return "unknown error".to_string();
    }
    unsafe { &*desc }.to_string()
}

// CoreFoundation symbols present on every OS, so linking them is dyld-safe. Declared here
// because objc2-core-foundation 0.3 does not bind them in the shape needed.
unsafe extern "C-unwind" {
    fn CFDictionaryGetValue(the_dict: *const CFDictionary, key: *const c_void) -> *const c_void;
    /// Returns a CF `Boolean` (`unsigned char`): non-zero iff the value was read.
    fn CFNumberGetValue(number: *const CFNumber, the_type: isize, value_ptr: *mut c_void) -> u8;
}

/// PROMPTING while unauthorized: on macOS 15+ `getShareableContent...` raises the system
/// screen-recording alert on every call. Callers need a `PromptPermit`, which only the
/// permission gate or a silent "already granted" read hands out.
pub fn probe_screen_recording(
    _permit: crate::macos_utils::permissions::PromptPermit,
) -> Option<bool> {
    let cls_content = AnyClass::get(c"SCShareableContent")?;

    let (tx, rx) = mpsc::channel::<bool>();
    let handler = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
        let _ = tx.send(error.is_null() && !content.is_null());
    });
    unsafe {
        let _: () = msg_send![
            cls_content,
            getShareableContentWithCompletionHandler: RcBlock::as_ptr(&handler),
        ];
    }
    rx.recv_timeout(Duration::from_secs(5)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The only piece testable on 10.15, where the SCK classes are absent.
    #[test]
    fn delegate_class_registers() {
        let cls = delegate_class();
        assert!(
            cls.instance_variable(CTX_IVAR_NAME).is_some(),
            "ctx ivar present"
        );
        assert!(
            cls.responds_to(sel!(stream:didOutputSampleBuffer:ofType:)),
            "hot-path selector present"
        );
        // idempotent: a second call returns the same class
        assert_eq!(cls as *const AnyClass, delegate_class() as *const AnyClass);
    }
}
