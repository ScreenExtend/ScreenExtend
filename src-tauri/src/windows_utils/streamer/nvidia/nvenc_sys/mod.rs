//! vendored NVIDIA NVENC FFI bindings: https://github.com/ViliamVadocz/nvidia-video-codec-sdk

#![allow(non_snake_case, non_camel_case_types, non_upper_case_globals, dead_code)]

#[allow(warnings)]
#[rustfmt::skip]
pub mod nvEncodeAPI;
mod guid;
mod version;

pub use guid::*;
pub use nvEncodeAPI::*;
pub use version::*;
