//! Scalars, plain structs, enum values and GUIDs of the AMF C ABI.
//!
//! Every number here is measured, not read off the headers by eye — see
//! `windows_utils/streamer/test/amd_layout.rs` and `src-tauri/tools/amf_layout_gen.c`.

use std::ffi::c_void;

// ===========================================================================================
// Scalars (core/Platform.h)
// ===========================================================================================

pub type amf_int32 = i32;
pub type amf_uint32 = u32;
pub type amf_int64 = i64;
pub type amf_uint64 = u64;
pub type amf_size = usize;
pub type amf_float = f32;
pub type amf_double = f64;

/// `amf_bool` is `amf_uint8` in the C branch and `bool` in C++; one byte either way on MSVC.
pub type amf_bool = u8;

/// `long` is 32-bit on Windows (LLP64). Typing this `i64` reads four bytes of register garbage
/// out of every `Acquire`/`Release`.
pub type amf_long = i32;

/// Presentation timestamp, in units of 100 ns.
pub type amf_pts = i64;

/// One second in `amf_pts` ticks (`AMF_SECOND`).
pub const AMF_SECOND: amf_pts = 10_000_000;

// ===========================================================================================
// Version packing (core/Version.h)
// ===========================================================================================

pub const fn make_full_version(major: u64, minor: u64, release: u64, build: u64) -> amf_uint64 {
    (major << 48) | (minor << 32) | (release << 16) | build
}

pub const fn version_major(v: amf_uint64) -> u64 {
    (v >> 48) & 0xFFFF
}
pub const fn version_minor(v: amf_uint64) -> u64 {
    (v >> 32) & 0xFFFF
}
pub const fn version_release(v: amf_uint64) -> u64 {
    (v >> 16) & 0xFFFF
}
pub const fn version_build(v: amf_uint64) -> u64 {
    v & 0xFFFF
}

/// `AMF_FULL_VERSION` of the headers vendored in `resources/amf-headers` (1.5.2.0).
/// Recorded for provenance only — see `AMFINIT_VERSION` for what we actually ask for.
pub const HEADERS_FULL_VERSION: amf_uint64 = 281_496_451_678_208;

/// The version handed to `AMFInit`. Pinned **low** on purpose: AMF's compatibility contract is
/// that a newer runtime serves an older client, never the reverse, so asking a 2023-era driver
/// for 1.5.2 would fail `AMFInit` on hardware whose VCN encodes perfectly well. 1.4.30 is
/// Adrenalin 23.5.2. Every optional feature is probed at runtime instead.
pub const AMFINIT_VERSION: amf_uint64 = make_full_version(1, 4, 30, 0);

// ===========================================================================================
// Plain structs (core/Platform.h, core/Interface.h, core/Variant.h)
// ===========================================================================================

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFGuid {
    pub data1: amf_uint32,
    pub data2: u16,
    pub data3: u16,
    pub data41: u8,
    pub data42: u8,
    pub data43: u8,
    pub data44: u8,
    pub data45: u8,
    pub data46: u8,
    pub data47: u8,
    pub data48: u8,
}

impl AMFGuid {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        data1: u32,
        data2: u16,
        data3: u16,
        b0: u8,
        b1: u8,
        b2: u8,
        b3: u8,
        b4: u8,
        b5: u8,
        b6: u8,
        b7: u8,
    ) -> Self {
        Self {
            data1,
            data2,
            data3,
            data41: b0,
            data42: b1,
            data43: b2,
            data44: b3,
            data45: b4,
            data46: b5,
            data47: b6,
            data48: b7,
        }
    }
}

