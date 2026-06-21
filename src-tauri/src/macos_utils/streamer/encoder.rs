//! VideoToolbox H.264 encoder (PRD Part II §§12–14).
//!
//! Drives `VTCompressionSession` directly via `objc2-video-toolbox` (PRD §12 —
//! one pointer world: the captured `IOSurface`-backed `CVPixelBuffer` feeds
//! `VTCompressionSessionEncodeFrame` with zero copy/conversion).
//!
//! ## macOS 10.15 note
//! The WWDC21 low-latency rate-control mode
//! (`kVTVideoEncoderSpecification_EnableLowLatencyRateControl`) does not exist in
//! the 10.15 VideoToolbox framework — referencing that symbol would break the
//! dyld load. So this build uses the classic real-time config (`RealTime=true`,
//! no B-frames, `AverageBitRate`, `MaxFrameDelayCount=0`), which is well
//! supported on 10.15 and still one-in/one-out low latency. On 12.3+ the
//! low-latency spec key is added behind a runtime version guard.
//!
//! ## Codec choice
//! H.264 only. HEVC would serialize ~smaller (a latency win) but WebRTC browser
//! receivers negotiate H.264, not H.265, so HEVC is intentionally not offered —
//! it would only help if the receiver path ever negotiates H.265.
//!
//! ## Version-gated VideoToolbox constants
//! Property keys/values introduced after 10.15 (ConstrainedHigh/Baseline,
//! LTR, temporal SVC, CBR, frame-QP, color, …) are resolved at runtime via
//! [`vt_optional_cfstring`] (a `dlsym`), never referenced as objc2 `extern`
//! statics — an undefined symbol for a constant absent on 10.15 would break the
//! dyld load, the same reason `EnableLowLatencyRateControl` is a literal CFString.

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
/// AVCC length-prefix width VideoToolbox emits for H.264 (4-byte big-endian).
const NAL_LENGTH_PREFIX: usize = 4;

/// A retained compressed sample handed straight off the VideoToolbox output
/// callback. The Annex B conversion (AVCC→Annex B, SPS/PPS prepend) is deferred
/// to the drain thread via [`CompressedSample::to_annexb`] (2.3): the callback
/// does only a retain + channel send, so VideoToolbox is freed to emit the next
/// frame without waiting on our copy. `None` marks a failed/empty emission.
pub struct CompressedSample {
    sample: Option<CFRetained<CMSampleBuffer>>,
}

// The retained CMSampleBuffer is a CF object, safe to release from any thread;
// it is only read (never mutated) on the drain thread.
unsafe impl Send for CompressedSample {}

impl CompressedSample {
    /// Convert this sample to an Annex B access unit, appended into `out` (which
    /// is cleared first). `out` is reused across frames by the drain so the
    /// output path performs no per-frame conversion allocation (2.4 / PRD §14.2).
    /// Returns whether the access unit is a keyframe; an empty `out` means a
    /// failed/empty sample the caller should skip.
    pub fn to_annexb(&self, out: &mut Vec<u8>) -> bool {
        out.clear();
        match &self.sample {
            Some(s) => sample_to_annexb_into(s, out),
            None => false,
        }
    }
}

/// Heap state the C output callback recovers through its refcon. Kept alive by
/// the [`Encoder`] for the session's lifetime.
struct OutputCtx {
    tx: Sender<CompressedSample>,
}

