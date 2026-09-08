use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::x264::encoder::{fill_synthetic_bgra, X264Encoder};
use crate::windows_utils::streamer::x264::x264_sys::X264_CSP_I420;

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
    super::show_measurements();
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
    super::show_measurements();
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

/// Convert a BGRA frame to NV12 with the same BT.601 limited-range matrix libx264 would use, so
/// the two paths can be compared on identical pixels rather than on the encoder's guess.
fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h + w * (h / 2)];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for row in 0..h {
        for col in 0..w {
            let p = row * w * 4 + col * 4;
            let (b, g, r) = (bgra[p] as f32, bgra[p + 1] as f32, bgra[p + 2] as f32);
            y_plane[row * w + col] =
                (16.0 + 0.257 * r + 0.504 * g + 0.098 * b).clamp(0.0, 255.0) as u8;
        }
    }
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let mut sb = 0.0f32;
            let mut sg = 0.0f32;
            let mut sr = 0.0f32;
            for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let p = (row + dr) * w * 4 + (col + dc) * 4;
                sb += bgra[p] as f32;
                sg += bgra[p + 1] as f32;
                sr += bgra[p + 2] as f32;
            }
            let (r, g, b) = (sr / 4.0, sg / 4.0, sb / 4.0);
            let i = (row / 2) * w + col;
            uv_plane[i] = (128.0 - 0.148 * r - 0.291 * g + 0.439 * b).clamp(0.0, 255.0) as u8;
            uv_plane[i + 1] = (128.0 + 0.439 * r - 0.368 * g - 0.071 * b).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// NV12 straight from the scaler must produce the same *kind* of stream as the BGRA path.
///
/// The whole reason `ReadbackFormat::Nv12` exists is that the GPU's video processor does the
/// scale and the colour conversion in one blt, so the CPU colour-matrix pass disappears. That is
/// only worth having if the result is still a stream the client can decode: the BGRA fast path
/// this replaces produced profile_idc 244 / 4:4:4 and passed its own test for months
/// (`blocker-x264-bgra-input-emits-444-stream`). So this asserts the SPS, not just that bytes
/// came out — and asserts it for both entry points, from the same pixels.
#[test]
fn x264_nv12_input_matches_the_bgra_path() {
    const W: u32 = 640;
    const H: u32 = 360;

    let cfg = || EncoderConfig {
        width: W,
        height: H,
        fps: 60,
        bitrate_bps: 4_000_000,
        max_bitrate_bps: 4_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    };

    let mut bgra_enc = match X264Encoder::new(cfg()) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 NV12 input test (libx264 unavailable)", e);
            return;
        }
    };
    let mut nv12_enc = X264Encoder::new(cfg()).expect("second x264 encoder");

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut bgra_total = 0usize;
    let mut nv12_total = 0usize;
    let mut nv12_sps = None;
    let mut nv12_us: Vec<u128> = Vec::new();
    let mut bgra_us: Vec<u128> = Vec::new();

    for i in 0..30u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let nv12 = bgra_to_nv12(&frame, W as usize, H as usize);

        let t = std::time::Instant::now();
        let a = bgra_enc
            .encode_bgra(&frame, i == 0)
            .expect("BGRA-path encode");
        bgra_us.push(t.elapsed().as_micros());

        let t = std::time::Instant::now();
        let b = nv12_enc
            .encode_nv12_padded(&nv12, W, i == 0)
            .expect("NV12-path encode");
        nv12_us.push(t.elapsed().as_micros());

        bgra_total += a.len();
        nv12_total += b.len();
        if i == 0 {
            assert!(
                starts_with_annexb(&b),
                "the NV12 path must emit Annex-B like the BGRA path"
            );
            assert!(
                is_idr_with_headers(&b),
                "the first NV12 frame must be an IDR carrying SPS and PPS"
            );
            nv12_sps = parse_sps(&b);
        }
    }

    let sps = nv12_sps.expect("the NV12 path must emit a parseable SPS");
    assert_eq!(
        sps.profile_idc, 66,
        "NV12 input must still honour the requested Baseline profile, got {sps:?}"
    );
    assert_eq!(
        sps.chroma_format_idc, 1,
        "NV12 input must produce a 4:2:0 stream, got {sps:?}"
    );

    // Same content through the same encoder settings: the sizes should be close. A large gap
    // means one path is coding something different from what it was handed.
    let ratio = nv12_total as f64 / bgra_total as f64;
    assert!(
        (0.5..2.0).contains(&ratio),
        "NV12 output {nv12_total} B and BGRA output {bgra_total} B differ by more than 2x; \
         the NV12 planes are probably being read as something else"
    );

    // Repeating must work off the retained NV12 frame, not a stale I420 one.
    let repeat = nv12_enc.encode_repeat(true).expect("NV12 repeat encode");
    assert!(
        is_idr_with_headers(&repeat),
        "a forced repeat on the NV12 path must produce an IDR with headers"
    );
    assert!(
        !nal_units(&repeat).is_empty(),
        "the NV12 repeat must carry NALs"
    );

    bgra_us.sort_unstable();
    nv12_us.sort_unstable();
    let med = |v: &[u128]| v[v.len() / 2];
    eprintln!(
        "x264 NV12 input OK: {W}x{H}, sps={sps:?}, bytes bgra={bgra_total} nv12={nv12_total} \
         (ratio {ratio:.2}), encode median bgra={}us nv12={}us",
        med(&bgra_us),
        med(&nv12_us),
    );
}

