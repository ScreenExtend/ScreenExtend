use std::collections::HashSet;
use std::ffi::{c_char, c_int, c_void};
use std::fmt::Write as _;
use std::ptr;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use anyhow::{bail, Result};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use objc2_core_foundation::{
    kCFAllocatorDefault, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFArray,
    CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, CMSampleBuffer, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionGetH264ParameterSetAtIndex,
};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_EncoderID, kVTCompressionPropertyKey_ExpectedFrameRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration,
    kVTCompressionPropertyKey_NumberOfPendingFrames, kVTCompressionPropertyKey_ProfileLevel,
    kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_UsingGPURegistryID,
    kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTEncodeFrameOptionKey_ForceKeyFrame, kVTProfileLevel_H264_Baseline_AutoLevel,
    kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_H264_Main_AutoLevel,
    kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, VTCompressionSession,
    VTEncodeInfoFlags, VTSession, VTSessionSetProperty,
};

use crate::streamer::config::H264Profile;
use crate::streamer::platform::BitrateApplied;
use crate::streamer::telemetry::FrameTiming;

use super::config::EncoderConfig;

const START_CODE: [u8; 4] = [0, 0, 0, 1];
const NAL_LENGTH_PREFIX: usize = 4;

/// `'avc1'`. Spelled as the literal FourCC rather than borrowed from the CoreMedia binding so the
/// hand-declared `VTCopy*` prototypes below do not depend on how that crate spells its newtype.
const CODEC_TYPE_H264: u32 = 0x6176_6331;

/// How many encoded access units may wait for the drain thread.
///
/// The channel used to be unbounded, which meant a drain thread that fell behind turned into
/// latency instead of into a visible drop: every queued frame is one the client will be shown
/// late. Four is one frame being written, one being read and two of slack.
const OUTPUT_CAPACITY: usize = 4;

/// Frames the encoder may hold at once before the loop stops feeding it.
///
/// VideoToolbox has no queue limit of its own, and a comparable product measured ~8 frames
/// resident when it was overfed — 130 ms at 60 fps, all of it invisible to every timer we have.
/// The alternative Apple offers, `MaxFrameDelayCount`, is documented as a *blocking* contract on
/// the calling thread, and that thread is the deadline-scheduled encode loop; counting here bounds
/// the same thing without ever blocking it.
pub const MAX_FRAMES_IN_FLIGHT: usize = 2;

/// The per-frame stamps that ride through VideoToolbox as `sourceFrameRefCon` and come back on
/// the output callback.
///
/// This used to be a `VecDeque` behind a `std::sync::Mutex` shared between the real-time encode
/// thread and the drain thread, which is a priority inversion waiting to happen and also assumes
/// submit order equals emit order. VideoToolbox carries an opaque pointer per frame for exactly
/// this purpose, so the stamps travel with their own frame and neither the lock nor the ordering
/// assumption is needed.
pub struct FrameTag {
    pub capture: Instant,
    pub timing: FrameTiming,
}

