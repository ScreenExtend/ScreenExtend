//! Minimal, hand-vendored FFI for libx264, loaded at runtime with `libloading`.
//!
//! We deliberately do **not** use the `x264-sys` crate (build-time pkg-config/vcpkg link).
//! This mirrors the project's `nvenc_sys` convention: vendor exactly the structs/enums we
//! touch and dynamically load the shared library, so a normal `cargo build` never needs
//! libx264 present and the DLL is only required at runtime when the software backend is
//! actually selected (the VM / no-GPU fallback case).
//!
//! # ABI safety
//! `x264_param_default_preset` and `x264_encoder_open` read/write the *entire* `x264_param_t`,
//! so its layout must be byte-exact against the loaded library. The structs below are
//! transcribed verbatim from x264.h at **`X264_BUILD 164`** (the current/vcpkg build). The
//! bundled `libx264.dll` MUST be a build-164-compatible library. `#[repr(C)]` reproduces the
//! platform C ABI (field order + alignment/padding) identically to the MSVC/Clang compiler.

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void};

use anyhow::{Context as _, Result, anyhow};
use libloading::{Library, Symbol};

/// The x264 build the bindings below match. The public `x264_encoder_open` is a macro that
/// expands to `x264_encoder_open_<X264_BUILD>`, so this is also the primary exported symbol
/// suffix we resolve.
pub const X264_BUILD: i32 = 164;

// --- colour space (i_csp) ---
pub const X264_CSP_MASK: c_int = 0x00ff;
pub const X264_CSP_I420: c_int = 0x0002;
pub const X264_CSP_NV12: c_int = 0x0004;
pub const X264_CSP_BGRA: c_int = 0x000f;

// --- picture type (i_type) ---
pub const X264_TYPE_AUTO: c_int = 0x0000;
pub const X264_TYPE_IDR: c_int = 0x0001;
pub const X264_TYPE_I: c_int = 0x0002;
pub const X264_TYPE_P: c_int = 0x0003;

// --- rate control (rc.i_rc_method) ---
pub const X264_RC_CQP: c_int = 0;
pub const X264_RC_CRF: c_int = 1;
pub const X264_RC_ABR: c_int = 2;

// --- log level (i_log_level) ---
pub const X264_LOG_NONE: c_int = -1;
pub const X264_LOG_ERROR: c_int = 0;
pub const X264_LOG_WARNING: c_int = 1;
pub const X264_LOG_INFO: c_int = 2;
pub const X264_LOG_DEBUG: c_int = 3;

pub const X264_KEYINT_MAX_INFINITE: c_int = 1 << 30;

// --- NAL unit types (nal_unit_type_e) ---
pub const NAL_UNKNOWN: c_int = 0;
pub const NAL_SLICE: c_int = 1;
pub const NAL_SLICE_IDR: c_int = 5;
pub const NAL_SEI: c_int = 6;
pub const NAL_SPS: c_int = 7;
pub const NAL_PPS: c_int = 8;
pub const NAL_AUD: c_int = 9;
pub const NAL_FILLER: c_int = 12;

