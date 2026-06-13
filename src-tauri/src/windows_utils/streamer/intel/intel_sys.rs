#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

use std::ffi::c_void;

use anyhow::{Context as _, Result};

// ===========================================================================================
// Primitive typedefs (mfxdefs.h)
// ===========================================================================================

pub type mfxU8 = u8;
pub type mfxI8 = i8;
pub type mfxU16 = u16;
pub type mfxI16 = i16;
pub type mfxU32 = u32;
pub type mfxI32 = i32;
pub type mfxU64 = u64;
pub type mfxI64 = i64;
pub type mfxF32 = f32;
pub type mfxF64 = f64;
pub type mfxHDL = *mut c_void;
pub type mfxMemId = *mut c_void;
pub type mfxThreadTask = *mut c_void;

pub type mfxStatus = i32;
pub type mfxVariantType = i32;
pub type mfxHandleType = i32;
pub type mfxResourceType = i32;
pub type mfxAccelerationMode = i32;
pub type mfxImplType = i32;
pub type mfxStructVersion = u16;

pub type mfxLoader = *mut c_void;
pub type mfxConfig = *mut c_void;
pub type mfxSession = *mut c_void;
pub type mfxSyncPoint = *mut c_void;

#[inline]
pub const fn make_fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

// ===========================================================================================
// Status codes (mfxdefs.h: mfxStatus)
// ===========================================================================================

pub const MFX_ERR_NONE: mfxStatus = 0;
pub const MFX_ERR_UNKNOWN: mfxStatus = -1;
pub const MFX_ERR_NULL_PTR: mfxStatus = -2;
pub const MFX_ERR_UNSUPPORTED: mfxStatus = -3;
pub const MFX_ERR_MEMORY_ALLOC: mfxStatus = -4;
pub const MFX_ERR_NOT_ENOUGH_BUFFER: mfxStatus = -5;
pub const MFX_ERR_INVALID_HANDLE: mfxStatus = -6;
pub const MFX_ERR_NOT_INITIALIZED: mfxStatus = -8;
pub const MFX_ERR_NOT_FOUND: mfxStatus = -9;
pub const MFX_ERR_MORE_DATA: mfxStatus = -10;
pub const MFX_ERR_MORE_SURFACE: mfxStatus = -11;
pub const MFX_ERR_DEVICE_FAILED: mfxStatus = -17;
pub const MFX_ERR_DEVICE_LOST: mfxStatus = -13;
pub const MFX_ERR_INCOMPATIBLE_VIDEO_PARAM: mfxStatus = -14;
pub const MFX_ERR_INVALID_VIDEO_PARAM: mfxStatus = -15;
pub const MFX_WRN_IN_EXECUTION: mfxStatus = 1;
pub const MFX_WRN_DEVICE_BUSY: mfxStatus = 2;
pub const MFX_WRN_VIDEO_PARAM_CHANGED: mfxStatus = 3;
pub const MFX_WRN_PARTIAL_ACCELERATION: mfxStatus = 4;
pub const MFX_WRN_INCOMPATIBLE_VIDEO_PARAM: mfxStatus = 5;
pub const MFX_WRN_VALUE_NOT_CHANGED: mfxStatus = 6;
pub const MFX_WRN_OUT_OF_RANGE: mfxStatus = 7;

pub const MFX_INFINITE: u32 = 0xFFFF_FFFF;

// ===========================================================================================
// Implementation / acceleration / handle / resource enums (mfxcommon.h, mfxstructures.h)
// ===========================================================================================

pub const MFX_IMPL_TYPE_SOFTWARE: mfxImplType = 0x0001;
pub const MFX_IMPL_TYPE_HARDWARE: mfxImplType = 0x0002;

pub const MFX_ACCEL_MODE_NA: mfxAccelerationMode = 0;
pub const MFX_ACCEL_MODE_VIA_D3D9: mfxAccelerationMode = 0x0200;
pub const MFX_ACCEL_MODE_VIA_D3D11: mfxAccelerationMode = 0x0300;

pub const MFX_HANDLE_D3D11_DEVICE: mfxHandleType = 3;

