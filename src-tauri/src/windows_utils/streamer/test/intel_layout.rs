//! Intel oneVPL ABI assertions: sizes, key offsets and alignment.
//!
//! Hand-derived from the oneVPL API 2.x headers cited in `intel_sys.rs` (run against a live
//! 2.7.0 dispatcher by `intel_encoder.rs`), not generated like `nvenc_layout.rs` and
//! `amd_layout.rs`: the dispatcher that is loaded (`libvpl.dll`) ships with the driver, so a
//! vendored header set would describe a oneVPL nobody runs. A struct missing a trailing field
//! passes every functional test, so `field_offsets_match_onevpl_abi_static_asserts` checks each
//! field against oneVPL's own compiled ABI assertions; the sizes come from the same file.
//!
//! Pointer-bearing assertions are gated on `target_pointer_width = "64"`, the only width this
//! backend ships on.

use std::mem::size_of;

use crate::windows_utils::streamer::intel::intel_sys::*;

#[test]
fn struct_sizes_match_onevpl_abi() {
    assert_eq!(size_of::<mfxFrameId>(), 8, "mfxFrameId");
    assert_eq!(size_of::<mfxFrameInfo>(), 68, "mfxFrameInfo");
    assert_eq!(size_of::<mfxInfoMFX>(), 136, "mfxInfoMFX");
    assert_eq!(size_of::<mfxInfoVPP>(), 168, "mfxInfoVPP");
    assert_eq!(
        size_of::<mfxInfoUnion>(),
        168,
        "mfxInfoUnion == sizeof(mfxInfoVPP)"
    );
    assert_eq!(size_of::<mfxVideoParam>(), 208, "mfxVideoParam");
    assert_eq!(size_of::<mfxBitstream>(), 72, "mfxBitstream");
    // 56, not 48: the struct ends with `mfxPayload **Payload`
    assert_eq!(size_of::<mfxEncodeCtrl>(), 56, "mfxEncodeCtrl");
    assert_eq!(size_of::<mfxFrameData>(), 96, "mfxFrameData");
    assert_eq!(size_of::<mfxFrameSurface1>(), 184, "mfxFrameSurface1");
    assert_eq!(
        size_of::<mfxFrameSurfaceInterface>(),
        112,
        "mfxFrameSurfaceInterface"
    );
    assert_eq!(size_of::<mfxVariant>(), 16, "mfxVariant");
    assert_eq!(size_of::<mfxExtCodingOption>(), 64, "mfxExtCodingOption");
    assert_eq!(size_of::<mfxExtCodingOption2>(), 68, "mfxExtCodingOption2");
    assert_eq!(size_of::<mfxExtCodingOption3>(), 512, "mfxExtCodingOption3");
    assert_eq!(size_of::<mfxExtBuffer>(), 8, "mfxExtBuffer");
}

/// Alignment, which a size check cannot substitute for.
///
/// A binding with the wrong alignment (an accidental `#[repr(packed)]`, a member declared as a
/// different integer width) can still size and place its own fields correctly, and then put
/// the *next* element of an array or an embedded member somewhere the runtime does not look.
/// `mfxVideoParam.ExtParam` is an array of pointers.
#[cfg(target_pointer_width = "64")]
#[test]
fn struct_alignments_match_onevpl_abi() {
    use std::mem::align_of;

    // pointer-bearing structs align to 8 on 64-bit
    assert_eq!(align_of::<mfxVideoParam>(), 8, "mfxVideoParam");
    assert_eq!(align_of::<mfxBitstream>(), 8, "mfxBitstream");
    assert_eq!(align_of::<mfxEncodeCtrl>(), 8, "mfxEncodeCtrl");
    assert_eq!(align_of::<mfxVariant>(), 8, "mfxVariant");
    assert_eq!(align_of::<mfxInfoVPP>(), 4, "mfxInfoVPP");
    assert_eq!(align_of::<mfxInfoUnion>(), 4, "mfxInfoUnion");

    // plain-data structs align to their widest member
    assert_eq!(align_of::<mfxFrameId>(), 2, "mfxFrameId");
    assert_eq!(align_of::<mfxFrameInfo>(), 4, "mfxFrameInfo");
    assert_eq!(align_of::<mfxInfoMFX>(), 4, "mfxInfoMFX");
    assert_eq!(align_of::<mfxExtBuffer>(), 4, "mfxExtBuffer");
    assert_eq!(align_of::<mfxExtCodingOption>(), 4, "mfxExtCodingOption");
    assert_eq!(align_of::<mfxExtCodingOption2>(), 4, "mfxExtCodingOption2");
    assert_eq!(align_of::<mfxExtCodingOption3>(), 4, "mfxExtCodingOption3");

    // C never produces a size that is not a multiple of the alignment
    assert_eq!(size_of::<mfxVideoParam>() % align_of::<mfxVideoParam>(), 0);
    assert_eq!(size_of::<mfxBitstream>() % align_of::<mfxBitstream>(), 0);
}

