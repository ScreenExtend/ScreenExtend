//! The AMF property-name catalogue as NUL-terminated UTF-16, plus the component and trace-writer
//! ids.
//!
//! These strings are the API. `SetProperty` on a name AMF does not know returns `AMF_NOT_FOUND`
//! and is trivially ignored, so a typo here is a silently missing setting rather than a crash —
//! which is why `test/amd_layout.rs` asserts every one of them against the vendored header.
//!
//! Four pairs of header constants share a single string, so setting "both" of a pair configures
//! one property, not two:
//! * `PREENCODE_ENABLE` == `RATE_CONTROL_PREANALYSIS_ENABLE` == `RateControlPreanalysisEnable`
//! * `LOWLATENCY_MODE` == "LOW_LATENCY_INTERNAL" == `LowLatencyInternal`
//! * `OUTPUT_FULL_RANGE_COLOR` == `FULL_RANGE_COLOR` == `FullRangeColor`
//! * `CAP_QUERY_TIMEOUT_SUPPORT` == `CAPS_QUERY_TIMEOUT_SUPPORT` == `QueryTimeoutSupport`

/// Widens an ASCII literal at compile time. Every AMF property name is ASCII.
pub const fn utf16<const N: usize>(s: &str) -> [u16; N] {
    let bytes = s.as_bytes();
    let mut out = [0u16; N];
    let mut i = 0;
    while i < bytes.len() {
        out[i] = bytes[i] as u16;
        i += 1;
    }
    out
}