pub const MFX_RESOURCE_DX11_TEXTURE: mfxResourceType = 5;

pub const MFX_VARIANT_TYPE_U32: mfxVariantType = 5; // == MFX_DATA_TYPE_U32

pub const MFX_MAP_READ: u32 = 0x1;
pub const MFX_MAP_WRITE: u32 = 0x2;

// ===========================================================================================
// Codec / format / rate-control enums (mfxstructures.h)
// ===========================================================================================

pub const MFX_FOURCC_NV12: u32 = make_fourcc(b'N', b'V', b'1', b'2');
pub const MFX_FOURCC_RGB4: u32 = make_fourcc(b'R', b'G', b'B', b'4'); // BGRA32
pub const MFX_FOURCC_BGRA: u32 = MFX_FOURCC_RGB4;

pub const MFX_CHROMAFORMAT_YUV420: u16 = 1;
pub const MFX_CHROMAFORMAT_YUV444: u16 = 3;

pub const MFX_PICSTRUCT_PROGRESSIVE: u16 = 0x01;

pub const MFX_CODEC_AVC: u32 = make_fourcc(b'A', b'V', b'C', b' ');

pub const MFX_PROFILE_AVC_BASELINE: u16 = 66;
pub const MFX_PROFILE_AVC_MAIN: u16 = 77;
pub const MFX_PROFILE_AVC_HIGH: u16 = 100;
/// MFX_PROFILE_AVC_CONSTRAINT_SET1 = (0x100 << 1) = 0x200
pub const MFX_PROFILE_AVC_CONSTRAINED_BASELINE: u16 = MFX_PROFILE_AVC_BASELINE + 0x200;

pub const MFX_RATECONTROL_CBR: u16 = 1;
pub const MFX_RATECONTROL_VBR: u16 = 2;
pub const MFX_RATECONTROL_CQP: u16 = 3;

pub const MFX_TARGETUSAGE_BEST_QUALITY: u16 = 1;
pub const MFX_TARGETUSAGE_BALANCED: u16 = 4;
pub const MFX_TARGETUSAGE_BEST_SPEED: u16 = 7;

pub const MFX_CODINGOPTION_UNKNOWN: u16 = 0x00;
pub const MFX_CODINGOPTION_ON: u16 = 0x10;
pub const MFX_CODINGOPTION_OFF: u16 = 0x20;

pub const MFX_IOPATTERN_IN_VIDEO_MEMORY: u16 = 0x01;
pub const MFX_IOPATTERN_IN_SYSTEM_MEMORY: u16 = 0x02;
pub const MFX_IOPATTERN_OUT_VIDEO_MEMORY: u16 = 0x10;
pub const MFX_IOPATTERN_OUT_SYSTEM_MEMORY: u16 = 0x20;

pub const MFX_FRAMETYPE_I: u16 = 0x0001;
pub const MFX_FRAMETYPE_P: u16 = 0x0002;
pub const MFX_FRAMETYPE_B: u16 = 0x0004;
pub const MFX_FRAMETYPE_REF: u16 = 0x0040;
pub const MFX_FRAMETYPE_IDR: u16 = 0x0080;

pub const MFX_SCENARIO_UNKNOWN: u16 = 0;
pub const MFX_SCENARIO_DISPLAY_REMOTING: u16 = 1;
pub const MFX_SCENARIO_REMOTE_GAMING: u16 = 8;

pub const MFX_B_REF_UNKNOWN: u16 = 0;
pub const MFX_B_REF_OFF: u16 = 1;

pub const MFX_REFRESH_NO: u16 = 0;
pub const MFX_REFRESH_VERTICAL: u16 = 1;
pub const MFX_REFRESH_HORIZONTAL: u16 = 2;
pub const MFX_REFRESH_SLICE: u16 = 3;

pub const MFX_GOP_CLOSED: u16 = 1;
pub const MFX_GOP_STRICT: u16 = 2;

pub const MFX_EXTBUFF_CODING_OPTION: u32 = make_fourcc(b'C', b'D', b'O', b'P');
pub const MFX_EXTBUFF_CODING_OPTION2: u32 = make_fourcc(b'C', b'D', b'O', b'2');
pub const MFX_EXTBUFF_CODING_OPTION3: u32 = make_fourcc(b'C', b'D', b'O', b'3');