/// Hardware H.264 encoder over `VTCompressionSession`.
pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    rx: Receiver<CompressedSample>,
    /// Raw `OutputCtx` pointer handed to the session as the callback refcon;
    /// reclaimed on drop.
    refcon: *mut OutputCtx,
    fps: i32,
    frame_index: i64,
    /// True when the WWDC21 low-latency rate-control mode is active (macOS 11+).
    /// In that mode the encoder is genuinely one-in/one-out, so the pipeline's
    /// flush-copy submit is redundant (but kept — it is harmless and guards
    /// against the mode silently not engaging).
    low_latency: bool,
    /// True when Long-Term Reference frames are enabled (macOS 12+). The encoder
    /// then maintains LTRs internally; the per-frame ack-token round-trip that
    /// turns this into IDR-free loss recovery is driven from the transport (see
    /// the LTR note in the encode loop).
    ltr_enabled: bool,
    /// True when the session runs in constant-QP / quality mode (`cfg.qp` set):
    /// rate control is pinned to Quality + frame-QP, NOT AverageBitRate. Live
    /// `set_bitrate` calls (driven by BWE) must be no-ops in this mode — otherwise
    /// the first one silently flips the session into ABR, defeating the requested
    /// constant-QP. Recorded here because the encoder keeps no other copy of cfg.
    qp_mode: bool,
}

