use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::x264::encoder::{fill_synthetic_bgra, X264Encoder};

use super::h264::{is_idr_with_headers, nal_units, parse_sps, starts_with_annexb};

/// Evidence for `blocker-x264-bgra-input-emits-444-stream`, kept so nobody re-derives it:
/// hand the shipped libx264 a packed BGRA csp and read back what it actually produces.
///
/// `x264_param_apply_profile("baseline")` runs first and succeeds; changing `i_csp` afterwards
/// bypasses its 4:4:4 rejection. libx264 does no RGB->YUV of its own — `frame_internal_csp`
/// maps every csp at or above `X264_CSP_I444` to I444, and `x264_sps_init` then forces
/// `PROFILE_HIGH444_PREDICTIVE` before any baseline/main/high decision is reached.
#[test]
fn x264_bgra_csp_would_emit_a_444_stream() {
    use std::ffi::c_int;

    use crate::windows_utils::streamer::x264::x264_sys::*;

    const W: c_int = 320;
    const H: c_int = 240;

    let api = match X264Api::load() {
        Ok(a) => a,
        Err(e) => {
            super::skip("x264 BGRA-444 evidence test (libx264 unavailable)", e);
            return;
        }
    };

    let mut p: x264_param_t = unsafe { std::mem::zeroed() };
    assert_eq!(
        unsafe {
            (api.param_default_preset)(&mut p, c"ultrafast".as_ptr(), c"zerolatency".as_ptr())
        },
        0
    );
    p.i_width = W;
    p.i_height = H;
    p.i_csp = X264_CSP_I420;
    p.i_fps_num = 60;
    p.i_fps_den = 1;
    p.b_annexb = 1;
    p.b_repeat_headers = 1;
    p.i_log_level = X264_LOG_NONE;
    assert_eq!(
        unsafe { (api.param_apply_profile)(&mut p, c"baseline".as_ptr()) },
        0,
        "apply_profile(baseline) must succeed on the I420 params"
    );

    p.i_csp = X264_CSP_BGRA;
    let handle = unsafe { (api.encoder_open)(&mut p) };
    if handle.is_null() {
        eprintln!("x264 BGRA-444 evidence: this build refuses a packed BGRA csp outright");
        return;
    }

    let mut bgra = vec![0u8; (W * H * 4) as usize];
    for (i, px) in bgra.chunks_exact_mut(4).enumerate() {
        px[0] = i as u8;
        px[1] = (i >> 3) as u8;
        px[2] = (i >> 5) as u8;
        px[3] = 255;
    }

    let mut pic_in: x264_picture_t = unsafe { std::mem::zeroed() };
    unsafe { (api.picture_init)(&mut pic_in) };
    pic_in.img.i_csp = X264_CSP_BGRA;
    pic_in.img.i_plane = 1;
    pic_in.img.plane[0] = bgra.as_mut_ptr();
    pic_in.img.i_stride[0] = W * 4;
    pic_in.i_type = X264_TYPE_IDR;

    let mut nals: *mut x264_nal_t = std::ptr::null_mut();
    let mut n_nal: c_int = 0;
    let mut pic_out: x264_picture_t = unsafe { std::mem::zeroed() };
    let size =
        unsafe { (api.encoder_encode)(handle, &mut nals, &mut n_nal, &mut pic_in, &mut pic_out) };
    let au = if size > 0 && !nals.is_null() {
        unsafe { std::slice::from_raw_parts((*nals).p_payload, size as usize) }.to_vec()
    } else {
        Vec::new()
    };
    unsafe { (api.encoder_close)(handle) };

    let sps = parse_sps(&au).expect("the BGRA encoder still emits a parseable SPS");
    eprintln!("x264 BGRA-444 evidence: {sps:?}");
    assert_eq!(
        sps.profile_idc, 244,
        "the BGRA fast path was expected to yield High 4:4:4 Predictive; got {sps:?}"
    );
    assert_eq!(sps.chroma_format_idc, 3, "expected 4:4:4 chroma ({sps:?})");
    assert!(
        is_idr_with_headers(&au),
        "and it satisfies SPS+PPS+IDR, which is why the old test passed"
    );
}