/// Every latency-critical x264 setting, read back from the encoder rather than from the request.
///
/// `ultrafast` is a preset and `zerolatency` is a tune; both move fields, `param_apply_profile`
/// moves more, and `x264_encoder_open` validates and can move them again. So until
/// `x264_encoder_parameters` was bound, everything this codebase said about the software
/// encoder's configuration was a statement about what had been asked for. Each field below is a
/// distinct way for the encoder to be quietly buffering a frame.
#[test]
fn x264_zerolatency_settings_were_accepted() {
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 60;

    let enc = match X264Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_bps: 8_000_000,
        max_bitrate_bps: 8_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: true,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 zerolatency verification (libx264 unavailable)", e);
            return;
        }
    };
    let p = enc.effective_params();

    assert_eq!(p.i_bframe, 0, "any B frame is a reordering delay");
    assert_eq!(
        p.b_sliced_threads, 1,
        "frame threads pipeline whole frames; sliced threads do not"
    );
    assert_eq!(p.rc.i_lookahead, 0, "rc-lookahead buffers frames");
    assert_eq!(p.i_sync_lookahead, 0, "sync-lookahead buffers frames");
    assert_eq!(
        p.b_repeat_headers, 1,
        "a client joining mid-stream needs SPS/PPS in band"
    );
    assert_eq!(p.b_annexb, 1, "the packetizer walks Annex-B start codes");
    assert_eq!(
        p.i_scenecut_threshold, 0,
        "a scene cut would insert an unrequested keyframe into an infinite GOP"
    );
    assert_eq!(p.i_csp, X264_CSP_I420, "the internal csp must stay 4:2:0");

    // Intra refresh was asked for, so keyint is the refresh period, not an IDR interval:
    // libx264 advances the refresh column by `mb_width / i_keyint_max` macroblocks per frame,
    // which is why leaving keyint infinite made the wave move ~1e-9 MB/frame and never sweep.
    assert_eq!(p.b_intra_refresh, 1, "intra refresh was requested");
    assert_eq!(
        p.i_keyint_max, FPS as std::ffi::c_int,
        "with intra refresh on, keyint is the sweep period and must be finite"
    );

    // The VBV is what makes ABR behave like CBR without HRD signalling and its filler data.
    assert!(
        p.rc.i_vbv_max_bitrate > 0 && p.rc.i_vbv_buffer_size > 0,
        "ABR without a VBV targets an average over the whole stream, which is not a per-frame \
         budget: maxrate={} bufsize={}",
        p.rc.i_vbv_max_bitrate,
        p.rc.i_vbv_buffer_size
    );
    assert_eq!(
        p.rc.i_vbv_max_bitrate, p.rc.i_bitrate,
        "peak must equal target or the VBV is a ceiling the encoder is allowed to exceed on average"
    );

    // BT.709 on both halves; see `bt709_y` and the VUI block in `X264Encoder::new`.
    assert_eq!(p.vui.i_colmatrix, 1, "matrix_coefficients must say BT.709");
    assert_eq!(p.vui.i_colorprim, 1, "colour_primaries must say BT.709");
    assert_eq!(
        p.vui.i_transfer, 1,
        "transfer_characteristics must say BT.709"
    );
    assert_eq!(p.vui.b_fullrange, 0, "the samples are limited range");

    eprintln!(
        "x264 effective params OK: threads={} sliced={} bframes={} rc_lookahead={} \
         sync_lookahead={} keyint={} intra_refresh={} scenecut={} nal_hrd={} \
         vbv_maxrate={} vbv_bufsize={} (build {})",
        p.i_threads,
        p.b_sliced_threads,
        p.i_bframe,
        p.rc.i_lookahead,
        p.i_sync_lookahead,
        p.i_keyint_max,
        p.b_intra_refresh,
        p.i_scenecut_threshold,
        p.i_nal_hrd,
        p.rc.i_vbv_max_bitrate,
        p.rc.i_vbv_buffer_size,
        enc.build(),
    );
}

