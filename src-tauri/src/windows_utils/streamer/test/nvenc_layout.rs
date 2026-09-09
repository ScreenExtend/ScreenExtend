//! NVENC ABI assertions — generated, do not hand-edit the `abi_*` bodies.
//!
//! The previous version of this file asserted the Rust structs' `size_of` and `offset_of`
//! against constants emitted by the same bindgen run that produced those structs. That is
//! self-referential: it detects a hand edit to the Rust and nothing else. In particular it could
//! not detect the thing it most needed to — bindings generated against one SDK header while
//! `nvenc_sys/version.rs`, which is hand-written, encodes the struct revision words of another.
//!
//! A wrong revision word is not a compile error and not a crash. `nvEncInitializeEncoder`
//! returns `NV_ENC_ERR_INVALID_VERSION`, `build_backend` catches it, and the whole NVIDIA path
//! degrades to Quick Sync or x264 — on a machine with a working NVENC in it. The same is true of
//! a wrong GUID (the driver reports an unsupported parameter set, not a bad GUID) and of a wrong
//! `NV_ENCODE_API_FUNCTION_LIST` offset, which is a call through the wrong function pointer.
//!
//! So every number below was measured by a C compiler reading the real header.
//!
//! ## Provenance
//!
//! * Header: `src-tauri/resources/nvenc-headers/nvEncodeAPI.h` — FFmpeg/nv-codec-headers at tag
//!   `n12.1.14.1` (commit `e1ff958747e9e08247cea31b6e629d61ece824c0`), which is NVENC API
//!   **12.1**. MIT, redistributable; SHA-256 pinned in `src-tauri/resources/SHA256SUMS` and
//!   recorded in `resources/PROVENANCE.md`.
//! * The bindings in `nvenc_sys/` were audited against it rather than assumed: every struct
//!   revision in the hand-written `version.rs` matches 12.1 exactly (CONFIG 8, INITIALIZE_PARAMS
//!   6, RECONFIGURE_PARAMS 1, PRESET_CONFIG 4, PIC_PARAMS 6, LOCK_BITSTREAM 1,
//!   REGISTER_RESOURCE 4, EVENT_PARAMS 1), and the members that looked like SDK-13 additions —
//!   `nvEncRestoreEncoderState`, `nvEncLookaheadPicture`, `numStateBuffers`, `outputStatsLevel`,
//!   `splitEncodeMode`, `NV_ENC_CAPS_EXPOSED_COUNT = 55` — are all present at 12.1. 13.0 raises
//!   every one of those revisions by one, so the bindings are 12.1 throughout and internally
//!   consistent.
//! * Generator: `src-tauri/tools/nvenc_layout_gen.c`, compiled in C mode (`cl /TC`).
//! * Toolchain: MSVC Build Tools 2022 x64, entered via `VC\Auxiliary\Buildcvars64.bat`.
//!   MinGW is the wrong tool: `nvEncodeAPI64.dll` is MSVC-built, and the two compilers do not
//!   pack the `unsigned int x : 1` runs in `NV_ENC_CONFIG_H264` and `NV_ENC_PIC_PARAMS_H264` the
//!   same way.
//!
//! ## Regenerating
//!
//! ```text
//! cd src-tauri\tools
//! nvenc_layout_build.bat        :: vcvars64 + cl /TC, writes nvenc_layout_generated.txt
//! ```
//!
//! then paste `nvenc_layout_generated.txt` over the bodies below, in order. Do **not** run the
//! generator from `build.rs`: the usual dev box for this repo is a macOS 10.15 machine with no
//! `cl.exe`, and the generated Rust is the checked-in product. A diff in these numbers after a
//! header bump is a required review item.
//!
//! What this file cannot check is whether the driver agrees — only running the encoder does
//! that, which is `test/nvenc_encoder.rs`.

use std::mem::{align_of, offset_of, size_of};

use crate::windows_utils::streamer::nvidia::nvenc_sys::*;
// The generator emits C's bare enum-constant names, because that is what the header calls them.
// bindgen puts each one inside its enum type, so these globs are what make the generated body a
// literal paste rather than a translation. No two of these enums share a variant name.
use crate::windows_utils::streamer::nvidia::nvenc_sys::NVENCSTATUS::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_BUFFER_FORMAT::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_BUFFER_USAGE::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_CAPS::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_DEVICE_TYPE::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_H264_ENTROPY_CODING_MODE::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_INPUT_RESOURCE_TYPE::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_MULTI_PASS::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_NUM_REF_FRAMES::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_PARAMS_RC_MODE::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_PIC_FLAGS::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_PIC_STRUCT::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_TUNING_INFO::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_VUI_COLOR_PRIMARIES::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_VUI_MATRIX_COEFFS::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_VUI_TRANSFER_CHARACTERISTIC::*;
use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_VUI_VIDEO_FORMAT::*;

/// Compare a `GUID` field by field, so a failure names which of the four parts moved.
#[track_caller]
fn assert_guid(g: GUID, d1: u32, d2: u16, d3: u16, d4: [u8; 8], what: &str) {
    assert_eq!(g.Data1, d1, "{what}.Data1");
    assert_eq!(g.Data2, d2, "{what}.Data2");
    assert_eq!(g.Data3, d3, "{what}.Data3");
    assert_eq!(g.Data4, d4, "{what}.Data4");
}

