use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::x264::encoder::{fill_synthetic_bgra, X264Encoder};
use crate::windows_utils::streamer::x264::x264_sys::X264_CSP_I420;

use super::h264::{is_idr_with_headers, nal_units, parse_sps, starts_with_annexb};

/// Why the encoder never hands libx264 a packed BGRA csp: this reads back what the shipped
/// library produces from one.
///
/// `x264_param_apply_profile("baseline")` runs first and succeeds; changing `i_csp` afterwards
/// bypasses its 4:4:4 rejection. libx264 does no RGB->YUV of its own: `frame_internal_csp`
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
    for (i, px) in bgra.as_chunks_mut::<4>().0.iter_mut().enumerate() {
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

/// The CI perf gate's x264 number (`scripts/perf-gate.mjs`, `x264 smoke OK: ... p95_encode=`).
///
/// It times the path a live session takes: an NV12 frame out of the scaler's staging readback
/// at a padded row pitch (`encode_nv12_padded`), not `encode_bgra`, which the live pipeline
/// reaches only when the scaler falls back to BGRA. The frames are generated and converted
/// outside the timed region; only the encode call is timed.
#[test]
fn x264_software_encodes_synthetic_frames() {
    super::show_measurements();
    const W: u32 = 1920;
    const H: u32 = 1080;
    /// What a D3D11 staging texture of a 1920-wide NV12 surface maps at on the drivers seen so
    /// far: the pitch is padded, never the picture width, so the per-row path is the one timed.
    const PITCH: usize = 2048;
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

    let mut bgra = vec![0u8; (W * H * 4) as usize];
    let mut nv12 = vec![0u8; PITCH * (H as usize * 3 / 2)];
    let mut total_bytes = 0usize;
    let mut outputs = 0u32;
    let mut max_nal = 0usize;
    let forced_idr_frame = 60u32;
    let mut first_ok = false;
    let mut forced_ok = false;

    let t0 = std::time::Instant::now();
    let mut per_frame_us: Vec<u128> = Vec::with_capacity(FRAMES as usize);

    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut bgra, W, H, i);
        bgra_to_nv12_padded(&bgra, W as usize, H as usize, PITCH, &mut nv12);
        let force = i == 0 || i == forced_idr_frame;
        let start = std::time::Instant::now();
        let au = encoder
            .encode_nv12_padded(&nv12, PITCH as u32, force)
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
            // a packed BGRA csp makes libx264 emit profile_idc 244 / chroma_format_idc 3
            // (High 4:4:4 Predictive), which has SPS+PPS+IDR and decodes on no browser
            assert!(
                matches!(sps.profile_idc, 66 | 77 | 100),
                "SPS profile_idc {} is not Baseline/Main/High: the client negotiated \
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
    assert_eq!(
        encoder.reordered(),
        0,
        "with 0 B-frames no access unit may come out with dts != pts"
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
    // `eprintln!`, not `tprintln!`: the log bus is never drained under `cargo test`. The
    // prefix and the `p95_encode=` / `wall=` spellings are what perf-gate.mjs parses; keep them
    eprintln!(
        "x264 smoke OK: {FRAMES} frames {W}x{H}@{FPS} path=nv12 pitch={PITCH}, {total_bytes} \
         bytes, achieved {:.0} kbps (ratio {ratio:.2}), max_nal={max_nal}B (ceiling \
         {per_nal_ceiling}B), p95_encode={p95}us (budget {frame_budget_us}us), wall={}ms",
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

/// Convert a BGRA frame to NV12 with the BT.709 limited-range integer matrix the encoder's own
/// CPU path uses (`bt709_y` / `bt709_u` / `bt709_v` in `x264/encoder.rs`), so the two entry
/// points are compared on identical samples.
fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h + w * (h / 2)];
    bgra_to_nv12_padded(bgra, w, h, w, &mut out);
    out
}

/// [`bgra_to_nv12`] into a buffer whose rows are `pitch` bytes apart, the shape a mapped D3D11
/// staging texture has. The padding bytes are left as they were.
fn bgra_to_nv12_padded(bgra: &[u8], w: usize, h: usize, pitch: usize, out: &mut [u8]) {
    assert!(pitch >= w && out.len() >= pitch * (h + h / 2));
    let clamp8 = |v: i32| v.clamp(0, 255) as u8;
    let (y_plane, uv_plane) = out.split_at_mut(pitch * h);
    for row in 0..h {
        let src = &bgra[row * w * 4..(row + 1) * w * 4];
        let dst = &mut y_plane[row * pitch..row * pitch + w];
        for (d, px) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
            let (b, g, r) = (px[0] as i32, px[1] as i32, px[2] as i32);
            *d = clamp8(((47 * r + 157 * g + 16 * b + 128) >> 8) + 16);
        }
    }
    for row in (0..h).step_by(2) {
        let r0 = &bgra[row * w * 4..(row + 1) * w * 4];
        let r1 = &bgra[(row + 1) * w * 4..(row + 2) * w * 4];
        let dst = &mut uv_plane[(row / 2) * pitch..(row / 2) * pitch + w];
        for cx in 0..w / 2 {
            let i = cx * 8;
            let avg = |o: usize| {
                (r0[i + o] as i32
                    + r0[i + 4 + o] as i32
                    + r1[i + o] as i32
                    + r1[i + 4 + o] as i32
                    + 2)
                    >> 2
            };
            let (b, g, r) = (avg(0), avg(1), avg(2));
            dst[cx * 2] = clamp8(((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128);
            dst[cx * 2 + 1] = clamp8(((112 * r - 102 * g - 10 * b + 128) >> 8) + 128);
        }
    }
}

/// NV12 straight from the scaler must produce the same *kind* of stream as the BGRA path.
///
/// Bytes coming out is not enough: a packed BGRA input yields profile_idc 244 / 4:4:4, which
/// no client decodes. So this asserts the SPS, for both entry points, from the same pixels.
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

    // same content, same settings: a large size gap means one path is coding something else
    let ratio = nv12_total as f64 / bgra_total as f64;
    assert!(
        (0.5..2.0).contains(&ratio),
        "NV12 output {nv12_total} B and BGRA output {bgra_total} B differ by more than 2x; \
         the NV12 planes are probably being read as something else"
    );

    // repeating must work off the retained NV12 frame, not a stale I420 one
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
/// moves more, and `x264_encoder_open` validates and can move them again. Each field below is
/// a distinct way for the encoder to be quietly buffering a frame.
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

    // with intra refresh, keyint is the refresh period, not an IDR interval: libx264 advances
    // the refresh column by `mb_width / i_keyint_max` macroblocks per frame, so an infinite
    // keyint moves the wave ~1e-9 MB/frame and it never sweeps
    assert_eq!(p.b_intra_refresh, 1, "intra refresh was requested");
    assert_eq!(
        p.i_keyint_max, FPS as std::ffi::c_int,
        "with intra refresh on, keyint is the sweep period and must be finite"
    );

    // the VBV is what makes ABR behave like CBR without HRD signalling and its filler data
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

    // BT.709 on both halves; see `bt709_y` and the VUI block in `X264Encoder::new`
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
/// If ABR-with-VBV held the stream only to a long-run average, the fix would be
/// `i_nal_hrd = X264_NAL_HRD_CBR`, which also inserts filler data: bandwidth spent on nothing.
/// So this measures the access-unit size distribution against the per-frame budget.
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

    // the VBV is two frame budgets (`vbv_buffer_kbit`), so a P frame may spend up to about
    // that. Four is slack for the measurement: what fails here is ABR ignoring the VBV and
    // targeting a whole-stream average
    assert!(
        max < budget_bytes * 4,
        "largest P access unit {max} B is more than 4x the {budget_bytes} B per-frame budget; \
         the VBV is not binding and ABR is targeting a long-run average"
    );
    // the mean must land near the target, or the rate control is not tracking
    assert!(
        mean < budget_bytes as f64 * 1.5,
        "mean access unit {mean:.0} B against a {budget_bytes} B budget: rate control is \
         overshooting the target"
    );
}

fn cfg_720p(bitrate: u32) -> EncoderConfig {
    EncoderConfig {
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }
}

/// A buffer too small for the picture is refused by name, and a refused frame never becomes
/// the repeat source: before any frame is accepted, a repeat is still empty.
#[test]
fn x264_rejected_nv12_names_its_entry_point_and_is_not_retained() {
    let mut enc = match X264Encoder::new(cfg_720p(4_000_000)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 rejected-frame test (libx264 unavailable)", e);
            return;
        }
    };
    let short = vec![0u8; 16];
    let err = enc
        .encode_nv12_padded(&short, 1280, true)
        .expect_err("a 16-byte buffer is not a 720p picture");
    assert!(format!("{err}").starts_with("encode_nv12_padded:"), "{err}");
    let err = enc
        .encode_nv12_unretained(&short, 1280, true)
        .expect_err("a 16-byte buffer is not a 720p picture");
    assert!(
        format!("{err}").starts_with("encode_nv12_unretained:"),
        "{err}"
    );
    assert!(
        enc.encode_repeat(true).expect("repeat").is_empty(),
        "nothing was accepted, so there is nothing to repeat"
    );
}

/// The unretained NV12 entry point encodes the caller's padded buffer in place and produces
/// exactly what the copying one does, and it never lets a plain repeat re-encode a stale
/// picture.
#[test]
fn x264_nv12_unretained_matches_the_copying_path() {
    const W: u32 = 640;
    const H: u32 = 360;
    const PITCH: usize = 704;
    // constant QP: VBV row rate control across sliced threads depends on thread timing, so two
    // ABR encoders of the same pictures need not agree to the byte
    let cfg = || EncoderConfig {
        width: W,
        height: H,
        qp: Some(26),
        ..cfg_720p(3_000_000)
    };
    let mut copying = match X264Encoder::new(cfg()) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 unretained NV12 test (libx264 unavailable)", e);
            return;
        }
    };
    let mut direct = X264Encoder::new(cfg()).expect("second x264 encoder");

    assert!(
        direct
            .encode_repeat(false)
            .expect("a repeat before any frame")
            .is_empty(),
        "a repeat before the first frame is an empty access unit, not an error"
    );

    let mut bgra = vec![0u8; (W * H * 4) as usize];
    // padding bytes are poisoned: an encoder that read them would differ from the copy
    let mut nv12 = vec![0xEEu8; PITCH * (H as usize * 3 / 2)];
    for i in 0..20u32 {
        fill_synthetic_bgra(&mut bgra, W, H, i);
        bgra_to_nv12_padded(&bgra, W as usize, H as usize, PITCH, &mut nv12);
        let a = copying
            .encode_nv12_padded(&nv12, PITCH as u32, i == 0)
            .expect("copying encode");
        let b = direct
            .encode_nv12_unretained(&nv12, PITCH as u32, i == 0)
            .expect("unretained encode");
        assert_eq!(
            a, b,
            "frame {i}: the unretained path must code exactly the picture the copying path does"
        );
    }

    assert!(
        direct.encode_repeat(true).is_err(),
        "after an unretained frame a plain repeat has nothing current to re-encode and must say \
         so, not re-encode an older retained picture"
    );
    let a = copying.encode_repeat(true).expect("retained repeat");
    let b = direct
        .encode_nv12_unretained(&nv12, PITCH as u32, true)
        .expect("caller-supplied repeat");
    assert!(is_idr_with_headers(&a) && is_idr_with_headers(&b));
}

