use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::ptr::NonNull;

use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use objc2_core_foundation::{
    CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, kCFAllocatorDefault,
    kCFTypeArrayCallBacks, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks,
};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
    kCMVideoCodecType_H264,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_DataRateLimits,
    kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxFrameDelayCount,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_MaximizePowerEfficiency,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTEncodeFrameOptionKey_ForceKeyFrame,
    kVTProfileLevel_H264_Baseline_AutoLevel, kVTProfileLevel_H264_High_AutoLevel,
    kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use crate::streamer::config::H264Profile;

use super::config::EncoderConfig;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_LENGTH_PREFIX: usize = 4;

pub struct CompressedSample {
    sample: Option<CFRetained<CMSampleBuffer>>,
}

unsafe impl Send for CompressedSample {}

impl CompressedSample {
    pub fn to_annexb(&self, out: &mut Vec<u8>) -> bool {
        out.clear();
        match &self.sample {
            Some(s) => sample_to_annexb_into(s, out),
            None => false,
        }
    }
}

struct OutputCtx {
    tx: Sender<CompressedSample>,
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    rx: Receiver<CompressedSample>,
    refcon: *mut OutputCtx,
    fps: i32,
    frame_index: i64,
    low_latency: bool,
    ltr_enabled: bool,
    qp_mode: bool,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        let (tx, rx) = unbounded::<CompressedSample>();
        let refcon = Box::into_raw(Box::new(OutputCtx { tx }));
        let want_low_latency = super::macos_at_least(11, 0);
        let mut specs: Vec<(bool, bool)> = Vec::with_capacity(3);
        if want_low_latency {
            specs.push((true, true));
        }
        specs.push((false, true));
        specs.push((false, false));

        let mut session = None;
        let mut low_latency = false;
        let mut hardware = true;
        let mut last_status = 0;
        for (ll, require_hw) in specs {
            match create_session(&cfg, &build_encoder_spec(ll, require_hw), refcon) {
                Ok(s) => {
                    session = Some(s);
                    low_latency = ll;
                    hardware = require_hw;
                    break;
                }
                Err(status) => {
                    last_status = status;
                    if require_hw {
                        teprintln!(
                            "[vt] HW encoder unavailable for {}x{} (low_latency={ll}, \
                             OSStatus {status}); trying a weaker encoder spec",
                            cfg.width,
                            cfg.height
                        );
                    }
                }
            }
        }
        let Some(session) = session else {
            drop(unsafe { Box::from_raw(refcon) });
            bail!("VTCompressionSessionCreate failed: OSStatus {last_status}");
        };
        if !hardware {
            teprintln!(
                "[vt] using SOFTWARE H.264 encoder for {}x{} — below this machine's \
                 hardware-encoder minimum frame size; honoring the requested scale",
                cfg.width,
                cfg.height
            );
        }