#[test]
fn abi_api_version_matches_nvencodeapi_h() {
    // `NVENCAPI_VERSION` is sent to the driver as `apiVersion` and is folded into
    // every struct version word, so it is the root of everything below.
    assert_eq!(NVENCAPI_MAJOR_VERSION, 12, "NVENCAPI_MAJOR_VERSION");
    assert_eq!(NVENCAPI_MINOR_VERSION, 1, "NVENCAPI_MINOR_VERSION");
    assert_eq!(NVENCAPI_VERSION, 0x0100000C, "NVENCAPI_VERSION");
}

#[test]
fn abi_struct_version_words_match_nvencodeapi_h() {
    // Hand-written in `version.rs`, which is why these are worth checking against a
    // compiler rather than against the file that declares them.
    assert_eq!(
        NV_ENCODE_API_FUNCTION_LIST_VER, 0x7102000C,
        "NV_ENCODE_API_FUNCTION_LIST_VER"
    );
    assert_eq!(
        NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, 0x7101000C,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER"
    );
    assert_eq!(
        NV_ENC_INITIALIZE_PARAMS_VER, 0xF106000C,
        "NV_ENC_INITIALIZE_PARAMS_VER"
    );
    assert_eq!(NV_ENC_CONFIG_VER, 0xF108000C, "NV_ENC_CONFIG_VER");
    assert_eq!(NV_ENC_RC_PARAMS_VER, 0x7101000C, "NV_ENC_RC_PARAMS_VER");
    assert_eq!(
        NV_ENC_PRESET_CONFIG_VER, 0xF104000C,
        "NV_ENC_PRESET_CONFIG_VER"
    );
    assert_eq!(
        NV_ENC_RECONFIGURE_PARAMS_VER, 0xF101000C,
        "NV_ENC_RECONFIGURE_PARAMS_VER"
    );
    assert_eq!(NV_ENC_PIC_PARAMS_VER, 0xF106000C, "NV_ENC_PIC_PARAMS_VER");
    assert_eq!(
        NV_ENC_LOCK_BITSTREAM_VER, 0xF101000C,
        "NV_ENC_LOCK_BITSTREAM_VER"
    );
    assert_eq!(
        NV_ENC_MAP_INPUT_RESOURCE_VER, 0x7104000C,
        "NV_ENC_MAP_INPUT_RESOURCE_VER"
    );
    assert_eq!(
        NV_ENC_REGISTER_RESOURCE_VER, 0x7104000C,
        "NV_ENC_REGISTER_RESOURCE_VER"
    );
    assert_eq!(
        NV_ENC_CREATE_BITSTREAM_BUFFER_VER, 0x7101000C,
        "NV_ENC_CREATE_BITSTREAM_BUFFER_VER"
    );
    assert_eq!(
        NV_ENC_EVENT_PARAMS_VER, 0x7101000C,
        "NV_ENC_EVENT_PARAMS_VER"
    );
    assert_eq!(NV_ENC_CAPS_PARAM_VER, 0x7101000C, "NV_ENC_CAPS_PARAM_VER");
    assert_eq!(
        NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER, 0x7101000C,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER"
    );
}