/// The cost of the copy the unretained path removes, at 1080p with a padded pitch. Printed,
/// not asserted: a timing assertion here would be a flaky gate.
#[test]
fn x264_nv12_copy_cost_at_1080p() {
    super::show_measurements();
    const W: u32 = 1920;
    const H: u32 = 1080;
    const PITCH: usize = 2048;
    const FRAMES: u32 = 40;
    let cfg = || EncoderConfig {
        width: W,
        height: H,
        ..cfg_720p(8_000_000)
    };
    let mut copying = match X264Encoder::new(cfg()) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 NV12 copy-cost measurement (libx264 unavailable)", e);
            return;
        }
    };
    let mut direct = X264Encoder::new(cfg()).expect("second x264 encoder");
    let mut bgra = vec![0u8; (W * H * 4) as usize];
    let mut nv12 = vec![0u8; PITCH * (H as usize * 3 / 2)];
    let (mut a_us, mut b_us) = (Vec::new(), Vec::new());
    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut bgra, W, H, i);
        bgra_to_nv12_padded(&bgra, W as usize, H as usize, PITCH, &mut nv12);
        let t = std::time::Instant::now();
        copying
            .encode_nv12_padded(&nv12, PITCH as u32, i == 0)
            .expect("copying encode");
        a_us.push(t.elapsed().as_micros());
        let t = std::time::Instant::now();
        direct
            .encode_nv12_unretained(&nv12, PITCH as u32, i == 0)
            .expect("unretained encode");
        b_us.push(t.elapsed().as_micros());
    }
    a_us.sort_unstable();
    b_us.sort_unstable();
    eprintln!(
        "x264 nv12 1080p pitch {PITCH}: encode median copying={}us unretained={}us",
        a_us[a_us.len() / 2],
        b_us[b_us.len() / 2]
    );
}