pub struct CompressedSample {
    sample: Option<CFRetained<CMSampleBuffer>>,
    /// `None` only if VideoToolbox emitted a frame we never tagged, which it should not.
    pub tag: Option<FrameTag>,
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
    /// The encoder's own end of the channel, used only to make room when it is full.
    rx: Receiver<CompressedSample>,
    in_flight: Arc<AtomicUsize>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl OutputCtx {
    /// Hand an encoded access unit to the drain thread, making room if the channel is full.
    ///
    /// The oldest queued frame goes, not the newest: the newest is the one that still has a
    /// chance of being shown on time. Either way a dropped access unit breaks the reference
    /// chain, so the count is published for the encode loop to turn into an IDR.
    fn deliver(&self, sample: CompressedSample) {
        match self.tx.try_send(sample) {
            Ok(()) => {}
            Err(TrySendError::Full(sample)) => {
                let _ = self.rx.try_recv();
                self.dropped.fetch_add(1, Ordering::Relaxed);
                let _ = self.tx.try_send(sample);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

pub struct Encoder {
    session: CFRetained<VTCompressionSession>,
    rx: Receiver<CompressedSample>,
    refcon: *mut OutputCtx,
    in_flight: Arc<AtomicUsize>,
    emitted: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    fps: i32,
    frame_index: i64,
    low_latency: bool,
    qp_mode: bool,
    identity: EncoderIdentity,
    supports_ltr: bool,
    /// Whether *this* session is on software because the user asked, which is the difference
    /// between an expected outcome and the loud warning below it.
    software_requested: bool,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(cfg: EncoderConfig) -> Result<Self> {
        encoder_catalog();

        let (tx, rx) = bounded::<CompressedSample>(OUTPUT_CAPACITY);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let emitted = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let refcon = Box::into_raw(Box::new(OutputCtx {
            tx,
            rx: rx.clone(),
            in_flight: Arc::clone(&in_flight),
            emitted: Arc::clone(&emitted),
            dropped: Arc::clone(&dropped),
        }));

        // The rungs, best first. There is no version gate on the low-latency rung any more: the
        // ladder already falls through when `EnableLowLatencyRateControl` is not honoured, and a
        // `sw_vers` fork was a slower, less accurate way of learning the same thing.
        //
        // When the user has asked for software encode there is no ladder: one rung that forbids
        // the hardware encoder outright. Falling back to hardware here would be the setting
        // silently refusing to do the one thing it says on the tin, so a failure to create that
        // session is reported as a failure rather than worked around.
        let ladder: &[(bool, HwPolicy)] = if cfg.software_encode {
            &[(false, HwPolicy::Forbid)]
        } else {
            &[
                (true, HwPolicy::Require),
                (false, HwPolicy::Require),
                (false, HwPolicy::Enable),
            ]
        };
        if cfg.software_encode {
            tprintln!(
                "[vt] software encode was requested, so this session forbids the hardware \
                 encoder (EnableHardwareAcceleratedVideoEncoder=false)"
            );
        }

        let mut session = None;
        let mut low_latency = false;
        let mut last_status = 0;
        for &(ll, hw) in ladder {
            let Some(spec) = build_encoder_spec(ll, hw, cfg.gpu_registry_id) else {
                continue;
            };
            match create_session(&cfg, &spec, refcon) {
                Ok(s) => {
                    session = Some(s);
                    low_latency = ll;
                    break;
                }
                Err(status) => {
                    last_status = status;
                    if hw == HwPolicy::Require {
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

        let mut enc = Encoder {
            session,
            rx,
            refcon,
            in_flight,
            emitted,
            dropped,
            fps: cfg.fps.max(1) as i32,
            frame_index: 0,
            low_latency,
            qp_mode: cfg.qp.is_some(),
            identity: EncoderIdentity::default(),
            supports_ltr: false,
            software_requested: cfg.software_encode,
        };
        enc.configure(&cfg)?;
        unsafe { enc.session.prepare_to_encode_frames() };
        tprintln!(
            "[vt] H.264 encoder ready: {}x{} @ {}fps, {} kbps, profile={:?}, low_latency={}",
            cfg.width,
            cfg.height,
            cfg.fps,
            cfg.bitrate_bps / 1000,
            cfg.profile,
            low_latency,
        );
        Ok(enc)
    }

    fn vt_session(&self) -> &VTSession {
        unsafe { &*((&*self.session) as *const VTCompressionSession as *const VTSession) }
    }

    fn configure(&mut self, cfg: &EncoderConfig) -> Result<()> {
        let advertised = supported_properties(cfg);
        let mut report = PropReport::default();
        {
            let s = self.vt_session();
            report.note(
                "RealTime",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_RealTime },
                    cfbool(true),
                ),
                &advertised,
            );
            report.note(
                "AllowFrameReordering",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
                    cfbool(false),
                ),
                &advertised,
            );

            let profile: &CFString = if self.low_latency {
                match cfg.profile {
                    H264Profile::High => {
                        vt_optional_cfstring("kVTProfileLevel_H264_ConstrainedHigh_AutoLevel")
                            .unwrap_or(unsafe { kVTProfileLevel_H264_High_AutoLevel })
                    }
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
            report.note(
                "ProfileLevel",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_ProfileLevel },
                    profile,
                ),
                &advertised,
            );

            for key in ["ColorPrimaries", "TransferFunction", "YCbCrMatrix"] {
                let value = CFString::from_str("ITU_R_709_2");
                report.note(key, set_optional(s, key, &value), &advertised);
            }

            let n = CFNumber::new_i32(cfg.fps as i32);
            report.note(
                "ExpectedFrameRate",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_ExpectedFrameRate },
                    &n,
                ),
                &advertised,
            );

            // Keyframes are for joining clients and for loss recovery, both of which arrive as an
            // explicit request. A periodic IDR on top of that is a bitrate spike every interval
            // that buys nothing: 10 s was frequent enough to be visible on a constrained link.
            let n = CFNumber::new_i32((cfg.fps as i32).saturating_mul(600).max(1));
            report.note(
                "MaxKeyFrameInterval",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
                    &n,
                ),
                &advertised,
            );
            let n = CFNumber::new_f64(600.0);
            report.note(
                "MaxKeyFrameIntervalDuration",
                set_named(
                    s,
                    unsafe { kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration },
                    &n,
                ),
                &advertised,
            );

            // No `MaxFrameDelayCount`. Apple defines it as a hard contract on the *calling*
            // thread — "before the call to encode frame N returns, frame N-M must have been
            // emitted" — so the value that looks like the lowest latency (1) is the one that
            // blocks the deadline-scheduled encode loop on the encoder. `MAX_FRAMES_IN_FLIGHT`
            // bounds the same queue from outside without ever blocking.
            report.note(
                "MaximizePowerEfficiency",
                set_optional_bool(s, "MaximizePowerEfficiency", false),
                &advertised,
            );
            report.note(
                "PrioritizeEncodingSpeedOverQuality",
                set_optional_bool(s, "PrioritizeEncodingSpeedOverQuality", true),
                &advertised,
            );
            report.note("BaseLayerFrameRate", PropOutcome::NotRequested, &advertised);
            report.note("ConstantBitRate", PropOutcome::NotRequested, &advertised);

            // Long-term reference frames would let a lost frame be repaired against an
            // acknowledged older one instead of a full IDR. The encoder half is one property;
            // the other half is a client that acknowledges tokens over the control channel,
            // which belongs to the transport. Probe and report so the transport knows whether
            // the machine can do it at all.
            self.supports_ltr = advertised.contains("EnableLTR");
            report.note("EnableLTR", PropOutcome::NotRequested, &advertised);

            if cfg.intra_refresh {
                teprintln!(
                    "[vt] intra-refresh requested but unsupported on macOS VideoToolbox \
                     (no rolling intra-refresh); using PLI→IDR loss recovery instead"
                );
            }
        }

        self.configure_rate_control(cfg, &mut report, &advertised);
        report.emit();
        self.read_identity();
        Ok(())
    }

    fn configure_rate_control(
        &self,
        cfg: &EncoderConfig,
        report: &mut PropReport,
        advertised: &HashSet<String>,
    ) {
        let s = self.vt_session();
        if let Some(qp) = cfg.qp {
            let quality = ((51.0 - qp as f64) / 50.0).clamp(0.0, 1.0);
            let n = CFNumber::new_f64(quality);
            report.note("Quality", set_optional(s, "Quality", &n), advertised);
            let n = CFNumber::new_i32(qp as i32);
            report.note(
                "MaxAllowedFrameQP",
                set_optional(s, "MaxAllowedFrameQP", &n),
                advertised,
            );
            report.note(
                "MinAllowedFrameQP",
                set_optional(s, "MinAllowedFrameQP", &n),
                advertised,
            );
            return;
        }
        // Quality ceiling only: a floor on QP can only shrink the stream, so the loss-based
        // controller keeps the top end. Pinning MaxAllowedFrameQP too would be constant QP.
        if let Some(q) = cfg.min_qp.filter(|&q| q > 1) {
            let n = CFNumber::new_i32(q as i32);
            report.note(
                "MinAllowedFrameQP",
                set_optional(s, "MinAllowedFrameQP", &n),
                advertised,
            );
        }
        let n = CFNumber::new_i32(cfg.bitrate_bps as i32);
        report.note(
            "AverageBitRate",
            set_named(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &n),
            advertised,
        );
        // No `DataRateLimits`. It is a hard cap enforced by stuffing or by dropping quality
        // inside a short window, layered on top of a target the loss-based controller is already
        // steering from the network's own signal. Two controllers on one knob, and the fast one
        // could not see what the slow one had done.
        let n = CFNumber::new_i32(45);
        report.note(
            "MaxAllowedFrameQP",
            set_optional(s, "MaxAllowedFrameQP", &n),
            advertised,
        );
    }

    pub fn submit(
        &mut self,
        image_buffer: &CVImageBuffer,
        force_idr: bool,
        tag: FrameTag,
    ) -> Result<()> {
        let pts = CMTime {
            value: self.frame_index,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        let dur = CMTime {
            value: 1,
            timescale: self.fps,
            flags: CMTimeFlags::Valid,
            epoch: 0,
        };
        self.frame_index += 1;

        let frame_props = if force_idr {
            dict_from_pairs(&[(
                unsafe { kVTEncodeFrameOptionKey_ForceKeyFrame },
                cfbool(true),
            )])
        } else {
            None
        };

        let source_refcon = Box::into_raw(Box::new(tag)).cast::<c_void>();
        self.in_flight.fetch_add(1, Ordering::AcqRel);

        let mut info = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                image_buffer,
                pts,
                dur,
                frame_props.as_deref(),
                source_refcon,
                &mut info,
            )
        };
        if status != 0 {
            // A rejected frame never reaches the output callback, so its tag and its slot come
            // back here or they leak.
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            drop(unsafe { Box::from_raw(source_refcon.cast::<FrameTag>()) });
            bail!("VTCompressionSessionEncodeFrame failed: OSStatus {status}");
        }
        Ok(())
    }

    pub fn output(&self) -> Receiver<CompressedSample> {
        self.rx.clone()
    }

    /// Frames VideoToolbox has been handed and not yet emitted.
    pub fn frames_in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    /// Frames VideoToolbox has emitted, whether or not they carried data. The encode loop watches
    /// it to tell an encoder that is keeping up from one that is holding frames.
    pub fn frames_emitted(&self) -> u64 {
        self.emitted.load(Ordering::Relaxed)
    }

    /// Encoded access units discarded because the drain thread fell behind. Any non-zero value
    /// means the reference chain is broken and the stream needs an IDR.
    pub fn output_drops(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn flush(&mut self) -> Result<()> {
        self.complete_pending()
    }

    /// Block until VideoToolbox has emitted everything it holds.
    ///
    /// Only for teardown and for the probe harness. Never call it from the encode loop: it is
    /// synchronous, unbounded, and that loop runs on the deadline scheduler.
    pub fn complete_pending(&mut self) -> Result<()> {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
        let st = unsafe { self.session.complete_frames(invalid) };
        if st != 0 {
            bail!("VTCompressionSessionCompleteFrames failed: OSStatus {st}");
        }
        Ok(())
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.qp_mode {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        let s = self.vt_session();
        let n = CFNumber::new_i32(bps as i32);
        set_cftype(s, unsafe { kVTCompressionPropertyKey_AverageBitRate }, &n);
        Ok(BitrateApplied::Applied)
    }

    pub fn low_latency(&self) -> bool {
        self.low_latency
    }

    /// Whether this session's encoder advertises long-term reference frames. The transport needs
    /// it to decide between a PLI→IDR and a token-acknowledged refresh.
    pub fn supports_ltr(&self) -> bool {
        self.supports_ltr
    }

    /// Ask the session what it actually became.
    ///
    /// VideoToolbox picks the encoder implementation for us and the choice is worth more than
    /// any property: forum 748918 measured one-in/one-out on `…h264.gva` against a 4-to-16-frame
    /// hold on `…h264.ave.avc` under an identical configuration. `EncoderID` (10.13) names it,
    /// `UsingHardwareAcceleratedVideoEncoder` (10.9) says whether it is hardware at all, and
    /// `UsingGPURegistryID` (10.15) says which GPU it landed on — NULL there means *either* an
    /// integrated-GPU encoder or software, so it can never stand in for the hardware flag.
    fn read_identity(&mut self) {
        let s = self.vt_session();
        self.identity = EncoderIdentity {
            encoder_id: copy_string(s, unsafe { kVTCompressionPropertyKey_EncoderID }),
            hardware: copy_bool(s, unsafe {
                kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder
            }),
            gpu_registry_id: copy_i64(s, unsafe { kVTCompressionPropertyKey_UsingGPURegistryID }),
        };
        tprintln!(
            "[vt] encoder identity: id={} hardware={} gpu_registry_id={}",
            self.identity.encoder_id.as_deref().unwrap_or("unknown"),
            match self.identity.hardware {
                Some(v) => v.to_string(),
                None => "unknown".to_string(),
            },
            match self.identity.gpu_registry_id {
                Some(v) => v.to_string(),
                None => "none (integrated or software)".to_string(),
            }
        );
        if self.identity.hardware == Some(false) {
            if self.software_requested {
                tprintln!(
                    "[vt] this session is on the software H.264 encoder because software encode \
                     is switched on. Expect higher latency and CPU; that is the setting working."
                );
            } else {
                teprintln!(
                    "[vt] this session is on the SOFTWARE H.264 encoder. Expect higher latency \
                     and CPU. The usual causes are an encode size below this Mac's hardware \
                     minimum (~640x480 on Intel) and another application already holding the \
                     hardware encoder; both are machine-wide, not specific to this device."
                );
            }
        }
    }

    pub fn identity(&self) -> &EncoderIdentity {
        &self.identity
    }

    /// The tier string reported to `CompatibilityReport` and the telemetry summary.
    pub fn backend_name(&self) -> &'static str {
        match self.identity.hardware {
            Some(true) => "videotoolbox-hardware",
            Some(false) => "videotoolbox-software",
            None => "videotoolbox",
        }
    }

    /// Live count of frames VideoToolbox is holding, by its own reckoning. Cross-check against
    /// [`Encoder::frames_in_flight`]: if the two disagree the encoder is holding frames for a
    /// reason of its own and no external cap will help.
    pub fn pending_frames(&self) -> Option<i64> {
        copy_i64(self.vt_session(), unsafe {
            kVTCompressionPropertyKey_NumberOfPendingFrames
        })
    }
}
/// What VideoToolbox turned our request into.
#[derive(Clone, Debug, Default)]
pub struct EncoderIdentity {
    pub encoder_id: Option<String>,
    pub hardware: Option<bool>,
    pub gpu_registry_id: Option<i64>,
}

// ---------------------------------------------------------------------------
// Asking VideoToolbox what it can do, instead of asking `sw_vers` what year it is.
// ---------------------------------------------------------------------------

/// What `VTCopyVideoEncoderList` says this Mac has, enumerated and logged once.
///
/// The hardware encoder is a machine-wide resource with a per-encoder instance limit, so "why did
/// my session land on software" is usually answered by this list plus the identity read back after
/// creation — and neither was ever printed.
#[derive(Default)]
struct EncoderCatalog {
    has_hardware: bool,
    instance_limited: bool,
}

fn encoder_catalog() -> &'static EncoderCatalog {
    static CATALOG: OnceLock<EncoderCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let mut catalog = EncoderCatalog::default();
        let Some(list) = video_encoder_list() else {
            tprintln!("[vt] VTCopyVideoEncoderList unavailable; encoder inventory unknown");
            return catalog;
        };
        let key_id = vt_optional_cfstring("kVTVideoEncoderList_EncoderID");
        let key_codec = vt_optional_cfstring("kVTVideoEncoderList_CodecType");
        let key_hw = vt_optional_cfstring("kVTVideoEncoderList_IsHardwareAccelerated");
        let key_gpu = vt_optional_cfstring("kVTVideoEncoderList_GPURegistryID");
        let key_limit = vt_optional_cfstring("kVTVideoEncoderList_InstanceLimit");

        let mut found = 0usize;
        for i in 0..list.count() {
            let entry = unsafe { list.value_at_index(i) } as *const CFDictionary;
            let Some(entry) = (unsafe { entry.as_ref() }) else {
                continue;
            };
            if let Some(k) = key_codec {
                if dict_i64(entry, k) != Some(CODEC_TYPE_H264 as i64) {
                    continue;
                }
            }
            found += 1;
            let hardware = key_hw.and_then(|k| dict_bool(entry, k));
            // `InstanceLimit` is a CFBoolean, not a count: present and true means "this encoder is
            // a scarce resource which may potentially be unavailable". That is the documented
            // mechanism behind "it only works when I quit everything else".
            let capped = key_limit.and_then(|k| dict_bool(entry, k)).unwrap_or(false);
            catalog.has_hardware |= hardware.unwrap_or(false);
            catalog.instance_limited |= capped;
            tprintln!(
                "[vt] encoder: id={} hardware={} gpu_registry_id={} instance_limited={capped}",
                key_id
                    .and_then(|k| dict_string(entry, k))
                    .unwrap_or_else(|| "unknown".into()),
                hardware
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                key_gpu
                    .and_then(|k| dict_i64(entry, k))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "none".into()),
            );
        }
        if found == 0 {
            tprintln!("[vt] no H.264 encoder listed by VTCopyVideoEncoderList");
        }
        catalog
    })
}

