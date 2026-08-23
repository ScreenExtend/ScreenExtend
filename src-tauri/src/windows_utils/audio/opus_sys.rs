//! Hand-written libopus FFI, in the style of `windows_utils/streamer/x264/x264_sys.rs`.
//!
//! We bundle `libopus.dll` in `src-tauri/resources/` (provenance in
//! `resources/PROVENANCE.md`) and load it at runtime with `libloading`, exactly the way
//! x264 is loaded — no build-time link, no `*-sys` crate dependency.
//!
//! The control-macro values below are transcribed from the real
//! `.sources/repos/opus/include/opus_defines.h`. Getting one wrong silently misconfigures
//! the encoder rather than failing to compile, so `test/opus_layout.rs` pins every value.
#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int};

use anyhow::{anyhow, Context as _, Result};
use libloading::{Library, Symbol};

// ─── Error codes (opus_defines.h) ───────────────────────────────────────────
pub const OPUS_OK: c_int = 0;
pub const OPUS_BAD_ARG: c_int = -1;
pub const OPUS_BUFFER_TOO_SMALL: c_int = -2;
pub const OPUS_INTERNAL_ERROR: c_int = -3;
pub const OPUS_INVALID_PACKET: c_int = -4;
pub const OPUS_UNIMPLEMENTED: c_int = -5;
pub const OPUS_INVALID_STATE: c_int = -6;
pub const OPUS_ALLOC_FAIL: c_int = -7;

// ─── Pre-defined CTL values ─────────────────────────────────────────────────
pub const OPUS_AUTO: c_int = -1000;
pub const OPUS_BITRATE_MAX: c_int = -1;

pub const OPUS_APPLICATION_VOIP: c_int = 2048;
pub const OPUS_APPLICATION_AUDIO: c_int = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;

pub const OPUS_SIGNAL_VOICE: c_int = 3001;
pub const OPUS_SIGNAL_MUSIC: c_int = 3002;

// ─── Encoder CTL request numbers (SETs even, GETs odd) ──────────────────────
pub const OPUS_SET_APPLICATION_REQUEST: c_int = 4000;
pub const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
pub const OPUS_SET_MAX_BANDWIDTH_REQUEST: c_int = 4004;
pub const OPUS_SET_VBR_REQUEST: c_int = 4006;
pub const OPUS_SET_BANDWIDTH_REQUEST: c_int = 4008;
pub const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
pub const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
pub const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
pub const OPUS_SET_DTX_REQUEST: c_int = 4016;
pub const OPUS_SET_VBR_CONSTRAINT_REQUEST: c_int = 4020;
pub const OPUS_SET_FORCE_CHANNELS_REQUEST: c_int = 4022;
pub const OPUS_SET_SIGNAL_REQUEST: c_int = 4024;
pub const OPUS_GET_LOOKAHEAD_REQUEST: c_int = 4027;
pub const OPUS_RESET_STATE: c_int = 4028;
pub const OPUS_GET_SAMPLE_RATE_REQUEST: c_int = 4029;
pub const OPUS_SET_LSB_DEPTH_REQUEST: c_int = 4036;

/// Opaque encoder handle (`OpusEncoder`).
#[repr(C)]
pub struct OpusEncoder {
    _private: [u8; 0],
}

type FnEncoderGetSize = unsafe extern "C" fn(c_int) -> c_int;
type FnEncoderCreate = unsafe extern "C" fn(i32, c_int, c_int, *mut c_int) -> *mut OpusEncoder;
type FnEncode = unsafe extern "C" fn(*mut OpusEncoder, *const i16, c_int, *mut u8, i32) -> i32;
type FnEncodeFloat = unsafe extern "C" fn(*mut OpusEncoder, *const f32, c_int, *mut u8, i32) -> i32;
type FnEncoderDestroy = unsafe extern "C" fn(*mut OpusEncoder);
type FnStrerror = unsafe extern "C" fn(c_int) -> *const c_char;
type FnGetVersion = unsafe extern "C" fn() -> *const c_char;

// `opus_encoder_ctl(st, request, ...)` is variadic. Every control we issue passes exactly
// one trailing argument — an `i32` for SETs, an `*mut i32` for the GETs we use. On the
// x86_64 Windows C ABI both of those pass in the same integer register slot as the first
// variadic argument would, so binding the one symbol under two concrete (non-variadic)
// signatures is ABI-safe here. This mirrors how audiopus_sys/opus-rs treat the same call.
type FnEncoderCtlSet = unsafe extern "C" fn(*mut OpusEncoder, c_int, i32) -> c_int;
type FnEncoderCtlGet = unsafe extern "C" fn(*mut OpusEncoder, c_int, *mut i32) -> c_int;