#[test]
fn x264_software_encodes_synthetic_frames() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 60;
    const BITRATE: u32 = 8_000_000;
    const FRAMES: u32 = 120;

    let mut encoder = match X264Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_bps: BITRATE,
        max_bitrate_bps: BITRATE,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 software smoke test (libx264 unavailable)", e);
            return;
        }
    };

    let vbv_bytes = (2.0 * (BITRATE as f64 / FPS as f64) / 8.0) as usize;
    let per_nal_ceiling = vbv_bytes * 4;

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut total_bytes = 0usize;
    let mut outputs = 0u32;
    let mut max_nal = 0usize;
    let forced_idr_frame = 60u32;
    let mut first_ok = false;
    let mut forced_ok = false;

    let t0 = std::time::Instant::now();
    let mut per_frame_us: Vec<u128> = Vec::with_capacity(FRAMES as usize);

    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let force = i == 0 || i == forced_idr_frame;
        let start = std::time::Instant::now();
        let au = encoder
            .encode_bgra(&frame, force)
            .unwrap_or_else(|e| panic!("encode frame {i} failed: {e:?}"));
        per_frame_us.push(start.elapsed().as_micros());

        assert!(
            !au.is_empty(),
            "frame {i}: zerolatency must emit output every frame"
        );
        outputs += 1;
        total_bytes += au.len();
        assert!(
            starts_with_annexb(&au),
            "frame {i}: output must start with an Annex-B start code"
        );

        for (_, len) in nal_units(&au) {
            max_nal = max_nal.max(len);
            assert!(
                len <= per_nal_ceiling,
                "frame {i}: NAL payload {len} exceeds VBV-implied ceiling {per_nal_ceiling} \
                 (VBV not bounding frame size?)"
            );
        }

        if i == 0 {
            first_ok = is_idr_with_headers(&au);
            let sps = parse_sps(&au).expect("frame 0 must carry a parseable SPS");
            // The regression this guards: feeding libx264 a packed BGRA csp silently produced
            // profile_idc 244 / chroma_format_idc 3 (High 4:4:4 Predictive), which satisfies
            // "SPS+PPS+IDR present" and decodes on no browser.
            assert!(
                matches!(sps.profile_idc, 66 | 77 | 100),
                "SPS profile_idc {} is not Baseline/Main/High — the client negotiated \
                 profile-level-id 42e01f and cannot decode this ({sps:?})",
                sps.profile_idc
            );
            assert_eq!(
                sps.chroma_format_idc, 1,
                "SPS must be 4:2:0; chroma_format_idc {} means the encoder was handed a \
                 packed/444 colour space ({sps:?})",
                sps.chroma_format_idc
            );
            eprintln!("x264 SPS: {sps:?}");
        }
        if i == forced_idr_frame {
            forced_ok = is_idr_with_headers(&au);
        }
    }

    assert_eq!(
        outputs, FRAMES,
        "frame-in/packet-out: {FRAMES} frames must yield {FRAMES} outputs"
    );
    assert!(first_ok, "frame 0 must contain SPS + PPS + IDR");
    assert!(
        forced_ok,
        "mid-stream forced keyframe must contain SPS + PPS + IDR"
    );

    let achieved_bps = (total_bytes as f64 * 8.0 * FPS as f64) / FRAMES as f64;
    let ratio = achieved_bps / BITRATE as f64;
    assert!(
        (0.6..=1.4).contains(&ratio),
        "achieved bitrate {achieved_bps:.0} bps is too far from target {BITRATE} (ratio {ratio:.2})"
    );

    per_frame_us.sort_unstable();
    let p95 = per_frame_us[(per_frame_us.len() * 95 / 100).min(per_frame_us.len() - 1)];
    let frame_budget_us = 1_000_000u128 / FPS as u128;
    // `eprintln!`, not `tprintln!`: the log bus ring is never drained under `cargo test`,
    // so a measurement written with `tprintln!` is discarded at exit.
    eprintln!(
        "x264 smoke OK: {FRAMES} frames {W}x{H}@{FPS}, {total_bytes} bytes, achieved {:.0} kbps \
         (ratio {ratio:.2}), max_nal={max_nal}B (ceiling {per_nal_ceiling}B), \
         p95_encode={p95}us (budget {frame_budget_us}us), wall={}ms",
        achieved_bps / 1000.0,
        t0.elapsed().as_millis()
    );
}

#[test]
fn x264_software_runtime_bitrate_change() {
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 60;
    const HIGH: u32 = 10_000_000;
    const LOW: u32 = 2_000_000;

    let mut encoder = match X264Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_bps: HIGH,
        max_bitrate_bps: HIGH,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 runtime-bitrate test (libx264 unavailable)", e);
            return;
        }
    };

    let mut frame = vec![0u8; (W * H * 4) as usize];

    let mut high_bytes = 0usize;
    for i in 0..60 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .expect("high-rate encode");
        if i >= 5 {
            high_bytes += au.len();
        }
    }

    encoder
        .set_bitrate(LOW)
        .expect("runtime bitrate reconfigure");

    let mut low_bytes = 0usize;
    for i in 60..120 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder.encode_bgra(&frame, false).expect("low-rate encode");
        low_bytes += au.len();
    }

    assert!(
        low_bytes < high_bytes,
        "lowering the target bitrate should shrink output: high={high_bytes}B low={low_bytes}B"
    );
    eprintln!(
        "x264 runtime bitrate change OK: high segment {high_bytes}B -> low segment {low_bytes}B"
    );
}