/// The encoder tier this Mac *has*, answered without creating a session.
///
/// `CompatibilityReport` is built before any device joins, so this is the honest answer there. The
/// tier a given session actually got is separate, read back per session and reported through the
/// telemetry `encoder` slot, because a busy hardware encoder makes the two differ.
pub fn probe_encoder_backend() -> &'static str {
    if encoder_catalog().has_hardware {
        "videotoolbox-hardware"
    } else {
        "videotoolbox-software"
    }
}

fn video_encoder_list() -> Option<CFRetained<CFArray>> {
    type CopyEncoderListFn = unsafe extern "C" fn(*const CFDictionary, *mut *const CFArray) -> i32;
    let sym = dlsym_fn("VTCopyVideoEncoderList")?;
    let f: CopyEncoderListFn = unsafe { std::mem::transmute(sym) };
    let mut out: *const CFArray = ptr::null();
    let st = unsafe { f(ptr::null(), &mut out) };
    if st != 0 {
        return None;
    }
    NonNull::new(out as *mut CFArray).map(|p| unsafe { CFRetained::from_raw(p) })
}

/// The property keys this encoder advertises for this exact frame size.
///
/// `VTCopySupportedPropertyDictionaryForEncoder` has existed since 10.13 and answers directly what
/// the code used to infer from the OS version — and inference was wrong in both directions, since
/// the property set depends on the encoder VideoToolbox picks, which depends on the size and the
/// machine, not on the year.
fn supported_properties(cfg: &EncoderConfig) -> HashSet<String> {
    type CopySupportedFn = unsafe extern "C" fn(
        i32,
        i32,
        u32,
        *const CFDictionary,
        *mut *const CFString,
        *mut *const CFDictionary,
    ) -> i32;
    let mut set = HashSet::new();
    let Some(sym) = dlsym_fn("VTCopySupportedPropertyDictionaryForEncoder") else {
        return set;
    };
    let f: CopySupportedFn = unsafe { std::mem::transmute(sym) };
    let spec = build_encoder_spec(false, HwPolicy::Enable, cfg.gpu_registry_id);
    let mut id_out: *const CFString = ptr::null();
    let mut props_out: *const CFDictionary = ptr::null();
    let st = unsafe {
        f(
            cfg.width as i32,
            cfg.height as i32,
            CODEC_TYPE_H264,
            spec.as_deref()
                .map_or(ptr::null(), |d| d as *const CFDictionary),
            &mut id_out,
            &mut props_out,
        )
    };
    let id = NonNull::new(id_out as *mut CFString).map(|p| unsafe { CFRetained::from_raw(p) });
    let props =
        NonNull::new(props_out as *mut CFDictionary).map(|p| unsafe { CFRetained::from_raw(p) });
    if st != 0 {
        tprintln!("[vt] VTCopySupportedPropertyDictionaryForEncoder failed: OSStatus {st}");
        return set;
    }
    if let Some(props) = props.as_deref() {
        set.extend(dictionary_keys(props));
    }
    tprintln!(
        "[vt] {}x{} would use encoder {} advertising {} properties",
        cfg.width,
        cfg.height,
        id.as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown".into()),
        set.len()
    );
    set
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PropOutcome {
    Set,
    Rejected(i32),
    MissingSymbol,
    /// Deliberately not set. The report still names it, so the log answers "was this tried?"
    /// rather than leaving the reader to grep the source.
    NotRequested,
}

/// One line per session saying what each latency-relevant property actually did.
///
/// Every optional property used to be set through a helper whose `bool` return was discarded, so
/// a key whose symbol did not resolve on this OS was indistinguishable from one that applied.
#[derive(Default)]
struct PropReport {
    entries: Vec<(&'static str, PropOutcome, bool)>,
}

impl PropReport {
    fn note(&mut self, name: &'static str, outcome: PropOutcome, advertised: &HashSet<String>) {
        self.entries
            .push((name, outcome, advertised.contains(name)));
    }

    fn emit(&self) {
        let mut line = String::from("[vt] properties:");
        for (name, outcome, advertised) in &self.entries {
            let state = match outcome {
                PropOutcome::Set => "ok".to_string(),
                PropOutcome::Rejected(st) => format!("rejected({st})"),
                PropOutcome::MissingSymbol => "no-symbol".to_string(),
                PropOutcome::NotRequested if *advertised => "available".to_string(),
                PropOutcome::NotRequested => "absent".to_string(),
            };
            let _ = write!(line, " {name}={state}");
        }
        tprintln!("{line}");
    }
}

fn copy_property(session: &VTSession, key: &CFString) -> Option<CFRetained<CFType>> {
    let mut value: *const CFType = ptr::null();
    let st = unsafe { VTSessionCopyProperty(session, key, ptr::null(), &mut value) };
    if st != 0 {
        return None;
    }
    NonNull::new(value as *mut CFType).map(|p| unsafe { CFRetained::from_raw(p) })
}

fn copy_bool(session: &VTSession, key: &CFString) -> Option<bool> {
    let v = copy_property(session, key)?;
    v.downcast_ref::<CFBoolean>().map(|b| b.as_bool())
}

fn copy_string(session: &VTSession, key: &CFString) -> Option<String> {
    let v = copy_property(session, key)?;
    v.downcast_ref::<CFString>().map(|s| s.to_string())
}

fn copy_i64(session: &VTSession, key: &CFString) -> Option<i64> {
    let v = copy_property(session, key)?;
    v.downcast_ref::<CFNumber>().and_then(|n| n.as_i64())
}

fn dict_value<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFType> {
    let v = unsafe { dict.value((key as *const CFString).cast()) };
    unsafe { (v as *const CFType).as_ref() }
}

/// A dictionary's keys, for the ones that are CFStrings.
///
/// Turns VideoToolbox's supported-property dictionary into something a membership test can run
/// against once per session rather than a CF call per property.
fn dictionary_keys(dict: &CFDictionary) -> Vec<String> {
    let count = dict.count();
    if count <= 0 {
        return Vec::new();
    }
    let mut keys: Vec<*const c_void> = vec![ptr::null(); count as usize];
    unsafe { dict.keys_and_values(keys.as_mut_ptr(), ptr::null_mut()) };
    keys.into_iter()
        .filter_map(|k| unsafe { (k as *const CFType).as_ref() })
        .filter_map(|t| t.downcast_ref::<CFString>().map(|s| s.to_string()))
        .collect()
}

fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    dict_value(dict, key)?
        .downcast_ref::<CFString>()
        .map(|s| s.to_string())
}

fn dict_bool(dict: &CFDictionary, key: &CFString) -> Option<bool> {
    dict_value(dict, key)?
        .downcast_ref::<CFBoolean>()
        .map(|b| b.as_bool())
}

fn dict_i64(dict: &CFDictionary, key: &CFString) -> Option<i64> {
    dict_value(dict, key)?
        .downcast_ref::<CFNumber>()
        .and_then(|n| n.as_i64())
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let invalid = CMTime {
            value: 0,
            timescale: 0,
            flags: CMTimeFlags(0),
            epoch: 0,
        };
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
    src_frame_refcon: *mut c_void,
    status: i32,
    _info: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // Reclaim the per-frame stamps first: they were boxed in `submit` and this is the only place
    // that frees them, whatever else the callback decides to do.
    let tag = NonNull::new(src_frame_refcon.cast::<FrameTag>())
        .map(|p| *unsafe { Box::from_raw(p.as_ptr()) });
    if refcon.is_null() {
        return;
    }
    let ctx = unsafe { &*(refcon as *const OutputCtx) };
    ctx.emitted.fetch_add(1, Ordering::Relaxed);
    let _ = ctx
        .in_flight
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            Some(n.saturating_sub(1))
        });
    let sample = if status == 0 {
        NonNull::new(sample).map(|p| unsafe { CFRetained::retain(p) })
    } else {
        None
    };
    ctx.deliver(CompressedSample { sample, tag });
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
        let nal_len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
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
        let nal_len =
            u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
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