/// `AMFSize` is **signed**; `AMFRate`/`AMFRatio` are **unsigned**. All three are eight bytes of
/// two 32-bit ints, so only the variant type tag distinguishes them — passing a `Rate` where a
/// `Size` is wanted compiles and is rejected by AMF at runtime.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFSize {
    pub width: amf_int32,
    pub height: amf_int32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFRate {
    pub num: amf_uint32,
    pub den: amf_uint32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFRatio {
    pub num: amf_uint32,
    pub den: amf_uint32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFPoint {
    pub x: amf_int32,
    pub y: amf_int32,
}

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFRect {
    pub left: amf_int32,
    pub top: amf_int32,
    pub right: amf_int32,
    pub bottom: amf_int32,
}

#[repr(C, packed(1))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AMFColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

pub type AMF_VARIANT_TYPE = amf_int32;
pub const AMF_VARIANT_EMPTY: AMF_VARIANT_TYPE = 0;
pub const AMF_VARIANT_BOOL: AMF_VARIANT_TYPE = 1;
pub const AMF_VARIANT_INT64: AMF_VARIANT_TYPE = 2;
pub const AMF_VARIANT_DOUBLE: AMF_VARIANT_TYPE = 3;
pub const AMF_VARIANT_RECT: AMF_VARIANT_TYPE = 4;
pub const AMF_VARIANT_SIZE: AMF_VARIANT_TYPE = 5;
pub const AMF_VARIANT_POINT: AMF_VARIANT_TYPE = 6;
pub const AMF_VARIANT_RATE: AMF_VARIANT_TYPE = 7;
pub const AMF_VARIANT_RATIO: AMF_VARIANT_TYPE = 8;
pub const AMF_VARIANT_COLOR: AMF_VARIANT_TYPE = 9;
pub const AMF_VARIANT_STRING: AMF_VARIANT_TYPE = 10;
pub const AMF_VARIANT_WSTRING: AMF_VARIANT_TYPE = 11;
pub const AMF_VARIANT_INTERFACE: AMF_VARIANT_TYPE = 12;
pub const AMF_VARIANT_FLOAT: AMF_VARIANT_TYPE = 13;

#[repr(C)]
#[derive(Clone, Copy)]
pub union AMFVariantValue {
    pub boolValue: amf_bool,
    pub int64Value: amf_int64,
    pub doubleValue: amf_double,
    pub floatValue: amf_float,
    pub stringValue: *mut i8,
    pub wstringValue: *mut u16,
    pub pInterface: *mut c_void,
    pub rectValue: AMFRect,
    pub sizeValue: AMFSize,
    pub pointValue: AMFPoint,
    pub rateValue: AMFRate,
    pub ratioValue: AMFRatio,
    pub colorValue: AMFColor,
}

/// The 24-byte property value. `type` is four bytes at offset 0 followed by four bytes of
/// padding, because the union is 8-aligned; `_pad` models that explicitly rather than leaving it
/// to `repr(C)` so the layout test can assert the union starts at 8.
///
/// `SetProperty` takes this **by value**. On x64 Windows a 24-byte aggregate is passed by hidden
/// reference, which `extern "system" fn(..., AMFVariantStruct)` over a `#[repr(C)]` type
/// reproduces exactly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct AMFVariantStruct {
    pub r#type: AMF_VARIANT_TYPE,
    pub _pad: amf_uint32,
    pub value: AMFVariantValue,
}

impl AMFVariantStruct {
    /// `AMFVariantInit` is `static AMF_INLINE` in the header — not exported from the DLL — so it
    /// is reimplemented here. Every variant this backend writes is Bool / Int64 / Size / Rate,
    /// none of which owns memory, so "init" is a zeroed struct and "clear" is a no-op. Only
    /// STRING / WSTRING (`CoTaskMemFree`) and INTERFACE (`Release`) would need cleanup, and the
    /// encoder never reads one.
    pub const fn empty() -> Self {
        Self {
            r#type: AMF_VARIANT_EMPTY,
            _pad: 0,
            value: AMFVariantValue { int64Value: 0 },
        }
    }

    pub const fn bool(v: bool) -> Self {
        Self {
            r#type: AMF_VARIANT_BOOL,
            _pad: 0,
            value: AMFVariantValue {
                boolValue: v as amf_bool,
            },
        }
    }

    pub const fn int64(v: amf_int64) -> Self {
        Self {
            r#type: AMF_VARIANT_INT64,
            _pad: 0,
            value: AMFVariantValue { int64Value: v },
        }
    }

    pub const fn size(width: amf_int32, height: amf_int32) -> Self {
        Self {
            r#type: AMF_VARIANT_SIZE,
            _pad: 0,
            value: AMFVariantValue {
                sizeValue: AMFSize { width, height },
            },
        }
    }

    pub const fn rate(num: amf_uint32, den: amf_uint32) -> Self {
        Self {
            r#type: AMF_VARIANT_RATE,
            _pad: 0,
            value: AMFVariantValue {
                rateValue: AMFRate { num, den },
            },
        }
    }

    /// Reads the payload as an `amf_int64` when the tag says so. `Bool` is read through the same
    /// accessor because AMF returns booleans as `AMF_VARIANT_BOOL` with a one-byte payload in a
    /// zeroed union.
    pub fn as_i64(&self) -> Option<amf_int64> {
        // SAFETY: the union member is chosen by the tag, which is what AMF sets on return.
        unsafe {
            match self.r#type {
                AMF_VARIANT_INT64 => Some(self.value.int64Value),
                AMF_VARIANT_BOOL => Some(self.value.boolValue as amf_int64),
                AMF_VARIANT_DOUBLE => Some(self.value.doubleValue as amf_int64),
                _ => None,
            }
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        self.as_i64().map(|v| v != 0)
    }

    /// The raw interface pointer of an `AMF_VARIANT_INTERFACE`. The caller owns one reference
    /// and must `Release` it.
    pub fn as_interface(&self) -> Option<*mut c_void> {
        if self.r#type != AMF_VARIANT_INTERFACE {
            return None;
        }
        // SAFETY: tag-checked immediately above.
        let p = unsafe { self.value.pInterface };
        if p.is_null() {
            None
        } else {
            Some(p)
        }
    }
}

