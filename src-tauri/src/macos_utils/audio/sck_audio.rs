use std::cell::UnsafeCell;
use std::ffi::{c_void, CStr};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchRetained, GlobalQueueIdentifier};
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
use objc2::{msg_send, sel};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList};
use objc2_core_foundation::CFRetained;
use objc2_core_media::{
    kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
    CMAudioFormatDescriptionGetStreamBasicDescription, CMBlockBuffer, CMSampleBuffer, CMTime,
    CMTimeFlags,
};
use objc2_foundation::NSString;

use super::format::{self, AudioFormatDesc};
use super::{AudioCaptureError, AudioFrameSink, AudioSource};
use crate::streamer::audio::AudioDiagnostics;

const SC_OUTPUT_TYPE_AUDIO: isize = 1;
const SILENCE_THRESHOLD: f32 = 1.0 / 32768.0;

const DELEGATE_CLASS_NAME: &CStr = c"ScreenExtendSckAudioOutput";
const CTX_IVAR_NAME: &CStr = c"ctx";

struct OutputCtx {
    producer: Arc<super::ring::Producer>,
    abl_scratch: UnsafeCell<Box<[u8]>>,
    f32_scratch: UnsafeCell<Box<[f32]>>,
    nonsilent_samples: AtomicU64,
    diag: Arc<AudioDiagnostics>,
}

pub struct SckAudioCapture {
    stream: Option<Retained<AnyObject>>,
    delegate: Option<Retained<AnyObject>>,
    _content_filter: Option<Retained<AnyObject>>,
    _config: Option<Retained<AnyObject>>,
    _queue: Option<DispatchRetained<DispatchQueue>>,
    ctx: *mut OutputCtx,
    nonsilent_probe: *const AtomicU64,
}

// SAFETY: the retained objc2/dispatch objects are thread-safe to create/release
unsafe impl Send for SckAudioCapture {}

impl SckAudioCapture {
    pub fn new() -> Self {
        Self {
            stream: None,
            delegate: None,
            _content_filter: None,
            _config: None,
            _queue: None,
            ctx: ptr::null_mut(),
            nonsilent_probe: ptr::null(),
        }
    }

    pub fn runtime_available() -> bool {
        AnyClass::get(c"SCStream").is_some()
            && AnyClass::get(c"SCShareableContent").is_some()
            && AnyClass::get(c"SCContentFilter").is_some()
            && AnyClass::get(c"SCStreamConfiguration").is_some()
    }

    pub fn try_create() -> Result<(), AudioCaptureError> {
        if !Self::runtime_available() {
            return Err(AudioCaptureError::Unsupported(
                "ScreenCaptureKit not present (needs macOS 13.0+)".into(),
            ));
        }
        let (producer, _consumer, consumer_thread) = super::ring::ring(4096);
        let sink = AudioFrameSink {
            producer: Arc::new(producer),
            diagnostics: Arc::new(AudioDiagnostics::default()),
            control_tx: None,
            consumer_thread,
        };
        let mut probe = SckAudioCapture::new();
        probe.build_and_start(&sink)?;
        Ok(())
    }