        let mut enc = Encoder {
            session,
            rx,
            refcon,
            fps: cfg.fps.max(1) as i32,
            frame_index: 0,
            low_latency,
            ltr_enabled: false,
            qp_mode: cfg.qp.is_some(),
        };
        enc.configure(&cfg)?;
        unsafe { enc.session.prepare_to_encode_frames() };
        tprintln!(
            "[vt] H.264 encoder ready: {}x{} @ {}fps, {} kbps, profile={:?}, low_latency={}, ltr={}",
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_bps / 1000,
            cfg.profile,
            low_latency,
            enc.ltr_enabled
        );
        Ok(enc)
    }

    fn vt_session(&self) -> &VTSession {
        unsafe { &*((&*self.session) as *const VTCompressionSession as *const VTSession) }
    }

    fn configure(&mut self, cfg: &EncoderConfig) -> Result<()> {
        let ltr;
        {
            let s = self.vt_session();
            set_bool(s, unsafe { kVTCompressionPropertyKey_RealTime }, true);
            set_bool(s, unsafe { kVTCompressionPropertyKey_AllowFrameReordering }, false);

            let profile: &CFString = if self.low_latency {
                match cfg.profile {
                    H264Profile::High => vt_optional_cfstring(
                        "kVTProfileLevel_H264_ConstrainedHigh_AutoLevel",
                    )
                    .unwrap_or(unsafe { kVTProfileLevel_H264_High_AutoLevel }),
                    H264Profile::Main => unsafe { kVTProfileLevel_H264_Main_AutoLevel },
                    H264Profile::Baseline => {
                        vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel")
                            .unwrap_or(unsafe { kVTProfileLevel_H264_Baseline_AutoLevel })
                    }
                }
            } else {
                match cfg.profile {
                    H264Profile::Baseline => {
                        vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel")
                            .unwrap_or(unsafe { kVTProfileLevel_H264_Baseline_AutoLevel })
                    }
                    H264Profile::Main => unsafe { kVTProfileLevel_H264_Main_AutoLevel },
                    H264Profile::High => unsafe { kVTProfileLevel_H264_High_AutoLevel },
                }
            };
            set_cftype(s, unsafe { kVTCompressionPropertyKey_ProfileLevel }, profile);
            set_optional_str(s, "kVTCompressionPropertyKey_ColorPrimaries", "ITU_R_709_2");
            set_optional_str(s, "kVTCompressionPropertyKey_TransferFunction", "ITU_R_709_2");
            set_optional_str(s, "kVTCompressionPropertyKey_YCbCrMatrix", "ITU_R_709_2");
            set_i32(s, unsafe { kVTCompressionPropertyKey_ExpectedFrameRate }, cfg.fps as i32);
            set_i32(
                s,
                unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
                (cfg.fps as i32).saturating_mul(60).max(1),
            );
            set_f64(s, unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration }, 10.0);
            set_i32(s, unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount }, 0);
            set_bool(s, unsafe { kVTCompressionPropertyKey_MaximizePowerEfficiency }, false);
            set_bool(s, unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality }, true);
            ltr = set_optional(s, "kVTCompressionPropertyKey_EnableLTR", cfbool(true));
            if cfg.intra_refresh {
                teprintln!(
                    "[vt] intra-refresh requested but unsupported on macOS VideoToolbox \
                     (no rolling intra-refresh); using PLI→IDR loss recovery instead"
                );
            }
        }

        self.ltr_enabled = ltr;
        self.configure_rate_control(cfg);
        self.log_hw_acceleration();
        Ok(())
    }

    fn configure_rate_control(&self, cfg: &EncoderConfig) {
        let s = self.vt_session();
        if let Some(qp) = cfg.qp {
            let quality = ((51.0 - qp as f64) / 50.0).clamp(0.0, 1.0);
            set_optional_f64(s, "kVTCompressionPropertyKey_Quality", quality);
            set_optional_i32(s, "kVTCompressionPropertyKey_MaxAllowedFrameQP", qp as i32);
            set_optional_i32(s, "kVTCompressionPropertyKey_MinAllowedFrameQP", qp as i32);
            return;
        }
        set_i32(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, cfg.bitrate_bps as i32);
        let cap_bytes = (((cfg.bitrate_bps as f64) * 1.5) / 8.0) as i64;
        let limits = data_rate_limits(cap_bytes, 1.0);
        set_cftype(s, unsafe { kVTCompressionPropertyKey_DataRateLimits }, &limits);
        set_optional_i32(s, "kVTCompressionPropertyKey_MaxAllowedFrameQP", 45);
    }

    pub fn submit(&mut self, image_buffer: &CVImageBuffer, force_idr: bool) -> Result<()> {
        let pts = CMTime {
            value: self.frame_index,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let dur = CMTime { value: 1, timescale: self.fps, flags: CMTimeFlags::Valid, epoch: 0 };
        self.frame_index += 1;

        let frame_props = if force_idr {
            Some(dict_from_pairs(&[(
                unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame },
                cfbool(true),
            )]))
        } else {
            None
        };

        let mut info = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                image_buffer,
                pts,
                dur,
                frame_props.as_deref(),
                ptr::null_mut(),
                &mut info,
            )
        };
        if status != 0 {
            bail!("VTCompressionSessionEncodeFrame failed: OSStatus {status}");
        }
        Ok(())
    }

    pub fn output(&self) -> Receiver<CompressedSample> {
        self.rx.clone()
    }

    pub fn flush(&mut self) -> Result<()> {
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        let st = unsafe { self.session.complete_frames(invalid) };
        if st != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: OSStatus {st}");
        }
        Ok(())
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if self.qp_mode {
            return Ok(());
        }
        let s = self.vt_session();
        set_i32(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, bps as i32);
        let cap_bytes = (((bps as f64) * 1.5) / 8.0) as i64;
        let limits = data_rate_limits(cap_bytes, 1.0);
        set_cftype(s, unsafe { kVTCompressionPropertyKey_DataRateLimits }, &limits);
        Ok(())
    }

    pub fn low_latency(&self) -> bool {
        self.low_latency
    }

    fn log_hw_acceleration(&self) {
        let Some(key) =
            vt_optional_cfstring("kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder")
        else {
            return;
        };
        let mut value: *const CFType = ptr::null();
        let st = unsafe { VTSessionCopyProperty(self.vt_session(), key, ptr::null(), &mut value) };
        if st == 0 && !value.is_null() {
            let using_hw = unsafe { CFBooleanGetValue(value as *const c_void) } != 0;
            tprintln!("[vt] UsingHardwareAcceleratedVideoEncoder = {using_hw}");
            unsafe { CFRelease(value as *const c_void) };
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        unsafe {
            let _ = self.session.complete_frames(invalid);
            self.session.invalidate();
        }
        if !self.refcon.is_null() {
            drop(unsafe { Box::from_raw(self.refcon) });
        }
    }
}

unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _src_frame_refcon: *mut c_void,
    status: i32,
    _info: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    let ctx = unsafe { &*(refcon as *const OutputCtx) };
    let Some(sample_ptr) = NonNull::new(sample) else {
        let _ = ctx.tx.send(CompressedSample { sample: None });
        return;
    };
    if status != 0 {
        let _ = ctx.tx.send(CompressedSample { sample: None });
        return;
    }
    let retained = unsafe { CFRetained::retain(sample_ptr) };
    let _ = ctx.tx.send(CompressedSample { sample: Some(retained) });
}

fn sample_to_annexb_into(sample: &CMSampleBuffer, out: &mut Vec<u8>) -> bool {
    let Some(bb) = (unsafe { sample.data_buffer() }) else {
        return false;
    };
    let mut total: usize = 0;
    let mut base: *mut c_char = ptr::null_mut();
    let st = unsafe { bb.data_pointer(0, ptr::null_mut(), &mut total, &mut base) };
    if st != 0 || base.is_null() {
        return false;
    }
    let data = unsafe { std::slice::from_raw_parts(base as *const u8, total) };

    let mut is_keyframe = false;
    let mut off = 0usize;
    while off + NAL_LENGTH_PREFIX <= data.len() {
        let nal_len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        off += NAL_LENGTH_PREFIX;
        if nal_len == 0 || off + nal_len > data.len() {
            break;
        }
        if (data[off] & 0x1F) == 5 {
            is_keyframe = true;
            break;
        }
        off += nal_len;
    }

    out.reserve(total + if is_keyframe { 256 } else { 16 });

    if is_keyframe {
        if let Some(fmt) = unsafe { sample.format_description() } {
            let mut count: usize = 0;
            let _ = unsafe {
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    &fmt,
                    0,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    &mut count,
                    ptr::null_mut(),
                )
            };
            for i in 0..count {
                let mut p: *const u8 = ptr::null();
                let mut len: usize = 0;
                let mut hdr: c_int = 0;
                let st = unsafe {
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        &fmt,
                        i,
                        &mut p,
                        &mut len,
                        ptr::null_mut(),
                        &mut hdr,
                    )
                };
                if st == 0 && !p.is_null() && len > 0 {
                    out.extend_from_slice(&START_CODE);
                    out.extend_from_slice(unsafe { std::slice::from_raw_parts(p, len) });
                }
            }
        }
    }

    let mut off = 0usize;
    while off + NAL_LENGTH_PREFIX <= data.len() {
        let nal_len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        off += NAL_LENGTH_PREFIX;
        if nal_len == 0 || off + nal_len > data.len() {
            break;
        }
        if (data[off] & 0x1F) == 9 {
            off += nal_len;
            continue;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[off..off + nal_len]);
        off += nal_len;
    }

    is_keyframe
}

