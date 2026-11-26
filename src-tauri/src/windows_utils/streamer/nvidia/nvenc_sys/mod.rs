//! Vendored NVIDIA NVENC FFI bindings, from
//! <https://github.com/ViliamVadocz/nvidia-video-codec-sdk>, at NVENC API 12.1
//! (`resources/nvenc-headers/nvEncodeAPI.h`, FFmpeg/nv-codec-headers `n12.1.14.1`, MIT).
//!
//! `version.rs` is transcribed by hand, and a wrong revision word is not a compile error: the
//! driver returns `NV_ENC_ERR_INVALID_VERSION` and the NVIDIA path degrades to Quick Sync or
//! x264. `windows_utils/streamer/test/nvenc_layout.rs` asserts every size, offset, GUID, enum
//! value and `NV_ENC_*_VER` word against numbers `tools/nvenc_layout_gen.c` reads from that
//! header with a C compiler.
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