    fn build_and_start(&mut self, sink: &AudioFrameSink) -> Result<(), AudioCaptureError> {
        let cls_content = runtime_class(c"SCShareableContent")?;
        let cls_filter = runtime_class(c"SCContentFilter")?;
        let cls_config = runtime_class(c"SCStreamConfiguration")?;
        let cls_stream = runtime_class(c"SCStream")?;

        let display = resolve_any_display(cls_content)?;

        let arr_cls = runtime_class(c"NSArray")?;
        // SAFETY: standard runtime construction, mirroring the video sck.rs
        let filter = unsafe {
            let empty: *mut AnyObject = msg_send![arr_cls, array];
            let alloc: *mut AnyObject = msg_send![cls_filter, alloc];
            let f: *mut AnyObject =
                msg_send![alloc, initWithDisplay: &*display, excludingWindows: empty];
            retain_new(f).ok_or_else(err_setup)?
        };

        let config = unsafe {
            let alloc: *mut AnyObject = msg_send![cls_config, alloc];
            let c: *mut AnyObject = msg_send![alloc, init];
            retain_new(c).ok_or_else(err_setup)?
        };
        // SAFETY: public SCStreamConfiguration property setters with documented arg types
        unsafe {
            let _: () = msg_send![&*config, setCapturesAudio: true];
            let _: () = msg_send![&*config, setSampleRate: 48_000isize];
            let _: () = msg_send![&*config, setChannelCount: 2isize];
            let _: () = msg_send![&*config, setExcludesCurrentProcessAudio: true];
            let _: () = msg_send![&*config, setWidth: 2isize];
            let _: () = msg_send![&*config, setHeight: 2isize];
            let interval = CMTime {
                value: 1,
                timescale: 1,
                flags: CMTimeFlags::Valid,
                epoch: 0,
            };
            let _: () = msg_send![&*config, setMinimumFrameInterval: interval];
        }

        let abl_scratch = vec![0u8; 4096].into_boxed_slice();
        let f32_scratch = vec![0.0f32; 8192 * format::OUT_CHANNELS].into_boxed_slice();
        let ctx = Box::into_raw(Box::new(OutputCtx {
            producer: Arc::clone(&sink.producer),
            abl_scratch: UnsafeCell::new(abl_scratch),
            f32_scratch: UnsafeCell::new(f32_scratch),
            nonsilent_samples: AtomicU64::new(0),
            diag: Arc::clone(&sink.diagnostics),
        }));
        self.nonsilent_probe = unsafe { &(*ctx).nonsilent_samples as *const AtomicU64 };

        let delegate = unsafe {
            let d: *mut AnyObject = msg_send![delegate_class(), new];
            let d = retain_new(d).ok_or_else(err_setup)?;
            set_ctx_ivar(&d, ctx);
            d
        };

        // the delegate doubles as SCStreamDelegate for invalidation
        let stream = unsafe {
            let alloc: *mut AnyObject = msg_send![cls_stream, alloc];
            let s: *mut AnyObject = msg_send![alloc, initWithFilter: &*filter, configuration: &*config, delegate: &*delegate];
            retain_new(s).ok_or_else(err_setup)?
        };

        // UserInteractive serial delivery queue, same as the video path
        let ui_target = DispatchQueue::global_queue(GlobalQueueIdentifier::QualityOfService(
            DispatchQoS::UserInteractive,
        ));
        let queue =
            DispatchQueue::new_with_target("com.screenextend.sck.audio", None, Some(&ui_target));

        let mut add_err: *mut AnyObject = ptr::null_mut();
        let added: bool = unsafe {
            let r: Bool = msg_send![
                &*stream,
                addStreamOutput: &*delegate,
                type: SC_OUTPUT_TYPE_AUDIO,
                sampleHandlerQueue: &*queue,
                error: &mut add_err,
            ];
            r.as_bool()
        };
        if !added {
            // SAFETY: the stream never took the output; reclaim the ctx
            drop(unsafe { Box::from_raw(ctx) });
            return Err(AudioCaptureError::Setup(
                "addStreamOutput(audio) failed".into(),
            ));
        }

        // startCapture bridged async --> sync
        let (tx, rx) = mpsc::channel::<Option<String>>();
        let handler = RcBlock::new(move |error: *mut AnyObject| {
            let _ = tx.send(if error.is_null() {
                None
            } else {
                Some(nserror_description(error))
            });
        });
        unsafe {
            let _: () =
                msg_send![&*stream, startCaptureWithCompletionHandler: RcBlock::as_ptr(&handler)];
        }
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(None) => {}
            Ok(Some(detail)) => {
                drop(unsafe { Box::from_raw(ctx) });
                return Err(AudioCaptureError::Setup(format!(
                    "SCK startCapture failed: {detail}"
                )));
            }
            Err(_) => {
                drop(unsafe { Box::from_raw(ctx) });
                return Err(AudioCaptureError::Setup(
                    "SCK startCapture timed out".into(),
                ));
            }
        }

        sink.diagnostics
            .sample_rate
            .store(48_000, Ordering::Relaxed);
        sink.diagnostics
            .channels
            .store(format::OUT_CHANNELS as u32, Ordering::Relaxed);

