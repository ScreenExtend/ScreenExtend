//! ScreenCaptureKit backend (primary, macOS 12.3+).
//!
//! ## Why this file links and loads on macOS 10.15
//!
//! ScreenCaptureKit only exists on macOS 12.3+. A *link-time* dependency on
//! `ScreenCaptureKit.framework` (i.e. `objc2-screen-capture-kit`, an
//! `extern_class!`, or any reference to a 12.3+ `extern static` constant) would
//! add an undefined dyld symbol that is **absent on 10.15**, breaking the load of
//! the *entire app* binary there — a shipped regression. So this backend uses
//! **objc2 runtime interop only**:
//!   * classes are obtained with [`AnyClass::get`] (returns `None` on 10.15),
//!   * messages are sent with [`msg_send!`],
//!   * the `SCStreamOutput` delegate is built at runtime with
//!     [`ClassBuilder`], and
//!   * SCK/CoreMedia string constants (`SCStreamFrameInfoStatus`,
//!     `SCStreamFrameInfoDirtyRects`) are resolved at runtime via `dlsym`
//!     ([`sck_optional_cfstring`]), never referenced as link-time statics.
//!
//! There is **no `#[link]`-producing import** of any 12.3+ framework anywhere in
//! this module, so the binary's dyld load has zero dependency on SCK and loads on
//! 10.15 by construction. This is the same philosophy as
//! [`hosted_network.rs`] (private CoreWLAN selectors via `msg_send!`) and the
//! VideoToolbox encoder (version-gated constants via `dlsym`). It makes the
//! historical "weak-link / dlopen ScreenCaptureKit" build task moot:
//! a no-link runtime path is strictly safer than `-weak_framework`.
//!
//! ## When this code runs
//!
//! `mod.rs::start_capture` only constructs/starts `SckBackend` when
//! [`super::screencapturekit_available`] (12.3+) is true. On 10.15 none of this
//! runs. As a belt-and-suspenders guard, [`SckBackend::start`] re-checks each SCK
//! class via `AnyClass::get` and bails to a [`CaptureError`] if absent.
//!
//! ## The flow
//!   1. `+[SCShareableContent getShareableContentWithCompletionHandler:]` →
//!      resolve the `SCDisplay` whose `displayID == self.display_id`. The async
//!      completion handler is bridged to sync at startup with a channel + a
//!      bounded wait (→ [`CaptureError`] on timeout).
//!   2. `-[SCContentFilter initWithDisplay:excludingWindows:]` (whole display,
//!      empty exclusion array).
//!   3. `SCStreamConfiguration`: native `width`/`height`, `420f`,
//!      `minimumFrameInterval` slightly *shorter* than a frame period,
//!      `queueDepth = 8` (Apple's own documented default), `showsCursor = true`
//!      (window-server composites the cursor into the frame — same low-latency
//!      cursor path as the CGDisplayStream backend), `capturesAudio = false`,
//!      and the same 709 colour handling the CGDisplayStream backend forces.
//!   4. an `SCStreamOutput` delegate built with [`ClassBuilder`]; its
//!      `stream:didOutputSampleBuffer:ofType:` is the hot path.
//!   5. frame-status filter — forward only `SCFrameStatusComplete` (read from
//!      the sample's `SCStreamFrameInfo` attachment).
//!   6. dirty-rect skipping — if the `SCStreamFrameInfoDirtyRects` attachment is
//!      present and empty (zero rects changed), the frame is skipped (no
//!      publish), avoiding a needless encode of an unchanged screen.
//!   7. zero-copy: wrap the sample's `IOSurface` in a `CVPixelBuffer` exactly
//!      like [`super::cgds::handle_frame`] and publish a [`Frame`].
//!   8. the output is attached on a `UserInteractive` serial dispatch queue,
//!      then `-[SCStream startCaptureWithCompletionHandler:]`.
//!
//! All retained objc2 objects (stream, delegate, content, queue) are held in
//! [`SckBackend`] for the session, mirroring how `cgds` keeps its stream/queue.

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