#[test]
fn abi_struct_sizes_match_nvencodeapi_h() {
    assert_eq!(
        size_of::<NV_ENCODE_API_FUNCTION_LIST>(),
        2552,
        "NV_ENCODE_API_FUNCTION_LIST"
    );
    assert_eq!(
        align_of::<NV_ENCODE_API_FUNCTION_LIST>(),
        8,
        "NV_ENCODE_API_FUNCTION_LIST"
    );
    assert_eq!(
        size_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>(),
        1552,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS"
    );
    assert_eq!(
        align_of::<NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS>(),
        8,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS"
    );
    assert_eq!(
        size_of::<NV_ENC_INITIALIZE_PARAMS>(),
        1808,
        "NV_ENC_INITIALIZE_PARAMS"
    );
    assert_eq!(
        align_of::<NV_ENC_INITIALIZE_PARAMS>(),
        8,
        "NV_ENC_INITIALIZE_PARAMS"
    );
    assert_eq!(size_of::<NV_ENC_CONFIG>(), 3584, "NV_ENC_CONFIG");
    assert_eq!(align_of::<NV_ENC_CONFIG>(), 8, "NV_ENC_CONFIG");
    assert_eq!(size_of::<NV_ENC_RC_PARAMS>(), 128, "NV_ENC_RC_PARAMS");
    assert_eq!(align_of::<NV_ENC_RC_PARAMS>(), 4, "NV_ENC_RC_PARAMS");
    assert_eq!(size_of::<NV_ENC_QP>(), 12, "NV_ENC_QP");
    assert_eq!(align_of::<NV_ENC_QP>(), 4, "NV_ENC_QP");
    assert_eq!(
        size_of::<NV_ENC_CODEC_CONFIG>(),
        1792,
        "NV_ENC_CODEC_CONFIG"
    );
    assert_eq!(align_of::<NV_ENC_CODEC_CONFIG>(), 8, "NV_ENC_CODEC_CONFIG");
    assert_eq!(size_of::<NV_ENC_CONFIG_H264>(), 1792, "NV_ENC_CONFIG_H264");
    assert_eq!(align_of::<NV_ENC_CONFIG_H264>(), 8, "NV_ENC_CONFIG_H264");
    assert_eq!(
        size_of::<NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
        112,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS"
    );
    assert_eq!(
        align_of::<NV_ENC_CONFIG_H264_VUI_PARAMETERS>(),
        4,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS"
    );
    assert_eq!(
        size_of::<NV_ENC_PRESET_CONFIG>(),
        5128,
        "NV_ENC_PRESET_CONFIG"
    );
    assert_eq!(
        align_of::<NV_ENC_PRESET_CONFIG>(),
        8,
        "NV_ENC_PRESET_CONFIG"
    );
    assert_eq!(
        size_of::<NV_ENC_RECONFIGURE_PARAMS>(),
        1824,
        "NV_ENC_RECONFIGURE_PARAMS"
    );
    assert_eq!(
        align_of::<NV_ENC_RECONFIGURE_PARAMS>(),
        8,
        "NV_ENC_RECONFIGURE_PARAMS"
    );
    assert_eq!(size_of::<NV_ENC_PIC_PARAMS>(), 3360, "NV_ENC_PIC_PARAMS");
    assert_eq!(align_of::<NV_ENC_PIC_PARAMS>(), 8, "NV_ENC_PIC_PARAMS");
    assert_eq!(
        size_of::<NV_ENC_CODEC_PIC_PARAMS>(),
        1552,
        "NV_ENC_CODEC_PIC_PARAMS"
    );
    assert_eq!(
        align_of::<NV_ENC_CODEC_PIC_PARAMS>(),
        8,
        "NV_ENC_CODEC_PIC_PARAMS"
    );
    assert_eq!(
        size_of::<NV_ENC_PIC_PARAMS_H264>(),
        1536,
        "NV_ENC_PIC_PARAMS_H264"
    );
    assert_eq!(
        align_of::<NV_ENC_PIC_PARAMS_H264>(),
        8,
        "NV_ENC_PIC_PARAMS_H264"
    );
    assert_eq!(
        size_of::<NV_ENC_LOCK_BITSTREAM>(),
        1552,
        "NV_ENC_LOCK_BITSTREAM"
    );
    assert_eq!(
        align_of::<NV_ENC_LOCK_BITSTREAM>(),
        8,
        "NV_ENC_LOCK_BITSTREAM"
    );
    assert_eq!(
        size_of::<NV_ENC_MAP_INPUT_RESOURCE>(),
        1544,
        "NV_ENC_MAP_INPUT_RESOURCE"
    );
    assert_eq!(
        align_of::<NV_ENC_MAP_INPUT_RESOURCE>(),
        8,
        "NV_ENC_MAP_INPUT_RESOURCE"
    );
    assert_eq!(
        size_of::<NV_ENC_REGISTER_RESOURCE>(),
        1536,
        "NV_ENC_REGISTER_RESOURCE"
    );
    assert_eq!(
        align_of::<NV_ENC_REGISTER_RESOURCE>(),
        8,
        "NV_ENC_REGISTER_RESOURCE"
    );
    assert_eq!(
        size_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>(),
        776,
        "NV_ENC_CREATE_BITSTREAM_BUFFER"
    );
    assert_eq!(
        align_of::<NV_ENC_CREATE_BITSTREAM_BUFFER>(),
        8,
        "NV_ENC_CREATE_BITSTREAM_BUFFER"
    );
    assert_eq!(
        size_of::<NV_ENC_EVENT_PARAMS>(),
        1544,
        "NV_ENC_EVENT_PARAMS"
    );
    assert_eq!(align_of::<NV_ENC_EVENT_PARAMS>(), 8, "NV_ENC_EVENT_PARAMS");
    assert_eq!(size_of::<NV_ENC_CAPS_PARAM>(), 256, "NV_ENC_CAPS_PARAM");
    assert_eq!(align_of::<NV_ENC_CAPS_PARAM>(), 4, "NV_ENC_CAPS_PARAM");
    assert_eq!(
        size_of::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>(),
        1544,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD"
    );
    assert_eq!(
        align_of::<NV_ENC_SEQUENCE_PARAM_PAYLOAD>(),
        8,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD"
    );
}