#[test]
fn x264_loader_moves_past_a_library_that_is_not_build_164() {
    use crate::windows_utils::streamer::x264::x264_sys::{library_candidates, X264Api};

    if let Err(e) = X264Api::load() {
        super::skip("x264 loader order test (libx264 unavailable)", e);
        return;
    }
    // any DLL that loads and exports no x264 symbols stands in for a libx264 of another build:
    // the refusal is the same (`x264_encoder_open_164` does not resolve)
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let impostor = std::path::Path::new(&system_root)
        .join("System32")
        .join("version.dll");
    assert!(impostor.is_file(), "{} must exist", impostor.display());

    let mut candidates = vec![impostor.clone()];
    candidates.extend(library_candidates());
    let api = X264Api::load_from(&candidates)
        .expect("the search must continue past a library that loads but is refused");
    assert_ne!(
        api.library_path(),
        impostor.as_path(),
        "the impostor must have been refused"
    );

    let only_impostor = X264Api::load_from(std::slice::from_ref(&impostor));
    let err = match only_impostor {
        Ok(_) => panic!("version.dll must be refused"),
        Err(e) => format!("{e:?}"),
    };
    assert!(
        err.contains("x264_encoder_open_164") && err.contains("version.dll"),
        "the refusal must name the library and the missing ABI stamp: {err}"
    );
}