/// `SCFrameStatusComplete` — the only status that carries new pixels. The numeric
/// value is part of Apple's stable ABI (see `SCStreamFrameInfo` /
/// `objc2-screen-capture-kit`'s `SCFrameStatus::Complete = 0`), so we inline it
/// rather than referencing the 12.3+ enum symbol.
const SC_FRAME_STATUS_COMPLETE: i64 = 0;

/// `SCFrameStatusStarted` — the first frame after `startCapture`, carrying the whole display.
/// Discarding it is what made a client joining a still screen wait for the first thing to move.
const SC_FRAME_STATUS_STARTED: i64 = 4;

/// `SCStreamOutputType.screen` (= 0). Inlined for the same reason as above.
const SC_OUTPUT_TYPE_SCREEN: isize = 0;

/// `CFNumberType.kCFNumberSInt64Type` (= 4) — the type tag for reading the frame
/// status out of its `CFNumber`. Stable CoreFoundation ABI.
const K_CF_NUMBER_S_INT64_TYPE: isize = 4;

/// The runtime name of the delegate class we build once. Unique to this app so it
/// can never collide with a framework class.
const DELEGATE_CLASS_NAME: &CStr = c"ScreenExtendSckOutput";
/// The ivar (a single boxed-context pointer) the delegate carries.
const CTX_IVAR_NAME: &CStr = c"ctx";

// ---------------------------------------------------------------------------
// dyld-safe optional SCK/CoreMedia string constants.
//
// `SCStreamFrameInfoStatus` and `SCStreamFrameInfoDirtyRects` are
// `extern NSString * const` symbols that DO NOT EXIST in the 10.15
// ScreenCaptureKit (the framework itself is absent). Referencing the objc2
// `extern static` for one of them would add an undefined symbol that breaks the
// binary's dyld load on 10.15 — the exact reason the VideoToolbox encoder builds
// `EnableLowLatencyRateControl` as a literal/`dlsym` constant. `dlsym` resolves
// the real framework CFString at runtime, so `CFDictionaryGetValue` compares it
// by the *same* constant the SCK frame attachments were keyed with. No link-time
// reference → nothing can fail to load. Called once per frame off any UI thread,
// on the delivery queue.
// ---------------------------------------------------------------------------

/// Resolve an SCK CFString constant by its C symbol name, or `None` when the
/// running framework does not export it (always `None` on < 12.3).
fn sck_optional_cfstring(symbol: &CStr) -> Option<&'static CFString> {
    // SAFETY: RTLD_DEFAULT search for an exported data symbol. When present it is
    // `const CFStringRef NAME`, so dlsym returns `&NAME` (a `*const CFStringRef`);
    // deref once to the CFStringRef. The constant is a framework static → 'static.
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
    /// Count of complete frames published — read by the latency probe.
    frames: Arc<AtomicU64>,
    #[allow(dead_code)]
    telemetry: SharedTelemetry,
    // Kept alive for the session, mirroring cgds's stream/_queue/_handler:
    stream: Option<Retained<AnyObject>>,
    delegate: Option<Retained<AnyObject>>,
    _content_filter: Option<Retained<AnyObject>>,
    _queue: Option<DispatchRetained<DispatchQueue>>,
    /// Raw boxed `OutputCtx` stored in the delegate's ivar; reclaimed on stop.
    ctx: *mut OutputCtx,
}

// The retained objc2/dispatch objects are only created/dropped here and are safe
// to release from any thread; the frame path is lock-free through the sink. The
// raw `ctx` pointer is owned solely by this struct (set on start, freed on stop).
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

/// Look a class up at runtime; `None` on 10.15 (belt-and-suspenders — SCK paths
/// are only entered on 12.3+).
fn sck_class(name: &CStr) -> Result<&'static AnyClass, CaptureError> {
    AnyClass::get(name).ok_or(CaptureError::StreamCreateFailed)
}