#[test]
fn abi_function_list_offsets_match_nvencodeapi_h() {
    // The driver writes this table. An offset that disagrees means every call after
    // it goes through the wrong pointer.
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, version),
        0,
        "NV_ENCODE_API_FUNCTION_LIST.version"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, reserved),
        4,
        "NV_ENCODE_API_FUNCTION_LIST.reserved"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSession),
        8,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncOpenEncodeSession"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDCount),
        16,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodeGUIDCount"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeGUIDs),
        40,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodeGUIDs"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetInputFormatCount),
        48,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetInputFormatCount"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetInputFormats),
        56,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetInputFormats"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeCaps),
        64,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodeCaps"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetCount),
        72,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodePresetCount"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetGUIDs),
        80,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodePresetGUIDs"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfig),
        88,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodePresetConfig"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncInitializeEncoder),
        96,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncInitializeEncoder"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateInputBuffer),
        104,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncCreateInputBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyInputBuffer),
        112,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncDestroyInputBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateBitstreamBuffer),
        120,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncCreateBitstreamBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyBitstreamBuffer),
        128,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncDestroyBitstreamBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncEncodePicture),
        136,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncEncodePicture"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLockBitstream),
        144,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncLockBitstream"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockBitstream),
        152,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncUnlockBitstream"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncLockInputBuffer),
        160,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncLockInputBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnlockInputBuffer),
        168,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncUnlockInputBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodeStats),
        176,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodeStats"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParams),
        184,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetSequenceParams"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterAsyncEvent),
        192,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncRegisterAsyncEvent"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterAsyncEvent),
        200,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncUnregisterAsyncEvent"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncMapInputResource),
        208,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncMapInputResource"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnmapInputResource),
        216,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncUnmapInputResource"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyEncoder),
        224,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncDestroyEncoder"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncInvalidateRefFrames),
        232,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncInvalidateRefFrames"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncOpenEncodeSessionEx),
        240,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncOpenEncodeSessionEx"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncRegisterResource),
        248,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncRegisterResource"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncUnregisterResource),
        256,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncUnregisterResource"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncReconfigureEncoder),
        264,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncReconfigureEncoder"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncCreateMVBuffer),
        280,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncCreateMVBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncDestroyMVBuffer),
        288,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncDestroyMVBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncRunMotionEstimationOnly),
        296,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncRunMotionEstimationOnly"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetLastErrorString),
        304,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetLastErrorString"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncSetIOCudaStreams),
        312,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncSetIOCudaStreams"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetEncodePresetConfigEx),
        320,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetEncodePresetConfigEx"
    );
    assert_eq!(
        offset_of!(NV_ENCODE_API_FUNCTION_LIST, nvEncGetSequenceParamEx),
        328,
        "NV_ENCODE_API_FUNCTION_LIST.nvEncGetSequenceParamEx"
    );
}