fn build_encoder_spec(low_latency: bool, require_hw: bool) -> CFRetained<CFDictionary> {
    let hw_key = if require_hw {
        unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }
    } else {
        unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder }
    };
    let ll_key = CFString::from_str("EnableLowLatencyRateControl");
    let mut pairs: Vec<(&CFString, &CFType)> = Vec::with_capacity(2);
    pairs.push((hw_key, cfbool(true)));
    if low_latency {
        pairs.push((&ll_key, cfbool(true)));
    }
    dict_from_pairs(&pairs)
}

fn create_session(
    cfg: &EncoderConfig,
    spec: &CFDictionary,
    refcon: *mut OutputCtx,
) -> Result<CFRetained<VTCompressionSession>, i32> {
    let mut session_ptr: *mut VTCompressionSession = ptr::null_mut();
    let status = unsafe {
        VTCompressionSession::create(
            kCFAllocatorDefault,
            cfg.width as i32,
            cfg.height as i32,
            kCMVideoCodecType_H264,
            Some(spec),
            None,
            None,
            Some(output_callback),
            refcon as *mut c_void,
            NonNull::from(&mut session_ptr),
        )
    };
    match NonNull::new(session_ptr) {
        Some(p) if status == 0 => Ok(unsafe { CFRetained::from_raw(p) }),
        _ => Err(status),
    }
}

fn cfbool(v: bool) -> &'static CFBoolean {
    CFBoolean::new(v)
}

unsafe extern "C" {
    fn VTSessionCopyProperty(
        session: &VTSession,
        property_key: &CFString,
        allocator: *const c_void,
        property_value_out: *mut *const CFType,
    ) -> i32;
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
    fn CFRelease(cf: *const c_void);
}

fn vt_optional_cfstring(symbol: &str) -> Option<&'static CFString> {
    let cname = std::ffi::CString::new(symbol).ok()?;
    unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr());
        if p.is_null() {
            return None;
        }
        (*(p as *const *const CFString)).as_ref()
    }
}

fn set_optional(session: &VTSession, key_symbol: &str, value: &CFType) -> bool {
    match vt_optional_cfstring(key_symbol) {
        Some(k) => {
            set_cftype(session, k, value);
            true
        }
        None => false,
    }
}

fn set_optional_str(session: &VTSession, key_symbol: &str, value: &str) -> bool {
    let v = CFString::from_str(value);
    set_optional(session, key_symbol, &v)
}

fn set_optional_i32(session: &VTSession, key_symbol: &str, value: i32) -> bool {
    let n = CFNumber::new_i32(value);
    set_optional(session, key_symbol, &n)
}

fn set_optional_f64(session: &VTSession, key_symbol: &str, value: f64) -> bool {
    let n = CFNumber::new_f64(value);
    set_optional(session, key_symbol, &n)
}

fn dict_from_pairs(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary> {
    let keys: Vec<*const c_void> =
        pairs.iter().map(|(k, _)| (*k as *const CFString).cast()).collect();
    let values: Vec<*const c_void> =
        pairs.iter().map(|(_, v)| (*v as *const CFType).cast()).collect();
    unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            pairs.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
        .expect("CFDictionaryCreate")
    }
}

