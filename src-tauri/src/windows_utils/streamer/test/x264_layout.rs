//! libx264 ABI, measured against the library that is actually loaded.
//!
//! No vendored header here, unlike `nvenc_layout.rs` and `amd_layout.rs`: `x264.h` is
//! GPL-2.0-or-later, and a header cannot speak for the `libx264-164.dll` that really loads.
//! `X264Api::load` already refuses a build without `x264_encoder_open_164`; these tests catch
//! an `x264_param_t` binding that is short (libx264 writes past the allocation) or has a
//! displaced field (a resolution or bitrate the encoder never sees).

use std::mem::size_of;

use crate::windows_utils::streamer::x264::x264_sys::{
    x264_param_t, X264Api, X264_BUILD, X264_CSP_I420, X264_RC_ABR,
};

use super::skip;

/// Guard bytes right after the struct. A binding one `c_int` short is the realistic mistake,
/// so a whole cache line is generous.
const GUARD_BYTES: usize = 64;
const GUARD_PATTERN: u8 = 0xA5;

fn api() -> Option<X264Api> {
    match X264Api::load() {
        Ok(api) => Some(api),
        Err(e) => {
            skip("libx264 ABI (library unavailable)", e);
            None
        }
    }
}

#[test]
fn the_loaded_library_is_the_build_the_bindings_describe() {
    let Some(api) = api() else { return };
    // `load()` already refuses a mismatch, but that guarantee is one `if` a refactor could drop
    assert_eq!(
        api.build,
        X264_BUILD,
        "loaded {} reports build {}, bindings are pinned to {X264_BUILD}",
        api.library_path().display(),
        api.build
    );
}

#[test]
fn param_default_preset_stays_inside_the_struct_we_gave_it() {
    // if `x264_param_t` here is smaller than libx264's, `x264_param_default_preset` writes past
    // the end of it: in `X264Encoder::new` a corrupted neighbour, and a crash somewhere else
    let Some(api) = api() else { return };

    // one allocation, struct then guard, so an overrun lands in bytes the test can check
    let mut buf = vec![GUARD_PATTERN; size_of::<x264_param_t>() + GUARD_BYTES];
    let param = buf.as_mut_ptr() as *mut x264_param_t;

    unsafe {
        let rc = (api.param_default_preset)(param, c"ultrafast".as_ptr(), c"zerolatency".as_ptr());
        assert_eq!(rc, 0, "x264_param_default_preset(ultrafast, zerolatency)");
    }

    let guard = &buf[size_of::<x264_param_t>()..];
    assert!(
        guard.iter().all(|b| *b == GUARD_PATTERN),
        "libx264 wrote {} byte(s) past our x264_param_t ({} bytes): the vendored struct is \
         short for build {X264_BUILD}",
        guard.iter().filter(|b| **b != GUARD_PATTERN).count(),
        size_of::<x264_param_t>()
    );

    // same check for `x264_param_apply_profile`, which the encoder calls next
    unsafe {
        let rc = (api.param_apply_profile)(param, c"baseline".as_ptr());
        assert_eq!(rc, 0, "x264_param_apply_profile(baseline)");
    }
    let guard = &buf[size_of::<x264_param_t>()..];
    assert!(
        guard.iter().all(|b| *b == GUARD_PATTERN),
        "x264_param_apply_profile wrote past the end of x264_param_t"
    );
}

#[test]
fn the_encoder_reads_the_fields_back_from_where_we_wrote_them() {
    // a displaced field is silent: `i_bitrate` lands at the wrong offset, libx264 encodes at
    // its default, and the stream just ignores the bitrate controller. `x264_encoder_parameters`
    // returns the effective configuration, so a distinctive value read back proves the offset
    let Some(api) = api() else { return };

    let mut param: x264_param_t = unsafe { std::mem::zeroed() };
    unsafe {
        assert_eq!(
            (api.param_default_preset)(&mut param, c"ultrafast".as_ptr(), c"zerolatency".as_ptr()),
            0
        );
    }

    // unlike any default, so a field read from the wrong offset cannot match by chance
    param.i_width = 642;
    param.i_height = 362;
    // I420, like the real encoder: libx264 maps anything at or above I444 to 4:4:4, which
    // `param_apply_profile("baseline")` then rejects outright
    param.i_csp = X264_CSP_I420;
    param.i_fps_num = 37;
    param.i_fps_den = 1;
    param.i_keyint_max = 123;
    param.rc.i_rc_method = X264_RC_ABR;
    param.rc.i_bitrate = 1234; // kbit/s
    param.rc.i_vbv_max_bitrate = 1234;
    param.rc.i_vbv_buffer_size = 1234;
    param.b_repeat_headers = 1;
    param.b_annexb = 1;
    param.i_threads = 1;
    param.i_log_level = crate::windows_utils::streamer::x264::x264_sys::X264_LOG_NONE;

    unsafe {
        assert_eq!(
            (api.param_apply_profile)(&mut param, c"baseline".as_ptr()),
            0
        );
    }

    let encoder = unsafe { (api.encoder_open)(&mut param) };
    if encoder.is_null() {
        skip(
            "libx264 ABI (encoder_open refused)",
            "x264_encoder_open returned null",
        );
        return;
    }

    let mut effective: x264_param_t = unsafe { std::mem::zeroed() };
    unsafe { (api.encoder_parameters)(encoder, &mut effective) };

    assert_eq!(
        effective.i_width, 642,
        "i_width came back from a different offset"
    );
    assert_eq!(
        effective.i_height, 362,
        "i_height came back from a different offset"
    );
    assert_eq!(effective.i_csp & 0x00ff, X264_CSP_I420 & 0x00ff, "i_csp");
    assert_eq!(effective.i_fps_num, 37, "i_fps_num");
    assert_eq!(effective.i_fps_den, 1, "i_fps_den");
    assert_eq!(effective.rc.i_rc_method, X264_RC_ABR, "rc.i_rc_method");
    assert_eq!(effective.rc.i_bitrate, 1234, "rc.i_bitrate");
    assert_eq!(effective.b_annexb, 1, "b_annexb");
    assert_eq!(effective.b_repeat_headers, 1, "b_repeat_headers");
    // libx264 may reinterpret `i_keyint_max` (zerolatency and the profile both have opinions),
    // so only a sane positive number is required: garbage still means a displaced field
    assert!(
        effective.i_keyint_max > 0 && effective.i_keyint_max < 100_000,
        "i_keyint_max came back as {}",
        effective.i_keyint_max
    );

    unsafe { (api.encoder_close)(encoder) };
}