#[test]
fn x264_loader_keeps_searching_and_never_asks_for_the_current_directory() {
    use crate::windows_utils::streamer::x264::x264_sys::{first_accepted, load_flags};
    use libloading::os::windows::{
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    };
    use std::path::PathBuf;

    let candidates: Vec<PathBuf> = ["a", "b", "c"].iter().map(PathBuf::from).collect();
    let mut tried = Vec::new();
    let got = first_accepted(&candidates, |p| {
        tried.push(p.to_path_buf());
        if p == std::path::Path::new("b") {
            Ok(2)
        } else {
            Err(format!("{}: refused, wrong build", p.display()))
        }
    })
    .expect("b is accepted");
    assert_eq!(got, (2, PathBuf::from("b")));
    assert_eq!(tried, vec![PathBuf::from("a"), PathBuf::from("b")]);

    let none = first_accepted(&candidates, |_| Err::<(), _>("no".to_string()));
    assert_eq!(none.unwrap_err().len(), 3, "every refusal is reported");

    // a bare name gets the safe default set only: application dir, System32, AddDllDirectory.
    // Flags 0 (what `Library::new` passes) is the legacy order that includes the CWD and PATH
    assert_eq!(
        load_flags(std::path::Path::new("libx264.dll")),
        LOAD_LIBRARY_SEARCH_DEFAULT_DIRS
    );
    assert_eq!(
        load_flags(std::path::Path::new(r"C:\app\resources\libx264-164.dll")),
        LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS
    );
}