fn data_rate_limits(max_bytes: i64, window_secs: f64) -> CFRetained<CFArray> {
    let bytes = CFNumber::new_isize(max_bytes as isize);
    let secs = CFNumber::new_f64(window_secs);
    let values: [*const c_void; 2] =
        [(&*bytes as *const CFNumber).cast(), (&*secs as *const CFNumber).cast()];
    unsafe {
        CFArray::new(None, values.as_ptr() as *mut *const c_void, 2, &kCFTypeArrayCallBacks)
            .expect("CFArrayCreate for DataRateLimits")
    }
}

fn set_cftype(session: &VTSession, key: &CFString, value: &CFType) {
    let st = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if st != 0 {
        teprintln!("[vt] VTSessionSetProperty failed: OSStatus {st}");
    }
}

fn set_bool(session: &VTSession, key: &CFString, value: bool) {
    set_cftype(session, key, cfbool(value));
}

fn set_i32(session: &VTSession, key: &CFString, value: i32) {
    let n = CFNumber::new_i32(value);
    set_cftype(session, key, &n);
}

fn set_f64(session: &VTSession, key: &CFString, value: f64) {
    let n = CFNumber::new_f64(value);
    set_cftype(session, key, &n);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::config::H264Profile;
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddress,
        CVPixelBufferGetBytesPerRow, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
        CVPixelBufferUnlockBaseAddress,
    };

    const BGRA: u32 = 0x4247_5241; // kCVPixelFormatType_32BGRA

    fn make_bgra_frame(w: usize, h: usize, tick: usize) -> CFRetained<CVPixelBuffer> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = unsafe {
            CVPixelBufferCreate(kCFAllocatorDefault, w, h, BGRA, None, NonNull::from(&mut out))
        };
        assert_eq!(r, 0, "CVPixelBufferCreate");
        let pb = unsafe { CFRetained::from_raw(NonNull::new(out).unwrap()) };
        unsafe {
            CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
            let bpr = CVPixelBufferGetBytesPerRow(&pb);
            for y in 0..h {
                let row = base.add(y * bpr);
                for x in 0..w {
                    let p = row.add(x * 4);
                    *p = (x + tick) as u8; // B
                    *p.add(1) = (y + tick) as u8; // G
                    *p.add(2) = tick as u8; // R
                    *p.add(3) = 255; // A
                }
            }
            CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        }
        pb
    }

    fn nal_types(au: &[u8]) -> Vec<u8> {
        let mut types = Vec::new();
        let mut i = 0;
        while i + 4 <= au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
                if i + 4 < au.len() {
                    types.push(au[i + 4] & 0x1F);
                }
                i += 4;
            } else {
                i += 1;
            }
        }
        types
    }

    #[test]
    #[ignore]
    fn encode_synthetic() {
        let (w, h) = (640usize, 480usize);
        let cfg = EncoderConfig {
            width: w as u32,
            height: h as u32,
            fps: 60,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 6_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            intra_refresh: false,
        };
        let mut enc = Encoder::new(cfg).expect("encoder creates (HW required)");
        let out = enc.output();

        for i in 0..60usize {
            let pb = make_bgra_frame(w, h, i);
            enc.submit(&pb, i == 30).expect("submit frame");
        }
        enc.flush().expect("flush");

        let mut bitstream: Vec<u8> = Vec::new();
        let mut per_frame: Vec<Vec<u8>> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(cs) = out.recv_timeout(std::time::Duration::from_millis(500)) {
            cs.to_annexb(&mut buf);
            if buf.is_empty() {
                continue;
            }
            per_frame.push(nal_types(&buf));
            bitstream.extend_from_slice(&buf);
        }

        std::fs::write("/tmp/screenextend_test.h264", &bitstream).expect("write bitstream");
        println!(
            "wrote {} bytes, {} frames to /tmp/screenextend_test.h264",
            bitstream.len(),
            per_frame.len()
        );
        let keyframes: Vec<usize> =
            per_frame.iter().enumerate().filter(|(_, n)| n.contains(&5)).map(|(i, _)| i).collect();
        println!("frame 0 NALs: {:?}", per_frame[0]);
        println!("frame 1 NALs: {:?}", per_frame[1]);
        println!("keyframe indices: {keyframes:?}");

        assert!(per_frame.len() >= 55, "expected ~60 frames, got {}", per_frame.len());
        assert!(per_frame[0].contains(&7), "frame 0 missing SPS");
        assert!(per_frame[0].contains(&8), "frame 0 missing PPS");
        assert!(per_frame[0].contains(&5), "frame 0 missing IDR slice");
        assert!(per_frame[1].contains(&1), "frame 1 not a P-frame");
        assert!(!per_frame[1].contains(&5), "frame 1 unexpectedly an IDR");
        assert!(keyframes.len() <= 3, "too many keyframes: {keyframes:?} (P-frames not used)");
        assert!(keyframes.iter().any(|&i| (28..=31).contains(&i)), "forced keyframe ~30 missing");

        for (i, nals) in per_frame.iter().enumerate() {
            for &t in nals {
                assert!(matches!(t, 1 | 5 | 6 | 7 | 8 | 9), "frame {i} unexpected NAL type {t}");
            }
        }
    }

    fn cfg_at(w: u32, h: u32) -> EncoderConfig {
        EncoderConfig {
            width: w,
            height: h,
            fps: 60,
            bitrate_bps: 6_000_000,
            max_bitrate_bps: 6_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            intra_refresh: false,
        }
    }

    #[test]
    #[ignore]
    fn recreate_churn_releases_hw() {
        for i in 0..8 {
            let enc = Encoder::new(cfg_at(1280, 720));
            assert!(enc.is_ok(), "create #{i} at 1280x720 failed: {:?}", enc.err());
            drop(enc);
            println!("ok: churn iteration {i} created+released at 1280x720");
        }
    }

    fn hw_create_status(w: u32, h: u32) -> Result<(), i32> {
        let (tx, _rx) = unbounded::<CompressedSample>();
        let refcon = Box::into_raw(Box::new(OutputCtx { tx }));
        let r = create_session(&cfg_at(w, h), &build_encoder_spec(false, true), refcon);
        let out = match r {
            Ok(s) => {
                unsafe { s.invalidate() };
                Ok(())
            }
            Err(e) => Err(e),
        };
        drop(unsafe { Box::from_raw(refcon) });
        out
    }

    #[test]
    #[ignore]
    fn creates_small_via_software() {
        for (w, h) in [(480u32, 270u32), (448u32, 252u32), (160u32, 90u32)] {
            let hw = hw_create_status(w, h);
            let enc = Encoder::new(cfg_at(w, h));
            assert!(enc.is_ok(), "Encoder::new must succeed at {w}x{h}: {:?}", enc.err());
            println!("ok: {w}x{h} created (require-HW was {hw:?}; Encoder::new used SW fallback)");
        }
    }

    #[test]
    #[ignore]
    fn hw_capability_probe() {
        for (w, h) in [
            (1280u32, 720u32),
            (1024, 576),
            (960, 540),
            (854, 480),
            (768, 432),
            (720, 480),
            (640, 360),
            (576, 324),
            (512, 288),
            (480, 270),
            (426, 240),
            (320, 180),
        ] {
            match hw_create_status(w, h) {
                Ok(()) => println!("ladder {w}x{h}: HW OK"),
                Err(e) => println!("ladder {w}x{h}: HW FAILED (OSStatus {e})"),
            }
        }
        let mut held: Vec<Encoder> = Vec::new();
        for n in 1..=6 {
            match Encoder::new(cfg_at(1280, 720)) {
                Ok(enc) => {
                    held.push(enc);
                    println!("concurrency: {n} simultaneous HW session(s) OK");
                }
                Err(e) => {
                    println!("concurrency: capped at {} simultaneous HW session(s) ({e})", n - 1);
                    break;
                }
            }
        }
        drop(held);
    }
}