// ===========================================================================================
// AMF_RESULT (core/Result.h)
//
// A plain sequential C enum with no explicit values, so these are positional and must be
// regenerated if AMD ever inserts a member. Bound as `i32` rather than a Rust enum: a value
// outside the declared set is UB in a `#[repr(i32)] enum` and AMF is free to return one.
// ===========================================================================================

pub type AMF_RESULT = amf_int32;

pub const AMF_OK: AMF_RESULT = 0;
pub const AMF_FAIL: AMF_RESULT = 1;
pub const AMF_UNEXPECTED: AMF_RESULT = 2;
pub const AMF_ACCESS_DENIED: AMF_RESULT = 3;
pub const AMF_INVALID_ARG: AMF_RESULT = 4;
pub const AMF_OUT_OF_RANGE: AMF_RESULT = 5;
pub const AMF_OUT_OF_MEMORY: AMF_RESULT = 6;
pub const AMF_INVALID_POINTER: AMF_RESULT = 7;
pub const AMF_NO_INTERFACE: AMF_RESULT = 8;
pub const AMF_NOT_IMPLEMENTED: AMF_RESULT = 9;
pub const AMF_NOT_SUPPORTED: AMF_RESULT = 10;
pub const AMF_NOT_FOUND: AMF_RESULT = 11;
pub const AMF_ALREADY_INITIALIZED: AMF_RESULT = 12;
pub const AMF_NOT_INITIALIZED: AMF_RESULT = 13;
pub const AMF_INVALID_FORMAT: AMF_RESULT = 14;
pub const AMF_WRONG_STATE: AMF_RESULT = 15;
pub const AMF_FILE_NOT_OPEN: AMF_RESULT = 16;
pub const AMF_NO_DEVICE: AMF_RESULT = 17;
pub const AMF_DIRECTX_FAILED: AMF_RESULT = 18;
pub const AMF_OPENCL_FAILED: AMF_RESULT = 19;
pub const AMF_EOF: AMF_RESULT = 23;
pub const AMF_REPEAT: AMF_RESULT = 24;
pub const AMF_INPUT_FULL: AMF_RESULT = 25;
pub const AMF_RESOLUTION_CHANGED: AMF_RESULT = 26;
pub const AMF_RESOLUTION_UPDATED: AMF_RESULT = 27;
pub const AMF_INVALID_DATA_TYPE: AMF_RESULT = 28;
pub const AMF_INVALID_RESOLUTION: AMF_RESULT = 29;
pub const AMF_CODEC_NOT_SUPPORTED: AMF_RESULT = 30;
pub const AMF_SURFACE_FORMAT_NOT_SUPPORTED: AMF_RESULT = 31;
pub const AMF_SURFACE_MUST_BE_SHARED: AMF_RESULT = 32;
pub const AMF_ENCODER_NOT_PRESENT: AMF_RESULT = 36;
pub const AMF_NEED_MORE_INPUT: AMF_RESULT = 44;
pub const AMF_VULKAN_FAILED: AMF_RESULT = 45;