        self.ctx = ctx;
        self.stream = Some(stream);
        self.delegate = Some(delegate);
        self._content_filter = Some(filter);
        self._config = Some(config);
        self._queue = Some(queue);
        crate::tprintln!("audio: ScreenCaptureKit audio started (48kHz stereo + 2x2 dummy video)");
        Ok(())
    }
}

impl AudioSource for SckAudioCapture {
    fn start(&mut self, sink: AudioFrameSink) -> Result<(), AudioCaptureError> {
        self.build_and_start(&sink)
    }

    fn stop(&mut self) {
        if let Some(stream) = &self.stream {
            let (tx, rx) = mpsc::channel::<()>();
            let handler = RcBlock::new(move |_error: *mut AnyObject| {
                let _ = tx.send(());
            });
            // SAFETY: stopping a running stream; safe from any thread
            unsafe {
                let _: () = msg_send![
                    &**stream,
                    stopCaptureWithCompletionHandler: RcBlock::as_ptr(&handler)
                ];
            }
            let _ = rx.recv_timeout(Duration::from_secs(5));
        }
        self.stream = None;
        self.delegate = None;
        self._content_filter = None;
        self._config = None;
        self._queue = None;
        self.nonsilent_probe = ptr::null();
        if !self.ctx.is_null() {
            // SAFETY: the stream + delegate are released, so no callback can still read ctx
            drop(unsafe { Box::from_raw(self.ctx) });
            self.ctx = ptr::null_mut();
        }
    }

    fn backend_name(&self) -> &'static str {
        "screencapturekit"
    }

    fn nonsilent_samples(&self) -> u64 {
        if self.nonsilent_probe.is_null() {
            0
        } else {
            // SAFETY: the counter lives in the boxed ctx, alive until stop() nulls this pointer
            unsafe { &*self.nonsilent_probe }.load(Ordering::Relaxed)
        }
    }
}

impl Drop for SckAudioCapture {
    fn drop(&mut self) {
        if self.stream.is_some() || !self.ctx.is_null() {
            self.stop();
        }
    }
}

fn delegate_class() -> &'static AnyClass {
    use std::sync::OnceLock;
    static CLS: OnceLock<usize> = OnceLock::new();
    let ptr = *CLS.get_or_init(|| {
        if let Some(existing) = AnyClass::get(DELEGATE_CLASS_NAME) {
            return existing as *const AnyClass as usize;
        }
        let superclass = AnyClass::get(c"NSObject").expect("NSObject present");
        let mut builder = ClassBuilder::new(DELEGATE_CLASS_NAME, superclass)
            .expect("ClassBuilder for SCK audio delegate");
        builder.add_ivar::<*mut c_void>(CTX_IVAR_NAME);
        unsafe {
            builder.add_method(
                sel!(stream:didOutputSampleBuffer:ofType:),
                did_output_audio
                    as extern "C-unwind" fn(
                        *mut AnyObject,
                        Sel,
                        *mut AnyObject,
                        *mut AnyObject,
                        isize,
                    ),
            );
        }
        if let Some(proto) = objc2::runtime::AnyProtocol::get(c"SCStreamOutput") {
            builder.add_protocol(proto);
        }
        builder.register() as *const AnyClass as usize
    });
    // SAFETY: the stored usize is a `&'static AnyClass`
    unsafe { &*(ptr as *const AnyClass) }
}

extern "C-unwind" fn did_output_audio(
    this: *mut AnyObject,
    _cmd: Sel,
    _stream: *mut AnyObject,
    sample: *mut AnyObject,
    of_type: isize,
) {
    if of_type != SC_OUTPUT_TYPE_AUDIO {
        return; // ignore the dummy video / any other output
    }
    let (Some(this), Some(sample)) = (NonNull::new(this), NonNull::new(sample)) else {
        return;
    };
    // SAFETY: `this` is a live instance of our delegate class for the call
    let ctx_ptr = unsafe { get_ctx_ivar(this.as_ref()) };
    if ctx_ptr.is_null() {
        return;
    }
    // SAFETY: ctx lives until stop() frees it, after the stream is released and its queue drained
    let ctx = unsafe { &*ctx_ptr };
    // SAFETY: the SCK sample buffer is a CMSampleBuffer, valid for the callback's duration
    let sample = unsafe { sample.cast::<CMSampleBuffer>().as_ref() };

    handle_audio_sample(sample, ctx);
}