#[test]
fn abi_field_offsets_match_nvencodeapi_h() {
    assert_eq!(
        offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, version),
        0,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, deviceType),
        4,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.deviceType"
    );
    assert_eq!(
        offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, device),
        8,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.device"
    );
    assert_eq!(
        offset_of!(NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS, apiVersion),
        24,
        "NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS.apiVersion"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, version),
        0,
        "NV_ENC_INITIALIZE_PARAMS.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeGUID),
        4,
        "NV_ENC_INITIALIZE_PARAMS.encodeGUID"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, presetGUID),
        20,
        "NV_ENC_INITIALIZE_PARAMS.presetGUID"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeWidth),
        36,
        "NV_ENC_INITIALIZE_PARAMS.encodeWidth"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeHeight),
        40,
        "NV_ENC_INITIALIZE_PARAMS.encodeHeight"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, darWidth),
        44,
        "NV_ENC_INITIALIZE_PARAMS.darWidth"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, darHeight),
        48,
        "NV_ENC_INITIALIZE_PARAMS.darHeight"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, frameRateNum),
        52,
        "NV_ENC_INITIALIZE_PARAMS.frameRateNum"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, frameRateDen),
        56,
        "NV_ENC_INITIALIZE_PARAMS.frameRateDen"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, enableEncodeAsync),
        60,
        "NV_ENC_INITIALIZE_PARAMS.enableEncodeAsync"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, enablePTD),
        64,
        "NV_ENC_INITIALIZE_PARAMS.enablePTD"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, encodeConfig),
        88,
        "NV_ENC_INITIALIZE_PARAMS.encodeConfig"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeWidth),
        96,
        "NV_ENC_INITIALIZE_PARAMS.maxEncodeWidth"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, maxEncodeHeight),
        100,
        "NV_ENC_INITIALIZE_PARAMS.maxEncodeHeight"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, tuningInfo),
        136,
        "NV_ENC_INITIALIZE_PARAMS.tuningInfo"
    );
    assert_eq!(
        offset_of!(NV_ENC_INITIALIZE_PARAMS, bufferFormat),
        140,
        "NV_ENC_INITIALIZE_PARAMS.bufferFormat"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG, version),
        0,
        "NV_ENC_CONFIG.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG, profileGUID),
        4,
        "NV_ENC_CONFIG.profileGUID"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG, gopLength),
        20,
        "NV_ENC_CONFIG.gopLength"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG, frameIntervalP),
        24,
        "NV_ENC_CONFIG.frameIntervalP"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG, rcParams),
        40,
        "NV_ENC_CONFIG.rcParams"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG, encodeCodecConfig),
        168,
        "NV_ENC_CONFIG.encodeCodecConfig"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, version),
        0,
        "NV_ENC_RC_PARAMS.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, rateControlMode),
        4,
        "NV_ENC_RC_PARAMS.rateControlMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, constQP),
        8,
        "NV_ENC_RC_PARAMS.constQP"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, averageBitRate),
        20,
        "NV_ENC_RC_PARAMS.averageBitRate"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, maxBitRate),
        24,
        "NV_ENC_RC_PARAMS.maxBitRate"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, vbvBufferSize),
        28,
        "NV_ENC_RC_PARAMS.vbvBufferSize"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, vbvInitialDelay),
        32,
        "NV_ENC_RC_PARAMS.vbvInitialDelay"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, minQP),
        40,
        "NV_ENC_RC_PARAMS.minQP"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, maxQP),
        52,
        "NV_ENC_RC_PARAMS.maxQP"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, targetQuality),
        88,
        "NV_ENC_RC_PARAMS.targetQuality"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, lookaheadDepth),
        90,
        "NV_ENC_RC_PARAMS.lookaheadDepth"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, lowDelayKeyFrameScale),
        92,
        "NV_ENC_RC_PARAMS.lowDelayKeyFrameScale"
    );
    assert_eq!(
        offset_of!(NV_ENC_RC_PARAMS, multiPass),
        100,
        "NV_ENC_RC_PARAMS.multiPass"
    );
    assert_eq!(offset_of!(NV_ENC_QP, qpInterP), 0, "NV_ENC_QP.qpInterP");
    assert_eq!(offset_of!(NV_ENC_QP, qpInterB), 4, "NV_ENC_QP.qpInterB");
    assert_eq!(offset_of!(NV_ENC_QP, qpIntra), 8, "NV_ENC_QP.qpIntra");
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, idrPeriod),
        8,
        "NV_ENC_CONFIG_H264.idrPeriod"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, intraRefreshPeriod),
        52,
        "NV_ENC_CONFIG_H264.intraRefreshPeriod"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, intraRefreshCnt),
        56,
        "NV_ENC_CONFIG_H264.intraRefreshCnt"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, maxNumRefFrames),
        60,
        "NV_ENC_CONFIG_H264.maxNumRefFrames"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, sliceMode),
        64,
        "NV_ENC_CONFIG_H264.sliceMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, sliceModeData),
        68,
        "NV_ENC_CONFIG_H264.sliceModeData"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, h264VUIParameters),
        72,
        "NV_ENC_CONFIG_H264.h264VUIParameters"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, ltrNumFrames),
        184,
        "NV_ENC_CONFIG_H264.ltrNumFrames"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, ltrTrustMode),
        188,
        "NV_ENC_CONFIG_H264.ltrTrustMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, entropyCodingMode),
        44,
        "NV_ENC_CONFIG_H264.entropyCodingMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, adaptiveTransformMode),
        32,
        "NV_ENC_CONFIG_H264.adaptiveTransformMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, numRefL0),
        204,
        "NV_ENC_CONFIG_H264.numRefL0"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264, numRefL1),
        208,
        "NV_ENC_CONFIG_H264.numRefL1"
    );
    assert_eq!(
        offset_of!(
            NV_ENC_CONFIG_H264_VUI_PARAMETERS,
            videoSignalTypePresentFlag
        ),
        8,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.videoSignalTypePresentFlag"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFormat),
        12,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.videoFormat"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, videoFullRangeFlag),
        16,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.videoFullRangeFlag"
    );
    assert_eq!(
        offset_of!(
            NV_ENC_CONFIG_H264_VUI_PARAMETERS,
            colourDescriptionPresentFlag
        ),
        20,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.colourDescriptionPresentFlag"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourPrimaries),
        24,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.colourPrimaries"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, transferCharacteristics),
        28,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.transferCharacteristics"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, colourMatrix),
        32,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.colourMatrix"
    );
    assert_eq!(
        offset_of!(NV_ENC_CONFIG_H264_VUI_PARAMETERS, bitstreamRestrictionFlag),
        48,
        "NV_ENC_CONFIG_H264_VUI_PARAMETERS.bitstreamRestrictionFlag"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, version),
        0,
        "NV_ENC_PIC_PARAMS.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, inputWidth),
        4,
        "NV_ENC_PIC_PARAMS.inputWidth"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, inputHeight),
        8,
        "NV_ENC_PIC_PARAMS.inputHeight"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, inputPitch),
        12,
        "NV_ENC_PIC_PARAMS.inputPitch"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, encodePicFlags),
        16,
        "NV_ENC_PIC_PARAMS.encodePicFlags"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, inputBuffer),
        40,
        "NV_ENC_PIC_PARAMS.inputBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, outputBitstream),
        48,
        "NV_ENC_PIC_PARAMS.outputBitstream"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, completionEvent),
        56,
        "NV_ENC_PIC_PARAMS.completionEvent"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, bufferFmt),
        64,
        "NV_ENC_PIC_PARAMS.bufferFmt"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, pictureStruct),
        68,
        "NV_ENC_PIC_PARAMS.pictureStruct"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, inputTimeStamp),
        24,
        "NV_ENC_PIC_PARAMS.inputTimeStamp"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, frameIdx),
        20,
        "NV_ENC_PIC_PARAMS.frameIdx"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS, codecPicParams),
        80,
        "NV_ENC_PIC_PARAMS.codecPicParams"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS_H264, ltrMarkFrameIdx),
        56,
        "NV_ENC_PIC_PARAMS_H264.ltrMarkFrameIdx"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS_H264, ltrUseFrameBitmap),
        60,
        "NV_ENC_PIC_PARAMS_H264.ltrUseFrameBitmap"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS_H264, ltrUsageMode),
        64,
        "NV_ENC_PIC_PARAMS_H264.ltrUsageMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS_H264, sliceMode),
        48,
        "NV_ENC_PIC_PARAMS_H264.sliceMode"
    );
    assert_eq!(
        offset_of!(NV_ENC_PIC_PARAMS_H264, sliceModeData),
        52,
        "NV_ENC_PIC_PARAMS_H264.sliceModeData"
    );
    assert_eq!(
        offset_of!(NV_ENC_LOCK_BITSTREAM, version),
        0,
        "NV_ENC_LOCK_BITSTREAM.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_LOCK_BITSTREAM, outputBitstream),
        8,
        "NV_ENC_LOCK_BITSTREAM.outputBitstream"
    );
    assert_eq!(
        offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamBufferPtr),
        56,
        "NV_ENC_LOCK_BITSTREAM.bitstreamBufferPtr"
    );
    assert_eq!(
        offset_of!(NV_ENC_LOCK_BITSTREAM, bitstreamSizeInBytes),
        36,
        "NV_ENC_LOCK_BITSTREAM.bitstreamSizeInBytes"
    );
    assert_eq!(
        offset_of!(NV_ENC_LOCK_BITSTREAM, outputTimeStamp),
        40,
        "NV_ENC_LOCK_BITSTREAM.outputTimeStamp"
    );
    assert_eq!(
        offset_of!(NV_ENC_LOCK_BITSTREAM, pictureType),
        64,
        "NV_ENC_LOCK_BITSTREAM.pictureType"
    );
    assert_eq!(
        offset_of!(NV_ENC_MAP_INPUT_RESOURCE, version),
        0,
        "NV_ENC_MAP_INPUT_RESOURCE.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_MAP_INPUT_RESOURCE, registeredResource),
        16,
        "NV_ENC_MAP_INPUT_RESOURCE.registeredResource"
    );
    assert_eq!(
        offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedResource),
        24,
        "NV_ENC_MAP_INPUT_RESOURCE.mappedResource"
    );
    assert_eq!(
        offset_of!(NV_ENC_MAP_INPUT_RESOURCE, mappedBufferFmt),
        32,
        "NV_ENC_MAP_INPUT_RESOURCE.mappedBufferFmt"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, version),
        0,
        "NV_ENC_REGISTER_RESOURCE.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, resourceType),
        4,
        "NV_ENC_REGISTER_RESOURCE.resourceType"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, width),
        8,
        "NV_ENC_REGISTER_RESOURCE.width"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, height),
        12,
        "NV_ENC_REGISTER_RESOURCE.height"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, pitch),
        16,
        "NV_ENC_REGISTER_RESOURCE.pitch"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, resourceToRegister),
        24,
        "NV_ENC_REGISTER_RESOURCE.resourceToRegister"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, registeredResource),
        32,
        "NV_ENC_REGISTER_RESOURCE.registeredResource"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, bufferFormat),
        40,
        "NV_ENC_REGISTER_RESOURCE.bufferFormat"
    );
    assert_eq!(
        offset_of!(NV_ENC_REGISTER_RESOURCE, bufferUsage),
        44,
        "NV_ENC_REGISTER_RESOURCE.bufferUsage"
    );
    assert_eq!(
        offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, version),
        0,
        "NV_ENC_CREATE_BITSTREAM_BUFFER.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_CREATE_BITSTREAM_BUFFER, bitstreamBuffer),
        16,
        "NV_ENC_CREATE_BITSTREAM_BUFFER.bitstreamBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENC_EVENT_PARAMS, version),
        0,
        "NV_ENC_EVENT_PARAMS.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_EVENT_PARAMS, completionEvent),
        8,
        "NV_ENC_EVENT_PARAMS.completionEvent"
    );
    assert_eq!(
        offset_of!(NV_ENC_CAPS_PARAM, version),
        0,
        "NV_ENC_CAPS_PARAM.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_CAPS_PARAM, capsToQuery),
        4,
        "NV_ENC_CAPS_PARAM.capsToQuery"
    );
    assert_eq!(
        offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, version),
        0,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, inBufferSize),
        4,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD.inBufferSize"
    );
    assert_eq!(
        offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, spsppsBuffer),
        16,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD.spsppsBuffer"
    );
    assert_eq!(
        offset_of!(NV_ENC_SEQUENCE_PARAM_PAYLOAD, outSPSPPSPayloadSize),
        24,
        "NV_ENC_SEQUENCE_PARAM_PAYLOAD.outSPSPPSPayloadSize"
    );
    assert_eq!(
        offset_of!(NV_ENC_RECONFIGURE_PARAMS, version),
        0,
        "NV_ENC_RECONFIGURE_PARAMS.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_RECONFIGURE_PARAMS, reInitEncodeParams),
        8,
        "NV_ENC_RECONFIGURE_PARAMS.reInitEncodeParams"
    );
    assert_eq!(
        offset_of!(NV_ENC_PRESET_CONFIG, version),
        0,
        "NV_ENC_PRESET_CONFIG.version"
    );
    assert_eq!(
        offset_of!(NV_ENC_PRESET_CONFIG, presetCfg),
        8,
        "NV_ENC_PRESET_CONFIG.presetCfg"
    );
}