#[test]
fn key_field_offsets_match_onevpl_abi() {
    use std::mem::offset_of;

    assert_eq!(offset_of!(mfxFrameInfo, FourCC), 32, "mfxFrameInfo.FourCC");
    assert_eq!(offset_of!(mfxFrameInfo, Width), 36, "mfxFrameInfo.Width");
    assert_eq!(
        offset_of!(mfxFrameInfo, FrameRateExtN),
        48,
        "mfxFrameInfo.FrameRateExtN"
    );

    assert_eq!(
        offset_of!(mfxInfoMFX, FrameInfo),
        32,
        "mfxInfoMFX.FrameInfo"
    );
    assert_eq!(
        offset_of!(mfxInfoMFX, InitialDelayInKB),
        122,
        "mfxInfoMFX.InitialDelayInKB (==QPI)"
    );
    assert_eq!(
        offset_of!(mfxInfoMFX, TargetKbps),
        126,
        "mfxInfoMFX.TargetKbps (==QPP)"
    );
    assert_eq!(
        offset_of!(mfxInfoMFX, MaxKbps),
        128,
        "mfxInfoMFX.MaxKbps (==QPB)"
    );

    assert_eq!(
        offset_of!(mfxVideoParam, AsyncDepth),
        14,
        "mfxVideoParam.AsyncDepth"
    );
    assert_eq!(
        offset_of!(mfxVideoParam, IOPattern),
        186,
        "mfxVideoParam.IOPattern"
    );
    assert_eq!(
        offset_of!(mfxVideoParam, ExtParam),
        192,
        "mfxVideoParam.ExtParam"
    );

    assert_eq!(
        offset_of!(mfxEncodeCtrl, ExtParam),
        40,
        "mfxEncodeCtrl.ExtParam"
    );
    assert_eq!(
        offset_of!(mfxEncodeCtrl, Payload),
        48,
        "mfxEncodeCtrl.Payload"
    );
    assert_eq!(offset_of!(mfxBitstream, Data), 40, "mfxBitstream.Data");
    assert_eq!(
        offset_of!(mfxBitstream, DataLength),
        52,
        "mfxBitstream.DataLength"
    );

    assert_eq!(
        offset_of!(mfxExtCodingOption2, IntRefType),
        8,
        "CO2.IntRefType"
    );
    assert_eq!(
        offset_of!(mfxExtCodingOption3, WinBRCSize),
        16,
        "CO3.WinBRCSize"
    );
    assert_eq!(
        offset_of!(mfxExtCodingOption3, ScenarioInfo),
        54,
        "CO3.ScenarioInfo"
    );
    assert_eq!(
        offset_of!(mfxExtCodingOption3, LowDelayBRC),
        166,
        "CO3.LowDelayBRC"
    );
}

/// The rate-control enum has reserved gaps, so its values cannot be inferred from their order.
/// A wrong one is neither a compile error nor a clean runtime error: `MFX_RATECONTROL_LA` (8)
/// in place of `MFX_RATECONTROL_VCM` (10) selects lookahead, which buffers frames, and the
/// runtime faults inside `MFXVideoENCODE_Init` rather than refusing it.
#[test]
fn intel_rate_control_values_match_mfxstructures_h() {
    assert_eq!(MFX_RATECONTROL_CBR, 1, "MFX_RATECONTROL_CBR");
    assert_eq!(MFX_RATECONTROL_VBR, 2, "MFX_RATECONTROL_VBR");
    assert_eq!(MFX_RATECONTROL_CQP, 3, "MFX_RATECONTROL_CQP");
    assert_eq!(MFX_RATECONTROL_AVBR, 4, "MFX_RATECONTROL_AVBR");
    assert_eq!(MFX_RATECONTROL_LA, 8, "MFX_RATECONTROL_LA");
    assert_eq!(MFX_RATECONTROL_ICQ, 9, "MFX_RATECONTROL_ICQ");
    assert_eq!(MFX_RATECONTROL_VCM, 10, "MFX_RATECONTROL_VCM");
    assert_eq!(MFX_RATECONTROL_QVBR, 14, "MFX_RATECONTROL_QVBR");
}