#[allow(deprecated)]
fn handle_audio_sample(sample: &CMSampleBuffer, ctx: &OutputCtx) {
    // SAFETY: `sample` is a valid CMSampleBuffer; the getter returns a retained format description
    let Some(fmt) = (unsafe { objc2_core_media::CMSampleBufferGetFormatDescription(sample) })
    else {
        return;
    };
    // SAFETY: returns a read-only pointer to the ASBD owned by `fmt`, valid while `fmt` is alive
    let asbd_ptr = unsafe { CMAudioFormatDescriptionGetStreamBasicDescription(&fmt) };
    if asbd_ptr.is_null() {
        return;
    }
    let asbd = unsafe { *asbd_ptr };
    let Ok(desc): Result<AudioFormatDesc, _> = format::parse_asbd(&asbd) else {
        return;
    };

    // SAFETY: single-thread access to the scratch cells on the SCK delivery queue
    let abl_bytes: &mut [u8] = unsafe { &mut *ctx.abl_scratch.get() };
    let abl = abl_bytes.as_mut_ptr() as *mut AudioBufferList;
    let mut block_buf: *mut CMBlockBuffer = ptr::null_mut();
    // SAFETY: floor-present CoreMedia call; `abl` has `abl_bytes.len()` writable bytes
    let st = unsafe {
        objc2_core_media::CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample,
            ptr::null_mut(),
            abl,
            abl_bytes.len(),
            None,
            None,
            kCMSampleBufferFlag_AudioBufferList_Assure16ByteAlignment,
            &mut block_buf,
        )
    };
    let _block_owner = NonNull::new(block_buf).map(|p| {
        // SAFETY: +1 CMBlockBuffer from the call above
        unsafe { CFRetained::from_raw(p) }
    });
    if st != 0 {
        return;
    }

    // SAFETY: `abl` was filled by the call; read its buffers
    let list = unsafe { &*abl };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return;
    }
    // SAFETY: `mBuffers` is a flexible array of `mNumberBuffers` AudioBuffers
    let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };

    let bps = match desc.kind {
        format::SampleKind::F32 | format::SampleKind::I32 => 4,
        format::SampleKind::I16 => 2,
    };
    // SAFETY: single-thread scratch access on the delivery queue
    let out: &mut [f32] = unsafe { &mut *ctx.f32_scratch.get() };

    let written = if desc.non_interleaved || nbuf > 1 {
        let ch = nbuf.min(8);
        let mut planes: [&[u8]; 8] = [&[][..]; 8];
        let frames = if bufs[0].mData.is_null() {
            0
        } else {
            bufs[0].mDataByteSize as usize / bps
        };
        let mut ok = true;
        for (i, p) in planes.iter_mut().enumerate().take(ch) {
            let b: &AudioBuffer = &bufs[i];
            if b.mData.is_null() {
                ok = false;
                break;
            }
            // SAFETY: channel `i`'s plane, `mDataByteSize` bytes
            *p = unsafe {
                std::slice::from_raw_parts(b.mData as *const u8, b.mDataByteSize as usize)
            };
        }
        if !ok {
            return;
        }
        format::convert_planar(&planes[..ch], frames, &desc, out)
    } else {
        let b0 = &bufs[0];
        if b0.mData.is_null() {
            return;
        }
        let stride = desc.channels as usize * bps;
        if stride == 0 {
            return;
        }
        let frames = b0.mDataByteSize as usize / stride;
        // SAFETY: interleaved buffer of `mDataByteSize` bytes
        let src =
            unsafe { std::slice::from_raw_parts(b0.mData as *const u8, b0.mDataByteSize as usize) };
        format::convert_interleaved(src, frames, &desc, out)
    };

    if written == 0 {
        return;
    }
    let frame = &out[..written];
    let mut nonsilent = 0u64;
    for &x in frame.iter() {
        if x.abs() > SILENCE_THRESHOLD {
            nonsilent += 1;
        }
    }
    if nonsilent > 0 {
        ctx.nonsilent_samples
            .fetch_add(nonsilent, Ordering::Relaxed);
    }
    let dropped = ctx.producer.push(frame);
    if dropped > 0 {
        ctx.diag
            .dropped_backpressure
            .fetch_add(dropped as u64, Ordering::Relaxed);
    }
}