/// What the session specification should say about the hardware encoder.
///
/// Three states, not two, because "prefer hardware" and "refuse hardware" are different requests
/// and VideoToolbox spells them with different keys. The middle rung exists so a machine whose
/// hardware encoder is busy still gets a session; the last one exists because the user asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HwPolicy {
    /// `RequireHardwareAcceleratedVideoEncoder` - fail the create rather than land quietly on
    /// software. This is what makes the fallback a logged decision instead of an invisible one.
    Require,
    /// `EnableHardwareAcceleratedVideoEncoder = true` - take hardware if any is free.
    Enable,
    /// `EnableHardwareAcceleratedVideoEncoder = false` - the user asked for software encode.
    Forbid,
}

fn build_encoder_spec(
    low_latency: bool,
    hw: HwPolicy,
    gpu_registry_id: Option<u64>,
) -> Option<CFRetained<CFDictionary>> {
    let hw_key = if hw == HwPolicy::Require {
        unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder }
    } else {
        unsafe { kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder }
    };
    let ll_key = CFString::from_str("EnableLowLatencyRateControl");
    // `Preferred`, never `Required`: the header says Required silently overrides Preferred and
    // would defeat the fallback ladder on a machine whose chosen GPU has no encoder.
    let gpu_key =
        vt_optional_cfstring("kVTVideoEncoderSpecification_PreferredEncoderGPURegistryID");
    let gpu_num = gpu_registry_id.map(|id| CFNumber::new_i64(id as i64));

    let mut pairs: Vec<(&CFString, &CFType)> = Vec::with_capacity(3);
    pairs.push((hw_key, cfbool(hw != HwPolicy::Forbid)));
    if low_latency {
        pairs.push((&ll_key, cfbool(true)));
    }
    if let (Some(k), Some(n)) = (gpu_key, gpu_num.as_deref()) {
        let v: &CFType = n;
        pairs.push((k, v));
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
}

