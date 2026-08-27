use std::cell::UnsafeCell;
use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, rc::Retained};
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceMainSubDeviceKey, kAudioAggregateDeviceNameKey,
    kAudioAggregateDeviceSubDeviceListKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultOutputDevice,
    kAudioHardwarePropertyDefaultSystemOutputDevice, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioSubDeviceUIDKey,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID, AudioDeviceStart,
    AudioDeviceStop, AudioHardwareCreateAggregateDevice, AudioHardwareDestroyAggregateDevice,
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertyListenerProc, AudioObjectRemovePropertyListener,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription};
use objc2_core_foundation::CFString;

use super::format::{self, AudioFormatDesc};
use super::{AudioCaptureError, AudioFrameSink, AudioSource, ControlMsg};
use crate::streamer::audio::AudioDiagnostics;

const SILENCE_THRESHOLD: f32 = 1.0 / 32768.0;
const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const CATAP_UNMUTED: isize = 0;

type FnCreateTap = unsafe extern "C-unwind" fn(*const AnyObject, *mut AudioObjectID) -> i32;
type FnDestroyTap = unsafe extern "C-unwind" fn(AudioObjectID) -> i32;

fn tap_fns() -> Option<(FnCreateTap, FnDestroyTap)> {
    // SAFETY: RTLD_DEFAULT lookup of two documented CoreAudio C fns
    unsafe {
        let c = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"AudioHardwareCreateProcessTap".as_ptr(),
        );
        let d = libc::dlsym(
            libc::RTLD_DEFAULT,
            c"AudioHardwareDestroyProcessTap".as_ptr(),
        );
        if c.is_null() || d.is_null() {
            return None;
        }
        Some((
            std::mem::transmute::<*mut c_void, FnCreateTap>(c),
            std::mem::transmute::<*mut c_void, FnDestroyTap>(d),
        ))
    }
}

pub fn runtime_available() -> bool {
    tap_fns().is_some() && AnyClass::get(c"CATapDescription").is_some()
}

struct IoCtx {
    producer: Arc<super::ring::Producer>,
    desc: AudioFormatDesc,
    scratch: UnsafeCell<Box<[f32]>>,
    nonsilent_samples: AtomicU64,
    diag: Arc<AudioDiagnostics>,
}

unsafe impl Send for IoCtx {}
unsafe impl Sync for IoCtx {}

extern "C-unwind" fn tap_ioproc(
    _in_device: AudioObjectID,
    _in_now: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    in_input_data: NonNull<AudioBufferList>,
    _in_input_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    _out_output_data: NonNull<AudioBufferList>,
    _in_output_time: NonNull<objc2_core_audio_types::AudioTimeStamp>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    // SAFETY: `client_data` is the `Box<IoCtx>` pointer registered
    let ctx = unsafe { &*(client_data as *const IoCtx) };
    let list = unsafe { in_input_data.as_ref() };
    let nbuf = list.mNumberBuffers as usize;
    if nbuf == 0 {
        return 0;
    }
    // SAFETY: `mBuffers` is a C flexible array of `mNumberBuffers` `AudioBuffer`s
    let bufs = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), nbuf) };

    let desc = &ctx.desc;
    let bps = match desc.kind {
        format::SampleKind::F32 | format::SampleKind::I32 => 4,
        format::SampleKind::I16 => 2,
    };
    // SAFETY: only this thread touches the scratch cell
    let scratch: &mut [f32] = unsafe { &mut *ctx.scratch.get() };

    let written = if desc.non_interleaved {
        let ch = nbuf.min(8);
        let mut planes: [&[u8]; 8] = [&[][..]; 8];
        let frames = if bufs[0].mData.is_null() {
            0
        } else {
            bufs[0].mDataByteSize as usize / bps
        };
        let mut ok = true;
        for (i, p) in planes.iter_mut().enumerate().take(ch) {
            let b = &bufs[i];
            if b.mData.is_null() {
                ok = false;
                break;
            }
            // SAFETY: `mData` points at `mDataByteSize` bytes of channel `i`'s plane
            *p = unsafe {
                std::slice::from_raw_parts(b.mData as *const u8, b.mDataByteSize as usize)
            };
        }
        if !ok {
            return 0;
        }
        format::convert_planar(&planes[..ch], frames, desc, scratch)
    } else {
        let b0 = &bufs[0];
        if b0.mData.is_null() {
            return 0;
        }
        let stride = desc.channels as usize * bps;
        if stride == 0 {
            return 0;
        }
        let frames = b0.mDataByteSize as usize / stride;
        // SAFETY: `mData` points at `mDataByteSize` bytes of interleaved frames
        let src =
            unsafe { std::slice::from_raw_parts(b0.mData as *const u8, b0.mDataByteSize as usize) };
        format::convert_interleaved(src, frames, desc, scratch)
    };

    if written == 0 {
        return 0;
    }
    let frame = &scratch[..written];
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
    0
}