pub struct OpusApi {
    pub encoder_get_size: FnEncoderGetSize,
    pub encoder_create: FnEncoderCreate,
    pub encode: FnEncode,
    pub encode_float: FnEncodeFloat,
    pub encoder_destroy: FnEncoderDestroy,
    pub encoder_ctl_set: FnEncoderCtlSet,
    pub encoder_ctl_get: FnEncoderCtlGet,
    pub strerror: FnStrerror,
    pub get_version: FnGetVersion,
    _lib: Library,
}

// SAFETY: the function pointers are stateless entry points into libopus; the encoder state
// they operate on is owned separately (`OpusEncoderHandle`) and is not shared across threads
// without external synchronization. The `Library` stays loaded for the process lifetime.
unsafe impl Send for OpusApi {}
unsafe impl Sync for OpusApi {}

const LIB_NAMES: &[&str] = &["libopus.dll", "opus.dll", "libopus-0.dll"];

fn open_library() -> Result<Library> {
    let mut last_err: Option<String> = None;
    let attempt = |candidate: std::path::PathBuf, last_err: &mut Option<String>| match unsafe {
        Library::new(&candidate)
    } {
        Ok(lib) => Some(lib),
        Err(e) => {
            *last_err = Some(format!("{}: {e}", candidate.display()));
            None
        }
    };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in LIB_NAMES {
                if let Some(lib) = attempt(dir.join("resources").join(name), &mut last_err) {
                    return Ok(lib);
                }
                if let Some(lib) = attempt(dir.join(name), &mut last_err) {
                    return Ok(lib);
                }
            }
        }
    }

    for name in LIB_NAMES {
        if let Some(lib) = attempt(std::path::PathBuf::from(name), &mut last_err) {
            return Ok(lib);
        }
    }

    Err(anyhow!(
        "could not load libopus (tried bundled resources, exe dir, and PATH for {:?}): {}",
        LIB_NAMES,
        last_err.unwrap_or_default()
    ))
}

impl OpusApi {
    pub fn load() -> Result<Self> {
        let lib = open_library()?;
        // SAFETY: symbol names are the documented libopus C exports; the fn-pointer types
        // match the signatures in `.sources/repos/opus/include/opus.h`.
        unsafe {
            let encoder_get_size: Symbol<FnEncoderGetSize> = lib
                .get(b"opus_encoder_get_size\0")
                .context("resolving opus_encoder_get_size")?;
            let encoder_create: Symbol<FnEncoderCreate> = lib
                .get(b"opus_encoder_create\0")
                .context("resolving opus_encoder_create")?;
            let encode: Symbol<FnEncode> =
                lib.get(b"opus_encode\0").context("resolving opus_encode")?;
            let encode_float: Symbol<FnEncodeFloat> = lib
                .get(b"opus_encode_float\0")
                .context("resolving opus_encode_float")?;
            let encoder_destroy: Symbol<FnEncoderDestroy> = lib
                .get(b"opus_encoder_destroy\0")
                .context("resolving opus_encoder_destroy")?;
            let encoder_ctl_set: Symbol<FnEncoderCtlSet> = lib
                .get(b"opus_encoder_ctl\0")
                .context("resolving opus_encoder_ctl (set view)")?;
            let encoder_ctl_get: Symbol<FnEncoderCtlGet> = lib
                .get(b"opus_encoder_ctl\0")
                .context("resolving opus_encoder_ctl (get view)")?;
            let strerror: Symbol<FnStrerror> = lib
                .get(b"opus_strerror\0")
                .context("resolving opus_strerror")?;
            let get_version: Symbol<FnGetVersion> = lib
                .get(b"opus_get_version_string\0")
                .context("resolving opus_get_version_string")?;

            Ok(Self {
                encoder_get_size: *encoder_get_size,
                encoder_create: *encoder_create,
                encode: *encode,
                encode_float: *encode_float,
                encoder_destroy: *encoder_destroy,
                encoder_ctl_set: *encoder_ctl_set,
                encoder_ctl_get: *encoder_ctl_get,
                strerror: *strerror,
                get_version: *get_version,
                _lib: lib,
            })
        }
    }

    /// Human-readable libopus version (e.g. `"libopus 1.5.2"`).
    pub fn version(&self) -> String {
        // SAFETY: `opus_get_version_string` returns a static NUL-terminated C string.
        unsafe {
            let ptr = (self.get_version)();
            if ptr.is_null() {
                return "unknown".to_string();
            }
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }

    /// Map an opus error code to its human-readable string.
    pub fn strerror(&self, code: c_int) -> String {
        // SAFETY: `opus_strerror` returns a static NUL-terminated C string for any int.
        unsafe {
            let ptr = (self.strerror)(code);
            if ptr.is_null() {
                return format!("opus error {code}");
            }
            std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}