/// Opaque encoder handle (`x264_t`).
#[repr(C)]
pub struct x264_t {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_nal_t {
    pub i_ref_idc: c_int,
    pub i_type: c_int,
    pub b_long_startcode: c_int,
    pub i_first_mb: c_int,
    pub i_last_mb: c_int,
    pub i_payload: c_int,
    pub p_payload: *mut u8,
    pub i_padding: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_zone_t {
    pub i_start: c_int,
    pub i_end: c_int,
    pub b_force_qp: c_int,
    pub i_qp: c_int,
    pub f_bitrate_factor: f32,
    pub param: *mut x264_param_t,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_vui {
    pub i_sar_height: c_int,
    pub i_sar_width: c_int,
    pub i_overscan: c_int,
    pub i_vidformat: c_int,
    pub b_fullrange: c_int,
    pub i_colorprim: c_int,
    pub i_transfer: c_int,
    pub i_colmatrix: c_int,
    pub i_chroma_loc: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_analyse {
    pub intra: c_uint,
    pub inter: c_uint,
    pub b_transform_8x8: c_int,
    pub i_weighted_pred: c_int,
    pub b_weighted_bipred: c_int,
    pub i_direct_mv_pred: c_int,
    pub i_chroma_qp_offset: c_int,
    pub i_me_method: c_int,
    pub i_me_range: c_int,
    pub i_mv_range: c_int,
    pub i_mv_range_thread: c_int,
    pub i_subpel_refine: c_int,
    pub b_chroma_me: c_int,
    pub b_mixed_references: c_int,
    pub i_trellis: c_int,
    pub b_fast_pskip: c_int,
    pub b_dct_decimate: c_int,
    pub i_noise_reduction: c_int,
    pub f_psy_rd: f32,
    pub f_psy_trellis: f32,
    pub b_psy: c_int,
    pub b_mb_info: c_int,
    pub b_mb_info_update: c_int,
    pub i_luma_deadzone: [c_int; 2],
    pub b_psnr: c_int,
    pub b_ssim: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_rc {
    pub i_rc_method: c_int,
    pub i_qp_constant: c_int,
    pub i_qp_min: c_int,
    pub i_qp_max: c_int,
    pub i_qp_step: c_int,
    pub i_bitrate: c_int,
    pub f_rf_constant: f32,
    pub f_rf_constant_max: f32,
    pub f_rate_tolerance: f32,
    pub i_vbv_max_bitrate: c_int,
    pub i_vbv_buffer_size: c_int,
    pub f_vbv_buffer_init: f32,
    pub f_ip_factor: f32,
    pub f_pb_factor: f32,
    pub b_filler: c_int,
    pub i_aq_mode: c_int,
    pub f_aq_strength: f32,
    pub b_mb_tree: c_int,
    pub i_lookahead: c_int,
    pub b_stat_write: c_int,
    pub psz_stat_out: *mut c_char,
    pub b_stat_read: c_int,
    pub psz_stat_in: *mut c_char,
    pub f_qcompress: f32,
    pub f_qblur: f32,
    pub f_complexity_blur: f32,
    pub zones: *mut x264_zone_t,
    pub i_zones: c_int,
    pub psz_zones: *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_crop_rect {
    pub i_left: c_int,
    pub i_top: c_int,
    pub i_right: c_int,
    pub i_bottom: c_int,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_mastering_display {
    pub b_mastering_display: c_int,
    pub i_green_x: c_int,
    pub i_green_y: c_int,
    pub i_blue_x: c_int,
    pub i_blue_y: c_int,
    pub i_red_x: c_int,
    pub i_red_y: c_int,
    pub i_white_x: c_int,
    pub i_white_y: c_int,
    pub i_display_max: i64,
    pub i_display_min: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_content_light_level {
    pub b_cll: c_int,
    pub i_max_cll: c_int,
    pub i_max_fall: c_int,
}

/// `x264_param_t` at `X264_BUILD 164`. Every field, in exact order — the whole struct is
/// written by `x264_param_default_preset`, so nothing here may be reordered or omitted.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_param_t {
    pub cpu: c_uint,
    pub i_threads: c_int,
    pub i_lookahead_threads: c_int,
    pub b_sliced_threads: c_int,
    pub b_deterministic: c_int,
    pub b_cpu_independent: c_int,
    pub i_sync_lookahead: c_int,

    pub i_width: c_int,
    pub i_height: c_int,
    pub i_csp: c_int,
    pub i_bitdepth: c_int,
    pub i_level_idc: c_int,
    pub i_frame_total: c_int,

    pub i_nal_hrd: c_int,

    pub vui: x264_param_vui,

    pub i_frame_reference: c_int,
    pub i_dpb_size: c_int,
    pub i_keyint_max: c_int,
    pub i_keyint_min: c_int,
    pub i_scenecut_threshold: c_int,
    pub b_intra_refresh: c_int,

    pub i_bframe: c_int,
    pub i_bframe_adaptive: c_int,
    pub i_bframe_bias: c_int,
    pub i_bframe_pyramid: c_int,
    pub b_open_gop: c_int,
    pub b_bluray_compat: c_int,
    pub i_avcintra_class: c_int,
    pub i_avcintra_flavor: c_int,

    pub b_deblocking_filter: c_int,
    pub i_deblocking_filter_alphac0: c_int,
    pub i_deblocking_filter_beta: c_int,

    pub b_cabac: c_int,
    pub i_cabac_init_idc: c_int,

    pub b_interlaced: c_int,
    pub b_constrained_intra: c_int,

    pub i_cqm_preset: c_int,
    pub psz_cqm_file: *mut c_char,
    pub cqm_4iy: [u8; 16],
    pub cqm_4py: [u8; 16],
    pub cqm_4ic: [u8; 16],
    pub cqm_4pc: [u8; 16],
    pub cqm_8iy: [u8; 64],
    pub cqm_8py: [u8; 64],
    pub cqm_8ic: [u8; 64],
    pub cqm_8pc: [u8; 64],

    /// `void (*)(void*, int, const char*, va_list)` — set by x264 to its default logger.
    /// We never call it ourselves; kept as an opaque pointer slot for correct struct size.
    pub pf_log: *mut c_void,
    pub p_log_private: *mut c_void,
    pub i_log_level: c_int,
    pub b_full_recon: c_int,
    pub psz_dump_yuv: *mut c_char,

    pub analyse: x264_param_analyse,
    pub rc: x264_param_rc,
    pub crop_rect: x264_param_crop_rect,

    pub i_frame_packing: c_int,

    pub mastering_display: x264_param_mastering_display,
    pub content_light_level: x264_param_content_light_level,

    pub i_alternative_transfer: c_int,

    pub b_aud: c_int,
    pub b_repeat_headers: c_int,
    pub b_annexb: c_int,
    pub i_sps_id: c_int,
    pub b_vfr_input: c_int,
    pub b_pulldown: c_int,
    pub i_fps_num: c_uint,
    pub i_fps_den: c_uint,
    pub i_timebase_num: c_uint,
    pub i_timebase_den: c_uint,

    pub b_tff: c_int,
    pub b_pic_struct: c_int,
    pub b_fake_interlaced: c_int,
    pub b_stitchable: c_int,

    pub b_opencl: c_int,
    pub i_opencl_device: c_int,
    pub opencl_device_id: *mut c_void,
    pub psz_clbin_file: *mut c_char,

    pub i_slice_max_size: c_int,
    pub i_slice_max_mbs: c_int,
    pub i_slice_min_mbs: c_int,
    pub i_slice_count: c_int,
    pub i_slice_count_max: c_int,

    /// `void (*)(void*)`
    pub param_free: *mut c_void,
    /// `void (*)(x264_t*, x264_nal_t*, void*)`
    pub nalu_process: *mut c_void,
    pub opaque: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_image_t {
    pub i_csp: c_int,
    pub i_plane: c_int,
    pub i_stride: [c_int; 4],
    pub plane: [*mut u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_image_properties_t {
    pub quant_offsets: *mut f32,
    pub quant_offsets_free: *mut c_void,
    pub mb_info: *mut u8,
    pub mb_info_free: *mut c_void,
    pub f_ssim: f64,
    pub f_psnr_avg: f64,
    pub f_psnr: [f64; 3],
    pub f_crf_avg: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_hrd_t {
    pub cpb_initial_arrival_time: f64,
    pub cpb_final_arrival_time: f64,
    pub cpb_removal_time: f64,
    pub dpb_output_time: f64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_sei_payload_t {
    pub payload_size: c_int,
    pub payload_type: c_int,
    pub payload: *mut u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_sei_t {
    pub num_payloads: c_int,
    pub payloads: *mut x264_sei_payload_t,
    pub sei_free: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct x264_picture_t {
    pub i_type: c_int,
    pub i_qpplus1: c_int,
    pub i_pic_struct: c_int,
    pub b_keyframe: c_int,
    pub i_pts: i64,
    pub i_dts: i64,
    pub param: *mut x264_param_t,
    pub img: x264_image_t,
    pub prop: x264_image_properties_t,
    pub hrd_timing: x264_hrd_t,
    pub extra_sei: x264_sei_t,
    pub opaque: *mut c_void,
}

type FnParamDefaultPreset =
    unsafe extern "C" fn(*mut x264_param_t, *const c_char, *const c_char) -> c_int;
type FnParamApplyProfile = unsafe extern "C" fn(*mut x264_param_t, *const c_char) -> c_int;
type FnEncoderOpen = unsafe extern "C" fn(*mut x264_param_t) -> *mut x264_t;
type FnEncoderHeaders =
    unsafe extern "C" fn(*mut x264_t, *mut *mut x264_nal_t, *mut c_int) -> c_int;
type FnEncoderEncode = unsafe extern "C" fn(
    *mut x264_t,
    *mut *mut x264_nal_t,
    *mut c_int,
    *mut x264_picture_t,
    *mut x264_picture_t,
) -> c_int;
type FnEncoderReconfig = unsafe extern "C" fn(*mut x264_t, *mut x264_param_t) -> c_int;
type FnEncoderIntraRefresh = unsafe extern "C" fn(*mut x264_t);
type FnEncoderDelayedFrames = unsafe extern "C" fn(*mut x264_t) -> c_int;
type FnEncoderClose = unsafe extern "C" fn(*mut x264_t);
type FnPictureInit = unsafe extern "C" fn(*mut x264_picture_t);

/// Resolved libx264 entry points. Raw fn pointers are extracted from the loaded library and
/// remain valid for as long as `_lib` is kept alive (it is, for the life of this struct).
pub struct X264Api {
    pub param_default_preset: FnParamDefaultPreset,
    pub param_apply_profile: FnParamApplyProfile,
    pub encoder_open: FnEncoderOpen,
    pub encoder_headers: FnEncoderHeaders,
    pub encoder_encode: FnEncoderEncode,
    pub encoder_reconfig: FnEncoderReconfig,
    pub encoder_intra_refresh: FnEncoderIntraRefresh,
    pub encoder_delayed_frames: FnEncoderDelayedFrames,
    pub encoder_close: FnEncoderClose,
    pub picture_init: FnPictureInit,
    _lib: Library,
}

// SAFETY: the resolved pointers are into a library that stays loaded for the struct's life,
// and libx264's stateless helpers are safe to reference from any thread. The encoder *handle*
// (`x264_t`) — not this table — carries the single-writer-thread requirement.
unsafe impl Send for X264Api {}
unsafe impl Sync for X264Api {}

/// Candidate file names for the libx264 shared object, in preference order. We ship
/// `libx264-164.dll` (bundled as a Tauri resource), so it's first; the others cover a
/// dev machine that has libx264 from vcpkg (`libx264.dll`) or another source on PATH.
const LIB_NAMES: &[&str] = &["libx264-164.dll", "libx264.dll", "x264.dll"];

fn open_library() -> Result<Library> {
    let mut last_err: Option<String> = None;
    let mut attempt = |candidate: std::path::PathBuf, last_err: &mut Option<String>| {
        match unsafe { Library::new(&candidate) } {
            Ok(lib) => Some(lib),
            Err(e) => {
                *last_err = Some(format!("{}: {e}", candidate.display()));
                None
            }
        }
    };

    // 1. Explicit exe-relative locations. In a packaged Tauri app the bundled DLL lands in
    //    `<exe_dir>/resources/`; Windows' default DLL search does NOT look there, so we must
    //    name the full path. Also try directly beside the exe.
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

    // 2. Bare names: lets the OS loader find a DLL on PATH or in the working dir (dev builds,
    //    `cargo test` with the DLL on PATH).
    for name in LIB_NAMES {
        if let Some(lib) = attempt(std::path::PathBuf::from(name), &mut last_err) {
            return Ok(lib);
        }
    }

    // 3. Last resort: other build-suffixed names some distributions ship (e.g. libx264-155.dll).
    for build in (150..=175).rev() {
        if let Some(lib) = attempt(
            std::path::PathBuf::from(format!("libx264-{build}.dll")),
            &mut last_err,
        ) {
            return Ok(lib);
        }
    }

    Err(anyhow!(
        "could not load libx264 (tried bundled resources, exe dir, PATH for {:?}, and \
         libx264-<150..175>.dll): {}",
        LIB_NAMES,
        last_err.unwrap_or_default()
    ))
}

/// Resolve `x264_encoder_open`. The public name is a version-suffixed symbol
/// (`x264_encoder_open_<build>`); the exact suffix depends on the loaded library's build, so
/// probe our target build first, then a neighbourhood, then the unsuffixed name as a last resort.
unsafe fn resolve_encoder_open(lib: &Library) -> Result<FnEncoderOpen> {
    let primary = format!("x264_encoder_open_{X264_BUILD}");
    let names = std::iter::once(primary.clone())
        .chain((150..=175).rev().map(|b| format!("x264_encoder_open_{b}")))
        .chain(std::iter::once("x264_encoder_open".to_string()));
    for name in names {
        let cname = format!("{name}\0");
        if let Ok(sym) = lib.get::<FnEncoderOpen>(cname.as_bytes()) {
            return Ok(*sym);
        }
    }
    Err(anyhow!(
        "libx264 exports no x264_encoder_open_<build> symbol (looked for {primary} and neighbours); \
         library build likely incompatible with the vendored bindings (X264_BUILD {X264_BUILD})"
    ))
}

impl X264Api {
    pub fn load() -> Result<Self> {
        let lib = open_library()?;
        unsafe {
            let param_default_preset: Symbol<FnParamDefaultPreset> = lib
                .get(b"x264_param_default_preset\0")
                .context("resolving x264_param_default_preset")?;
            let param_apply_profile: Symbol<FnParamApplyProfile> = lib
                .get(b"x264_param_apply_profile\0")
                .context("resolving x264_param_apply_profile")?;
            let encoder_headers: Symbol<FnEncoderHeaders> = lib
                .get(b"x264_encoder_headers\0")
                .context("resolving x264_encoder_headers")?;
            let encoder_encode: Symbol<FnEncoderEncode> = lib
                .get(b"x264_encoder_encode\0")
                .context("resolving x264_encoder_encode")?;
            let encoder_reconfig: Symbol<FnEncoderReconfig> = lib
                .get(b"x264_encoder_reconfig\0")
                .context("resolving x264_encoder_reconfig")?;
            let encoder_intra_refresh: Symbol<FnEncoderIntraRefresh> = lib
                .get(b"x264_encoder_intra_refresh\0")
                .context("resolving x264_encoder_intra_refresh")?;
            let encoder_delayed_frames: Symbol<FnEncoderDelayedFrames> = lib
                .get(b"x264_encoder_delayed_frames\0")
                .context("resolving x264_encoder_delayed_frames")?;
            let encoder_close: Symbol<FnEncoderClose> = lib
                .get(b"x264_encoder_close\0")
                .context("resolving x264_encoder_close")?;
            let picture_init: Symbol<FnPictureInit> = lib
                .get(b"x264_picture_init\0")
                .context("resolving x264_picture_init")?;

            let encoder_open = resolve_encoder_open(&lib)?;

            Ok(Self {
                param_default_preset: *param_default_preset,
                param_apply_profile: *param_apply_profile,
                encoder_open,
                encoder_headers: *encoder_headers,
                encoder_encode: *encoder_encode,
                encoder_reconfig: *encoder_reconfig,
                encoder_intra_refresh: *encoder_intra_refresh,
                encoder_delayed_frames: *encoder_delayed_frames,
                encoder_close: *encoder_close,
                picture_init: *picture_init,
                _lib: lib,
            })
        }
    }
}