// ===========================================================================================
// Common structs (mfxdefs.h, mfxcommon.h)
// ===========================================================================================

#[repr(C)]
#[derive(Clone, Copy)]
pub union mfxVariantData {
    pub U8: u8,
    pub I8: i8,
    pub U16: u16,
    pub I16: i16,
    pub U32: u32,
    pub I32: i32,
    pub U64: u64,
    pub I64: i64,
    pub F32: f32,
    pub F64: f64,
    pub FP16: u16,
    pub Ptr: mfxHDL,
}

/// mfxVariant (mfxdefs.h, `MFX_PACK_BEGIN_STRUCT_W_PTR` = pack(8); natural on x64).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxVariant {
    pub Version: mfxStructVersion,
    pub Type: mfxVariantType,
    pub Data: mfxVariantData,
}

/// mfxExtBuffer (mfxcommon.h).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct mfxExtBuffer {
    pub BufferId: mfxU32,
    pub BufferSz: mfxU32,
}

/// mfxBitstream (mfxcommon.h, pack(8)). Leading union modeled as the `reserved[6]` variant; we
/// only use Data/DataOffset/DataLength/MaxLength/PicStruct/FrameType.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxBitstream {
    pub reserved_union: [mfxU32; 6],
    pub DecodeTimeStamp: mfxI64,
    pub TimeStamp: mfxU64,
    pub Data: *mut mfxU8,
    pub DataOffset: mfxU32,
    pub DataLength: mfxU32,
    pub MaxLength: mfxU32,
    pub PicStruct: mfxU16,
    pub FrameType: mfxU16,
    pub DataFlag: mfxU16,
    pub reserved2: mfxU16,
}

// ===========================================================================================
// Frame structs (mfxstructures.h)
// ===========================================================================================

/// mfxFrameId (pack(4)). Union {{DependencyId,QualityId}|{ViewId}} flattened (ViewId == DependencyId).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct mfxFrameId {
    pub TemporalId: mfxU16,
    pub PriorityId: mfxU16,
    pub DependencyId: mfxU16,
    pub QualityId: mfxU16,
}

/// mfxFrameInfo (pack(4); 68 bytes). The frame/buffer union is modeled as the frame-params
/// variant (Width/Height/Crop*), which is layout-identical for all video (non-P8) formats.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct mfxFrameInfo {
    pub reserved: [mfxU32; 4],
    pub ChannelId: mfxU16,
    pub BitDepthLuma: mfxU16,
    pub BitDepthChroma: mfxU16,
    pub Shift: mfxU16,
    pub FrameId: mfxFrameId,
    pub FourCC: mfxU32,
    pub Width: mfxU16,
    pub Height: mfxU16,
    pub CropX: mfxU16,
    pub CropY: mfxU16,
    pub CropW: mfxU16,
    pub CropH: mfxU16,
    pub FrameRateExtN: mfxU32,
    pub FrameRateExtD: mfxU32,
    pub reserved3: mfxU16,
    pub AspectRatioW: mfxU16,
    pub AspectRatioH: mfxU16,
    pub PicStruct: mfxU16,
    pub ChromaFormat: mfxU16,
    pub reserved2: mfxU16,
}

/// mfxFrameData (pack(8)). Pointer unions flattened to their first member (overlapping aliases).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxFrameData {
    pub ExtParam_or_reserved2: mfxU64, // union { mfxExtBuffer** ExtParam; mfxU64 reserved2; }
    pub NumExtParam: mfxU16,
    pub reserved: [mfxU16; 9],
    pub MemType: mfxU16,
    pub PitchHigh: mfxU16,
    pub TimeStamp: mfxU64,
    pub FrameOrder: mfxU32,
    pub Locked: mfxU16,
    pub Pitch: mfxU16, // union { Pitch; PitchLow; }
    pub Y: *mut mfxU8, // union { Y; Y16; R; }
    pub UV: *mut mfxU8, // union { UV; ...; U; Cb; G; }
    pub Cr: *mut mfxU8, // union { Cr; V; B; ... } -- for RGB4, B == base of BGRA
    pub A: *mut mfxU8,
    pub MemId: mfxMemId,
    pub Corrupted: mfxU16,
    pub DataFlag: mfxU16,
}