#[test]
fn x264_library_is_loaded_once_per_process() {
    use crate::windows_utils::streamer::x264::x264_sys::X264Api;
    let a = match X264Api::shared() {
        Ok(a) => a,
        Err(e) => {
            super::skip("x264 shared loader (libx264 unavailable)", e);
            return;
        }
    };
    let b = X264Api::shared().expect("second call");
    assert!(
        std::ptr::eq(a, b),
        "both calls must return the one cached API"
    );
}

/// With `x264_worker_tuning` the slice workers libx264 starts are found and raised.
#[test]
fn x264_worker_tuning_reaches_the_slice_workers() {
    use crate::windows_utils::streamer::x264::encoder::X264Options;
    use crate::windows_utils::streamer::x264::workers::thread_priority;
    use windows::Win32::System::Threading::{THREAD_PRIORITY_HIGHEST, THREAD_PRIORITY_NORMAL};

    let options = X264Options {
        worker_tuning: true,
        ..Default::default()
    };
    let enc = match X264Encoder::with_options(cfg_720p(6_000_000), options) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 worker tuning (libx264 unavailable)", e);
            return;
        }
    };
    let threads = enc.effective_params().i_threads;
    if threads <= 1 {
        crate::test_support::skip_because(
            "x264 worker tuning",
            "this machine gives libx264 one thread, so there is no pool to tune",
        );
        return;
    }
    let tuned = enc.tuned_workers();
    // `>=`: other tests open encoders in parallel, and their workers started in libx264 too,
    // so any that land in the same window are tuned as well
    assert!(
        tuned.len() >= threads as usize,
        "expected at least {threads} libx264 workers to be tuned, got {}",
        tuned.len()
    );
    let mut checked = 0;
    for &tid in tuned {
        // a worker belonging to another test's encoder may already have exited
        if let Some(p) = thread_priority(tid) {
            assert_eq!(
                p, THREAD_PRIORITY_HIGHEST.0,
                "worker {tid} is at priority {p}, not HIGHEST"
            );
            checked += 1;
        }
    }
    assert!(
        checked >= threads as usize,
        "the encoder's own workers are alive"
    );
    assert_ne!(THREAD_PRIORITY_HIGHEST.0, THREAD_PRIORITY_NORMAL.0);
}

#[test]
fn x264_worker_selection_keeps_only_new_threads_started_in_the_library() {
    use crate::windows_utils::streamer::x264::workers::select_new_workers;
    use std::collections::BTreeSet;

    const LIB: isize = 0x7ff0_0000;
    const OTHER: isize = 0x7ff8_0000;
    let before: BTreeSet<u32> = [1, 2, 3].into_iter().collect();
    let after: BTreeSet<u32> = [1, 2, 3, 10, 11, 12, 13].into_iter().collect();
    let start = |tid: u32| match tid {
        10 | 11 => Some(LIB),
        12 => Some(OTHER), // another subsystem's thread created in the same window
        13 => None,        // exited before it could be queried
        _ => Some(LIB),    // 1..3 are an older encoder's workers: not new
    };
    assert_eq!(
        select_new_workers(&before, &after, LIB, start),
        vec![10, 11]
    );
}

#[test]
fn x264_slice_thread_count_rules() {
    use crate::windows_utils::streamer::x264::workers::slice_thread_count;
    // default: the machine's count, capped at 8
    assert_eq!(slice_thread_count(16, 8, None, 1080, None), 8);
    assert_eq!(slice_thread_count(4, 8, None, 1080, None), 4);
    assert_eq!(slice_thread_count(0, 8, None, 1080, None), 1);
    // worker tuning holds it to the P-cores the workers are confined to
    assert_eq!(slice_thread_count(16, 8, Some(6), 1080, None), 6);
    assert_eq!(slice_thread_count(16, 8, Some(0), 1080, None), 8);
    // slices by height: at least 8 macroblock rows each
    assert_eq!(slice_thread_count(16, 8, None, 1080, Some(8)), 8); // 68 rows
    assert_eq!(slice_thread_count(16, 8, None, 720, Some(8)), 5); // 45 rows
    assert_eq!(slice_thread_count(16, 8, None, 360, Some(8)), 2); // 23 rows
    assert_eq!(slice_thread_count(16, 8, None, 64, Some(8)), 1); // 4 rows
}