/// Wrap a freshly created (+1) object returned by `alloc`/`init`/`new`/factory
/// methods in a [`Retained`], without adding another retain. `None` if null.
///
/// # Safety
/// `obj` must be a `+1`-owned object pointer (the ownership convention of
/// `alloc`/`init`/`new` and `+array`-style autoreleased… — see call sites; for
/// autoreleased factory results we retain explicitly instead).
unsafe fn retain_new(obj: *mut AnyObject) -> Option<Retained<AnyObject>> {
    let ptr = NonNull::new(obj)?;
    unsafe { Retained::from_raw(ptr.as_ptr()) }
}

/// Register (once) and return the `SCStreamOutput` delegate class. Built with the
/// objc2 runtime `ClassBuilder` so we never reference the typed `SCStreamOutput`
/// protocol (a 12.3+ symbol). SCK dispatches via `respondsToSelector:`, so the
/// method merely existing is what matters; we also best-effort declare protocol
/// conformance when the protocol is obtainable at runtime.
fn delegate_class() -> &'static AnyClass {
    use std::sync::OnceLock;
    static CLS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLS.get_or_init(|| {
        // If a previous init already registered it (or a name clash), reuse it.
        if let Some(existing) = AnyClass::get(DELEGATE_CLASS_NAME) {
            return existing as *const AnyClass as usize;
        }
        let superclass = class_nsobject();
        let mut builder = ClassBuilder::new(DELEGATE_CLASS_NAME, superclass)
            .expect("ClassBuilder for SCK output delegate");

        // One pointer-sized ivar carrying the boxed `OutputCtx`.
        builder.add_ivar::<*mut c_void>(CTX_IVAR_NAME);

        // SAFETY: the IMP's Rust signature matches the Obj-C selector
        // `-stream:didOutputSampleBuffer:ofType:` (id, SEL, id, id, NSInteger).
        // The receiver is taken as `*mut AnyObject` (not `&AnyObject`) so the
        // function type is not higher-ranked over a lifetime, which is what
        // `MethodImplementation` requires.
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

        // Best-effort protocol conformance. SCK only needs `respondsToSelector:`,
        // so this is not load-bearing; it is skipped silently if the protocol is
        // not registered (e.g. always on 10.15).
        if let Some(proto) = objc2::runtime::AnyProtocol::get(c"SCStreamOutput") {
            builder.add_protocol(proto);
        }

        builder.register() as *const AnyClass as usize
    });
    // SAFETY: the stored pointer is a `&'static AnyClass` reinterpreted as usize.
    unsafe { &*(ptr as *const AnyClass) }
}