fn runtime_class(name: &CStr) -> Result<&'static AnyClass, AudioCaptureError> {
    AnyClass::get(name).ok_or_else(|| {
        AudioCaptureError::Unsupported(format!(
            "{} not present (needs macOS 13.0+)",
            name.to_string_lossy()
        ))
    })
}

unsafe fn retain_new(obj: *mut AnyObject) -> Option<Retained<AnyObject>> {
    let ptr = NonNull::new(obj)?;
    unsafe { Retained::from_raw(ptr.as_ptr()) }
}

fn err_setup() -> AudioCaptureError {
    AudioCaptureError::Setup("SCK Obj-C construction returned nil".into())
}

unsafe fn set_ctx_ivar(delegate: &AnyObject, ctx: *mut OutputCtx) {
    let ivar = delegate
        .class()
        .instance_variable(CTX_IVAR_NAME)
        .expect("SCK audio delegate must have the ctx ivar");
    // SAFETY: the ivar was declared as `*mut c_void` on this exact class
    let slot = unsafe { ivar.load_ptr::<*mut c_void>(delegate) };
    unsafe { *slot = ctx.cast::<c_void>() };
}

unsafe fn get_ctx_ivar(delegate: &AnyObject) -> *mut OutputCtx {
    let Some(ivar) = delegate.class().instance_variable(CTX_IVAR_NAME) else {
        return ptr::null_mut();
    };
    // SAFETY: declared as `*mut c_void` on this class
    let slot = unsafe { ivar.load_ptr::<*mut c_void>(delegate) };
    unsafe { (*slot).cast::<OutputCtx>() }
}

fn resolve_any_display(cls_content: &AnyClass) -> Result<Retained<AnyObject>, AudioCaptureError> {
    let (tx, rx) = mpsc::channel::<Option<usize>>();
    let handler = RcBlock::new(move |content: *mut AnyObject, error: *mut AnyObject| {
        if !error.is_null() || content.is_null() {
            let _ = tx.send(None);
            return;
        }
        // SAFETY: `content` is a valid (+0) SCShareableContent for the block's duration
        let found = unsafe { first_display(&*content) };
        let _ = tx.send(found.map(|d| Retained::into_raw(d) as usize));
    });
    // SAFETY: standard class method; the block is copied by SCK and invoked once
    unsafe {
        let _: () = msg_send![
            cls_content,
            getShareableContentWithCompletionHandler: RcBlock::as_ptr(&handler),
        ];
    }
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Some(raw)) => {
            let ptr = NonNull::new(raw as *mut AnyObject).ok_or(AudioCaptureError::Setup(
                "no display available for SCK audio".into(),
            ))?;
            // SAFETY: re-wrap the +1 retain leaked across the channel
            Ok(unsafe { Retained::from_raw(ptr.as_ptr()).unwrap() })
        }
        Ok(None) => Err(AudioCaptureError::Setup(
            "SCShareableContent had no displays (Screen Recording permission?)".into(),
        )),
        Err(_) => Err(AudioCaptureError::Setup(
            "SCShareableContent fetch timed out".into(),
        )),
    }
}

unsafe fn first_display(content: &AnyObject) -> Option<Retained<AnyObject>> {
    let displays: Retained<AnyObject> = unsafe { msg_send![content, displays] };
    let count: usize = unsafe { msg_send![&*displays, count] };
    if count == 0 {
        return None;
    }
    let d: *mut AnyObject = unsafe { msg_send![&*displays, objectAtIndex: 0usize] };
    NonNull::new(d).map(|_| unsafe { Retained::retain(d).unwrap() })
}

fn nserror_description(error: *mut AnyObject) -> String {
    if error.is_null() {
        return "unknown error".to_string();
    }
    // SAFETY: `error` is a valid NSError for the call
    let desc: *mut NSString = unsafe { msg_send![&*error, localizedDescription] };
    if desc.is_null() {
        "unknown error".to_string()
    } else {
        unsafe { &*desc }.to_string()
    }
}