/// A readable name for an `AMF_RESULT`, used when `AMFTrace::GetResultText` is unavailable.
pub fn result_name(r: AMF_RESULT) -> &'static str {
    match r {
        AMF_OK => "AMF_OK",
        AMF_FAIL => "AMF_FAIL",
        AMF_UNEXPECTED => "AMF_UNEXPECTED",
        AMF_ACCESS_DENIED => "AMF_ACCESS_DENIED",
        AMF_INVALID_ARG => "AMF_INVALID_ARG",
        AMF_OUT_OF_RANGE => "AMF_OUT_OF_RANGE",
        AMF_OUT_OF_MEMORY => "AMF_OUT_OF_MEMORY",
        AMF_INVALID_POINTER => "AMF_INVALID_POINTER",
        AMF_NO_INTERFACE => "AMF_NO_INTERFACE",
        AMF_NOT_IMPLEMENTED => "AMF_NOT_IMPLEMENTED",
        AMF_NOT_SUPPORTED => "AMF_NOT_SUPPORTED",
        AMF_NOT_FOUND => "AMF_NOT_FOUND",
        AMF_ALREADY_INITIALIZED => "AMF_ALREADY_INITIALIZED",
        AMF_NOT_INITIALIZED => "AMF_NOT_INITIALIZED",
        AMF_INVALID_FORMAT => "AMF_INVALID_FORMAT",
        AMF_WRONG_STATE => "AMF_WRONG_STATE",
        AMF_FILE_NOT_OPEN => "AMF_FILE_NOT_OPEN",
        AMF_NO_DEVICE => "AMF_NO_DEVICE",
        AMF_DIRECTX_FAILED => "AMF_DIRECTX_FAILED",
        AMF_OPENCL_FAILED => "AMF_OPENCL_FAILED",
        AMF_EOF => "AMF_EOF",
        AMF_REPEAT => "AMF_REPEAT",
        AMF_INPUT_FULL => "AMF_INPUT_FULL",
        AMF_RESOLUTION_CHANGED => "AMF_RESOLUTION_CHANGED",
        AMF_RESOLUTION_UPDATED => "AMF_RESOLUTION_UPDATED",
        AMF_INVALID_DATA_TYPE => "AMF_INVALID_DATA_TYPE",
        AMF_INVALID_RESOLUTION => "AMF_INVALID_RESOLUTION",
        AMF_CODEC_NOT_SUPPORTED => "AMF_CODEC_NOT_SUPPORTED",
        AMF_SURFACE_FORMAT_NOT_SUPPORTED => "AMF_SURFACE_FORMAT_NOT_SUPPORTED",
        AMF_SURFACE_MUST_BE_SHARED => "AMF_SURFACE_MUST_BE_SHARED",
        AMF_ENCODER_NOT_PRESENT => "AMF_ENCODER_NOT_PRESENT",
        AMF_NEED_MORE_INPUT => "AMF_NEED_MORE_INPUT",
        AMF_VULKAN_FAILED => "AMF_VULKAN_FAILED",
        _ => "AMF_RESULT(unknown)",
    }
}

// ===========================================================================================
// Core enums (core/Surface.h, core/Data.h, core/Context.h, components/ComponentCaps.h)
// ===========================================================================================

pub type AMF_SURFACE_FORMAT = amf_int32;
pub const AMF_SURFACE_UNKNOWN: AMF_SURFACE_FORMAT = 0;
pub const AMF_SURFACE_NV12: AMF_SURFACE_FORMAT = 1;
pub const AMF_SURFACE_YV12: AMF_SURFACE_FORMAT = 2;
pub const AMF_SURFACE_BGRA: AMF_SURFACE_FORMAT = 3;
pub const AMF_SURFACE_ARGB: AMF_SURFACE_FORMAT = 4;
pub const AMF_SURFACE_RGBA: AMF_SURFACE_FORMAT = 5;
pub const AMF_SURFACE_P010: AMF_SURFACE_FORMAT = 10;
pub const AMF_SURFACE_R10G10B10A2: AMF_SURFACE_FORMAT = 13;