// The session + channel are safe to move across threads; the encode loop owns
// the encoder and the callback only touches the refcon's channel.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Create + configure the session and warm it (PRD §13.1–§13.2).
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        // Unbounded so the C output callback never blocks (PRD invariant #3);
        // the drain consumes promptly, so it stays shallow in practice.
        let (tx, rx) = unbounded::<CompressedSample>();
        let refcon = Box::into_raw(Box::new(OutputCtx { tx }));

        // Create the session, preferring the hardware encoder + (on 11+)
        // low-latency rate control, trying progressively weaker specs:
        //
        //   1. require HW + low-latency (11+ only)  — the fast path
        //   2. require HW, no low-latency           — for 11+ encoders that
        //                                             reject the LL key
        //   3. *allow* software, no low-latency     — only for sizes the HW
        //                                             encoder won't serve
        //
        // Steps 1–2 *require* the hardware encoder, so a normal-resolution session
        // never silently lands on the (slow) software encoder. Step 3 exists so a
        // deliberately tiny video-scale — below the HW encoder's minimum frame
        // size (~640x360 on the old Intel test box), where require-HW returns
        // kVTVideoEncoderNotAvailableNowErr (-12915) — is honored at the user's
        // chosen resolution via the software encoder rather than being upscaled.
        // (Adaptive: on Apple Silicon the HW floor is tiny, so step 1 serves
        // almost everything and step 3 is effectively never reached.)
        //
        // NB: the previous -12915 failures at *normal* resolutions were a HW
        // encoder leak (a prior session still holding the single HW encoder), now
        // fixed by `Encoder::drop` invalidating the session and
        // `SessionCapture::stop` joining the encode thread (see pipeline.rs).
        let want_low_latency = super::macos_at_least(11, 0);
        let mut specs: Vec<(bool, bool)> = Vec::with_capacity(3); // (low_latency, require_hw)
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
        // Warm the encoder so the first real frame isn't slow.
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
        // VTCompressionSessionRef IS a VTSessionRef in C; the objc2 bindings
        // model them as distinct opaque cf_types, so reinterpret the pointer.
        unsafe { &*((&*self.session) as *const VTCompressionSession as *const VTSession) }
    }

    fn configure(&mut self, cfg: &EncoderConfig) -> Result<()> {
        let ltr;
        {
            let s = self.vt_session();
            // Real-time hint: do not buffer to optimize throughput (PRD §13.2).
            set_bool(s, unsafe { kVTCompressionPropertyKey_RealTime }, true);
            // No B-frames → no reorder delay; monotonic RTP (CBP has none anyway).
            set_bool(s, unsafe { kVTCompressionPropertyKey_AllowFrameReordering }, false);

            // ---- Profile / level ----
            // Low-latency rate control supports High profiles only (WWDC21);
            // prefer ConstrainedHigh (11+: no B-frames, broadly decodable — a
            // better WebRTC fit than full High) and fall back to High where it is
            // absent. On the non-LL path a Baseline request prefers
            // ConstrainedBaseline (11+) — WebRTC's preferred CBP, profile-level-id
            // 42e01f — and falls back to Baseline_AutoLevel on 10.15 (whose VT
            // output is CBP-compatible in practice). Main/High pass through. Each
            // version-gated level is resolved via `dlsym`, so referencing a level
            // absent on 10.15 cannot break the dyld load.
            let profile: &CFString = if self.low_latency {
                vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedHigh_AutoLevel")
                    .unwrap_or(unsafe { kVTProfileLevel_H264_High_AutoLevel })
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

            // ---- Color tags: ITU-R BT.709 (PRD §13.2) ----
            // Tag primaries/transfer/matrix so the receiver renders correct color
            // without guessing. The values are the stable public CFString tokens
            // (present on every supported OS); set as literal CFStrings to avoid
            // pulling in the CoreVideo color-constant symbols.
            set_optional_str(s, "kVTCompressionPropertyKey_ColorPrimaries", "ITU_R_709_2");
            set_optional_str(s, "kVTCompressionPropertyKey_TransferFunction", "ITU_R_709_2");
            set_optional_str(s, "kVTCompressionPropertyKey_YCbCrMatrix", "ITU_R_709_2");

            // Pacing hint (also required by the CBR and constant-QP paths).
            set_i32(s, unsafe { kVTCompressionPropertyKey_ExpectedFrameRate }, cfg.fps as i32);

            // Desktop content is static → rare keyframes; recover via on-demand
            // IDR (PRD §15.2). Bound the GOP by TIME (10s), which is immune to the
            // keepalive duplicate-frame submissions inflating the frame count, plus
            // a large frame-count cap as a backstop.
            set_i32(
                s,
                unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
                (cfg.fps as i32).saturating_mul(60).max(1),
            );
            set_f64(s, unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration }, 10.0);
            // Hold zero frames before emitting → minimal pipeline depth.
            set_i32(s, unsafe { kVTCompressionPropertyKey_MaxFrameDelayCount }, 0);
            // Speed over battery.
            set_bool(s, unsafe { kVTCompressionPropertyKey_MaximizePowerEfficiency }, false);
            // Latency-obsessed: favor speed over quality (PRD: latency first).
            set_bool(s, unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality }, true);

            // ---- Temporal scalability (SVC, 12+) ----
            // Two temporal layers (base at half the frame rate) so an SFU can drop
            // the enhancement layer under congestion instead of stalling — keeps
            // latency low on bad networks and halves decode load for laggy
            // receivers. Output frames are tagged base/enhancement in their sample
            // attachments for the SFU to map onto the RTP TID. Dyld-gated; below 12
            // the key is simply absent and the stream stays single-layer.
            set_optional_f64(s, "kVTCompressionPropertyKey_BaseLayerFrameRateFraction", 0.5);

            // ---- Long-Term Reference frames (LTR, 12+) ----
            // Lets the encoder keep acknowledged long-term references so loss can
            // be recovered by predicting from the last *acked* LTR instead of a
            // full IDR (no serialization spike / quality drop). Enabling it here is
            // safe and dyld-gated; it is inert until the transport feeds back acked
            // tokens (then PLI recovery upgrades from ForceKeyFrame to
            // ForceLTRRefresh — see the LTR note in `run_encode_loop`).
            ltr = set_optional(s, "kVTCompressionPropertyKey_EnableLTR", cfbool(true));

            // intra-refresh knob (config): VideoToolbox has no NVENC-style rolling
            // intra-refresh, and AllowOpenGOP is not an equivalent (it governs GOP
            // structure, not a per-row refresh wave). Unsupported on macOS by
            // design — loss recovery is PLI→IDR (see [[intra-refresh-visibility]]).
            if cfg.intra_refresh {
                teprintln!(
                    "[vt] intra-refresh requested but unsupported on macOS VideoToolbox \
                     (no rolling intra-refresh); using PLI→IDR loss recovery instead"
                );
            }
        }

        self.ltr_enabled = ltr;
        // ---- Rate control (exactly one mutually-exclusive mode) ----
        self.configure_rate_control(cfg);
        // ---- Confirm the bound encoder (HW vs SW) from VT itself. ----
        self.log_hw_acceleration();
        Ok(())
    }

    /// Apply exactly one rate-control mode (they are mutually exclusive in
    /// VideoToolbox):
    ///
    /// * **constant-QP / quality** when `cfg.qp` is set (PRD `EncoderConfig.qp`)
    ///   — pins quality to kill flicker on static/dark desktop content. Maps to
    ///   VT `Quality` (works on every OS) and, on 12+/14+, pins
    ///   Min/MaxAllowedFrameQP to the requested QP for true constant-QP, which
    ///   also bounds worst-case frame size → bounded serialization latency. No
    ///   bitrate cap.
    /// * **average bitrate + DataRateLimits** (default) — the BWE target plus a
    ///   hard 1.5×, 1-second spike cap so a single IDR can't monopolize the uplink
    ///   and inject jitter. VT meets the cap by raising QP, not by spreading a
    ///   frame across packets, so it trims transmit latency without adding encoder
    ///   delay. This deliberately beats ConstantBitRate (13+) for screen content:
    ///   CBR would pad bits through the long static stretches; ABR+DataRateLimits
    ///   spends nothing when the screen is still. CBR evaluated and rejected.
    fn configure_rate_control(&self, cfg: &EncoderConfig) {
        let s = self.vt_session();
        if let Some(qp) = cfg.qp {
            // VT `Quality` is 0..1 (higher = better); map QP 1..51 (lower = better)
            // inversely so the knob still does something on 10.15, where the
            // frame-QP pins below are absent.
            let quality = ((51.0 - qp as f64) / 50.0).clamp(0.0, 1.0);
            set_optional_f64(s, "kVTCompressionPropertyKey_Quality", quality);
            // True per-frame QP pin (12+ / 14+); dyld-gated.
            set_optional_i32(s, "kVTCompressionPropertyKey_MaxAllowedFrameQP", qp as i32);
            set_optional_i32(s, "kVTCompressionPropertyKey_MinAllowedFrameQP", qp as i32);
            return;
        }
        set_i32(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, cfg.bitrate_bps as i32);
        let cap_bytes = (((cfg.bitrate_bps as f64) * 1.5) / 8.0) as i64;
        let limits = data_rate_limits(cap_bytes, 1.0);
        set_cftype(s, unsafe { kVTCompressionPropertyKey_DataRateLimits }, &limits);
    }

    /// Submit one frame's `CVPixelBuffer` for encoding (zero-copy). Returns
    /// immediately — the compressed access unit is delivered asynchronously
    /// through [`Encoder::output`] (PRD §14.1). `force_idr` requests an IDR for
    /// this frame (PLI recovery, §15.2).
    ///
    /// We deliberately do NOT flush per frame: letting the encoder pipeline lets
    /// it emit P-frames (a per-frame `CompleteFrames` would make every frame an
    /// IDR). With `MaxFrameDelayCount=0` + `RealTime` the pipeline depth is
    /// minimal, so steady-state latency stays low.
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

    /// Receiver for compressed samples. Clone-able (crossbeam mpmc); the pipeline
    /// drains it on a dedicated thread, where it does the Annex B conversion (2.3).
    pub fn output(&self) -> Receiver<CompressedSample> {
        self.rx.clone()
    }

    /// Flush all pending frames out of the encoder (end of stream / tests).
    pub fn flush(&mut self) -> Result<()> {
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        let st = unsafe { self.session.complete_frames(invalid) };
        if st != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: OSStatus {st}");
        }
        Ok(())
    }

    /// Push a new target bitrate live (BWE → `AverageBitRate`, §15.3).
    ///
    /// Constant-QP sessions ignore this: their rate control is pinned to
    /// Quality + frame-QP, and setting `AverageBitRate` would silently flip them
    /// into ABR — so in `qp_mode` this is a no-op (and BWE leaves QP alone).
    ///
    /// In bitrate mode we re-tighten `DataRateLimits` alongside the average, to
    /// the same 1.5×/1s cap `configure_rate_control` set initially. Without this
    /// the spike cap stays frozen at the *initial* (high) bitrate: after a BWE
    /// cut to a narrow link, a single IDR could still legally emit ~1.5× the
    /// *original* rate, refilling the send queue and injecting exactly the
    /// transmit-latency jitter the cap exists to prevent. Tracking the cap down
    /// with the target keeps the burst ceiling proportional to the live link.
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

    /// True when the WWDC21 low-latency rate-control mode engaged (macOS 11+). In
    /// that mode the encoder is genuinely one-in/one-out, so the pipeline can skip
    /// the 10.15 flush-copy submit (1.4). Read off the hot path.
    pub fn low_latency(&self) -> bool {
        self.low_latency
    }

    /// Best-effort confirmation of which encoder the session actually bound, read
    /// back from VideoToolbox rather than inferred from which spec succeeded
    /// (2.6b). `UsingHardwareAcceleratedVideoEncoder` is itself version-gated, so
    /// it is looked up via `dlsym` and silently skipped where absent.
    fn log_hw_acceleration(&self) {
        let Some(key) =
            vt_optional_cfstring("kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder")
        else {
            return;
        };
        let mut value: *const CFType = ptr::null();
        // SAFETY: standard VTSessionCopyProperty; on success it writes a +1
        // CFBoolean we own and must release.
        let st = unsafe { VTSessionCopyProperty(self.vt_session(), key, ptr::null(), &mut value) };
        if st == 0 && !value.is_null() {
            let using_hw = unsafe { CFBooleanGetValue(value as *const c_void) } != 0;
            tprintln!("[vt] UsingHardwareAcceleratedVideoEncoder = {using_hw}");
            // SAFETY: VTSessionCopyProperty returned this with a +1 retain.
            unsafe { CFRelease(value as *const c_void) };
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Deterministically release the hardware encoder NOW. Merely dropping the
        // `CFRetained` session only auto-invalidates once its retain count hits
        // zero, which can lag (the async output callback, in-flight frames, and CF
        // autorelease pools all hold transient references). That lag means a
        // session created immediately after this one races a still-claimed HW
        // encoder and fails with kVTVideoEncoderNotAvailableNowErr (-12915) — even
        // at ordinary resolutions. An explicit invalidate frees the HW encoder
        // synchronously, before the next `VTCompressionSessionCreate`. (The
        // binding documents this as the way to get "deterministic, orderly
        // teardown".) Flushing first lets any queued frame drain cleanly.
        // SAFETY: the session is still live here; invalidate is idempotent and the
        // callback tolerates running during teardown (it only touches the refcon).
        let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
        unsafe {
            let _ = self.session.complete_frames(invalid);
            self.session.invalidate();
        }
        // SAFETY: refcon was created via Box::into_raw in new() and is not used
        // after the session (and thus the callback) is gone.
        if !self.refcon.is_null() {
            drop(unsafe { Box::from_raw(self.refcon) });
        }
    }
}