/// mfxFrameSurface1 (pack(8)). Leading union {{FrameInterface*}|{u32[2]}} modeled as the pointer.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxFrameSurface1 {
    pub FrameInterface: *mut mfxFrameSurfaceInterface,
    pub Version: mfxStructVersion,
    pub reserved1: [mfxU16; 3],
    pub Info: mfxFrameInfo,
    pub Data: mfxFrameData,
}

/// mfxFrameSurfaceInterface vtable (mfxstructures.h). Only the entries up through `Synchronize`
/// are called; the trailing entries are kept for slot-correct layout (offsets read from a
/// runtime-owned pointer, so only the leading offsets must match — they do).
#[repr(C)]
pub struct mfxFrameSurfaceInterface {
    pub Context: mfxHDL,
    pub Version: mfxStructVersion,
    pub reserved1: [mfxU16; 3],
    pub AddRef: Option<unsafe extern "C" fn(surface: *mut mfxFrameSurface1) -> mfxStatus>,
    pub Release: Option<unsafe extern "C" fn(surface: *mut mfxFrameSurface1) -> mfxStatus>,
    pub GetRefCounter:
        Option<unsafe extern "C" fn(surface: *mut mfxFrameSurface1, counter: *mut mfxU32) -> mfxStatus>,
    pub Map: Option<unsafe extern "C" fn(surface: *mut mfxFrameSurface1, flags: mfxU32) -> mfxStatus>,
    pub Unmap: Option<unsafe extern "C" fn(surface: *mut mfxFrameSurface1) -> mfxStatus>,
    pub GetNativeHandle: Option<
        unsafe extern "C" fn(
            surface: *mut mfxFrameSurface1,
            resource: *mut mfxHDL,
            resource_type: *mut mfxResourceType,
        ) -> mfxStatus,
    >,
    pub GetDeviceHandle: Option<
        unsafe extern "C" fn(
            surface: *mut mfxFrameSurface1,
            device_handle: *mut mfxHDL,
            device_type: *mut mfxHandleType,
        ) -> mfxStatus,
    >,
    pub Synchronize:
        Option<unsafe extern "C" fn(surface: *mut mfxFrameSurface1, wait: mfxU32) -> mfxStatus>,
    pub OnComplete: *mut c_void,
    pub QueryInterface: *mut c_void,
    pub Export: *mut c_void,
    pub reserved2: [mfxHDL; 2],
}

// ===========================================================================================
// mfxInfoMFX / mfxInfoVPP / mfxVideoParam (mfxstructures.h)
// ===========================================================================================

/// mfxInfoMFX (pack(4); 136 bytes). The big trailing union is modeled as its Encoding-Options
/// variant (13 × u16 = 26 bytes, exactly the union size). Rate-control sub-unions are flattened:
/// `InitialDelayInKB`(=QPI), `TargetKbps`(=QPP/ICQQuality), `MaxKbps`(=QPB/Convergence).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxInfoMFX {
    pub reserved: [mfxU32; 7],
    pub LowPower: mfxU16,
    pub BRCParamMultiplier: mfxU16,
    pub FrameInfo: mfxFrameInfo,
    pub CodecId: mfxU32,
    pub CodecProfile: mfxU16,
    pub CodecLevel: mfxU16,
    pub NumThread: mfxU16,
    // --- Encoding Options union variant ---
    pub TargetUsage: mfxU16,
    pub GopPicSize: mfxU16,
    pub GopRefDist: mfxU16,
    pub GopOptFlag: mfxU16,
    pub IdrInterval: mfxU16,
    pub RateControlMethod: mfxU16,
    pub InitialDelayInKB: mfxU16, // union: InitialDelayInKB | QPI | Accuracy
    pub BufferSizeInKB: mfxU16,
    pub TargetKbps: mfxU16, // union: TargetKbps | QPP | ICQQuality
    pub MaxKbps: mfxU16,    // union: MaxKbps | QPB | Convergence
    pub NumSlice: mfxU16,
    pub NumRefFrame: mfxU16,
    pub EncodedOrder: mfxU16,
}