pub fn surface_format_name(f: AMF_SURFACE_FORMAT) -> &'static str {
    match f {
        AMF_SURFACE_NV12 => "NV12",
        AMF_SURFACE_YV12 => "YV12",
        AMF_SURFACE_BGRA => "BGRA",
        AMF_SURFACE_ARGB => "ARGB",
        AMF_SURFACE_RGBA => "RGBA",
        AMF_SURFACE_P010 => "P010",
        AMF_SURFACE_R10G10B10A2 => "R10G10B10A2",
        _ => "unknown",
    }
}

pub type AMF_MEMORY_TYPE = amf_int32;
pub const AMF_MEMORY_UNKNOWN: AMF_MEMORY_TYPE = 0;
pub const AMF_MEMORY_HOST: AMF_MEMORY_TYPE = 1;
pub const AMF_MEMORY_DX9: AMF_MEMORY_TYPE = 2;
pub const AMF_MEMORY_DX11: AMF_MEMORY_TYPE = 3;
pub const AMF_MEMORY_OPENCL: AMF_MEMORY_TYPE = 4;
pub const AMF_MEMORY_VULKAN: AMF_MEMORY_TYPE = 10;
pub const AMF_MEMORY_DX12: AMF_MEMORY_TYPE = 11;

pub type AMF_DX_VERSION = amf_int32;
pub const AMF_DX9: AMF_DX_VERSION = 90;
pub const AMF_DX9_EX: AMF_DX_VERSION = 91;
pub const AMF_DX11_0: AMF_DX_VERSION = 110;
pub const AMF_DX11_1: AMF_DX_VERSION = 111;
pub const AMF_DX12: AMF_DX_VERSION = 120;

pub type AMF_DATA_TYPE = amf_int32;
pub const AMF_DATA_BUFFER: AMF_DATA_TYPE = 0;
pub const AMF_DATA_SURFACE: AMF_DATA_TYPE = 1;

pub type AMF_FRAME_TYPE = amf_int32;
pub const AMF_FRAME_UNKNOWN: AMF_FRAME_TYPE = -1;
pub const AMF_FRAME_PROGRESSIVE: AMF_FRAME_TYPE = 0;

pub type AMF_PLANE_TYPE = amf_int32;
pub const AMF_PLANE_UNKNOWN: AMF_PLANE_TYPE = 0;
pub const AMF_PLANE_PACKED: AMF_PLANE_TYPE = 1;
pub const AMF_PLANE_Y: AMF_PLANE_TYPE = 2;
pub const AMF_PLANE_UV: AMF_PLANE_TYPE = 3;

/// Note that `AMF_ACCEL_HARDWARE` is **0**, not a truthy "supported" flag: the "not supported"
/// case is the negative one.
pub type AMF_ACCELERATION_TYPE = amf_int32;
pub const AMF_ACCEL_NOT_SUPPORTED: AMF_ACCELERATION_TYPE = -1;
pub const AMF_ACCEL_HARDWARE: AMF_ACCELERATION_TYPE = 0;
pub const AMF_ACCEL_GPU: AMF_ACCELERATION_TYPE = 1;
pub const AMF_ACCEL_SOFTWARE: AMF_ACCELERATION_TYPE = 2;

pub fn acceleration_name(a: AMF_ACCELERATION_TYPE) -> &'static str {
    match a {
        AMF_ACCEL_NOT_SUPPORTED => "not-supported",
        AMF_ACCEL_HARDWARE => "hardware(VCN/VCE)",
        AMF_ACCEL_GPU => "gpu(shader)",
        AMF_ACCEL_SOFTWARE => "software(cpu)",
        _ => "unknown",
    }
}