/// Bytes `frames` synthetic 720p frames cost when submitted at `hz` into an encoder configured
/// for 60 fps and `bitrate`, with or without the delivered-rate pacer. The pacer's clock is
/// driven by the test, so this is exact and fast. Also returns the scale the pacer settled on.
fn bytes_at_delivered_rate(
    pacer: bool,
    hz: f64,
    frames: u32,
    bitrate: u32,
) -> Option<(usize, f64)> {
    use crate::windows_utils::streamer::x264::encoder::X264Options;
    let options = X264Options {
        rate_by_delivered_fps: pacer,
        ..Default::default()
    };
    let mut enc = match X264Encoder::with_options(cfg_720p(bitrate), options) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 delivered-rate pacing (libx264 unavailable)", e);
            return None;
        }
    };
    let mut bgra = vec![0u8; 1280 * 720 * 4];
    let t0 = std::time::Instant::now();
    let mut total = 0usize;
    for i in 0..frames {
        enc.set_test_clock(t0 + std::time::Duration::from_secs_f64(i as f64 / hz));
        fill_synthetic_bgra(&mut bgra, 1280, 720, i);
        let au = enc.encode_bgra(&bgra, i == 0).expect("encode");
        assert!(
            !au.is_empty(),
            "frame {i}: the pacer must never delay a picture"
        );
        // skip the first second: the IDR and the pacer's warm-up
        if i as f64 >= hz {
            total += au.len();
        }
    }
    Some((total, enc.rate_scale()))
}

/// Frames delivered faster than nominal (cursor frames skip the cadence gate) hold the stream
/// to its bitrate only with the delivered-rate pacer.
///
/// Asserts the direction and a wide margin only. ABR with a VBV over sliced threads is not
/// byte-deterministic, and the slice count follows the machine's core count, so the absolute
/// ratios (1.53 without, 0.70 with, on a 16-thread machine) are printed, not gated. The exact
/// rules live in `x264_rate_pacer_rules`.
#[test]
fn x264_rate_pacer_holds_the_bitrate_when_frames_arrive_faster_than_nominal() {
    // low enough that ABR spends its whole budget on this content at 60 fps
    const BITRATE: u32 = 1_000_000;
    const HZ: f64 = 120.0;
    const FRAMES: u32 = 360; // three seconds; the last two are measured
    let Some((off, _)) = bytes_at_delivered_rate(false, HZ, FRAMES, BITRATE) else {
        return;
    };
    let (on, scale) = bytes_at_delivered_rate(true, HZ, FRAMES, BITRATE).expect("second run");
    let seconds = (FRAMES as f64 - HZ) / HZ;
    let target = BITRATE as f64 / 8.0 * seconds;
    let (off_ratio, on_ratio) = (off as f64 / target, on as f64 / target);
    eprintln!(
        "x264 delivered {HZ} Hz into a 60 fps encoder at {BITRATE} bps: bytes/target \
         without pacer {off_ratio:.2}, with pacer {on_ratio:.2} (scale {scale:.2})"
    );
    assert!(
        off_ratio > 1.1,
        "libx264 charges each picture 1/60 s, so 120 Hz delivery should spend over the target \
         (got {off_ratio:.2}); if not, the premise of the pacer is gone"
    );
    assert!(
        on_ratio <= 0.8 * off_ratio,
        "the pacer must cut what 120 Hz delivery spends: {on_ratio:.2} with it against \
         {off_ratio:.2} without"
    );
    assert!(
        scale < 1.0,
        "the pacer must have lowered the budget at 120 Hz, settled on {scale:.2}"
    );
}