/// `objc2::class!(NSObject)` without the macro's compile-time link — fetched at
/// runtime so nothing is pulled in that 10.15 lacks (NSObject is universal, but
/// keep the whole module link-free for consistency).
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
        // Belt-and-suspenders: confirm SCK is actually present. On 10.15 these
        // return None and we bail without ever touching a missing framework.
        let cls_content = sck_class(c"SCShareableContent")?;
        let cls_filter = sck_class(c"SCContentFilter")?;
        let cls_config = sck_class(c"SCStreamConfiguration")?;
        let cls_stream = sck_class(c"SCStream")?;

        // ---- step 1: Resolve the SCDisplay (async → sync bridge) ----
        let display = resolve_display(cls_content, self.display_id)?;

        // ---- step 2: SCContentFilter for the whole display, no exclusions ----
        // We drive `alloc`/`init` through raw `*mut AnyObject` and wrap the final
        // +1 object in `Retained::from_raw`, rather than letting `msg_send!`'s
        // typed retain-semantics machinery run (it needs a concrete `Message`
        // type we deliberately don't have for these runtime-only classes).
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

        // ---- step 3: SCStreamConfiguration ----
        let config = unsafe {
            let alloc: *mut AnyObject = msg_send![cls_config, alloc];
            let c: *mut AnyObject = msg_send![alloc, init];
            retain_new(c).ok_or(CaptureError::StreamCreateFailed)?
        };
        configure_stream(&config, &self.cfg);

        // ---- step 4: Build + populate the delegate ----
        let status_key = sck_optional_cfstring(c"SCStreamFrameInfoStatus");
        let dirty_rects_key = sck_optional_cfstring(c"SCStreamFrameInfoDirtyRects");
        if status_key.is_none() {
            // Without the status key we cannot filter incomplete frames; this only
            // happens if SCK is somehow present but unexported — treat as failure.
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

        // ---- step 4: Create the stream (delegate doubles as the SCStreamDelegate) ----
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

        // ---- step 8: UserInteractive serial delivery queue (same pattern as cgds) ----
        let ui_target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInteractive,
        ));
        let queue = DispatchQueue::new_with_target("com.screenextend.sck", None, Some(&ui_target));

        // addStreamOutput:type:sampleHandlerQueue:error:
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

        // ---- step 8: startCaptureWithCompletionHandler: (bridged to sync) ----
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
             (queueDepth={QUEUE_DEPTH}, minimumFrameInterval=0.9/fps)",
            self.display_id,
            self.cfg.width,
            self.cfg.height,
            self.cfg.fps
        );

        self.ctx = ctx;
        self.stream = Some(stream);
        self.delegate = Some(delegate);
        self._content_filter = Some(filter);
        self._queue = Some(queue);
        Ok(())
    }

    fn stop(&mut self) {
        // Order matters. Blank the delegate's context pointer first, so a sample handler already
        // enqueued on the delivery queue finds null and returns instead of dereferencing a box we
        // are about to free. Releasing a dispatch queue handle does not drain what is already on
        // it, and `stopCaptureWithCompletionHandler:` is not documented as a barrier against an
        // in-flight `stream:didOutputSampleBuffer:ofType:`.
        if let Some(delegate) = &self.delegate {
            // SAFETY: `delegate` is an instance of our own class, which declares the ivar.
            unsafe { set_ctx_ivar(delegate, ptr::null_mut()) };
            // A false return here would mean the delegate is not ours, which cannot happen; the
            // barrier below is what actually makes the free safe either way.
        }
        if let Some(stream) = &self.stream {
            // Bridge the async stop to sync so the delegate is guaranteed not to
            // be called again before we free its context.
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
        // Then drain the serial delivery queue: anything enqueued before this block has run
        // by the time the synchronous dispatch returns, so nothing can still be inside the
        // handler when the context is freed below.
        if let Some(queue) = &self._queue {
            queue.exec_sync(|| {});
        }
        // Drop the retained objc2/dispatch objects first (detaches the output),
        // then reclaim the boxed context the delegate referenced.
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
        // Ensure the context is freed even if stop() was never called.
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

/// step 1 — resolve the `SCDisplay` whose `displayID == display_id`, bridging the
/// async `getShareableContentWithCompletionHandler:` to a synchronous result.
fn resolve_display(
    cls_content: &AnyClass,
    display_id: DisplayId,
) -> Result<Retained<AnyObject>, CaptureError> {
    // The completion block hands us the shareable content (or an error). We send
    // a retained `SCDisplay` (or `None`) back through the channel. `usize` carries
    // the raw pointer so the value is `Send` across the channel; we re-wrap it on
    // the receiving side.
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
            // Leak a +1 retain across the channel; reclaimed by `from_raw` below.
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
        // `-[SCDisplay displayID]` returns a CGDirectDisplayID (u32).
        let id: u32 = unsafe { msg_send![&*display, displayID] };
        if id == display_id {
            // SAFETY: retain the +0 element to hand it back across the bridge.
            return unsafe { Retained::retain(display) };
        }
    }
    None
}

/// Surfaces ScreenCaptureKit keeps in flight.
///
/// `SCStreamConfiguration.h` describes the key as the maximum number of frames the queue stores,
/// and says a larger queue "may allow you to process frame data without stalling the display
/// stream". Apple raised its own documented default from 3 to 8 between the 12.3 and 26.5 SDKs.
/// Ours was 3, carrying a comment claiming that was the lowest-latency value: depth is memory
/// against stalling, not latency, and this pipeline holds surfaces that came out of this queue —
/// the latest-wins slot plus the encode loop's keepalive frame.
const QUEUE_DEPTH: isize = 8;

/// step 3 — apply the low-latency SCStreamConfiguration knobs via objc2 setters.
fn configure_stream(config: &AnyObject, cfg: &CaptureConfig) {
    let fps = cfg.fps.max(1);
    // Nine tenths of a frame period, not one.
    //
    // Asking for exactly 1/fps beats against the compositor's own cadence. OpenDisplay measured
    // ~51 fps out of a 60 fps request; OBS found that "setting this property to the exact inverse
    // of our frame rate seems to provide a capture that drops many frames" and ships a 10 %
    // shortening. The property is a floor on delivery spacing, so asking for slightly less than a
    // frame period costs nothing and stops the beat.
    let interval = objc2_core_media::CMTime {
        value: 9,
        timescale: fps.saturating_mul(10),
        flags: objc2_core_media::CMTimeFlags::Valid,
        epoch: 0,
    };
    // SAFETY: each selector is a public `SCStreamConfiguration` property setter
    // with the documented argument type. `width`/`height`/`queueDepth` take
    // NSInteger (isize); `pixelFormat` takes an OSType (u32); the bools take BOOL.
    unsafe {
        let _: () = msg_send![config, setWidth: cfg.width as isize];
        let _: () = msg_send![config, setHeight: cfg.height as isize];
        let _: () = msg_send![config, setMinimumFrameInterval: interval];
        let _: () = msg_send![config, setPixelFormat: cfg.pixel_format];
        let _: () = msg_send![config, setQueueDepth: QUEUE_DEPTH];
        let _: () = msg_send![config, setShowsCursor: true];
        let _: () = msg_send![config, setCapturesAudio: false];
    }
    // Match the CGDisplayStream backend's colour handling instead of leaving SCK on its defaults.
    // Both tiers feed the same encoder, and a stream whose primaries depend on which capture tier
    // happened to start is a colour shift nobody would think to look for.
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

/// The hot path (steps 5-7). C-ABI IMP for
/// `-[ScreenExtendSckOutput stream:didOutputSampleBuffer:ofType:]`.
///
/// `extern "C-unwind"` so a (caught) Rust panic does not become UB crossing the
/// ObjC boundary; we never intentionally unwind here.
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

    // The SCK sample buffer is a CMSampleBuffer. Reinterpret the id as such.
    let sample = sample.cast::<CMSampleBuffer>();
    // SAFETY: valid CMSampleBuffer for the callback's duration.
    let sample_ref = unsafe { sample.as_ref() };

    // ---- step 5: frame-status gate + step 6 dirty-rect skip ----
    if !frame_should_publish(sample_ref, ctx) {
        return;
    }

    // ---- step 7: zero-copy IOSurface → CVPixelBuffer, publish ----
    publish_sample(sample_ref, ctx);
}

/// Read the `SCStreamFrameInfo` attachment and decide whether to forward the
/// frame: forward only `SCFrameStatusComplete`, and skip when the dirty-rects
/// attachment is present but empty (nothing changed — step 6).
fn frame_should_publish(sample: &CMSampleBuffer, ctx: &OutputCtx) -> bool {
    // CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false).
    let attachments: CFRetained<CFArray> = match unsafe { sample.sample_attachments_array(false) } {
        Some(a) if a.count() > 0 => a,
        _ => return false,
    };
    // First per-sample dictionary.
    let dict_ptr = unsafe { attachments.value_at_index(0) } as *const CFDictionary;
    let Some(dict) = (unsafe { dict_ptr.as_ref() }) else {
        return false;
    };

    // status == Complete ?
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

    // The very first frame is published whatever its dirty rects say, and so is a `Started`
    // frame: a client that has seen nothing needs this one, and on a freshly created virtual
    // display nothing will move until someone drags a window onto it.
    if status == SC_FRAME_STATUS_STARTED || !ctx.published_one.load(Ordering::Relaxed) {
        return true;
    }

    // step 6: dirty-rect skip. If the attachment exists and is an *empty* array,
    // nothing changed on screen — skip the encode. If the key is absent (older
    // SCK) or the array is non-empty, fall through and publish.
    if let Some(dirty_key) = ctx.dirty_rects_key {
        let dirty_val =
            unsafe { CFDictionaryGetValue(dict, dirty_key as *const CFString as *const c_void) };
        if !dirty_val.is_null() {
            let dirty_arr = dirty_val as *const CFArray;
            if let Some(arr) = unsafe { dirty_arr.as_ref() } {
                if arr.count() == 0 {
                    return false; // present and empty → unchanged frame
                }
            }
        }
    }
    true
}

/// Wrap the sample's `IOSurface` in a fresh zero-copy `CVPixelBuffer` and publish
/// a [`Frame`] — identical hand-off to [`super::cgds::handle_frame`].
fn publish_sample(sample: &CMSampleBuffer, ctx: &OutputCtx) {
    // SAFETY: returns the sample's backing CVImageBuffer (+1 retained by the
    // binding). NULL if the sample carries no image buffer.
    let Some(image_buffer): Option<CFRetained<CVImageBuffer>> = (unsafe { sample.image_buffer() })
    else {
        return;
    };
    // A screen CVImageBuffer is a CVPixelBuffer; reinterpret to get the IOSurface.
    let pixbuf_ptr = (&*image_buffer) as *const CVImageBuffer as *const CVPixelBuffer;
    let pixel_buffer = unsafe { &*pixbuf_ptr };

    // SAFETY: the pixel buffer's IOSurface is valid for the callback duration; we
    // retain it (binding returns +1) so it stays alive inside the Frame.
    let Some(surface) = (unsafe { CVPixelBufferGetIOSurface(Some(pixel_buffer)) }) else {
        return;
    };

    // Wrap the surface in a fresh CVPixelBuffer exactly like cgds (zero-copy: the
    // new pixel buffer just references the surface). We deliberately wrap rather
    // than reuse `image_buffer` so the Frame owns a self-contained, stable
    // surface→pixbuf pair released back to the pool on drop.
    // SAFETY: surface is a live IOSurfaceRef; the out-pointer receives +1.
    unsafe {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = CVPixelBufferCreateWithIOSurface(None, &surface, None, NonNull::from(&mut out));
        let Some(out) = NonNull::new(out) else {
            return;
        };
        // Take ownership before checking the status: CoreVideo can return non-zero and still
        // hand back a +1 buffer, and dropping it here is what stops that leaking.
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
/// ScreenCaptureKit stamps every sample from `CMClockGetHostTimeClock`, i.e. mach time expressed
/// as a `CMTime` in seconds — so it is already the same clock as CGDisplayStream's `displayTime`,
/// just in different units. Without it the SCK path can only measure from the moment its callback
/// ran, which hides everything the frame spent between the compositor and us.
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

// ---------------------------------------------------------------------------
// CoreFoundation functions used to read the frame-status CFNumber out of the
// attachments dictionary. These are stable, present-on-every-OS CF symbols, so
// referencing them is dyld-safe (unlike the SCK constants, which are dlsym'd).
// objc2-core-foundation 0.3 does not bind `CFNumberGetValue`/`CFDictionaryGetValue`
// in the exact shape we need here, so we declare them.
// ---------------------------------------------------------------------------
unsafe extern "C-unwind" {
    fn CFDictionaryGetValue(the_dict: *const CFDictionary, key: *const c_void) -> *const c_void;
    /// Returns a CF `Boolean` (`unsigned char`): non-zero iff the value was read.
    fn CFNumberGetValue(number: *const CFNumber, the_type: isize, value_ptr: *mut c_void) -> u8;
}

pub fn probe_screen_recording() -> Option<bool> {
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

    /// The delegate class registers without panicking and exposes the hot-path
    /// selector. This is the only piece testable on 10.15 (SCK classes are
    /// absent), but it exercises the `ClassBuilder` path end-to-end.
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
        // Idempotent: a second call returns the same registered class.
        assert_eq!(cls as *const AnyClass, delegate_class() as *const AnyClass);
    }
}
