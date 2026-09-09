//! Vendored NVIDIA NVENC FFI bindings, from
//! <https://github.com/ViliamVadocz/nvidia-video-codec-sdk>.
//!
//! **NVENC API 12.1** throughout, audited against
//! `resources/nvenc-headers/nvEncodeAPI.h` (FFmpeg/nv-codec-headers `n12.1.14.1`, MIT). Sizes,
//! field offsets, `NV_ENCODE_API_FUNCTION_LIST` offsets, GUIDs, enum values and every
//! `NV_ENC_*_VER` word are asserted against numbers a C compiler read out of that header — see
//! `windows_utils/streamer/test/nvenc_layout.rs` and `tools/nvenc_layout_gen.c`.
//!
//! `version.rs` is hand-written and therefore the part most able to drift: its revision words
//! are not derived from whatever header produced `nvEncodeAPI.rs`, they are transcribed. A wrong
//! one is not a compile error — the driver returns `NV_ENC_ERR_INVALID_VERSION` and the whole
//! NVIDIA path degrades to Quick Sync or x264 on a machine with a working NVENC. That is what
//! the generated assertions exist to catch.
//!
//! `NVENCAPI_VERSION` is also a floor: NVENC refuses an `apiVersion` newer than the driver
//! implements, so 12.1 is the minimum driver this build can use.

#![allow(
    non_snake_case,
    non_camel_case_types,
    non_upper_case_globals,
    dead_code
)]

#[allow(warnings)]
#[rustfmt::skip]
pub mod nvEncodeAPI;
mod guid;
mod version;

pub use guid::*;
pub use nvEncodeAPI::*;
pub use version::*;