#[test]
fn x264_rate_pacer_rules() {
    use crate::windows_utils::streamer::x264::encoder::RatePacer;
    use std::time::{Duration, Instant};

    let t0 = Instant::now();
    let at = |s: f64| t0 + Duration::from_secs_f64(s);

    // delivered at nominal: never reconfigures
    let mut p = RatePacer::new(60);
    for i in 0..240 {
        assert_eq!(p.note_frame(at(i as f64 / 60.0)), None);
    }

    // slower than nominal (30 Hz): the budget is never raised above nominal
    let mut p = RatePacer::new(60);
    for i in 0..240 {
        assert_eq!(p.note_frame(at(i as f64 / 30.0)), None, "frame {i}");
    }
    assert_eq!(p.applied(), 1.0);

    // 120 Hz: converges on half, through few reconfigures
    let mut p = RatePacer::new(60);
    let mut changes = 0;
    for i in 0..480 {
        if p.note_frame(at(i as f64 / 120.0)).is_some() {
            changes += 1;
        }
    }
    assert!((p.applied() - 0.5).abs() < 0.08, "applied {}", p.applied());
    assert!(changes <= 6, "{changes} reconfigures for one rate change");

    // an idle gap resets the measurement and a return to nominal restores the full budget
    let mut t = 480.0 / 120.0 + 2.0;
    // the first picture after the gap already finds no rate to act on and restores the budget
    let mut restored = p.note_frame(at(t)).is_some_and(|x| x > 0.95);
    for _ in 0..240 {
        t += 1.0 / 60.0;
        if p.note_frame(at(t)).is_some_and(|x| x > 0.95) {
            restored = true;
        }
    }
    assert!(restored && p.applied() > 0.95, "applied {}", p.applied());

    // extreme rates are floored
    let mut p = RatePacer::new(60);
    for i in 0..600 {
        p.note_frame(at(i as f64 / 1000.0));
    }
    assert!(p.applied() >= 0.25 - 1e-9, "applied {}", p.applied());
}

/// Why the delivered rate is not handled with `b_vfr_input`: libx264 takes a VFR picture's
/// duration from the gap to the *next* picture's pts, so it holds every picture until the next
/// one arrives, a frame of latency on every picture. With the frame-count timebase the first
/// encode returns the IDR.
#[test]
fn x264_vfr_input_holds_a_frame_back() {
    use crate::windows_utils::streamer::x264::x264_sys::*;
    use std::ffi::c_int;

    let api = match X264Api::shared() {
        Ok(a) => a,
        Err(e) => {
            super::skip("x264 VFR delay evidence (libx264 unavailable)", e);
            return;
        }
    };
    const W: c_int = 320;
    const H: c_int = 192;
    let sizes = |vfr: bool| -> Vec<c_int> {
        let mut p: x264_param_t = unsafe { std::mem::zeroed() };
        let rv = unsafe {
            (api.param_default_preset)(&mut p, c"ultrafast".as_ptr(), c"zerolatency".as_ptr())
        };
        assert_eq!(rv, 0);
        p.i_width = W;
        p.i_height = H;
        p.i_csp = X264_CSP_I420;
        p.i_fps_num = 60;
        p.i_fps_den = 1;
        p.i_timebase_num = 1;
        p.b_vfr_input = vfr as c_int;
        p.i_timebase_den = if vfr { 90_000 } else { 60 };
        p.b_sliced_threads = 1;
        p.i_bframe = 0;
        p.rc.i_lookahead = 0;
        p.i_sync_lookahead = 0;
        p.rc.i_rc_method = X264_RC_ABR;
        p.rc.i_bitrate = 1000;
        p.rc.i_vbv_max_bitrate = 1000;
        p.rc.i_vbv_buffer_size = 34;
        p.b_annexb = 1;
        p.b_repeat_headers = 1;
        p.i_log_level = X264_LOG_NONE;
        let h = unsafe { (api.encoder_open)(&mut p) };
        assert!(!h.is_null());
        let (w, hh) = (W as usize, H as usize);
        let mut buf = vec![0u8; w * hh * 3 / 2];
        let mut out = Vec::new();
        for i in 0..4i64 {
            for (n, b) in buf.iter_mut().enumerate() {
                *b = ((n as i64 * 7 + i * 13) & 0xff) as u8;
            }
            let mut pic: x264_picture_t = unsafe { std::mem::zeroed() };
            unsafe { (api.picture_init)(&mut pic) };
            pic.img.i_csp = X264_CSP_I420;
            pic.img.i_plane = 3;
            let (y, rest) = buf.split_at_mut(w * hh);
            let (u, v) = rest.split_at_mut(w * hh / 4);
            pic.img.plane[0] = y.as_mut_ptr();
            pic.img.plane[1] = u.as_mut_ptr();
            pic.img.plane[2] = v.as_mut_ptr();
            pic.img.i_stride = [W, W / 2, W / 2, 0];
            pic.i_pts = if vfr { i * 1500 } else { i };
            let mut nals: *mut x264_nal_t = std::ptr::null_mut();
            let mut n: c_int = 0;
            let mut pic_out: x264_picture_t = unsafe { std::mem::zeroed() };
            let size =
                unsafe { (api.encoder_encode)(h, &mut nals, &mut n, &mut pic, &mut pic_out) };
            assert!(size >= 0);
            out.push(size);
        }
        unsafe { (api.encoder_close)(h) };
        out
    };
    let cfr = sizes(false);
    let vfr = sizes(true);
    eprintln!("x264 first four AU sizes: cfr {cfr:?}, vfr {vfr:?}");
    assert!(cfr[0] > 0, "constant-rate input must emit the IDR at once");
    assert_eq!(
        vfr[0], 0,
        "VFR input was expected to hold the first picture back; if libx264 no longer does, \
         b_vfr_input may be the better fix for x264-05"
    );
}