/// mfxInfoVPP (pack(4); 168 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxInfoVPP {
    pub reserved: [mfxU32; 8],
    pub In: mfxFrameInfo,
    pub Out: mfxFrameInfo,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union mfxInfoUnion {
    pub mfx: mfxInfoMFX,
    pub vpp: mfxInfoVPP,
}

/// mfxVideoParam (pack(8)). The `mfx`/`vpp` union is sized for the larger `mfxInfoVPP` (168 B).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxVideoParam {
    pub AllocId: mfxU32,
    pub reserved: [mfxU32; 2],
    pub reserved3: mfxU16,
    pub AsyncDepth: mfxU16,
    pub info: mfxInfoUnion,
    pub Protected: mfxU16,
    pub IOPattern: mfxU16,
    pub ExtParam: *mut *mut mfxExtBuffer,
    pub NumExtParam: mfxU16,
    pub reserved2: mfxU16,
}

// ===========================================================================================
// Coding-option ext buffers (mfxstructures.h). Field order reproduced verbatim; trailing
// `reserved[]` make sizeof() match what the runtime validates against the BufferId.
// ===========================================================================================

#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxI16Pair {
    pub x: mfxI16,
    pub y: mfxI16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxExtCodingOption {
    pub Header: mfxExtBuffer,
    pub reserved1: mfxU16,
    pub RateDistortionOpt: mfxU16,
    pub MECostType: mfxU16,
    pub MESearchType: mfxU16,
    pub MVSearchWindow: mfxI16Pair,
    pub EndOfSequence: mfxU16, // deprecated
    pub FramePicture: mfxU16,
    pub CAVLC: mfxU16,
    pub reserved2: [mfxU16; 2],
    pub RecoveryPointSEI: mfxU16,
    pub ViewOutput: mfxU16,
    pub NalHrdConformance: mfxU16,
    pub SingleSeiNalUnit: mfxU16,
    pub VuiVclHrdParameters: mfxU16,
    pub RefPicListReordering: mfxU16,
    pub ResetRefList: mfxU16,
    pub RefPicMarkRep: mfxU16,
    pub FieldOutput: mfxU16,
    pub IntraPredBlockSize: mfxU16,
    pub InterPredBlockSize: mfxU16,
    pub MVPrecision: mfxU16,
    pub MaxDecFrameBuffering: mfxU16,
    pub AUDelimiter: mfxU16,
    pub EndOfStream: mfxU16, // deprecated
    pub PicTimingSEI: mfxU16,
    pub VuiNalHrdParameters: mfxU16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxExtCodingOption2 {
    pub Header: mfxExtBuffer,
    pub IntRefType: mfxU16,
    pub IntRefCycleSize: mfxU16,
    pub IntRefQPDelta: mfxI16,
    pub MaxFrameSize: mfxU32,
    pub MaxSliceSize: mfxU32,
    pub BitrateLimit: mfxU16, // deprecated
    pub MBBRC: mfxU16,
    pub ExtBRC: mfxU16,
    pub LookAheadDepth: mfxU16,
    pub Trellis: mfxU16,
    pub RepeatPPS: mfxU16,
    pub BRefType: mfxU16,
    pub AdaptiveI: mfxU16,
    pub AdaptiveB: mfxU16,
    pub LookAheadDS: mfxU16,
    pub NumMbPerSlice: mfxU16,
    pub SkipFrame: mfxU16,
    pub MinQPI: mfxU8,
    pub MaxQPI: mfxU8,
    pub MinQPP: mfxU8,
    pub MaxQPP: mfxU8,
    pub MinQPB: mfxU8,
    pub MaxQPB: mfxU8,
    pub FixedFrameRate: mfxU16,
    pub DisableDeblockingIdc: mfxU16,
    pub DisableVUI: mfxU16,
    pub BufferingPeriodSEI: mfxU16,
    pub EnableMAD: mfxU16,
    pub UseRawRef: mfxU16,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxExtCodingOption3 {
    pub Header: mfxExtBuffer,
    pub NumSliceI: mfxU16,
    pub NumSliceP: mfxU16,
    pub NumSliceB: mfxU16,
    pub WinBRCMaxAvgKbps: mfxU16,
    pub WinBRCSize: mfxU16,
    pub QVBRQuality: mfxU16,
    pub EnableMBQP: mfxU16,
    pub IntRefCycleDist: mfxU16,
    pub DirectBiasAdjustment: mfxU16,
    pub GlobalMotionBiasAdjustment: mfxU16,
    pub MVCostScalingFactor: mfxU16,
    pub MBDisableSkipMap: mfxU16,
    pub WeightedPred: mfxU16,
    pub WeightedBiPred: mfxU16,
    pub AspectRatioInfoPresent: mfxU16,
    pub OverscanInfoPresent: mfxU16,
    pub OverscanAppropriate: mfxU16,
    pub TimingInfoPresent: mfxU16,
    pub BitstreamRestriction: mfxU16,
    pub LowDelayHrd: mfxU16,
    pub MotionVectorsOverPicBoundaries: mfxU16,
    pub reserved1: [mfxU16; 2],
    pub ScenarioInfo: mfxU16,
    pub ContentInfo: mfxU16,
    pub PRefType: mfxU16,
    pub FadeDetection: mfxU16,
    pub reserved2: [mfxU16; 2],
    pub GPB: mfxU16,
    pub MaxFrameSizeI: mfxU32,
    pub MaxFrameSizeP: mfxU32,
    pub reserved3: [mfxU32; 3],
    pub EnableQPOffset: mfxU16,
    pub QPOffset: [mfxI16; 8],
    pub NumRefActiveP: [mfxU16; 8],
    pub NumRefActiveBL0: [mfxU16; 8],
    pub NumRefActiveBL1: [mfxU16; 8],
    pub reserved6: mfxU16,
    pub TransformSkip: mfxU16,
    pub TargetChromaFormatPlus1: mfxU16,
    pub TargetBitDepthLuma: mfxU16,
    pub TargetBitDepthChroma: mfxU16,
    pub BRCPanicMode: mfxU16,
    pub LowDelayBRC: mfxU16,
    pub EnableMBForceIntra: mfxU16,
    pub AdaptiveMaxFrameSize: mfxU16,
    pub RepartitionCheckEnable: mfxU16,
    pub reserved5: [mfxU16; 3],
    pub EncodedUnitsInfo: mfxU16,
    pub EnableNalUnitType: mfxU16,
    pub AdaptiveLTR: mfxU16, // union { ExtBrcAdaptiveLTR; AdaptiveLTR; }
    pub AdaptiveCQM: mfxU16,
    pub AdaptiveRef: mfxU16,
    pub reserved: [mfxU16; 161],
}

/// mfxEncodeCtrl (mfxstructures.h, pack(8)).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct mfxEncodeCtrl {
    pub Header: mfxExtBuffer,
    pub reserved: [mfxU32; 4],
    pub reserved1: mfxU16,
    pub MfxNalUnitType: mfxU16,
    pub SkipFrame: mfxU16,
    pub QP: mfxU16,
    pub FrameType: mfxU16,
    pub NumExtParam: mfxU16,
    pub NumPayload: mfxU16,
    pub reserved2: mfxU16,
    pub ExtParam: *mut *mut mfxExtBuffer,
}

// ===========================================================================================
// Dynamic dispatcher loader (libvpl.dll)
// ===========================================================================================

type FnMFXLoad = unsafe extern "C" fn() -> mfxLoader;
type FnMFXUnload = unsafe extern "C" fn(loader: mfxLoader);
type FnMFXCreateConfig = unsafe extern "C" fn(loader: mfxLoader) -> mfxConfig;
type FnMFXSetConfigFilterProperty =
    unsafe extern "C" fn(config: mfxConfig, name: *const mfxU8, value: mfxVariant) -> mfxStatus;
type FnMFXCreateSession =
    unsafe extern "C" fn(loader: mfxLoader, i: mfxU32, session: *mut mfxSession) -> mfxStatus;
type FnMFXClose = unsafe extern "C" fn(session: mfxSession) -> mfxStatus;
type FnMFXQueryVersion = unsafe extern "C" fn(session: mfxSession, version: *mut mfxU32) -> mfxStatus;
type FnMFXQueryIMPL = unsafe extern "C" fn(session: mfxSession, impl_: *mut mfxI32) -> mfxStatus;

type FnSetHandle =
    unsafe extern "C" fn(session: mfxSession, ty: mfxHandleType, hdl: mfxHDL) -> mfxStatus;
type FnSyncOperation =
    unsafe extern "C" fn(session: mfxSession, syncp: mfxSyncPoint, wait: mfxU32) -> mfxStatus;

type FnEncodeInit = unsafe extern "C" fn(session: mfxSession, par: *mut mfxVideoParam) -> mfxStatus;
type FnEncodeQuery = unsafe extern "C" fn(
    session: mfxSession,
    in_: *mut mfxVideoParam,
    out: *mut mfxVideoParam,
) -> mfxStatus;
type FnEncodeFrameAsync = unsafe extern "C" fn(
    session: mfxSession,
    ctrl: *mut mfxEncodeCtrl,
    surface: *mut mfxFrameSurface1,
    bs: *mut mfxBitstream,
    syncp: *mut mfxSyncPoint,
) -> mfxStatus;

type FnVppInit = unsafe extern "C" fn(session: mfxSession, par: *mut mfxVideoParam) -> mfxStatus;
type FnVppQuery = unsafe extern "C" fn(
    session: mfxSession,
    in_: *mut mfxVideoParam,
    out: *mut mfxVideoParam,
) -> mfxStatus;
type FnRunFrameVPPAsync = unsafe extern "C" fn(
    session: mfxSession,
    in_: *mut mfxFrameSurface1,
    out: *mut mfxFrameSurface1,
    aux: *mut c_void,
    syncp: *mut mfxSyncPoint,
) -> mfxStatus;

type FnGetSurface =
    unsafe extern "C" fn(session: mfxSession, surface: *mut *mut mfxFrameSurface1) -> mfxStatus;
type FnSessionOnly = unsafe extern "C" fn(session: mfxSession) -> mfxStatus;

/// Resolved oneVPL dispatcher entry points. The `Library` is kept alive for the lifetime of the
/// process (leaked via the field) so the function pointers stay valid.
pub struct Vpl {
    _lib: libloading::Library,
    pub MFXLoad: FnMFXLoad,
    pub MFXUnload: FnMFXUnload,
    pub MFXCreateConfig: FnMFXCreateConfig,
    pub MFXSetConfigFilterProperty: FnMFXSetConfigFilterProperty,
    pub MFXCreateSession: FnMFXCreateSession,
    pub MFXClose: FnMFXClose,
    pub MFXQueryVersion: FnMFXQueryVersion,
    pub MFXQueryIMPL: FnMFXQueryIMPL,
    pub MFXVideoCORE_SetHandle: FnSetHandle,
    pub MFXVideoCORE_SyncOperation: FnSyncOperation,
    pub MFXVideoENCODE_Init: FnEncodeInit,
    pub MFXVideoENCODE_Query: FnEncodeQuery,
    pub MFXVideoENCODE_Reset: FnEncodeInit,
    pub MFXVideoENCODE_GetVideoParam: FnEncodeInit,
    pub MFXVideoENCODE_Close: FnSessionOnly,
    pub MFXVideoENCODE_EncodeFrameAsync: FnEncodeFrameAsync,
    pub MFXVideoVPP_Init: FnVppInit,
    pub MFXVideoVPP_Query: FnVppQuery,
    pub MFXVideoVPP_Reset: FnVppInit,
    pub MFXVideoVPP_Close: FnSessionOnly,
    pub MFXVideoVPP_RunFrameVPPAsync: FnRunFrameVPPAsync,
    pub MFXMemory_GetSurfaceForVPP: FnGetSurface,
    pub MFXMemory_GetSurfaceForVPPOut: FnGetSurface,
    pub MFXMemory_GetSurfaceForEncode: FnGetSurface,
}

unsafe impl Send for Vpl {}
unsafe impl Sync for Vpl {}

macro_rules! resolve {
    ($lib:expr, $name:literal) => {{
        // `Symbol` borrows the library; deref copies out the (Copy) fn pointer, which stays valid
        // as long as the library is alive — and we move it into `_lib` below.
        let sym: libloading::Symbol<_> = unsafe { $lib.get(concat!($name, "\0").as_bytes()) }
            .with_context(|| format!("resolving {} from libvpl.dll", $name))?;
        *sym
    }};
}

impl Vpl {
    /// Dynamically load the oneVPL dispatcher and resolve all entry points used by the encoder.
    pub fn load() -> Result<Self> {
        let lib = unsafe { libloading::Library::new("libvpl.dll") }
            .context("loading libvpl.dll (Intel oneVPL dispatcher; ships with the Intel GPU driver)")?;

        // Resolve the symbols while the library is borrowed, then move the library into the struct.
        // `Symbol::into_raw` keeps the pointer valid as long as the library is alive.
        let v = Self {
            MFXLoad: resolve!(lib, "MFXLoad"),
            MFXUnload: resolve!(lib, "MFXUnload"),
            MFXCreateConfig: resolve!(lib, "MFXCreateConfig"),
            MFXSetConfigFilterProperty: resolve!(lib, "MFXSetConfigFilterProperty"),
            MFXCreateSession: resolve!(lib, "MFXCreateSession"),
            MFXClose: resolve!(lib, "MFXClose"),
            MFXQueryVersion: resolve!(lib, "MFXQueryVersion"),
            MFXQueryIMPL: resolve!(lib, "MFXQueryIMPL"),
            MFXVideoCORE_SetHandle: resolve!(lib, "MFXVideoCORE_SetHandle"),
            MFXVideoCORE_SyncOperation: resolve!(lib, "MFXVideoCORE_SyncOperation"),
            MFXVideoENCODE_Init: resolve!(lib, "MFXVideoENCODE_Init"),
            MFXVideoENCODE_Query: resolve!(lib, "MFXVideoENCODE_Query"),
            MFXVideoENCODE_Reset: resolve!(lib, "MFXVideoENCODE_Reset"),
            MFXVideoENCODE_GetVideoParam: resolve!(lib, "MFXVideoENCODE_GetVideoParam"),
            MFXVideoENCODE_Close: resolve!(lib, "MFXVideoENCODE_Close"),
            MFXVideoENCODE_EncodeFrameAsync: resolve!(lib, "MFXVideoENCODE_EncodeFrameAsync"),
            MFXVideoVPP_Init: resolve!(lib, "MFXVideoVPP_Init"),
            MFXVideoVPP_Query: resolve!(lib, "MFXVideoVPP_Query"),
            MFXVideoVPP_Reset: resolve!(lib, "MFXVideoVPP_Reset"),
            MFXVideoVPP_Close: resolve!(lib, "MFXVideoVPP_Close"),
            MFXVideoVPP_RunFrameVPPAsync: resolve!(lib, "MFXVideoVPP_RunFrameVPPAsync"),
            MFXMemory_GetSurfaceForVPP: resolve!(lib, "MFXMemory_GetSurfaceForVPP"),
            MFXMemory_GetSurfaceForVPPOut: resolve!(lib, "MFXMemory_GetSurfaceForVPPOut"),
            MFXMemory_GetSurfaceForEncode: resolve!(lib, "MFXMemory_GetSurfaceForEncode"),
            _lib: lib,
        };
        Ok(v)
    }
}

/// Map an `mfxStatus` to a `Result`, treating positive warnings as success.
pub fn check(status: mfxStatus, what: &str) -> Result<()> {
    if status >= MFX_ERR_NONE {
        Ok(())
    } else {
        anyhow::bail!("{what} failed: mfxStatus={status}");
    }
}

/// Strict variant: any non-zero status is an error.
pub fn check_strict(status: mfxStatus, what: &str) -> Result<()> {
    if status == MFX_ERR_NONE {
        Ok(())
    } else {
        anyhow::bail!("{what} failed: mfxStatus={status}");
    }
}