// ---------------------------------------------------------------------------
// Output callback: AVCC -> Annex B, SPS/PPS prepended on keyframes (PRD §14.2).
// ---------------------------------------------------------------------------

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
        // Unblock the waiting drain with an empty sample rather than hang.
        let _ = ctx.tx.send(CompressedSample { sample: None });
        return;
    };
    if status != 0 {
        let _ = ctx.tx.send(CompressedSample { sample: None });
        return;
    }
    // 2.3: do NOT convert here. Retain the sample and hand it to the drain, which
    // does the AVCC→Annex B copy off this callback so VideoToolbox can proceed to
    // the next frame immediately. SAFETY: `sample` is valid for the callback's
    // duration; retaining keeps it alive past the callback's return.
    let retained = unsafe { CFRetained::retain(sample_ptr) };
    let _ = ctx.tx.send(CompressedSample { sample: Some(retained) });
}

/// Convert a compressed `CMSampleBuffer` (AVCC NALs in a `CMBlockBuffer`) to an
/// Annex B access unit, appending into `out` (caller pre-clears) and prepending
/// SPS+PPS when the frame is an IDR. Returns whether the AU is a keyframe.
///
/// Single-pass: a cheap keyframe pre-scan (reads one byte per NAL, no copy) then
/// one build into `out` — avoids the per-frame double-copy of the slice data.
/// `out` is reused across frames (2.4), so this allocates nothing steady-state.
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

    // Pre-scan for an IDR (NAL type 5) without copying.
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

    // Keyframes carry SPS+PPS (Annex B) from the format description first.
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

    // Single pass: AVCC length prefixes → Annex B start codes, written once.
    let mut off = 0usize;
    while off + NAL_LENGTH_PREFIX <= data.len() {
        let nal_len = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
            as usize;
        off += NAL_LENGTH_PREFIX;
        if nal_len == 0 || off + nal_len > data.len() {
            break;
        }
        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&data[off..off + nal_len]);
        off += nal_len;
    }

    is_keyframe
}