// ===========================================================================================
// AVC encoder enums (components/VideoEncoderVCE.h)
//
// These are AVC's values. HEVC and AV1 renumber the "same" enums — AVC's CBR = 1 means
// LATENCY_CONSTRAINED_VBR on HEVC, AVC's SPEED = 1 is 10 on HEVC and 100 on AV1, and AV1 swaps
// ULTRA_LOW_LATENCY with LOW_LATENCY. Any future codec gets its own module, never these.
// ===========================================================================================

pub type AMF_VIDEO_ENCODER_USAGE = amf_int32;
pub const AMF_VIDEO_ENCODER_USAGE_TRANSCODING: AMF_VIDEO_ENCODER_USAGE = 0;
pub const AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY: AMF_VIDEO_ENCODER_USAGE = 1;
pub const AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY: AMF_VIDEO_ENCODER_USAGE = 2;
pub const AMF_VIDEO_ENCODER_USAGE_WEBCAM: AMF_VIDEO_ENCODER_USAGE = 3;
pub const AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY: AMF_VIDEO_ENCODER_USAGE = 4;
pub const AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY: AMF_VIDEO_ENCODER_USAGE = 5;

pub type AMF_VIDEO_ENCODER_PROFILE = amf_int32;
pub const AMF_VIDEO_ENCODER_PROFILE_UNKNOWN: AMF_VIDEO_ENCODER_PROFILE = 0;
pub const AMF_VIDEO_ENCODER_PROFILE_BASELINE: AMF_VIDEO_ENCODER_PROFILE = 66;
pub const AMF_VIDEO_ENCODER_PROFILE_MAIN: AMF_VIDEO_ENCODER_PROFILE = 77;
pub const AMF_VIDEO_ENCODER_PROFILE_HIGH: AMF_VIDEO_ENCODER_PROFILE = 100;
pub const AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE: AMF_VIDEO_ENCODER_PROFILE = 256;
pub const AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_HIGH: AMF_VIDEO_ENCODER_PROFILE = 257;

pub type AMF_H264_LEVEL = amf_int32;
pub const AMF_H264_LEVEL__3_1: AMF_H264_LEVEL = 31;
pub const AMF_H264_LEVEL__3_2: AMF_H264_LEVEL = 32;
pub const AMF_H264_LEVEL__4: AMF_H264_LEVEL = 40;
pub const AMF_H264_LEVEL__4_1: AMF_H264_LEVEL = 41;
pub const AMF_H264_LEVEL__4_2: AMF_H264_LEVEL = 42;
pub const AMF_H264_LEVEL__5: AMF_H264_LEVEL = 50;
pub const AMF_H264_LEVEL__5_1: AMF_H264_LEVEL = 51;
pub const AMF_H264_LEVEL__5_2: AMF_H264_LEVEL = 52;
pub const AMF_H264_LEVEL__6: AMF_H264_LEVEL = 60;
pub const AMF_H264_LEVEL__6_1: AMF_H264_LEVEL = 61;
pub const AMF_H264_LEVEL__6_2: AMF_H264_LEVEL = 62;

pub type AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = amf_int32;
pub const AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD =
    -1;
pub const AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP:
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = 0;
pub const AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = 1;
pub const AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR:
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = 2;
pub const AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR:
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = 3;
pub const AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_QUALITY_VBR:
    AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD = 4;

pub fn rate_control_name(rc: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD) -> &'static str {
    match rc {
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP => "CQP",
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR => "CBR",
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_PEAK_CONSTRAINED_VBR => "PeakVBR",
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR => "LatencyVBR",
        AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_QUALITY_VBR => "QVBR",
        _ => "unknown",
    }
}

pub type AMF_VIDEO_ENCODER_QUALITY_PRESET = amf_int32;
pub const AMF_VIDEO_ENCODER_QUALITY_PRESET_BALANCED: AMF_VIDEO_ENCODER_QUALITY_PRESET = 0;
pub const AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED: AMF_VIDEO_ENCODER_QUALITY_PRESET = 1;
pub const AMF_VIDEO_ENCODER_QUALITY_PRESET_QUALITY: AMF_VIDEO_ENCODER_QUALITY_PRESET = 2;