struct TapResources {
    destroy_tap: FnDestroyTap,
    tap_id: AudioObjectID,
    aggregate_id: AudioObjectID,
    io_proc: AudioDeviceIOProcID,
    ctx: *mut IoCtx,
    started: bool,
}

impl Drop for TapResources {
    fn drop(&mut self) {
        unsafe {
            if self.aggregate_id != 0 {
                if self.started {
                    let _ = AudioDeviceStop(self.aggregate_id, self.io_proc);
                }
                if self.io_proc.is_some() {
                    let _ = AudioDeviceDestroyIOProcID(self.aggregate_id, self.io_proc);
                }
            }
            if !self.ctx.is_null() {
                // SAFETY: ctx came from Box::into_raw; the IOProc is destroyed above
                drop(Box::from_raw(self.ctx));
                self.ctx = ptr::null_mut();
            }
            if self.aggregate_id != 0 {
                let _ = AudioHardwareDestroyAggregateDevice(self.aggregate_id);
                self.aggregate_id = 0;
            }
            if self.tap_id != 0 {
                let _ = (self.destroy_tap)(self.tap_id);
                self.tap_id = 0;
            }
        }
    }
}

struct ListenerCtx {
    tx: crossbeam_channel::Sender<ControlMsg>,
}

extern "C-unwind" fn default_device_listener(
    _in_object: AudioObjectID,
    _n: u32,
    _addrs: NonNull<AudioObjectPropertyAddress>,
    client_data: *mut c_void,
) -> i32 {
    if client_data.is_null() {
        return 0;
    }
    let ctx = unsafe { &*(client_data as *const ListenerCtx) };
    let _ = ctx.tx.try_send(ControlMsg::Reacquire);
    0
}

fn default_device_listener_address() -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultOutputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

pub struct ProcessTapCapture {
    resources: Option<TapResources>,
    listener_ctx: *mut ListenerCtx,
    nonsilent_probe: Option<Arc<AtomicU64>>,
    sink: Option<AudioFrameSink>,
}

unsafe impl Send for ProcessTapCapture {}

impl ProcessTapCapture {
    pub fn new() -> Self {
        Self {
            resources: None,
            listener_ctx: ptr::null_mut(),
            nonsilent_probe: None,
            sink: None,
        }
    }

    pub fn try_create() -> Result<(), AudioCaptureError> {
        if !runtime_available() {
            return Err(AudioCaptureError::Unsupported(
                "Core Audio Process Tap API not present (needs macOS 14.2+)".into(),
            ));
        }
        // throwaway sink; dropped with its ring right after the probe
        let (producer, _consumer, consumer_thread) = super::ring::ring(4096);
        let sink = AudioFrameSink {
            producer: Arc::new(producer),
            diagnostics: Arc::new(AudioDiagnostics::default()),
            control_tx: None,
            consumer_thread,
        };
        let mut probe = ProcessTapCapture::new();
        probe.build_and_start(&sink)?;
        Ok(())
    }