/// Encode 60 synthetic 720p frames at QP 26: the SPS, the tools libx264 settled on and the bytes.
fn profile_run(profile: H264Profile, negotiated: bool) -> Option<(u8, i32, i32, usize)> {
    use crate::windows_utils::streamer::x264::encoder::X264Options;
    let options = X264Options {
        negotiated_profile: negotiated,
        ..Default::default()
    };
    let mut cfg = cfg_720p(6_000_000);
    cfg.profile = profile;
    cfg.qp = Some(26);
    let mut enc = match X264Encoder::with_options(cfg, options) {
        Ok(e) => e,
        Err(e) => {
            super::skip("x264 negotiated profile (libx264 unavailable)", e);
            return None;
        }
    };
    let eff = enc.effective_params();
    let mut frame = vec![0u8; 1280 * 720 * 4];
    let mut bytes = 0usize;
    let mut sps = None;
    for i in 0..60u32 {
        fill_synthetic_bgra(&mut frame, 1280, 720, i);
        let au = enc.encode_bgra(&frame, i == 0).expect("encode");
        if i == 0 {
            sps = parse_sps(&au);
        }
        bytes += au.len();
    }
    let sps = sps.expect("the first frame carries an SPS");
    Some((
        sps.profile_idc,
        eff.b_cabac,
        eff.analyse.b_transform_8x8,
        bytes,
    ))
}

/// `x264_negotiated_profile`: a High request is a High stream (CABAC, 8x8 transform), Main is
/// Main, Baseline is unchanged; without the switch every request is constrained Baseline.
#[test]
fn x264_emits_the_profile_it_was_asked_for() {
    let Some(old_high) = profile_run(H264Profile::High, false) else {
        return;
    };
    assert_eq!(
        (old_high.0, old_high.1, old_high.2),
        (66, 0, 0),
        "without the switch `ultrafast` leaves a High request as constrained Baseline"
    );
    let high = profile_run(H264Profile::High, true).unwrap();
    assert_eq!((high.0, high.1, high.2), (100, 1, 1), "High: {high:?}");
    let main = profile_run(H264Profile::Main, true).unwrap();
    assert_eq!((main.0, main.1, main.2), (77, 1, 0), "Main: {main:?}");
    let base = profile_run(H264Profile::Baseline, true).unwrap();
    assert_eq!((base.0, base.1, base.2), (66, 0, 0), "Baseline: {base:?}");
    eprintln!(
        "x264 720p QP 26, 60 frames: High as shipped {} B, High with its tools {} B ({:+.1} %), Main {} B",
        old_high.3,
        high.3,
        (high.3 as f64 / old_high.3 as f64 - 1.0) * 100.0,
        main.3
    );
}