fn dlsym_fn(symbol: &str) -> Option<*mut c_void> {
    let cname = std::ffi::CString::new(symbol).ok()?;
    let p = unsafe { libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr()) };
    (!p.is_null()).then_some(p)
}

fn vt_optional_cfstring(symbol: &str) -> Option<&'static CFString> {
    let p = dlsym_fn(symbol)?;
    unsafe { (*(p as *const *const CFString)).as_ref() }
}

/// Set a property, naming the key when it is refused.
///
/// `OSStatus -12900` on an anonymous key is un-diagnosable; the same status next to
/// `PrioritizeEncodingSpeedOverQuality` says exactly which encoder does not honour what.
fn set_cftype(session: &VTSession, key: &CFString, value: &CFType) -> i32 {
    let st = unsafe { VTSessionSetProperty(session, key, Some(value)) };
    if st != 0 {
        teprintln!("[vt] rejected {key}: OSStatus {st}");
    }
    st
}

fn set_named(session: &VTSession, key: &CFString, value: &CFType) -> PropOutcome {
    match set_cftype(session, key, value) {
        0 => PropOutcome::Set,
        st => PropOutcome::Rejected(st),
    }
}

/// Set a compression property named by its short key, resolving the constant at runtime.
///
/// The `kVTCompressionPropertyKey_` prefix is added here so the report and the call sites read as
/// the property names Apple documents.
fn set_optional(session: &VTSession, short_key: &str, value: &CFType) -> PropOutcome {
    match vt_optional_cfstring(&format!("kVTCompressionPropertyKey_{short_key}")) {
        Some(k) => set_named(session, k, value),
        None => PropOutcome::MissingSymbol,
    }
}