/// Every field offset oneVPL's own ABI test pins, for every field this binding declares.
///
/// The numbers are copied by script from `api/tests/abi/mfx_static_assert_structs.cpp` in
/// intel/libvpl (the x86_64 blocks), whose `MSDK_STATIC_ASSERT_STRUCT_OFFSET(struct, field, N)`
/// lines the oneVPL build compiles against its own headers. Rows are matched by name to the
/// `pub` fields of `intel_sys.rs`; a field the binding names differently (a flattened union
/// member such as `ExtParam_or_reserved2`) is left to the size checks. To refresh, re-run the
/// match against a newer libvpl checkout and replace the table.
#[cfg(target_pointer_width = "64")]
#[test]
fn field_offsets_match_onevpl_abi_static_asserts() {
    macro_rules! offsets {
        ($(($s:ident, $f:ident, $o:expr)),* $(,)?) => {{
            let mut bad: Vec<String> = Vec::new();
            $(
                let got = std::mem::offset_of!($s, $f);
                if got != $o {
                    bad.push(format!("{}.{}: binding {} vs C {}", stringify!($s), stringify!($f), got, $o));
                }
            )*
            bad
        }};
    }
    let bad = offsets!(
        (mfxExtBuffer, BufferId, 0),
        (mfxExtBuffer, BufferSz, 4),
        (mfxBitstream, DecodeTimeStamp, 24),
        (mfxBitstream, TimeStamp, 32),
        (mfxBitstream, Data, 40),
        (mfxBitstream, DataOffset, 48),
        (mfxBitstream, DataLength, 52),
        (mfxBitstream, MaxLength, 56),
        (mfxBitstream, PicStruct, 60),
        (mfxBitstream, FrameType, 62),
        (mfxBitstream, DataFlag, 64),
        (mfxFrameId, TemporalId, 0),
        (mfxFrameId, PriorityId, 2),
        (mfxFrameId, DependencyId, 4),
        (mfxFrameId, QualityId, 6),
        (mfxFrameInfo, BitDepthLuma, 18),
        (mfxFrameInfo, BitDepthChroma, 20),
        (mfxFrameInfo, Shift, 22),
        (mfxFrameInfo, FrameId, 24),
        (mfxFrameInfo, FourCC, 32),
        (mfxFrameInfo, Width, 36),
        (mfxFrameInfo, Height, 38),
        (mfxFrameInfo, CropX, 40),
        (mfxFrameInfo, CropY, 42),
        (mfxFrameInfo, CropW, 44),
        (mfxFrameInfo, CropH, 46),
        (mfxFrameInfo, FrameRateExtN, 48),
        (mfxFrameInfo, FrameRateExtD, 52),
        (mfxFrameInfo, AspectRatioW, 58),
        (mfxFrameInfo, AspectRatioH, 60),
        (mfxFrameInfo, PicStruct, 62),
        (mfxFrameInfo, ChromaFormat, 64),
        (mfxFrameData, NumExtParam, 8),
        (mfxFrameData, MemType, 28),
        (mfxFrameData, PitchHigh, 30),
        (mfxFrameData, TimeStamp, 32),
        (mfxFrameData, FrameOrder, 40),
        (mfxFrameData, Locked, 44),
        (mfxFrameData, Pitch, 46),
        (mfxFrameData, Y, 48),
        (mfxFrameData, UV, 56),
        (mfxFrameData, Cr, 64),
        (mfxFrameData, A, 72),
        (mfxFrameData, MemId, 80),
        (mfxFrameData, Corrupted, 88),
        (mfxFrameData, DataFlag, 90),
        (mfxFrameSurface1, Info, 16),
        (mfxFrameSurface1, Data, 88),
        (mfxInfoMFX, LowPower, 28),
        (mfxInfoMFX, BRCParamMultiplier, 30),
        (mfxInfoMFX, FrameInfo, 32),
        (mfxInfoMFX, CodecId, 100),
        (mfxInfoMFX, CodecProfile, 104),
        (mfxInfoMFX, CodecLevel, 106),
        (mfxInfoMFX, NumThread, 108),
        (mfxInfoMFX, TargetUsage, 110),
        (mfxInfoMFX, GopPicSize, 112),
        (mfxInfoMFX, GopRefDist, 114),
        (mfxInfoMFX, GopOptFlag, 116),
        (mfxInfoMFX, IdrInterval, 118),
        (mfxInfoMFX, RateControlMethod, 120),
        (mfxInfoMFX, InitialDelayInKB, 122),
        (mfxInfoMFX, BufferSizeInKB, 124),
        (mfxInfoMFX, TargetKbps, 126),
        (mfxInfoMFX, MaxKbps, 128),
        (mfxInfoMFX, NumSlice, 130),
        (mfxInfoMFX, NumRefFrame, 132),
        (mfxInfoMFX, EncodedOrder, 134),
        (mfxInfoVPP, In, 32),
        (mfxInfoVPP, Out, 100),
        (mfxVideoParam, AllocId, 0),
        (mfxVideoParam, AsyncDepth, 14),
        (mfxVideoParam, Protected, 184),
        (mfxVideoParam, IOPattern, 186),
        (mfxVideoParam, ExtParam, 192),
        (mfxVideoParam, NumExtParam, 200),
        (mfxExtCodingOption, Header, 0),
        (mfxExtCodingOption, RateDistortionOpt, 10),
        (mfxExtCodingOption, MECostType, 12),
        (mfxExtCodingOption, MESearchType, 14),
        (mfxExtCodingOption, MVSearchWindow, 16),
        (mfxExtCodingOption, EndOfSequence, 20),
        (mfxExtCodingOption, FramePicture, 22),
        (mfxExtCodingOption, CAVLC, 24),
        (mfxExtCodingOption, RecoveryPointSEI, 30),
        (mfxExtCodingOption, ViewOutput, 32),
        (mfxExtCodingOption, NalHrdConformance, 34),
        (mfxExtCodingOption, SingleSeiNalUnit, 36),
        (mfxExtCodingOption, VuiVclHrdParameters, 38),
        (mfxExtCodingOption, RefPicListReordering, 40),
        (mfxExtCodingOption, ResetRefList, 42),
        (mfxExtCodingOption, RefPicMarkRep, 44),
        (mfxExtCodingOption, FieldOutput, 46),
        (mfxExtCodingOption, IntraPredBlockSize, 48),
        (mfxExtCodingOption, InterPredBlockSize, 50),
        (mfxExtCodingOption, MVPrecision, 52),
        (mfxExtCodingOption, MaxDecFrameBuffering, 54),
        (mfxExtCodingOption, AUDelimiter, 56),
        (mfxExtCodingOption, EndOfStream, 58),
        (mfxExtCodingOption, PicTimingSEI, 60),
        (mfxExtCodingOption, VuiNalHrdParameters, 62),
        (mfxExtCodingOption2, Header, 0),
        (mfxExtCodingOption2, IntRefType, 8),
        (mfxExtCodingOption2, IntRefCycleSize, 10),
        (mfxExtCodingOption2, IntRefQPDelta, 12),
        (mfxExtCodingOption2, MaxFrameSize, 16),
        (mfxExtCodingOption2, MaxSliceSize, 20),
        (mfxExtCodingOption2, BitrateLimit, 24),
        (mfxExtCodingOption2, MBBRC, 26),
        (mfxExtCodingOption2, ExtBRC, 28),
        (mfxExtCodingOption2, LookAheadDepth, 30),
        (mfxExtCodingOption2, Trellis, 32),
        (mfxExtCodingOption2, RepeatPPS, 34),
        (mfxExtCodingOption2, BRefType, 36),
        (mfxExtCodingOption2, AdaptiveI, 38),
        (mfxExtCodingOption2, AdaptiveB, 40),
        (mfxExtCodingOption2, LookAheadDS, 42),
        (mfxExtCodingOption2, NumMbPerSlice, 44),
        (mfxExtCodingOption2, SkipFrame, 46),
        (mfxExtCodingOption2, MinQPI, 48),
        (mfxExtCodingOption2, MaxQPI, 49),
        (mfxExtCodingOption2, MinQPP, 50),
        (mfxExtCodingOption2, MaxQPP, 51),
        (mfxExtCodingOption2, MinQPB, 52),
        (mfxExtCodingOption2, MaxQPB, 53),
        (mfxExtCodingOption2, FixedFrameRate, 54),
        (mfxExtCodingOption2, DisableDeblockingIdc, 56),
        (mfxExtCodingOption2, DisableVUI, 58),
        (mfxExtCodingOption2, BufferingPeriodSEI, 60),
        (mfxExtCodingOption2, EnableMAD, 62),
        (mfxExtCodingOption2, UseRawRef, 64),
        (mfxExtCodingOption3, Header, 0),
        (mfxExtCodingOption3, NumSliceI, 8),
        (mfxExtCodingOption3, NumSliceP, 10),
        (mfxExtCodingOption3, NumSliceB, 12),
        (mfxExtCodingOption3, WinBRCMaxAvgKbps, 14),
        (mfxExtCodingOption3, WinBRCSize, 16),
        (mfxExtCodingOption3, QVBRQuality, 18),
        (mfxExtCodingOption3, EnableMBQP, 20),
        (mfxExtCodingOption3, IntRefCycleDist, 22),
        (mfxExtCodingOption3, DirectBiasAdjustment, 24),
        (mfxExtCodingOption3, GlobalMotionBiasAdjustment, 26),
        (mfxExtCodingOption3, MVCostScalingFactor, 28),
        (mfxExtCodingOption3, MBDisableSkipMap, 30),
        (mfxExtCodingOption3, WeightedPred, 32),
        (mfxExtCodingOption3, WeightedBiPred, 34),
        (mfxExtCodingOption3, AspectRatioInfoPresent, 36),
        (mfxExtCodingOption3, OverscanInfoPresent, 38),
        (mfxExtCodingOption3, OverscanAppropriate, 40),
        (mfxExtCodingOption3, TimingInfoPresent, 42),
        (mfxExtCodingOption3, BitstreamRestriction, 44),
        (mfxExtCodingOption3, LowDelayHrd, 46),
        (mfxExtCodingOption3, MotionVectorsOverPicBoundaries, 48),
        (mfxExtCodingOption3, ScenarioInfo, 54),
        (mfxExtCodingOption3, ContentInfo, 56),
        (mfxExtCodingOption3, PRefType, 58),
        (mfxExtCodingOption3, FadeDetection, 60),
        (mfxExtCodingOption3, GPB, 66),
        (mfxExtCodingOption3, MaxFrameSizeI, 68),
        (mfxExtCodingOption3, MaxFrameSizeP, 72),
        (mfxExtCodingOption3, EnableQPOffset, 88),
        (mfxExtCodingOption3, QPOffset, 90),
        (mfxExtCodingOption3, NumRefActiveP, 106),
        (mfxExtCodingOption3, NumRefActiveBL0, 122),
        (mfxExtCodingOption3, NumRefActiveBL1, 138),
        (mfxExtCodingOption3, TransformSkip, 156),
        (mfxExtCodingOption3, BRCPanicMode, 164),
        (mfxExtCodingOption3, LowDelayBRC, 166),
        (mfxExtCodingOption3, EnableMBForceIntra, 168),
        (mfxExtCodingOption3, AdaptiveMaxFrameSize, 170),
        (mfxExtCodingOption3, RepartitionCheckEnable, 172),
        (mfxExtCodingOption3, EncodedUnitsInfo, 180),
        (mfxExtCodingOption3, EnableNalUnitType, 182),
        (mfxExtCodingOption3, AdaptiveLTR, 184),
        (mfxExtCodingOption3, AdaptiveCQM, 186),
        (mfxExtCodingOption3, AdaptiveRef, 188),
        (mfxEncodeCtrl, Header, 0),
        (mfxEncodeCtrl, MfxNalUnitType, 26),
        (mfxEncodeCtrl, SkipFrame, 28),
        (mfxEncodeCtrl, QP, 30),
        (mfxEncodeCtrl, FrameType, 32),
        (mfxEncodeCtrl, NumExtParam, 34),
        (mfxEncodeCtrl, NumPayload, 36),
        (mfxEncodeCtrl, ExtParam, 40),
        (mfxEncodeCtrl, Payload, 48),
        (mfxVariant, Version, 0),
        (mfxVariant, Type, 4),
        (mfxVariant, Data, 8),
        (mfxFrameSurfaceInterface, Context, 0),
        (mfxFrameSurfaceInterface, Version, 8),
        (mfxFrameSurfaceInterface, AddRef, 16),
        (mfxFrameSurfaceInterface, Release, 24),
        (mfxFrameSurfaceInterface, GetRefCounter, 32),
        (mfxFrameSurfaceInterface, Map, 40),
        (mfxFrameSurfaceInterface, Unmap, 48),
        (mfxFrameSurfaceInterface, GetNativeHandle, 56),
        (mfxFrameSurfaceInterface, GetDeviceHandle, 64),
        (mfxFrameSurfaceInterface, Synchronize, 72),
        (mfxFrameSurfaceInterface, OnComplete, 80),
        (mfxFrameSurfaceInterface, QueryInterface, 88),
    );
    assert!(bad.is_empty(), "fields off the oneVPL ABI: {bad:#?}");
}