    fn build_and_start(&mut self, sink: &AudioFrameSink) -> Result<(), AudioCaptureError> {
        let (create_tap, destroy_tap) =
            tap_fns().ok_or_else(|| AudioCaptureError::Unsupported("Process Tap absent".into()))?;

        let tap_desc_cls = AnyClass::get(c"CATapDescription")
            .ok_or_else(|| AudioCaptureError::Unsupported("CATapDescription absent".into()))?;
        let ns_array_cls = AnyClass::get(c"NSArray").ok_or_else(err_runtime)?;
        let ns_uuid_cls = AnyClass::get(c"NSUUID").ok_or_else(err_runtime)?;

        // SAFETY: standard Obj-C construction via the runtime
        let (tap_desc, tap_uuid_string) = unsafe {
            let empty: *mut AnyObject = msg_send![ns_array_cls, array];
            let alloc: *mut AnyObject = msg_send![tap_desc_cls, alloc];
            let desc: *mut AnyObject =
                msg_send![alloc, initStereoGlobalTapButExcludeProcesses: empty];
            let desc = Retained::from_raw(desc).ok_or_else(err_setup)?;

            let uuid: *mut AnyObject = msg_send![ns_uuid_cls, UUID];
            let uuid = Retained::retain(uuid).ok_or_else(err_setup)?;
            let _: () = msg_send![&*desc, setUUID: &*uuid];
            let _: () = msg_send![&*desc, setMuteBehavior: CATAP_UNMUTED];
            let _: () = msg_send![&*desc, setPrivate: true];
            let name = ns_string("ScreenExtend System Audio");
            let _: () = msg_send![&*desc, setName: &*name];

            let uuid_str: *mut AnyObject = msg_send![&*uuid, UUIDString];
            (desc, nsstring_to_string(uuid_str))
        };

        let mut tap_id: AudioObjectID = 0;
        // SAFETY: `create_tap` is the resolved AudioHardwareCreateProcessTap
        let st = unsafe { create_tap(Retained::as_ptr(&tap_desc), &mut tap_id) };
        if st != 0 || tap_id == 0 {
            return Err(AudioCaptureError::Setup(format!(
                "AudioHardwareCreateProcessTap failed (OSStatus {st})"
            )));
        }
        let mut resources = TapResources {
            destroy_tap,
            tap_id,
            aggregate_id: 0,
            io_proc: None,
            ctx: ptr::null_mut(),
            started: false,
        };

        let asbd = read_tap_format(tap_id)?;
        let desc = format::parse_asbd(&asbd)
            .map_err(|e| AudioCaptureError::Setup(format!("tap ASBD parse: {e}")))?;

        let output_dev = read_default_system_output_device()?;
        let output_uid = read_device_uid(output_dev)?;

        let _ = set_nominal_sample_rate(output_dev, 48_000.0);
        let aggregate_id = create_aggregate_device(&output_uid, &tap_uuid_string)?;
        resources.aggregate_id = aggregate_id;

        let asbd = read_tap_format(tap_id).unwrap_or(asbd);
        let desc = format::parse_asbd(&asbd).unwrap_or(desc);
        if desc.sample_rate != 48_000 {
            return Err(AudioCaptureError::Setup(format!(
                "tap delivers {} Hz, not 48000; deferring to SCK backend",
                desc.sample_rate
            )));
        }

        let nonsilent = Arc::new(AtomicU64::new(0));
        let scratch = vec![0.0f32; 8192 * format::OUT_CHANNELS].into_boxed_slice();
        let ctx = Box::into_raw(Box::new(IoCtx {
            producer: Arc::clone(&sink.producer),
            desc,
            scratch: UnsafeCell::new(scratch),
            nonsilent_samples: AtomicU64::new(0),
            diag: Arc::clone(&sink.diagnostics),
        }));
        resources.ctx = ctx;
        self.nonsilent_probe = Some(nonsilent);

        let proc_fn: objc2_core_audio::AudioDeviceIOProc = Some(tap_ioproc);
        let mut io_proc: AudioDeviceIOProcID = None;
        let st = unsafe {
            AudioDeviceCreateIOProcID(
                aggregate_id,
                proc_fn,
                ctx as *mut c_void,
                NonNull::from(&mut io_proc),
            )
        };
        if st != 0 || io_proc.is_none() {
            return Err(AudioCaptureError::Setup(format!(
                "AudioDeviceCreateIOProcID failed (OSStatus {st})"
            )));
        }
        resources.io_proc = io_proc;

        let st = unsafe { AudioDeviceStart(aggregate_id, io_proc) };
        if st != 0 {
            return Err(AudioCaptureError::Setup(format!(
                "AudioDeviceStart failed (OSStatus {st})"
            )));
        }
        resources.started = true;

        sink.diagnostics
            .sample_rate
            .store(48_000, Ordering::Relaxed);
        sink.diagnostics
            .channels
            .store(format::OUT_CHANNELS as u32, Ordering::Relaxed);

        self.resources = Some(resources);
        crate::tprintln!(
            "audio: Process Tap started (source {}Hz x{}ch {:?}, downmixed to 48kHz stereo)",
            desc.sample_rate,
            desc.channels,
            desc.kind
        );
        Ok(())
    }