#[test]
fn abi_enum_values_match_nvencodeapi_h() {
    assert_eq!(
        NV_ENC_DEVICE_TYPE_DIRECTX as i64, 0,
        "NV_ENC_DEVICE_TYPE_DIRECTX"
    );
    assert_eq!(
        NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX as i64, 0,
        "NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX"
    );
    assert_eq!(NV_ENC_INPUT_IMAGE as i64, 0, "NV_ENC_INPUT_IMAGE");
    assert_eq!(
        NV_ENC_BUFFER_FORMAT_ARGB as i64, 16777216,
        "NV_ENC_BUFFER_FORMAT_ARGB"
    );
    assert_eq!(
        NV_ENC_BUFFER_FORMAT_ABGR as i64, 268435456,
        "NV_ENC_BUFFER_FORMAT_ABGR"
    );
    assert_eq!(
        NV_ENC_BUFFER_FORMAT_NV12 as i64, 1,
        "NV_ENC_BUFFER_FORMAT_NV12"
    );
    assert_eq!(NV_ENC_PIC_STRUCT_FRAME as i64, 1, "NV_ENC_PIC_STRUCT_FRAME");
    assert_eq!(
        NV_ENC_PIC_FLAG_FORCEINTRA as i64, 1,
        "NV_ENC_PIC_FLAG_FORCEINTRA"
    );
    assert_eq!(
        NV_ENC_PIC_FLAG_FORCEIDR as i64, 2,
        "NV_ENC_PIC_FLAG_FORCEIDR"
    );
    assert_eq!(
        NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as i64, 4,
        "NV_ENC_PIC_FLAG_OUTPUT_SPSPPS"
    );
    assert_eq!(
        NV_ENC_PARAMS_RC_CONSTQP as i64, 0,
        "NV_ENC_PARAMS_RC_CONSTQP"
    );
    assert_eq!(NV_ENC_PARAMS_RC_VBR as i64, 1, "NV_ENC_PARAMS_RC_VBR");
    assert_eq!(NV_ENC_PARAMS_RC_CBR as i64, 2, "NV_ENC_PARAMS_RC_CBR");
    assert_eq!(
        NV_ENC_MULTI_PASS_DISABLED as i64, 0,
        "NV_ENC_MULTI_PASS_DISABLED"
    );
    assert_eq!(
        NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY as i64, 3,
        "NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY"
    );
    assert_eq!(
        NV_ENC_H264_ENTROPY_CODING_MODE_CABAC as i64, 1,
        "NV_ENC_H264_ENTROPY_CODING_MODE_CABAC"
    );
    assert_eq!(
        NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC as i64, 2,
        "NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC"
    );
    assert_eq!(
        NV_ENC_H264_ADAPTIVE_TRANSFORM_ENABLE as i64, 2,
        "NV_ENC_H264_ADAPTIVE_TRANSFORM_ENABLE"
    );
    assert_eq!(
        NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE as i64, 1,
        "NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE"
    );
    assert_eq!(NV_ENC_NUM_REF_FRAMES_1 as i64, 1, "NV_ENC_NUM_REF_FRAMES_1");
    assert_eq!(NV_ENC_NUM_REF_FRAMES_4 as i64, 4, "NV_ENC_NUM_REF_FRAMES_4");
    assert_eq!(
        NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED as i64, 5,
        "NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED"
    );
    assert_eq!(
        NV_ENC_VUI_COLOR_PRIMARIES_BT709 as i64, 1,
        "NV_ENC_VUI_COLOR_PRIMARIES_BT709"
    );
    assert_eq!(
        NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709 as i64, 1,
        "NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709"
    );
    assert_eq!(
        NV_ENC_VUI_MATRIX_COEFFS_BT709 as i64, 1,
        "NV_ENC_VUI_MATRIX_COEFFS_BT709"
    );
}