macro_rules! wide {
    ($($(#[$m:meta])* $name:ident = $s:literal;)*) => {
        $(
            $(#[$m])*
            pub const $name: &[u16] = &utf16::<{ $s.len() + 1 }>($s);
        )*
    };
}

/// Component ids passed to `AMFFactory::CreateComponent`. Note that the HEVC and AV1 macros do
/// **not** expand to their own names (`AMFVideoEncoderHW_HEVC`, `AMFVideoEncoderHW_AV1`) — only
/// the AVC one does.
pub mod codec {
    use super::utf16;
    wide! {
        VIDEO_ENCODER_VCE_AVC = "AMFVideoEncoderVCE_AVC";
    }
}

pub mod trace_writer {
    use super::utf16;
    wide! {
        CONSOLE = "Console";
        DEBUG_OUTPUT = "DebugOutput";
    }
}

pub mod names {
    use super::utf16;

    wide! {
        // ---- static: settable before Init() ----
        /// **Set this first.** It re-defaults most other rate-control and GOP properties, so
        /// anything written before it is lost.
        AMF_VIDEO_ENCODER_USAGE = "Usage";
        AMF_VIDEO_ENCODER_QUALITY_PRESET = "QualityPreset";
        AMF_VIDEO_ENCODER_FRAMESIZE = "FrameSize";
        AMF_VIDEO_ENCODER_PROFILE = "Profile";
        AMF_VIDEO_ENCODER_PROFILE_LEVEL = "ProfileLevel";
        /// Read-only: the Annex-B SPS+PPS, available after `Init()`.
        AMF_VIDEO_ENCODER_EXTRADATA = "ExtraData";
        AMF_VIDEO_ENCODER_MAX_LTR_FRAMES = "MaxOfLTRFrames";
        AMF_VIDEO_ENCODER_LTR_MODE = "LTRMode";
        AMF_VIDEO_ENCODER_MAX_NUM_REFRAMES = "MaxNumRefFrames";
        AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES = "MaxConsecutiveBPictures";
        AMF_VIDEO_ENCODER_ASPECT_RATIO = "AspectRatio";
        AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR = "InputFullRangeColor";
        AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR = "FullRangeColor";
        /// "enables low latency mode and POC mode 2 in the encoder". `USAGE = ULTRA_LOW_LATENCY`
        /// already sets it; writing it explicitly is belt and braces, and not every driver has it.
        AMF_VIDEO_ENCODER_LOWLATENCY_MODE = "LowLatencyInternal";
        AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE = "EnablePreAnalysis";
        AMF_VIDEO_ENCODER_PREENCODE_ENABLE = "RateControlPreanalysisEnable";
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = "RateControlMethod";
        AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE = "OutColorProfile";
        AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC = "OutColorTransferChar";
        AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES = "OutColorPrimaries";
        AMF_VIDEO_ENCODER_OUTPUT_MODE = "OutputMode";

        // ---- dynamic: settable on a running component, applied at the next SubmitInput ----
        AMF_VIDEO_ENCODER_FRAMERATE = "FrameRate";
        AMF_VIDEO_ENCODER_ENFORCE_HRD = "EnforceHRD";
        AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE = "FillerDataEnable";
        AMF_VIDEO_ENCODER_ENABLE_VBAQ = "EnableVBAQ";
        AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE = "HighMotionQualityBoostEnable";
        /// In **bits**, not bytes.
        AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE = "VBVBufferSize";
        /// 0 = 0%, 64 = 100%. The default is 64; zero is an *empty* buffer, which is worse.
        AMF_VIDEO_ENCODER_INITIAL_VBV_BUFFER_FULLNESS = "InitialVBVBufferFullness";
        AMF_VIDEO_ENCODER_MAX_AU_SIZE = "MaxAUSize";
        AMF_VIDEO_ENCODER_MIN_QP = "MinQP";
        AMF_VIDEO_ENCODER_MAX_QP = "MaxQP";
        AMF_VIDEO_ENCODER_QP_I = "QPI";
        AMF_VIDEO_ENCODER_QP_P = "QPP";
        AMF_VIDEO_ENCODER_TARGET_BITRATE = "TargetBitrate";
        AMF_VIDEO_ENCODER_PEAK_BITRATE = "PeakBitrate";
        AMF_VIDEO_ENCODER_RATE_CONTROL_SKIP_FRAME_ENABLE = "RateControlSkipFrameEnable";
        /// A spacing in frames (0..=1000), not a mode. In-band parameter sets come from the
        /// per-surface `InsertSPS`/`InsertPPS` instead.
        AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING = "HeaderInsertionSpacing";
        AMF_VIDEO_ENCODER_B_PIC_PATTERN = "BPicturesPattern";
        AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER = "DeBlockingFilter";
        AMF_VIDEO_ENCODER_IDR_PERIOD = "IDRPeriod";
        AMF_VIDEO_ENCODER_INTRA_PERIOD = "IntraPeriod";
        AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT = "IntraRefreshMBsNumberPerSlot";
        AMF_VIDEO_ENCODER_SLICES_PER_FRAME = "SlicesPerFrame";
        AMF_VIDEO_ENCODER_CABAC_ENABLE = "CABACEnable";
        /// Filed under "dynamic" in the header but effectively **static**: set it before
        /// `Init()` or it returns `AMF_OK`, echoes back from `GetProperty`, and does nothing.
        AMF_VIDEO_ENCODER_QUERY_TIMEOUT = "QueryTimeout";
        AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE = "InputQueueSize";
        AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO = "EnableEncoderSmartAccessVideo";

        // ---- per-submission: set on the input AMFSurface, not the component ----
        AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE = "ForcePictureType";
        AMF_VIDEO_ENCODER_INSERT_SPS = "InsertSPS";
        AMF_VIDEO_ENCODER_INSERT_PPS = "InsertPPS";
        AMF_VIDEO_ENCODER_INSERT_AUD = "InsertAUD";
        AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX = "MarkCurrentWithLTRIndex";
        AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD = "ForceLTRReferenceBitfield";

        // ---- set by the encoder on the output buffer ----
        AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE = "OutputDataType";
        AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE = "OutputBufferType";
        AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX = "MarkedLTRIndex";
        AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD = "ReferencedLTRIndexBitfield";

        // ---- capabilities: read from AMFCaps with GetProperty ----
        AMF_VIDEO_ENCODER_CAP_MAX_BITRATE = "MaxBitrate";
        AMF_VIDEO_ENCODER_CAP_NUM_OF_STREAMS = "NumOfStreams";
        AMF_VIDEO_ENCODER_CAP_MAX_PROFILE = "MaxProfile";
        AMF_VIDEO_ENCODER_CAP_MAX_LEVEL = "MaxLevel";
        AMF_VIDEO_ENCODER_CAP_BFRAMES = "BFrames";
        AMF_VIDEO_ENCODER_CAP_MAX_REFERENCE_FRAMES = "MaxReferenceFrames";
        AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES = "NumOfHwInstances";
        AMF_VIDEO_ENCODER_CAP_COLOR_CONVERSION = "ColorConversion";
        AMF_VIDEO_ENCODER_CAP_PRE_ANALYSIS = "PreAnalysis";
        /// In 16x16 macroblocks.
        AMF_VIDEO_ENCODER_CAP_MAX_THROUGHPUT = "MaxThroughput";
        AMF_VIDEO_ENCODER_CAP_REQUESTED_THROUGHPUT = "RequestedThroughput";
        AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT = "QueryTimeoutSupport";
        AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT = "SupportSliceOutput";
    }
}