    fn register_device_listener(&mut self, tx: crossbeam_channel::Sender<ControlMsg>) {
        let ctx = Box::into_raw(Box::new(ListenerCtx { tx }));
        let addr = default_device_listener_address();
        let listener: AudioObjectPropertyListenerProc = Some(default_device_listener);
        let st = unsafe {
            AudioObjectAddPropertyListener(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&addr),
                listener,
                ctx as *mut c_void,
            )
        };
        if st == 0 {
            self.listener_ctx = ctx;
        } else {
            // SAFETY: registration failed, reclaim the box
            drop(unsafe { Box::from_raw(ctx) });
            crate::teprintln!("audio: default-output-device listener registration failed ({st})");
        }
    }

    fn remove_device_listener(&mut self) {
        if self.listener_ctx.is_null() {
            return;
        }
        let addr = default_device_listener_address();
        let listener: AudioObjectPropertyListenerProc = Some(default_device_listener);
        // SAFETY: removing the listener we registered with the same address + clientData
        unsafe {
            let _ = AudioObjectRemovePropertyListener(
                kAudioObjectSystemObject as AudioObjectID,
                NonNull::from(&addr),
                listener,
                self.listener_ctx as *mut c_void,
            );
            drop(Box::from_raw(self.listener_ctx));
        }
        self.listener_ctx = ptr::null_mut();
    }
}

impl AudioSource for ProcessTapCapture {
    fn start(&mut self, sink: AudioFrameSink) -> Result<(), AudioCaptureError> {
        let control_tx = sink.control_tx.clone();
        self.sink = Some(sink.clone());
        self.build_and_start(&sink)?;
        if let Some(tx) = control_tx {
            self.register_device_listener(tx);
        }
        Ok(())
    }

    fn reacquire(&mut self) -> Result<(), AudioCaptureError> {
        crate::tprintln!("audio: default output device changed; re-acquiring Process Tap");
        drop(self.resources.take());
        if let Some(sink) = self.sink.clone() {
            self.build_and_start(&sink)?;
        }
        Ok(())
    }

    fn stop(&mut self) {
        self.remove_device_listener();
        self.resources = None; // RAII teardown
        self.sink = None;
    }

    fn backend_name(&self) -> &'static str {
        "process_tap"
    }

    fn nonsilent_samples(&self) -> u64 {
        self.resources
            .as_ref()
            .map(|r| {
                // SAFETY: ctx is a live IoCtx for the session
                unsafe { &*r.ctx }.nonsilent_samples.load(Ordering::Relaxed)
            })
            .unwrap_or(0)
    }
}

impl Drop for ProcessTapCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn err_runtime() -> AudioCaptureError {
    AudioCaptureError::Unsupported("required Obj-C class not present".into())
}
fn err_setup() -> AudioCaptureError {
    AudioCaptureError::Setup("Obj-C construction returned nil".into())
}

fn ns_string(s: &str) -> Retained<AnyObject> {
    let cls = AnyClass::get(c"NSString").expect("NSString present");
    let c = CString::new(s).unwrap();
    // SAFETY: `+[NSString stringWithUTF8String:]` returns an autoreleased NSString
    unsafe {
        let s: *mut AnyObject = msg_send![cls, stringWithUTF8String: c.as_ptr()];
        Retained::retain(s).expect("stringWithUTF8String returned nil")
    }
}