#[test]
fn abi_capability_ids_match_nvencodeapi_h() {
    // `EncoderCaps::query` asks by id. A wrong id silently reads a different
    // capability, and every gate built on it is then answering the wrong question.
    assert_eq!(
        NV_ENC_CAPS_NUM_MAX_BFRAMES as i64, 0,
        "NV_ENC_CAPS_NUM_MAX_BFRAMES"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES as i64, 1,
        "NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_FIELD_ENCODING as i64, 2,
        "NV_ENC_CAPS_SUPPORT_FIELD_ENCODING"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_MONOCHROME as i64, 3,
        "NV_ENC_CAPS_SUPPORT_MONOCHROME"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_INTRA_REFRESH as i64, 25,
        "NV_ENC_CAPS_SUPPORT_INTRA_REFRESH"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_CONSTRAINED_ENCODING as i64, 24,
        "NV_ENC_CAPS_SUPPORT_CONSTRAINED_ENCODING"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE as i64, 26,
        "NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE as i64, 27,
        "NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION as i64, 28,
        "NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION"
    );
    assert_eq!(NV_ENC_CAPS_WIDTH_MIN as i64, 45, "NV_ENC_CAPS_WIDTH_MIN");
    assert_eq!(NV_ENC_CAPS_WIDTH_MAX as i64, 16, "NV_ENC_CAPS_WIDTH_MAX");
    assert_eq!(NV_ENC_CAPS_HEIGHT_MIN as i64, 46, "NV_ENC_CAPS_HEIGHT_MIN");
    assert_eq!(NV_ENC_CAPS_HEIGHT_MAX as i64, 17, "NV_ENC_CAPS_HEIGHT_MAX");
    assert_eq!(NV_ENC_CAPS_LEVEL_MAX as i64, 13, "NV_ENC_CAPS_LEVEL_MAX");
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_CABAC as i64, 7,
        "NV_ENC_CAPS_SUPPORT_CABAC"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_ADAPTIVE_TRANSFORM as i64, 8,
        "NV_ENC_CAPS_SUPPORT_ADAPTIVE_TRANSFORM"
    );
    assert_eq!(
        NV_ENC_CAPS_NUM_ENCODER_ENGINES as i64, 49,
        "NV_ENC_CAPS_NUM_ENCODER_ENGINES"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK as i64, 23,
        "NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK"
    );
    assert_eq!(
        NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT as i64, 30,
        "NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT"
    );
    assert_eq!(
        NV_ENC_CAPS_NUM_MAX_LTR_FRAMES as i64, 40,
        "NV_ENC_CAPS_NUM_MAX_LTR_FRAMES"
    );
    assert_eq!(
        NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES as i64, 47,
        "NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES"
    );
    assert_eq!(
        NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH as i64, 50,
        "NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH"
    );
    assert_eq!(
        NV_ENC_CAPS_EXPOSED_COUNT as i64, 55,
        "NV_ENC_CAPS_EXPOSED_COUNT"
    );
}