fn set_optional_bool(session: &VTSession, short_key: &str, value: bool) -> PropOutcome {
    set_optional(session, short_key, cfbool(value))
}

fn dict_from_pairs(pairs: &[(&CFString, &CFType)]) -> Option<CFRetained<CFDictionary>> {
    let keys: Vec<*const c_void> = pairs
        .iter()
        .map(|(k, _)| (*k as *const CFString).cast())
        .collect();
    let values: Vec<*const c_void> = pairs
        .iter()
        .map(|(_, v)| (*v as *const CFType).cast())
        .collect();
    unsafe {
        CFDictionary::new(
            None,
            keys.as_ptr() as *mut *const c_void,
            values.as_ptr() as *mut *const c_void,
            pairs.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
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

    fn tag() -> FrameTag {
        FrameTag {
            capture: Instant::now(),
            timing: FrameTiming::entering_capture(),
        }
    }

    fn make_bgra_frame(w: usize, h: usize, tick: usize) -> CFRetained<CVPixelBuffer> {
        let mut out: *mut CVPixelBuffer = ptr::null_mut();
        let r = unsafe {
            CVPixelBufferCreate(
                kCFAllocatorDefault,
                w,
                h,
                BGRA,
                None,
                NonNull::from(&mut out),
            )
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

    /// Pull the first NAL of the given type out of an Annex B access unit, payload only.
    fn first_nal(au: &[u8], want: u8) -> Option<Vec<u8>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 4 <= au.len() {
            if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
                starts.push(i + 4);
                i += 4;
            } else {
                i += 1;
            }
        }
        for (n, &s) in starts.iter().enumerate() {
            if s >= au.len() || (au[s] & 0x1F) != want {
                continue;
            }
            let end = starts.get(n + 1).map(|&e| e - 4).unwrap_or(au.len());
            return Some(au[s + 1..end].to_vec());
        }
        None
    }

    // -----------------------------------------------------------------------
    // A minimal SPS reader.
    //
    // `AllowFrameReordering = false` is a request; `max_num_reorder_frames` in the SPS VUI is
    // what a decoder actually obeys, and a non-zero value there costs the client that many frames
    // of output delay no matter how fast the encoder is. Only the bitstream can answer it.
    // -----------------------------------------------------------------------

    struct BitReader {
        data: Vec<u8>,
        bit: usize,
    }

    impl BitReader {
        /// Strip H.264 emulation-prevention bytes before reading.
        fn new(rbsp_with_epb: &[u8]) -> BitReader {
            let mut data = Vec::with_capacity(rbsp_with_epb.len());
            let mut zeros = 0;
            for &b in rbsp_with_epb {
                if zeros == 2 && b == 0x03 {
                    zeros = 0;
                    continue;
                }
                if b == 0 {
                    zeros += 1;
                } else {
                    zeros = 0;
                }
                data.push(b);
            }
            BitReader { data, bit: 0 }
        }

        fn u1(&mut self) -> u32 {
            let byte = self.bit / 8;
            let shift = 7 - (self.bit % 8);
            self.bit += 1;
            match self.data.get(byte) {
                Some(b) => ((b >> shift) & 1) as u32,
                None => 0,
            }
        }

        fn u(&mut self, n: u32) -> u64 {
            let mut v = 0u64;
            for _ in 0..n {
                v = (v << 1) | self.u1() as u64;
            }
            v
        }

        fn flag(&mut self) -> bool {
            self.u1() == 1
        }

        fn ue(&mut self) -> u64 {
            let mut leading = 0u32;
            while self.u1() == 0 && leading < 32 && self.bit / 8 < self.data.len() {
                leading += 1;
            }
            if leading == 0 {
                return 0;
            }
            (1u64 << leading) - 1 + self.u(leading)
        }

        fn se(&mut self) -> i64 {
            let k = self.ue();
            let sign = if k.is_multiple_of(2) { -1i64 } else { 1 };
            sign * k.div_ceil(2) as i64
        }
    }

    #[derive(Debug, Default)]
    struct SpsVui {
        video_full_range_flag: bool,
        colour_primaries: u64,
        matrix_coefficients: u64,
        max_num_reorder_frames: Option<u64>,
    }

    fn parse_sps_vui(sps: &[u8]) -> SpsVui {
        let mut r = BitReader::new(sps);
        let profile_idc = r.u(8);
        let _constraints = r.u(8);
        let _level = r.u(8);
        let _sps_id = r.ue();
        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            let chroma_format_idc = r.ue();
            if chroma_format_idc == 3 {
                let _separate_colour_plane = r.flag();
            }
            let _bit_depth_luma = r.ue();
            let _bit_depth_chroma = r.ue();
            let _qpprime = r.flag();
            if r.flag() {
                let lists = if chroma_format_idc != 3 { 8 } else { 12 };
                for i in 0..lists {
                    if r.flag() {
                        let size = if i < 6 { 16 } else { 64 };
                        let mut next = 8i64;
                        let mut last = 8i64;
                        for _ in 0..size {
                            if next != 0 {
                                let delta = r.se();
                                next = (last + delta + 256) % 256;
                            }
                            last = if next == 0 { last } else { next };
                        }
                    }
                }
            }
        }
        let _log2_max_frame_num = r.ue();
        let poc_type = r.ue();
        if poc_type == 0 {
            let _log2_max_poc_lsb = r.ue();
        } else if poc_type == 1 {
            let _delta_always_zero = r.flag();
            let _offset_non_ref = r.se();
            let _offset_top_bottom = r.se();
            let cycle = r.ue();
            for _ in 0..cycle.min(256) {
                let _ = r.se();
            }
        }
        let _max_num_ref_frames = r.ue();
        let _gaps = r.flag();
        let _w_mbs = r.ue();
        let _h_map_units = r.ue();
        let frame_mbs_only = r.flag();
        if !frame_mbs_only {
            let _mb_adaptive = r.flag();
        }
        let _direct8x8 = r.flag();
        if r.flag() {
            for _ in 0..4 {
                let _ = r.ue();
            }
        }
        let mut vui = SpsVui::default();
        if !r.flag() {
            return vui;
        }
        // vui_parameters()
        if r.flag() {
            let idc = r.u(8);
            if idc == 255 {
                let _ = r.u(16);
                let _ = r.u(16);
            }
        }
        if r.flag() {
            let _overscan_appropriate = r.flag();
        }
        if r.flag() {
            let _video_format = r.u(3);
            vui.video_full_range_flag = r.flag();
            if r.flag() {
                vui.colour_primaries = r.u(8);
                let _transfer = r.u(8);
                vui.matrix_coefficients = r.u(8);
            }
        }
        if r.flag() {
            let _ = r.ue();
            let _ = r.ue();
        }
        if r.flag() {
            let _num_units_in_tick = r.u(32);
            let _time_scale = r.u(32);
            let _fixed_frame_rate = r.flag();
        }
        let nal_hrd = r.flag();
        if nal_hrd {
            hrd_parameters(&mut r);
        }
        let vcl_hrd = r.flag();
        if vcl_hrd {
            hrd_parameters(&mut r);
        }
        if nal_hrd || vcl_hrd {
            let _low_delay = r.flag();
        }
        let _pic_struct_present = r.flag();
        if r.flag() {
            let _mv_over_boundaries = r.flag();
            let _max_bytes_per_pic_denom = r.ue();
            let _max_bits_per_mb_denom = r.ue();
            let _log2_mv_h = r.ue();
            let _log2_mv_v = r.ue();
            vui.max_num_reorder_frames = Some(r.ue());
            let _max_dec_frame_buffering = r.ue();
        }
        vui
    }

    fn hrd_parameters(r: &mut BitReader) {
        let cpb_cnt_minus1 = r.ue();
        let _bit_rate_scale = r.u(4);
        let _cpb_size_scale = r.u(4);
        for _ in 0..=cpb_cnt_minus1.min(31) {
            let _ = r.ue();
            let _ = r.ue();
            let _ = r.flag();
        }
        let _ = r.u(5);
        let _ = r.u(5);
        let _ = r.u(5);
        let _ = r.u(5);
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
            min_qp: None,
            intra_refresh: false,
            gpu_registry_id: None,
            software_encode: false,
        }
    }

    #[test]
    fn the_bit_reader_agrees_with_the_exp_golomb_table() {
        // 0 -> "1", 1 -> "010", 2 -> "011", 3 -> "00100", 4 -> "00101"
        let bytes = [0b1010_0110, 0b0100_0010, 0b1000_0000];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.ue(), 0);
        assert_eq!(r.ue(), 1);
        assert_eq!(r.ue(), 2);
        assert_eq!(r.ue(), 3);
        assert_eq!(r.ue(), 4);
    }

    #[test]
    fn emulation_prevention_bytes_are_stripped() {
        let r = BitReader::new(&[0x00, 0x00, 0x03, 0x01]);
        assert_eq!(r.data, vec![0x00, 0x00, 0x01]);
    }

    #[test]
    #[ignore = "requires a working VideoToolbox H.264 encoder and writes /tmp/screenextend_test.h264; run with --ignored --nocapture"]
    fn encode_synthetic() {
        let (w, h) = (640usize, 480usize);
        let mut cfg = cfg_at(w as u32, h as u32);
        cfg.profile = H264Profile::Baseline;
        let mut enc = Encoder::new(cfg).expect("encoder creates (HW required)");
        let out = enc.output();

        let mut per_frame: Vec<Vec<u8>> = Vec::new();
        let mut bitstream: Vec<u8> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        fn drain(
            out: &Receiver<CompressedSample>,
            per_frame: &mut Vec<Vec<u8>>,
            bitstream: &mut Vec<u8>,
            buf: &mut Vec<u8>,
        ) {
            while let Ok(cs) = out.try_recv() {
                cs.to_annexb(buf);
                if buf.is_empty() {
                    continue;
                }
                per_frame.push(nal_types(buf));
                bitstream.extend_from_slice(buf);
            }
        }

        // Per-submit timing, so this test produces a number as well as a verdict. Until now the
        // only encode timing in the repo came from the Windows x264 test, which is why the CI
        // perf gate had no macOS row at all — not a platform judgement, a data problem.
        // `scripts/perf-gate.mjs` parses the `vt_encode_p95=<n>us` printed below as
        // `vt_p95_encode_us`.
        //
        // `submit` hands the frame to the compression session and returns; the encode itself is
        // asynchronous, which is exactly what makes this the right thing to watch. It is the
        // cost the capture thread pays per frame, and a regression in it stalls capture whatever
        // the encoder does afterwards. End-to-end latency is `pipeline.rs`'s `end_to_end_latency`.
        let mut submit_us: Vec<u128> = Vec::with_capacity(60);
        let started = std::time::Instant::now();
        for i in 0..60usize {
            let pb = make_bgra_frame(w, h, i);
            let t0 = std::time::Instant::now();
            enc.submit(&pb, i == 30, tag()).expect("submit frame");
            submit_us.push(t0.elapsed().as_micros());
            drain(&out, &mut per_frame, &mut bitstream, &mut buf);
        }
        let wall_ms = started.elapsed().as_millis();
        enc.flush().expect("flush");
        while let Ok(cs) = out.recv_timeout(std::time::Duration::from_millis(500)) {
            cs.to_annexb(&mut buf);
            if buf.is_empty() {
                continue;
            }
            per_frame.push(nal_types(&buf));
            bitstream.extend_from_slice(&buf);
        }

        std::fs::write("/tmp/screenextend_test.h264", &bitstream).expect("write bitstream");
        let p = crate::test_support::percentiles(&submit_us);
        println!(
            "videotoolbox encode OK: {} frames {w}x{h}, {} bytes, output drops {}, vt_encode_p50={}us vt_encode_p95={}us vt_encode_p99={}us vt_encode_max={}us, wall={wall_ms}ms",
            per_frame.len(),
            bitstream.len(),
            enc.output_drops(),
            p.p50,
            p.p95,
            p.p99,
            p.max,
        );
        let keyframes: Vec<usize> = per_frame
            .iter()
            .enumerate()
            .filter(|(_, n)| n.contains(&5))
            .map(|(i, _)| i)
            .collect();
        println!("frame 0 NALs: {:?}", per_frame[0]);
        println!("keyframe indices: {keyframes:?}");

        assert!(
            per_frame.len() >= 55,
            "expected ~60 frames, got {}",
            per_frame.len()
        );
        assert!(per_frame[0].contains(&7), "frame 0 missing SPS");
        assert!(per_frame[0].contains(&8), "frame 0 missing PPS");
        assert!(per_frame[0].contains(&5), "frame 0 missing IDR slice");
        assert!(per_frame[1].contains(&1), "frame 1 not a P-frame");
        assert!(!per_frame[1].contains(&5), "frame 1 unexpectedly an IDR");
        assert!(
            keyframes.len() <= 3,
            "too many keyframes: {keyframes:?} (P-frames not used)"
        );

        for (i, nals) in per_frame.iter().enumerate() {
            for &t in nals {
                assert!(
                    matches!(t, 1 | 5 | 6 | 7 | 8 | 9),
                    "frame {i} unexpected NAL type {t}"
                );
            }
        }
    }

    /// What the decoder is actually told about buffering and colour.
    #[test]
    #[ignore = "requires a working VideoToolbox H.264 encoder; run with --ignored --nocapture"]
    fn the_sps_promises_no_reordering() {
        let (w, h) = (640usize, 480usize);
        let mut enc = Encoder::new(cfg_at(w as u32, h as u32)).expect("encoder");
        let out = enc.output();
        for i in 0..4usize {
            let pb = make_bgra_frame(w, h, i);
            enc.submit(&pb, i == 0, tag()).expect("submit");
        }
        enc.flush().expect("flush");

        let mut sps = None;
        let mut buf: Vec<u8> = Vec::new();
        while let Ok(cs) = out.recv_timeout(std::time::Duration::from_millis(500)) {
            cs.to_annexb(&mut buf);
            if let Some(nal) = first_nal(&buf, 7) {
                sps = Some(nal);
                break;
            }
        }
        let sps = sps.expect("an SPS in the first access unit");
        let vui = parse_sps_vui(&sps);
        println!("SPS VUI: {vui:?}");
        match vui.max_num_reorder_frames {
            Some(n) => assert_eq!(
                n, 0,
                "the decoder is told to hold {n} frames before output; AllowFrameReordering=false \
                 did not reach the bitstream"
            ),
            None => println!(
                "no bitstream_restriction in the VUI: the decoder is not told how many frames to \
                 hold, so it may apply the level's DPB size. Recorded, not asserted."
            ),
        }
    }

    #[test]
    #[ignore = "creates and destroys eight 720p hardware sessions; run with --ignored --nocapture"]
    fn recreate_churn_releases_hw() {
        for i in 0..8 {
            let enc = Encoder::new(cfg_at(1280, 720));
            assert!(
                enc.is_ok(),
                "create #{i} at 1280x720 failed: {:?}",
                enc.err()
            );
            drop(enc);
            println!("ok: churn iteration {i} created+released at 1280x720");
        }
    }

    fn hw_create_status(w: u32, h: u32) -> Result<(), i32> {
        let (tx, rx) = bounded::<CompressedSample>(OUTPUT_CAPACITY);
        let refcon = Box::into_raw(Box::new(OutputCtx {
            tx,
            rx,
            in_flight: Arc::new(AtomicUsize::new(0)),
            emitted: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        }));
        let spec = build_encoder_spec(false, HwPolicy::Require, None).expect("encoder spec");
        let r = create_session(&cfg_at(w, h), &spec, refcon);
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
    #[ignore = "requires VideoToolbox and exercises the software fallback below the hardware floor; run with --ignored --nocapture"]
    fn creates_small_via_software() {
        for (w, h) in [(480u32, 270u32), (448u32, 252u32), (160u32, 90u32)] {
            let hw = hw_create_status(w, h);
            let enc = Encoder::new(cfg_at(w, h));
            assert!(
                enc.is_ok(),
                "Encoder::new must succeed at {w}x{h}: {:?}",
                enc.err()
            );
            println!("ok: {w}x{h} created (require-HW was {hw:?}; Encoder::new used SW fallback)");
        }
    }

    #[test]
    #[ignore = "sweeps every resolution and holds six concurrent hardware sessions; run with --ignored --nocapture"]
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
                    println!(
                        "concurrency: capped at {} simultaneous HW session(s) ({e})",
                        n - 1
                    );
                    break;
                }
            }
        }
        drop(held);
    }
}