pub type AMF_VIDEO_ENCODER_PICTURE_TYPE = amf_int32;
pub const AMF_VIDEO_ENCODER_PICTURE_TYPE_NONE: AMF_VIDEO_ENCODER_PICTURE_TYPE = 0;
pub const AMF_VIDEO_ENCODER_PICTURE_TYPE_SKIP: AMF_VIDEO_ENCODER_PICTURE_TYPE = 1;
pub const AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR: AMF_VIDEO_ENCODER_PICTURE_TYPE = 2;
pub const AMF_VIDEO_ENCODER_PICTURE_TYPE_I: AMF_VIDEO_ENCODER_PICTURE_TYPE = 3;
pub const AMF_VIDEO_ENCODER_PICTURE_TYPE_P: AMF_VIDEO_ENCODER_PICTURE_TYPE = 4;

/// Only `_IDR` marks a stream-startable access unit. A plain `_I` does not, and treating one as
/// a keyframe is the difference between a mid-stream joiner decoding and staring at green.
pub type AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE = amf_int32;
pub const AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR: AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE = 0;
pub const AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_I: AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE = 1;
pub const AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_P: AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE = 2;

pub type AMF_VIDEO_ENCODER_PREENCODE_MODE = amf_int32;
pub const AMF_VIDEO_ENCODER_PREENCODE_DISABLED: AMF_VIDEO_ENCODER_PREENCODE_MODE = 0;
pub const AMF_VIDEO_ENCODER_PREENCODE_ENABLED: AMF_VIDEO_ENCODER_PREENCODE_MODE = 1;

pub type AMF_VIDEO_ENCODER_CODING = amf_int32;
pub const AMF_VIDEO_ENCODER_UNDEFINED: AMF_VIDEO_ENCODER_CODING = 0;
pub const AMF_VIDEO_ENCODER_CABAC: AMF_VIDEO_ENCODER_CODING = 1;
/// Spelled `CALV` in the header; it is CAVLC.
pub const AMF_VIDEO_ENCODER_CALV: AMF_VIDEO_ENCODER_CODING = 2;

pub type AMF_VIDEO_ENCODER_OUTPUT_MODE = amf_int32;
pub const AMF_VIDEO_ENCODER_OUTPUT_MODE_FRAME: AMF_VIDEO_ENCODER_OUTPUT_MODE = 0;
pub const AMF_VIDEO_ENCODER_OUTPUT_MODE_SLICE: AMF_VIDEO_ENCODER_OUTPUT_MODE = 1;

pub type AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE = amf_int32;
pub const AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_FRAME: AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE = 0;
pub const AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE: AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE = 1;
pub const AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE_LAST: AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE =
    2;

/// `AMFTrace` levels (core/Trace.h).
pub const AMF_TRACE_ERROR: amf_int32 = 0;
pub const AMF_TRACE_WARNING: amf_int32 = 1;
pub const AMF_TRACE_INFO: amf_int32 = 2;
pub const AMF_TRACE_NOLOG: amf_int32 = 100;

// ===========================================================================================
// IIDs (AMF_DECLARE_IID, C branch)
//
// AMFCaps, AMFIOCaps, AMFFactory and AMFTrace have no IID in either branch and cannot be
// reached through QueryInterface.
// ===========================================================================================

pub const IID_AMF_BUFFER: AMFGuid = AMFGuid::new(
    0xb04b_7248,
    0xb6f0,
    0x4321,
    0xb6,
    0x91,
    0xba,
    0xa4,
    0x74,
    0x0f,
    0x9f,
    0xcb,
);

pub const IID_AMF_SURFACE: AMFGuid = AMFGuid::new(
    0x3075_dbe3,
    0x8718,
    0x4cfa,
    0x86,
    0xfb,
    0x21,
    0x14,
    0xc0,
    0xa5,
    0xa4,
    0x51,
);

pub const IID_AMF_CONTEXT1: AMFGuid = AMFGuid::new(
    0xd9e9_f868,
    0x6220,
    0x44c6,
    0xa2,
    0x2f,
    0x7c,
    0xd6,
    0xda,
    0xc6,
    0x86,
    0x46,
);