#[test]
fn abi_status_codes_match_nvencodeapi_h() {
    assert_eq!(NV_ENC_SUCCESS as i64, 0, "NV_ENC_SUCCESS");
    assert_eq!(
        NV_ENC_ERR_OUT_OF_MEMORY as i64, 10,
        "NV_ENC_ERR_OUT_OF_MEMORY"
    );
    assert_eq!(
        NV_ENC_ERR_INVALID_VERSION as i64, 15,
        "NV_ENC_ERR_INVALID_VERSION"
    );
    assert_eq!(
        NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY as i64, 21,
        "NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY"
    );
    assert_eq!(
        NV_ENC_ERR_ENCODER_BUSY as i64, 18,
        "NV_ENC_ERR_ENCODER_BUSY"
    );
    assert_eq!(
        NV_ENC_ERR_NEED_MORE_INPUT as i64, 17,
        "NV_ENC_ERR_NEED_MORE_INPUT"
    );
}

#[test]
fn abi_guids_match_nvencodeapi_h() {
    // bindgen mis-generates these (rust-bindgen#1888), so `nvenc_sys/guid.rs`
    // transcribes them by hand — exactly the place a typo would survive review.
    assert_guid(
        NV_ENC_CODEC_H264_GUID,
        0x6BC82762,
        0x4E63,
        0x4CA4,
        [170, 133, 30, 80, 243, 33, 246, 191],
        "NV_ENC_CODEC_H264_GUID",
    );
    assert_guid(
        NV_ENC_H264_PROFILE_BASELINE_GUID,
        0x0727BCAA,
        0x78C4,
        0x4C83,
        [140, 47, 239, 61, 255, 38, 124, 106],
        "NV_ENC_H264_PROFILE_BASELINE_GUID",
    );
    assert_guid(
        NV_ENC_H264_PROFILE_MAIN_GUID,
        0x60B5C1D4,
        0x67FE,
        0x4790,
        [148, 213, 196, 114, 109, 123, 110, 109],
        "NV_ENC_H264_PROFILE_MAIN_GUID",
    );
    assert_guid(
        NV_ENC_H264_PROFILE_HIGH_GUID,
        0xE7CBC309,
        0x4F7A,
        0x4B89,
        [175, 42, 213, 55, 201, 43, 227, 16],
        "NV_ENC_H264_PROFILE_HIGH_GUID",
    );
    assert_guid(
        NV_ENC_PRESET_P1_GUID,
        0xFC0A8D3E,
        0x45F8,
        0x4CF8,
        [128, 199, 41, 136, 113, 89, 14, 191],
        "NV_ENC_PRESET_P1_GUID",
    );
    assert_guid(
        NV_ENC_PRESET_P4_GUID,
        0x90A7B826,
        0xDF06,
        0x4862,
        [185, 210, 205, 109, 115, 160, 134, 129],
        "NV_ENC_PRESET_P4_GUID",
    );
    assert_guid(
        NV_ENC_PRESET_P7_GUID,
        0x84848C12,
        0x6F71,
        0x4C13,
        [147, 27, 83, 226, 131, 245, 121, 116],
        "NV_ENC_PRESET_P7_GUID",
    );
}