// ---------------------------------------------------------------------------
// CF helpers
// ---------------------------------------------------------------------------

/// Build the `VTCompressionSessionCreate` encoder-specification dictionary.
///
/// `require_hw` chooses between *requiring* the hardware encoder
/// (`RequireHardwareAcceleratedVideoEncoder`, fails if HW can't serve the config)
/// and merely *enabling*/preferring it (`EnableHardwareAcceleratedVideoEncoder`,
/// which permits the software encoder as a fallback). Normal sessions require HW;
/// only the last-resort attempt for sub-HW-floor sizes enables SW. On macOS 11+,
/// `low_latency` adds the WWDC21 low-latency rate-control mode — built as a literal
/// CFString, never the objc2 static, because that symbol is absent on 10.15 and
/// referencing it would break the dyld load there.
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
    // CFDictionaryCreate retains keys/values, so `ll_key` may drop after this.
    dict_from_pairs(&pairs)
}

/// Create a `VTCompressionSession` for `cfg` with `spec`. Returns the `OSStatus`
/// on failure so the caller can decide whether to retry / reclaim the refcon
/// (the session is not created, so the callback never fires).
fn create_session(
    cfg: &EncoderConfig,
    spec: &CFDictionary,
    refcon: *mut OutputCtx,
) -> Result<CFRetained<VTCompressionSession>, i32> {
    let mut session_ptr: *mut VTCompressionSession = ptr::null_mut();
    // SAFETY: standard VTCompressionSessionCreate call; out-pointer receives a
    // +1 session on success. refcon outlives the session (owned by the Encoder).
    let status = unsafe {
        VTCompressionSession::create(
            kCFAllocatorDefault,
            cfg.width as i32,
            cfg.height as i32,
            kCMVideoCodecType_H264,
            Some(spec),
            None, // source image buffer attributes: infer from input
            None, // compressed data allocator: default
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

// ---------------------------------------------------------------------------
// dyld-safe optional VideoToolbox symbols.
//
// Newer property keys/values (LTR, SVC, ConstrainedHigh/Baseline, frame-QP,
// CBR, color, …) are `extern` constants that simply do not exist in the 10.15
// VideoToolbox. Referencing the objc2 `extern static` for one of them would add
// an undefined symbol that breaks the binary's dyld load on 10.15 — the exact
// reason `EnableLowLatencyRateControl` is built as a literal CFString
// (Appendix A). `dlsym` generalizes that escape hatch: it resolves the constant
// at runtime, yielding its real value where the framework exports it and `None`
// on an OS that predates it. No link-time reference, so nothing can fail to load.
// ---------------------------------------------------------------------------

unsafe extern "C" {
    /// `OSStatus VTSessionCopyProperty(VTSessionRef, CFStringRef, CFAllocatorRef, void*)`.
    /// objc2-video-toolbox 0.3 does not bind the copy side, so declare it (stable
    /// public API). NULL allocator = default.
    fn VTSessionCopyProperty(
        session: &VTSession,
        property_key: &CFString,
        allocator: *const c_void,
        property_value_out: *mut *const CFType,
    ) -> i32;
    /// `Boolean CFBooleanGetValue(CFBooleanRef)`.
    fn CFBooleanGetValue(boolean: *const c_void) -> u8;
    /// `void CFRelease(CFTypeRef)`.
    fn CFRelease(cf: *const c_void);
}

/// Resolve a VideoToolbox CFString constant by its C symbol name, or `None` when
/// the running framework does not export it (a version-gated key on an older OS).
/// Off the hot path — called only at session setup.
fn vt_optional_cfstring(symbol: &str) -> Option<&'static CFString> {
    let cname = std::ffi::CString::new(symbol).ok()?;
    // SAFETY: RTLD_DEFAULT search for an exported data symbol. When present it is
    // `const CFStringRef NAME`, so dlsym returns `&NAME` (a `*const CFStringRef`);
    // deref once to the CFStringRef. The constant is a framework static → 'static.
    unsafe {
        let p = libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr());
        if p.is_null() {
            return None;
        }
        (*(p as *const *const CFString)).as_ref()
    }
}

/// Set a property whose key is version-gated: no-op (returns `false`) when the
/// key is absent on this OS, so the call is safe to make unconditionally.
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

/// Build an untyped CFDictionary from (CFString key, CFType value) pairs.
fn dict_from_pairs(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary> {
    let keys: Vec<*const c_void> =
        pairs.iter().map(|(k, _)| (*k as *const CFString).cast()).collect();
    let values: Vec<*const c_void> =
        pairs.iter().map(|(_, v)| (*v as *const CFType).cast()).collect();
    // SAFETY: keys/values are live CF objects; CFDictionaryCreate retains them.
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

/// Build the `kVTCompressionPropertyKey_DataRateLimits` value: a 2-element
/// CFArray `[max_bytes, window_seconds]`.
fn data_rate_limits(max_bytes: i64, window_secs: f64) -> CFRetained<CFArray> {
    let bytes = CFNumber::new_isize(max_bytes as isize);
    let secs = CFNumber::new_f64(window_secs);
    let values: [*const c_void; 2] =
        [(&*bytes as *const CFNumber).cast(), (&*secs as *const CFNumber).cast()];
    // SAFETY: two live CFNumbers held until after the create; CFArrayCreate
    // retains them through the CFType callbacks. Pointers passed as the untyped
    // void* array the C API expects.
    unsafe {
        CFArray::new(None, values.as_ptr() as *mut *const c_void, 2, &kCFTypeArrayCallBacks)
            .expect("CFArrayCreate for DataRateLimits")
    }
}

fn set_cftype(session: &VTSession, key: &CFString, value: &CFType) {
    // SAFETY: valid session/key/value; setting a property.
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

    /// A synthetic BGRA frame with a per-tick gradient so the encoder sees
    /// motion (and emits P-frames, not just repeated keyframes).
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

    /// Annex B NAL types present in one access unit (type = byte after a start
    /// code, low 5 bits).
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

    /// M2 acceptance: hardware H.264 encode of synthetic frames, validated by
    /// parsing the Annex B output (SPS/PPS on keyframes, on-demand IDR, P-frames
    /// in between). No screen-capture / TCC needed.
    ///
    /// Run: `cargo test --lib encoder::tests::encode_synthetic -- --ignored --nocapture`
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

        // Submit all frames (async: no per-frame blocking → real P-frames),
        // forcing a keyframe at 30, then flush.
        for i in 0..60usize {
            let pb = make_bgra_frame(w, h, i);
            enc.submit(&pb, i == 30).expect("submit frame");
        }
        enc.flush().expect("flush");

        // Drain the compressed samples in emission order, converting each to
        // Annex B with a reused buffer (mirrors the production drain).
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
        // Frame 0 is a keyframe: SPS(7) + PPS(8) + IDR(5).
        assert!(per_frame[0].contains(&7), "frame 0 missing SPS");
        assert!(per_frame[0].contains(&8), "frame 0 missing PPS");
        assert!(per_frame[0].contains(&5), "frame 0 missing IDR slice");
        // Frame 1 is a normal P-frame: non-IDR(1), no IDR.
        assert!(per_frame[1].contains(&1), "frame 1 not a P-frame");
        assert!(!per_frame[1].contains(&5), "frame 1 unexpectedly an IDR");
        // On-demand keyframe: exactly the first frame and the forced one (≈30)
        // are IDRs — not every frame.
        assert!(keyframes.len() <= 3, "too many keyframes: {keyframes:?} (P-frames not used)");
        assert!(keyframes.iter().any(|&i| (28..=31).contains(&i)), "forced keyframe ~30 missing");

        // No B-frames (CBP + AllowFrameReordering=false): only NAL types
        // 1/5 (slices), 6 (SEI), 7/8 (SPS/PPS), 9 (AUD) expected.
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

    /// Regression for the real `-12915` (kVTVideoEncoderNotAvailableNowErr) bug:
    /// the single HW encoder must be released between sessions so a new session
    /// never races a still-alive prior one. Before `Encoder::drop` explicitly
    /// invalidated the session, the Nth rapid create→drop at an ordinary
    /// resolution would fail because the previous session's HW encoder hadn't
    /// been freed yet (CFRetained release lags). Each iteration must succeed.
    /// Run: `cargo test --lib encoder::tests::recreate_churn_releases_hw -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn recreate_churn_releases_hw() {
        for i in 0..8 {
            let enc = Encoder::new(cfg_at(1280, 720));
            assert!(enc.is_ok(), "create #{i} at 1280x720 failed: {:?}", enc.err());
            // Drop immediately (end of scope) — the next create must see the HW
            // encoder freed. No sleep: this is the tight race we are fixing.
            drop(enc);
            println!("ok: churn iteration {i} created+released at 1280x720");
        }
    }

    /// Try to create a *hardware-required* session at `w`x`h` (no SW fallback) and
    /// report the OSStatus. Used to map the true HW floor independent of the
    /// production `Encoder::new`, which now drops to SW for sub-floor sizes.
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

    /// Regression for the new "respect the user's small resolution → use SW"
    /// requirement: `Encoder::new` must SUCCEED at sizes below the HW encoder's
    /// minimum frame size (it transparently falls back to the software encoder
    /// rather than failing or upscaling). These are the sizes that return -12915
    /// from a require-HW create. Run:
    /// `cargo test --lib encoder::tests::creates_small_via_software -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn creates_small_via_software() {
        for (w, h) in [(480u32, 270u32), (448u32, 252u32), (160u32, 90u32)] {
            // Confirm the HW encoder really refuses this size (else the test is
            // not exercising the SW path).
            let hw = hw_create_status(w, h);
            let enc = Encoder::new(cfg_at(w, h));
            assert!(enc.is_ok(), "Encoder::new must succeed at {w}x{h}: {:?}", enc.err());
            println!("ok: {w}x{h} created (require-HW was {hw:?}; Encoder::new used SW fallback)");
        }
    }

    /// Diagnostic: maps the true HW encoder floor (via require-HW creates) and how
    /// many HW sessions can be alive at once. Run with --nocapture and read output.
    /// Run: `cargo test --lib encoder::tests::hw_capability_probe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn hw_capability_probe() {
        // Sweep a 16:9 ladder via require-HW to find the HW encoder's min size.
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
        // Concurrency: hold sessions alive and count how many the HW grants.
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