fn nsstring_to_string(s: *mut AnyObject) -> String {
    if s.is_null() {
        return String::new();
    }
    // SAFETY: `s` is an NSString; `-UTF8String` returns a NUL-terminated C string valid for the call
    unsafe {
        let p: *const c_char = msg_send![&*s, UTF8String];
        if p.is_null() {
            String::new()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

unsafe fn get_property<T: Copy>(obj: AudioObjectID, selector: u32, out: &mut T) -> i32 {
    let addr = AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size = std::mem::size_of::<T>() as u32;
    // SAFETY: `out` points at a `T` of `size` bytes; no qualifier needed for these selectors
    unsafe {
        AudioObjectGetPropertyData(
            obj,
            NonNull::from(&addr),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::new(out as *mut T as *mut c_void).unwrap(),
        )
    }
}

fn read_default_system_output_device() -> Result<AudioObjectID, AudioCaptureError> {
    let mut dev: AudioObjectID = 0;
    // SAFETY: reading an AudioObjectID property off the system object
    let st = unsafe {
        get_property(
            kAudioObjectSystemObject as AudioObjectID,
            kAudioHardwarePropertyDefaultSystemOutputDevice,
            &mut dev,
        )
    };
    if st != 0 || dev == 0 {
        return Err(AudioCaptureError::Setup(format!(
            "read default system output device failed (OSStatus {st})"
        )));
    }
    Ok(dev)
}

fn read_device_uid(dev: AudioObjectID) -> Result<String, AudioCaptureError> {
    let mut cf: *const CFString = ptr::null();
    // SAFETY: kAudioDevicePropertyDeviceUID writes a (+1 retained) CFStringRef into `cf`
    let st = unsafe { get_property(dev, kAudioDevicePropertyDeviceUID, &mut cf) };
    if st != 0 || cf.is_null() {
        return Err(AudioCaptureError::Setup(format!(
            "read device UID failed (OSStatus {st})"
        )));
    }
    // SAFETY: own the returned +1 CFString and convert
    let s = unsafe {
        let owned = objc2_core_foundation::CFRetained::from_raw(NonNull::new_unchecked(
            cf as *mut CFString,
        ));
        owned.to_string()
    };
    Ok(s)
}

fn read_tap_format(
    tap_id: AudioObjectID,
) -> Result<AudioStreamBasicDescription, AudioCaptureError> {
    let mut asbd = AudioStreamBasicDescription {
        mSampleRate: 0.0,
        mFormatID: 0,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: 0,
        mBytesPerFrame: 0,
        mChannelsPerFrame: 0,
        mBitsPerChannel: 0,
        mReserved: 0,
    };
    // SAFETY: kAudioTapPropertyFormat writes an ASBD
    let st = unsafe { get_property(tap_id, kAudioTapPropertyFormat, &mut asbd) };
    if st != 0 {
        return Err(AudioCaptureError::Setup(format!(
            "read tap format failed (OSStatus {st})"
        )));
    }
    Ok(asbd)
}

fn set_nominal_sample_rate(dev: AudioObjectID, rate: f64) -> i32 {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut r = rate;
    // SAFETY: setting a f64 device property
    unsafe {
        objc2_core_audio::AudioObjectSetPropertyData(
            dev,
            NonNull::from(&addr),
            0,
            ptr::null(),
            std::mem::size_of::<f64>() as u32,
            NonNull::new(&mut r as *mut f64 as *mut c_void).unwrap(),
        )
    }
}

#[repr(C)]
struct CFCallbacks {
    _opaque: [u8; 0],
}
unsafe extern "C" {
    static kCFTypeDictionaryKeyCallBacks: CFCallbacks;
    static kCFTypeDictionaryValueCallBacks: CFCallbacks;
    static kCFTypeArrayCallBacks: CFCallbacks;
    static kCFBooleanTrue: *const c_void;
    static kCFBooleanFalse: *const c_void;
    fn CFStringCreateWithCString(
        alloc: *const c_void,
        c_str: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFArrayCreate(
        alloc: *const c_void,
        values: *const *const c_void,
        num_values: isize,
        callbacks: *const CFCallbacks,
    ) -> *const c_void;
    fn CFDictionaryCreate(
        alloc: *const c_void,
        keys: *const *const c_void,
        values: *const *const c_void,
        num_values: isize,
        key_callbacks: *const CFCallbacks,
        value_callbacks: *const CFCallbacks,
    ) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

fn cfstr(s: &str) -> *const c_void {
    let c = CString::new(s).unwrap_or_default();
    unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
}

fn cfstr_cstr(c: &CStr) -> *const c_void {
    // SAFETY: `c` is a NUL-terminated ASCII key constant; creates a +1 CFString
    unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), K_CF_STRING_ENCODING_UTF8) }
}

fn create_aggregate_device(
    output_uid: &str,
    tap_uuid: &str,
) -> Result<AudioObjectID, AudioCaptureError> {
    // SAFETY: self-contained CF container construction
    unsafe {
        let aggregate_uid = uuid_string();

        // taps: [ { drift: true, uid: <tap_uuid> } ]
        let sub_tap_keys = [
            cfstr_cstr(kAudioSubTapUIDKey),
            cfstr_cstr(kAudioSubTapDriftCompensationKey),
        ];
        let sub_tap_vals = [cfstr(tap_uuid), kCFBooleanTrue];
        let sub_tap = CFDictionaryCreate(
            ptr::null(),
            sub_tap_keys.as_ptr(),
            sub_tap_vals.as_ptr(),
            2,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        let tap_list_vals = [sub_tap];
        let tap_list = CFArrayCreate(
            ptr::null(),
            tap_list_vals.as_ptr(),
            1,
            &kCFTypeArrayCallBacks,
        );

        // subdevices: [ { uid: <output_uid> } ]
        let sub_dev_keys = [cfstr_cstr(kAudioSubDeviceUIDKey)];
        let sub_dev_vals = [cfstr(output_uid)];
        let sub_dev = CFDictionaryCreate(
            ptr::null(),
            sub_dev_keys.as_ptr(),
            sub_dev_vals.as_ptr(),
            1,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );
        let sub_dev_vals_arr = [sub_dev];
        let sub_dev_list = CFArrayCreate(
            ptr::null(),
            sub_dev_vals_arr.as_ptr(),
            1,
            &kCFTypeArrayCallBacks,
        );

        let keys = [
            cfstr_cstr(kAudioAggregateDeviceNameKey),
            cfstr_cstr(kAudioAggregateDeviceUIDKey),
            cfstr_cstr(kAudioAggregateDeviceMainSubDeviceKey),
            cfstr_cstr(kAudioAggregateDeviceIsPrivateKey),
            cfstr_cstr(kAudioAggregateDeviceIsStackedKey),
            cfstr_cstr(kAudioAggregateDeviceTapAutoStartKey),
            cfstr_cstr(kAudioAggregateDeviceSubDeviceListKey),
            cfstr_cstr(kAudioAggregateDeviceTapListKey),
        ];
        let vals = [
            cfstr("ScreenExtend System Audio"),
            cfstr(&aggregate_uid),
            cfstr(output_uid),
            kCFBooleanTrue,
            kCFBooleanFalse,
            kCFBooleanTrue,
            sub_dev_list,
            tap_list,
        ];
        let dict = CFDictionaryCreate(
            ptr::null(),
            keys.as_ptr(),
            vals.as_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        );

        for s in sub_tap_keys
            .iter()
            .chain(sub_dev_keys.iter())
            .chain(keys.iter())
        {
            CFRelease(*s);
        }
        CFRelease(sub_tap_vals[0]); // tap_uuid string
        CFRelease(sub_dev_vals[0]); // output_uid string (subdevice)
        CFRelease(vals[0]); // name string
        CFRelease(vals[1]); // aggregate uid string
        CFRelease(vals[2]); // output_uid string (main sub-device)
        CFRelease(sub_tap);
        CFRelease(tap_list);
        CFRelease(sub_dev);
        CFRelease(sub_dev_list);

        if dict.is_null() {
            return Err(AudioCaptureError::Setup(
                "aggregate device dictionary creation failed".into(),
            ));
        }

        let mut aggregate_id: AudioObjectID = 0;
        let dict_ref = &*(dict as *const objc2_core_foundation::CFDictionary);
        let st = AudioHardwareCreateAggregateDevice(dict_ref, NonNull::from(&mut aggregate_id));
        CFRelease(dict);
        if st != 0 || aggregate_id == 0 {
            return Err(AudioCaptureError::Setup(format!(
                "AudioHardwareCreateAggregateDevice failed (OSStatus {st})"
            )));
        }
        Ok(aggregate_id)
    }
}

fn uuid_string() -> String {
    let cls = AnyClass::get(c"NSUUID").expect("NSUUID present");
    // SAFETY: `+[NSUUID UUID]` then `-UUIDString`
    unsafe {
        let uuid: *mut AnyObject = msg_send![cls, UUID];
        let s: *mut AnyObject = msg_send![&*uuid, UUIDString];
        nsstring_to_string(s)
    }
}
