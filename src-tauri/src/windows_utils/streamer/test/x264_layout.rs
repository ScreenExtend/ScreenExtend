//! libx264 ABI, measured against the library that is actually loaded.
//!
//! `nvenc_layout.rs` and `amd_layout.rs` prove their bindings against a **vendored header**
//! compiled by `cl`. That is the right method there — NVENC and AMF ship no artifact this repo
//! can question at runtime, and a wrong struct revision degrades silently to another encoder.
//! It is the wrong method for libx264, and deliberately not what this file does:
//!
//! * `x264.h` is GPL-2.0-or-later. `resources/nvenc-headers/` and `resources/amf-headers/` are
//!   both MIT, which is what made vendoring them into an AGPL tree a licence question with an
//!   easy answer. Vendoring a GPL header to run a generator over is a different question and
//!   not one a test needs to ask.
//! * The header would describe *a* libx264. The one that matters is `libx264-164.dll` in
//!   `resources/`, or whatever a user's `PATH` puts in front of it — and a header cannot speak
//!   for that.
//!
//! So every assertion here is made by the loaded library. `X264Api::load` already refuses a
//! build that does not export `x264_encoder_open_164`, which is a strong ABI gate; what it
//! cannot catch is the vendored `x264_param_t` being the wrong *size* or having a field at the
//! wrong offset within a build that is otherwise correct. Those are what the two tests below
//! measure: a guard canary behind the struct catches a short binding, and a round trip through
//! `x264_encoder_parameters` catches a displaced field.
//!
//! The failure they exist for is not a crash. `x264_param_default_preset` memsets and fills a
//! struct whose size it knows and we do not; a binding one field short means libx264 writes
//! past our allocation, and a binding with a field at the wrong offset means we configure a
//! resolution or a bitrate the encoder never sees.

use std::mem::size_of;

use crate::windows_utils::streamer::x264::x264_sys::{
    x264_param_t, X264Api, X264_BUILD, X264_CSP_I420, X264_RC_ABR,
};

use super::skip;

/// Bytes of sacrificial padding placed immediately after the struct. A binding that is short by
/// one `c_int` field is the realistic mistake, so a whole cache line is generous.
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
    // `load()` refuses a mismatch outright, so reaching here means the two agree — assert it
    // anyway, because that guarantee lives in one `if` a refactor could drop.
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
    // The one thing a header-derived layout test would have caught and nothing else does:
    // if `x264_param_t` here is smaller than libx264's, `x264_param_default_preset` writes past
    // the end of it. On the stack that is a corrupted local; in `X264Encoder::new` it is a
    // corrupted neighbour, which is a crash somewhere else entirely.
    let Some(api) = api() else { return };

    // One allocation: the struct, then the guard, so anything written past the struct lands in
    // bytes we can check rather than in whatever the allocator put next.
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

    // And the same for `x264_param_apply_profile`, which the encoder calls next.
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
    // A displaced field is silent: we set `i_bitrate` at the wrong offset, libx264 encodes at
    // its default, and the only symptom is a stream that ignores the bitrate controller.
    // `x264_encoder_parameters` returns the *effective* configuration, so writing a distinctive
    // value and reading it back proves the offset — and it proves it against the loaded library
    // rather than against a header.
    let Some(api) = api() else { return };

    let mut param: x264_param_t = unsafe { std::mem::zeroed() };
    unsafe {
        assert_eq!(
            (api.param_default_preset)(&mut param, c"ultrafast".as_ptr(), c"zerolatency".as_ptr()),
            0
        );
    }

    // Values chosen to be unlike any default, so a field read from the wrong offset cannot
    // coincidentally match.
    param.i_width = 642;
    param.i_height = 362;
    // I420, like the real encoder: libx264 maps anything at or above I444 to 4:4:4, which
    // `param_apply_profile("baseline")` then rejects outright.
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
    // `i_keyint_max` is the one libx264 is allowed to reinterpret (zerolatency and the profile
    // both have opinions), so require only that it is a sane positive number rather than the
    // literal — a garbage value here still means a displaced field.
    assert!(
        effective.i_keyint_max > 0 && effective.i_keyint_max < 100_000,
        "i_keyint_max came back as {}",
        effective.i_keyint_max
    );

    unsafe { (api.encoder_close)(encoder) };
}