/// Does the VBV actually bind, or is `nal-hrd=cbr` needed?
///
/// `minor-x264-rate-control-is-abr-without-nal-hrd-cbr` asks whether ABR-with-VBV holds the
/// stream to a per-frame budget or merely to a long-run average. The answer decides whether to
/// set `i_nal_hrd = X264_NAL_HRD_CBR`, which would also start inserting filler data — bandwidth
/// spent on nothing, which is why NVENC disables filler explicitly and Quick Sync turns HRD
/// conformance off.
///
/// So: measure the access-unit size distribution against the per-frame budget rather than
/// arguing about it.
#[test]
fn x264_vbv_bounds_the_access_unit_size() {
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 60;
    const BITRATE: u32 = 8_000_000;
    const FRAMES: u32 = 240;

    let mut enc = match X264Encoder::new(EncoderConfig {
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
            super::skip("x264 VBV measurement (libx264 unavailable)", e);
            return;
        }
    };

    let budget_bytes = (BITRATE / 8 / FPS) as usize;
    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut sizes: Vec<usize> = Vec::with_capacity(FRAMES as usize);
    let mut idr_bytes = 0usize;
    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = enc.encode_bgra(&frame, i == 0).expect("encode");
        if i == 0 {
            idr_bytes = au.len();
        } else if !au.is_empty() {
            sizes.push(au.len());
        }
    }

    sizes.sort_unstable();
    let pick = |pct: usize| sizes[(sizes.len() * pct / 100).min(sizes.len() - 1)];
    let (p50, p95, p99, max) = (pick(50), pick(95), pick(99), *sizes.last().unwrap());
    let mean: f64 = sizes.iter().sum::<usize>() as f64 / sizes.len() as f64;

    eprintln!(
        "x264 AU sizes at {}x{}@{} / {} Mbps (budget {budget_bytes} B/frame): \
         mean={mean:.0} p50={p50} p95={p95} p99={p99} max={max} ({:.2}x budget), idr={idr_bytes} B",
        W,
        H,
        FPS,
        BITRATE / 1_000_000,
        max as f64 / budget_bytes as f64,
    );

    // The VBV is two frame budgets (`vbv_buffer_kbit`), so a P frame may spend up to about that
    // and no more. Four is slack for the measurement, not a target: what would fail here is ABR
    // ignoring the VBV and targeting a whole-stream average, which is the case `nal-hrd=cbr`
    // would exist to fix.
    assert!(
        max < budget_bytes * 4,
        "largest P access unit {max} B is more than 4x the {budget_bytes} B per-frame budget; \
         the VBV is not binding and ABR is targeting a long-run average"
    );
    // And the mean must actually land near the target, or the rate control is not tracking.
    assert!(
        mean < budget_bytes as f64 * 1.5,
        "mean access unit {mean:.0} B against a {budget_bytes} B budget: rate control is \
         overshooting the target"
    );
}
