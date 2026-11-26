use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE};

use super::h264::{leading_ues, nal_slices, parse_sps};
use crate::streamer::config::H264Profile;
use crate::streamer::packetizer::H264Packetizer;
use crate::streamer::webrtc_session::RTP_PAYLOAD_MTU_LAN;
use crate::windows_utils::streamer::intel::encoder::{
    brc_fields, brc_fields_at, bridge_acquire_verdict, cpb_frames, create_intel_d3d11_device,
    fill_synthetic_bgra, luid_to_bytes, needs_rebuild, query_fallback,
    requery_fallback_before_init, reset_retry_ladder, search_max_fps, test_hooks, Brc, Encoder,
    QueryFallback, SliceMode, REBUILD_MARKER,
};
use crate::windows_utils::streamer::intel::intel_sys::{
    MFX_ERR_INVALID_VIDEO_PARAM, MFX_ERR_NONE, MFX_ERR_UNSUPPORTED, MFX_RATECONTROL_CBR,
    MFX_RATECONTROL_CQP, MFX_RATECONTROL_VCM, MFX_WRN_INCOMPATIBLE_VIDEO_PARAM,
};
use crate::windows_utils::streamer::keyed::Acquired;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;

fn is_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        let nal = if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            i += 3;
            au[i] & 0x1f
        } else if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
            i += 4;
            au[i] & 0x1f
        } else {
            i += 1;
            continue;
        };
        if nal == 5 || nal == 7 {
            return true;
        }
        i += 1;
    }
    false
}

#[test]
fn intel_quicksync_clamps_extreme_frame_rate() {
    super::show_measurements();
    const W: u32 = 1920;
    const H: u32 = 1080;

    let mut encoder = match Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: 357,
        bitrate_bps: 30_000_000,
        max_bitrate_bps: 30_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("Intel extreme-fps clamp test (no HW/runtime)", e);
            return;
        }
    };

    // read back after `Init`: either the probe capped the request or the runtime accepted 357.
    // it must never run at a rate nobody asked for
    let (n, d) = encoder.effective_frame_rate();
    assert!(d >= 1, "FrameRateExtD must be set, got {n}/{d}");
    let fps = n / d;
    assert!(
        (30..=357).contains(&fps),
        "effective frame rate {n}/{d} is outside the probe's range 30..=357"
    );
    assert_eq!(
        n % d,
        0,
        "the encoder asks for whole frame rates; {n}/{d} means the runtime rewrote it"
    );

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut total = 0usize;
    for i in 0..10 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .expect("encode should succeed after frame-rate clamp");
        total += au.len();
    }
    assert!(
        total > 0,
        "clamped-fps encoder should produce bitstream bytes"
    );
    eprintln!(
        "Intel Quick Sync extreme-fps clamp OK: 1920x1080@357 requested, runtime runs at {n}/{d} \
         ({}), encoded {total} bytes",
        if fps < 357 {
            "clamped"
        } else {
            "accepted unclamped"
        }
    );
}

#[test]
fn intel_quicksync_encodes_synthetic_frames() {
    super::show_measurements();
    const W: u32 = 1280;
    const H: u32 = 720;

    let mut encoder = match Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: 60,
        bitrate_bps: 6_000_000,
        max_bitrate_bps: 6_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: true,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("Intel Quick Sync smoke test (no HW/runtime)", e);
            return;
        }
    };

    let mut dump = std::env::var("INTEL_PROBE_OUT")
        .ok()
        .map(|p| std::fs::File::create(p).expect("create INTEL_PROBE_OUT"));

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut first_keyframe = false;
    let mut total = 0usize;
    let mut per_frame_us: Vec<u128> = Vec::with_capacity(30);
    let mut sps = None;
    let mut slice_bytes: Vec<usize> = Vec::new();
    for i in 0..30 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let t = std::time::Instant::now();
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .expect("Intel encode_bgra should succeed once the session initialized");
        per_frame_us.push(t.elapsed().as_micros());
        total += au.len();
        if let Some(f) = dump.as_mut() {
            use std::io::Write as _;
            f.write_all(&au).expect("write dump");
        }
        if sps.is_none() {
            sps = parse_sps(&au);
        }
        for (nal, payload) in nal_slices(&au) {
            // 1 = non-IDR slice, 5 = IDR slice
            if nal == 1 || nal == 5 {
                slice_bytes.push(payload.len());
            }
        }
        if i == 0 {
            first_keyframe = is_keyframe(&au);
        }
    }
    assert!(
        first_keyframe,
        "first frame should be an IDR with in-band SPS/PPS"
    );
    assert!(total > 0, "encoder should have produced bitstream bytes");

    // the client negotiates `42e01f`: it cannot decode anything above High (100), or 4:4:4
    let sps = sps.expect("the Intel stream must carry an SPS");
    assert!(
        matches!(sps.profile_idc, 66 | 77 | 100),
        "profile_idc {} is not one a browser decoder accepts",
        sps.profile_idc
    );
    assert_eq!(
        sps.chroma_format_idc, 1,
        "the encoder must emit 4:2:0, got chroma_format_idc {}",
        sps.chroma_format_idc
    );

    encoder
        .set_bitrate(3_000_000)
        .expect("in-place bitrate Reset should succeed");
    for i in 30..40 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let _ = encoder
            .encode_bgra(&frame, false)
            .expect("encode after bitrate reset");
    }

    per_frame_us.sort_unstable();
    slice_bytes.sort_unstable();
    let p = |v: &[u128], pct: usize| -> u128 {
        if v.is_empty() {
            0
        } else {
            v[(v.len() * pct / 100).min(v.len() - 1)]
        }
    };
    eprintln!(
        "Intel Quick Sync smoke test OK: 40 frames, {total} bytes, IDR with SPS and PPS verified, \
         slices={:?}, sps={sps:?}, encode p50={}us p95={}us p99={}us, \
         slice_bytes n={} p50={} max={}",
        encoder.slice_mode(),
        p(&per_frame_us, 50),
        p(&per_frame_us, 95),
        p(&per_frame_us, 99),
        slice_bytes.len(),
        slice_bytes.get(slice_bytes.len() / 2).copied().unwrap_or(0),
        slice_bytes.last().copied().unwrap_or(0),
    );
}

#[test]
fn intel_quicksync_fused_downscale() {
    super::show_measurements();
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    const NW: u32 = 1920;
    const NH: u32 = 1080;
    const OW: u32 = 1280;
    const OH: u32 = 720;

    let (device, context) = match create_intel_d3d11_device() {
        Ok(v) => v,
        Err(e) => {
            super::skip("Intel fused-downscale test (no Intel device)", e);
            return;
        }
    };
    let mut encoder = match Encoder::new_on_device(
        EncoderConfig {
            width: OW,
            height: OH,
            fps: 60,
            bitrate_bps: 8_000_000,
            max_bitrate_bps: 8_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            min_qp: None,
            intra_refresh: true,
        },
        NW,
        NH,
        &device,
        &context,
    ) {
        Ok(e) => e,
        Err(e) => {
            super::skip("Intel fused-downscale test (encoder init)", e);
            return;
        }
    };

    let mut buf = vec![0u8; (NW * NH * 4) as usize];
    fill_synthetic_bgra(&mut buf, NW, NH, 0);
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: buf.as_ptr() as *const _,
        SysMemPitch: NW * 4,
        SysMemSlicePitch: 0,
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: NW,
        Height: NH,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
        .expect("create native source texture");
    let tex = tex.expect("native texture");

    let au0 = encoder
        .encode_texture(&tex, true)
        .expect("fused-scale encode frame 0");
    assert!(
        is_keyframe(&au0),
        "first fused-scale frame should be an IDR"
    );
    // the fused path exists for the output size: check the SPS, not just that bytes came out
    let dims = sps_dimensions(&au0).expect("the fused-scale IDR must carry a parseable SPS");
    assert_eq!(
        dims,
        (OW, OH),
        "the fused VPP downscale must emit {OW}x{OH} (SPS says {}x{})",
        dims.0,
        dims.1
    );
    for _ in 0..9 {
        let _ = encoder
            .encode_texture(&tex, false)
            .expect("fused-scale encode");
    }
    // the same-adapter path's per-stage split. if the full-frame `CopySubresourceRegion` into
    // the VPP input surface is a small fraction of the VPP pass it feeds, oneVPL surface import
    // (a 2.9+-only API) would replace a cheap blit with new FFI
    let (get_us, copy_us, vpp_us) = encoder.stage_medians_us();
    eprintln!(
        "Intel fused-downscale OK: {NW}x{NH} -> {OW}x{OH} in one VPP pass; per-frame medians \
         get_surface={get_us}us copy_into_surface={copy_us}us vpp_submit={vpp_us}us"
    );
}

/// Three encoders built at once must all get a session.
///
/// oneVPL implementation enumeration is dispatcher-wide: two `MFXCreateSession` calls at once
/// against the same iGPU can leave one holding `MFX_ERR_NOT_FOUND`, which reads as "no Intel
/// hardware" and sends that session to x264 for its whole life. An encoder is built per
/// streaming session, so this is what two clients joining together does.
///
/// The *serial* probe decides the skip: a box with no Intel hardware skips, but one that has it
/// cannot pass by failing every thread the same way.
#[test]
fn intel_quicksync_survives_concurrent_session_creation() {
    const W: u32 = 640;
    const H: u32 = 360;
    const THREADS: usize = 3;

    let cfg = || EncoderConfig {
        width: W,
        height: H,
        fps: 30,
        bitrate_bps: 2_000_000,
        max_bitrate_bps: 2_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    };

    match Encoder::new(cfg()) {
        Ok(e) => drop(e),
        Err(e) => {
            super::skip("Intel concurrent-session test (no HW/runtime)", e);
            return;
        }
    }

    let failures: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..THREADS)
            .map(|i| {
                s.spawn(move || match Encoder::new(cfg()) {
                    Ok(mut enc) => {
                        let mut frame = vec![0u8; (W * H * 4) as usize];
                        fill_synthetic_bgra(&mut frame, W, H, i as u32);
                        enc.encode_bgra(&frame, true)
                            .map(|au| au.len())
                            .map_err(|e| format!("thread {i} encode: {e:#}"))
                            .map(|_| ())
                    }
                    Err(e) => Err(format!("thread {i} session: {e:#}")),
                })
            })
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().expect("encoder thread should not panic").err())
            .collect()
    });

    assert!(
        failures.is_empty(),
        "every concurrent Quick Sync session should be created; got {} failure(s): {failures:#?}",
        failures.len(),
    );
    eprintln!("Intel concurrent-session OK: {THREADS} sessions created and encoded in parallel");
}

/// The CPU-bridge NV12 door must produce the same kind of stream as the BGRA one.
///
/// On BGRA the bridge re-uploads 4 bytes a pixel and runs a `VideoProcessorBlt` to reach NV12.
/// The NV12 door skips that VPP pass entirely; skipping it must not change what comes out.
#[test]
fn intel_nv12_input_matches_the_bgra_path() {
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

    let mut bgra_enc = match Encoder::new(cfg()) {
        Ok(e) => e,
        Err(e) => {
            super::skip("Intel NV12 input test (no HW/runtime)", e);
            return;
        }
    };
    let mut nv12_enc = Encoder::new(cfg()).expect("second Quick Sync encoder");

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let (mut bgra_total, mut nv12_total) = (0usize, 0usize);
    let mut nv12_sps = None;
    let (mut bgra_us, mut nv12_us): (Vec<u128>, Vec<u128>) = (Vec::new(), Vec::new());

    for i in 0..30u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let nv12 = bgra_to_nv12_bt709(&frame, W as usize, H as usize);

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
                is_keyframe(&b),
                "the first NV12 frame must be an IDR with in-band SPS/PPS"
            );
            nv12_sps = parse_sps(&b);
        }
    }

    let sps = nv12_sps.expect("the NV12 path must emit a parseable SPS");
    assert_eq!(
        sps.profile_idc, 66,
        "NV12 input must honour Baseline, got {sps:?}"
    );
    assert_eq!(
        sps.chroma_format_idc, 1,
        "NV12 input must stay 4:2:0, got {sps:?}"
    );

    let ratio = nv12_total as f64 / bgra_total as f64;
    assert!(
        (0.5..2.0).contains(&ratio),
        "NV12 output {nv12_total} B and BGRA output {bgra_total} B differ by more than 2x; \
         the planes are probably being read as something else"
    );

    // the NV12 door fills the encode surface directly, so a repeat has to work off that
    // surface, not a stale VPP output
    let repeat = nv12_enc.encode_repeat(true).expect("NV12 repeat encode");
    assert!(
        !repeat.is_empty(),
        "a forced NV12 repeat must produce bytes"
    );

    bgra_us.sort_unstable();
    nv12_us.sort_unstable();
    let med = |v: &[u128]| v[v.len() / 2];
    eprintln!(
        "Intel NV12 input OK: {W}x{H}, sps={sps:?}, bytes bgra={bgra_total} nv12={nv12_total} \
         (ratio {ratio:.2}), encode median bgra={}us nv12={}us",
        med(&bgra_us),
        med(&nv12_us),
    );
}

/// BT.709 limited-range BGRA -> NV12, matching what the D3D11 video processor emits, so the two
/// encoders are handed the same picture.
fn bgra_to_nv12_bt709(bgra: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut out = vec![0u8; w * h + w * (h / 2)];
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let y = |r: f32, g: f32, b: f32| {
        (16.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b).clamp(0.0, 255.0) as u8
    };
    for row in 0..h {
        for col in 0..w {
            let p = row * w * 4 + col * 4;
            y_plane[row * w + col] = y(bgra[p + 2] as f32, bgra[p + 1] as f32, bgra[p] as f32);
        }
    }
    for row in (0..h).step_by(2) {
        for col in (0..w).step_by(2) {
            let (mut sb, mut sg, mut sr) = (0.0f32, 0.0f32, 0.0f32);
            for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let p = (row + dr) * w * 4 + (col + dc) * 4;
                sb += bgra[p] as f32;
                sg += bgra[p + 1] as f32;
                sr += bgra[p + 2] as f32;
            }
            let (r, g, b) = (sr / 4.0, sg / 4.0, sb / 4.0);
            let i = (row / 2) * w + col;
            uv_plane[i] = (128.0 - 0.1006 * r - 0.3386 * g + 0.4392 * b).clamp(0.0, 255.0) as u8;
            uv_plane[i + 1] =
                (128.0 + 0.4392 * r - 0.3989 * g - 0.0403 * b).clamp(0.0, 255.0) as u8;
        }
    }
    out
}

/// `(width, height)` of the first SPS in an access unit, after frame cropping. Only what the
/// fused-downscale test needs: 4:2:0, progressive (`frame_mbs_only_flag`), any profile.
fn sps_dimensions(au: &[u8]) -> Option<(u32, u32)> {
    let (_, payload) = nal_slices(au).into_iter().find(|&(t, _)| t == 7)?;
    // drop emulation-prevention bytes (00 00 03 -> 00 00)
    let mut rbsp = Vec::with_capacity(payload.len());
    let mut zeros = 0;
    for &b in payload {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        rbsp.push(b);
    }
    let mut pos = 0usize;
    let mut bit = |pos: &mut usize| -> Option<u32> {
        let byte = *rbsp.get(*pos / 8)?;
        let v = (byte >> (7 - (*pos % 8))) & 1;
        *pos += 1;
        Some(u32::from(v))
    };
    fn bits(
        n: u32,
        pos: &mut usize,
        bit: &mut dyn FnMut(&mut usize) -> Option<u32>,
    ) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            v = (v << 1) | bit(pos)?;
        }
        Some(v)
    }
    fn ue(pos: &mut usize, bit: &mut dyn FnMut(&mut usize) -> Option<u32>) -> Option<u32> {
        let mut lz = 0;
        while bit(pos)? == 0 {
            lz += 1;
            if lz > 31 {
                return None;
            }
        }
        Some((1u32 << lz) - 1 + bits(lz, pos, bit)?)
    }
    let b: &mut dyn FnMut(&mut usize) -> Option<u32> = &mut bit;
    let profile_idc = bits(8, &mut pos, b)?;
    bits(16, &mut pos, b)?; // constraint flags + level_idc
    ue(&mut pos, b)?; // seq_parameter_set_id
    if [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135].contains(&profile_idc) {
        let chroma = ue(&mut pos, b)?;
        if chroma != 1 {
            return None;
        }
        ue(&mut pos, b)?;
        ue(&mut pos, b)?;
        bits(1, &mut pos, b)?;
        if bits(1, &mut pos, b)? == 1 {
            return None; // scaling matrices: not emitted by this encoder
        }
    }
    ue(&mut pos, b)?; // log2_max_frame_num_minus4
    let poc_type = ue(&mut pos, b)?;
    if poc_type == 0 {
        ue(&mut pos, b)?;
    } else if poc_type == 1 {
        return None;
    }
    ue(&mut pos, b)?; // max_num_ref_frames
    bits(1, &mut pos, b)?; // gaps_in_frame_num_value_allowed_flag
    let w_mbs = ue(&mut pos, b)? + 1;
    let h_map = ue(&mut pos, b)? + 1;
    if bits(1, &mut pos, b)? != 1 {
        return None; // interlaced: not emitted by this encoder
    }
    bits(1, &mut pos, b)?; // direct_8x8_inference_flag
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if bits(1, &mut pos, b)? == 1 {
        cl = ue(&mut pos, b)?;
        cr = ue(&mut pos, b)?;
        ct = ue(&mut pos, b)?;
        cb = ue(&mut pos, b)?;
    }
    // 4:2:0 progressive: CropUnitX = CropUnitY = 2
    Some((w_mbs * 16 - 2 * (cl + cr), h_map * 16 - 2 * (ct + cb)))
}

/// A hard `MFX_ERR_UNSUPPORTED` with the two fields zeroed must reach both fallbacks.
#[test]
fn query_unsupported_reaches_the_rate_control_and_slice_fallbacks() {
    let fb = query_fallback(MFX_ERR_UNSUPPORTED, false, 0, 0, true)
        .expect("UNSUPPORTED with VCM and MaxSliceSize zeroed is what the fallbacks are for");
    assert_eq!(
        fb,
        QueryFallback {
            cbr: true,
            num_slice: true
        }
    );

    // only the slice size refused
    let fb = query_fallback(MFX_ERR_UNSUPPORTED, false, MFX_RATECONTROL_VCM, 0, true).unwrap();
    assert_eq!(
        fb,
        QueryFallback {
            cbr: false,
            num_slice: true
        }
    );

    // with the switch off it stays an error
    assert!(query_fallback(MFX_ERR_UNSUPPORTED, false, 0, 0, false).is_err());

    // UNSUPPORTED about something else: nothing to fall back on
    assert!(query_fallback(MFX_ERR_UNSUPPORTED, false, MFX_RATECONTROL_VCM, 1150, true).is_err());

    // any other hard error stays a hard error
    assert!(query_fallback(MFX_ERR_INVALID_VIDEO_PARAM, false, 0, 0, true).is_err());
}

#[test]
fn query_warnings_and_success_keep_their_old_meaning() {
    // accepted as asked: no fallback
    assert_eq!(
        query_fallback(MFX_ERR_NONE, false, MFX_RATECONTROL_VCM, 1150, true).unwrap(),
        QueryFallback::default()
    );
    // a warning that changed the rate control: CBR
    let fb = query_fallback(
        MFX_WRN_INCOMPATIBLE_VIDEO_PARAM,
        false,
        MFX_RATECONTROL_CBR,
        1150,
        true,
    )
    .unwrap();
    assert!(fb.cbr && !fb.num_slice);
    // constant QP never takes the CBR fallback
    let fb = query_fallback(MFX_ERR_NONE, true, MFX_RATECONTROL_CQP, 0, true).unwrap();
    assert!(!fb.cbr && fb.num_slice);
}

/// The fallback parameter set gets a second `ENCODE_Query` only when the first one was a hard
/// error. A fallback chosen from a warning goes straight to `Init`, so a negative answer to a
/// second `Query` cannot cost the Intel tier on that path.
#[test]
fn second_query_only_after_a_hard_error() {
    let cbr = QueryFallback {
        cbr: true,
        num_slice: false,
    };
    let slices = QueryFallback {
        cbr: false,
        num_slice: true,
    };
    // the warning path: MaxSliceSize zeroed with a positive status, common on LowPower parts
    assert!(!requery_fallback_before_init(
        MFX_WRN_INCOMPATIBLE_VIDEO_PARAM,
        slices
    ));
    assert!(!requery_fallback_before_init(
        MFX_WRN_INCOMPATIBLE_VIDEO_PARAM,
        cbr
    ));
    assert!(!requery_fallback_before_init(MFX_ERR_NONE, slices));
    // no fallback: nothing new to ask about
    assert!(!requery_fallback_before_init(
        MFX_ERR_UNSUPPORTED,
        QueryFallback::default()
    ));
    // a hard error with a fallback: the only case that asks again
    assert!(requery_fallback_before_init(MFX_ERR_UNSUPPORTED, cbr));
    assert!(requery_fallback_before_init(MFX_ERR_UNSUPPORTED, slices));
}

/// A refused `Reset` is retried smallest CPB first, and the old (larger) CPB is only the last
/// resort, so an upshift followed by a downshift does not keep a multi-frame CPB when a smaller
/// one would be accepted.
#[test]
fn reset_retry_ladder_tries_the_smallest_cpb_first() {
    // 12 Mbps then 3 Mbps at 60 fps: old CPB 24 KB (runtime-held), old delay 12, new CPB 6
    let l = reset_retry_ladder(6, 24, 12);
    assert_eq!(l, vec![(6, 0), (6, 6), (12, 12), (24, 12)]);
    assert_eq!(l.last(), Some(&(24, 12)), "the old CPB is the last resort");
    assert!(l.windows(2).all(|w| w[0].0 <= w[1].0), "smallest CPB first");
    // old delay already inside the new CPB: rungs collapse, each pair asked once
    assert_eq!(reset_retry_ladder(6, 24, 4), vec![(6, 0), (6, 4), (24, 4)]);
    // an upshift asks for the new CPB first; the unchanged old CPB stays the last resort
    assert_eq!(reset_retry_ladder(24, 6, 3), vec![(24, 0), (24, 3), (6, 3)]);
    // a zero is never asked for as a CPB
    assert!(reset_retry_ladder(0, 0, 0).iter().all(|&(cpb, _)| cpb >= 1));
}

#[test]
fn cpb_frames_counts_frames_of_buffering() {
    // one frame at 3 Mbps / 60 fps is 6.1 KB; a 24 KB CPB holds about four
    let f = cpb_frames(24, 0, 3_000_000, 60);
    assert!((f - 3.93).abs() < 0.01, "{f}");
    // the multiplier scales the field
    assert!((cpb_frames(12, 2, 3_000_000, 60) - f).abs() < 1e-9);
    assert!(cpb_frames(6, 1, 3_000_000, 60) < 1.0);
}

/// Above 65 Mbps the rate is expressed through `BRCParamMultiplier` instead of being clamped;
/// at and below it nothing about the fields changes.
#[test]
fn brc_fields_use_the_multiplier_only_where_the_rate_does_not_fit() {
    // ordinary rates: multiplier unused, same fields with the switch off
    let b = brc_fields(10_000_000, 12_000_000, 20, true);
    assert_eq!(
        b,
        Brc {
            mult: 0,
            target_kbps: 10_000,
            max_kbps: 12_000,
            buffer_kb: 20
        }
    );
    assert_eq!(b, brc_fields(10_000_000, 12_000_000, 20, false));
    // max below target is lifted to target
    assert_eq!(brc_fields(8_000_000, 1_000_000, 16, true).max_kbps, 8_000);

    // with the switch off 100 Mbps comes out as 65 Mbps
    let old = brc_fields(100_000_000, 100_000_000, 203, false);
    assert_eq!(
        (old.mult, old.target_kbps, old.max_kbps),
        (0, 65_000, 65_000)
    );
    let new = brc_fields(100_000_000, 100_000_000, 203, true);
    assert_eq!(new.mult, 2);
    let effective = u32::from(new.target_kbps) * u32::from(new.mult);
    assert!(
        (99_000..=100_000).contains(&effective),
        "100 Mbps must survive the multiplier, got {effective} kbps"
    );
    assert!(u32::from(new.max_kbps) * u32::from(new.mult) >= 100_000);
    assert!(new.buffer_kb >= 1);
}

/// `Reset` works in the units `Init` chose; at multiplier 0/1 that is plain kbps, clamped at 65000.
#[test]
fn brc_fields_at_keep_the_init_multiplier() {
    for mult in [0u16, 1] {
        let (b, clamped) = brc_fields_at(3_000_000, 3_000_000, 3, mult);
        assert_eq!(
            (b.target_kbps, b.max_kbps, b.buffer_kb, clamped),
            (3_000, 3_000, 3, false)
        );
        // above the field: clamped to 65000 and reported
        let (b, clamped) = brc_fields_at(100_000_000, 100_000_000, 203, mult);
        assert_eq!((b.target_kbps, b.max_kbps, clamped), (65_000, 65_000, true));
    }
    let (b, clamped) = brc_fields_at(80_000_000, 80_000_000, 163, 2);
    assert_eq!(
        (b.mult, b.target_kbps, b.max_kbps, clamped),
        (2, 40_000, 40_000, false)
    );
    let (b, clamped) = brc_fields_at(10_000_000, 10_000_000, 20, 2);
    assert_eq!((b.target_kbps, b.buffer_kb, clamped), (5_000, 10, false));
}

/// The fps search, separated from `ENCODE_Query`.
#[test]
fn fps_search_finds_the_top_of_the_accepted_run() {
    assert_eq!(search_max_fps(357, |f| f <= 144), Some(144));
    assert_eq!(search_max_fps(60, |f| f <= 144), Some(60));
    assert_eq!(search_max_fps(357, |f| f <= 30), Some(30));
    assert_eq!(search_max_fps(357, |_| false), None);
    // every probe is a real Query against the runtime, so no rate may be asked about twice
    let mut asked = Vec::new();
    let _ = search_max_fps(240, |f| {
        asked.push(f);
        f <= 100
    });
    let mut dedup = asked.clone();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), asked.len(), "asked twice: {asked:?}");
}

/// A keyed-mutex timeout or abandonment is never ownership, and is a rebuild.
#[test]
fn bridge_keyed_mutex_outcomes() {
    assert!(bridge_acquire_verdict(Acquired::Owned, "writer").is_ok());
    for outcome in [Acquired::TimedOut, Acquired::Abandoned] {
        for side in ["writer", "encoder"] {
            let e = bridge_acquire_verdict(outcome, side).expect_err("must not be ownership");
            assert!(e.contains(REBUILD_MARKER), "{outcome:?}/{side}: {e}");
            assert!(needs_rebuild(&anyhow::anyhow!(e)));
        }
    }
    assert!(!needs_rebuild(&anyhow::anyhow!(
        "MFXVideoENCODE_EncodeFrameAsync: device busy for 50.0 ms over 400 retries"
    )));
}

/// The LUID filter value is the bytes the dispatcher compares against.
#[test]
fn luid_bytes_are_the_in_memory_luid() {
    use windows::Win32::Foundation::LUID;
    let luid = LUID {
        LowPart: 0x1122_3344,
        HighPart: 0x5566_7788,
    };
    assert_eq!(
        luid_to_bytes(luid),
        [0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55]
    );
    // HighPart is signed; a negative one must not sign-extend into the low half
    let luid = LUID {
        LowPart: 1,
        HighPart: -1,
    };
    assert_eq!(luid_to_bytes(luid), [1, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
}

/// The CBR + NumSlice fallback, end to end on the real runtime.
///
/// `test_hooks::set_refuse_vcm` makes the *first* `ENCODE_Query` (and the VCM fps probe) answer
/// the way a VDEnc part without VCM and `MaxSliceSize` does: `MFX_ERR_UNSUPPORTED` with both
/// fields zeroed. Everything after that (the second `Query` on the fallback parameters, `Init`,
/// the encodes and a bitrate `Reset` under CBR) is the runtime's own answer.
#[test]
fn intel_quicksync_cbr_fallback_builds_encodes_and_resets() {
    super::show_measurements();
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 120;
    let cfg = EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_bps: 6_000_000,
        max_bitrate_bps: 6_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    };
    // hardware first, without the hook, so a box with no Quick Sync skips rather than fails
    match Encoder::new(cfg) {
        Ok(e) => drop(e),
        Err(e) => {
            super::skip("Intel CBR fallback test (no HW/runtime)", e);
            return;
        }
    }

    test_hooks::set_refuse_vcm(true);
    let built = Encoder::new(cfg);
    test_hooks::set_refuse_vcm(false);
    let mut encoder = built.expect(
        "a Query that refuses VCM and MaxSliceSize with MFX_ERR_UNSUPPORTED must fall back to \
         CBR + NumSlice, not lose the Intel tier",
    );

    assert_eq!(
        encoder.rate_control(),
        MFX_RATECONTROL_CBR,
        "the runtime must be running the CBR fallback"
    );
    assert!(
        matches!(encoder.slice_mode(), SliceMode::FixedCount(n) if n >= 1),
        "MaxSliceSize was refused, so slices must be a fixed count, got {:?}",
        encoder.slice_mode()
    );
    let (n, d) = encoder.effective_frame_rate();
    assert_eq!(
        n / d.max(1),
        FPS,
        "the fps probe must ask under the rate control it will run; a VCM-only probe capped this \
         at the 60 fps guess"
    );

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut produced = 0usize;
    for i in 0..20u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .expect("CBR-fallback encode");
        if i == 0 {
            assert!(
                is_keyframe(&au),
                "the first CBR-fallback frame must be an IDR"
            );
        }
        produced += au.len();
    }
    assert!(produced > 0);

    // CBR has been seen to refuse a live bitrate Reset with -14; this runtime must accept one
    let reset = encoder.set_bitrate(3_000_000);
    let reset_text = format!("{reset:?}");
    let (held_kbps, _, _) = encoder
        .read_back_brc()
        .expect("GetVideoParam after the CBR Reset");
    let (held_cpb, held_delay, held_mult) = encoder
        .read_back_cpb()
        .expect("GetVideoParam after the CBR Reset");
    let mut after = 0usize;
    for i in 20..40u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        after += encoder
            .encode_bgra(&frame, false)
            .expect("encode after the CBR bitrate Reset")
            .len();
    }
    eprintln!(
        "Intel CBR fallback OK: rc=CBR, slices={:?}, fps={n}/{d}, {produced} bytes before, \
         {after} bytes after set_bitrate(3 Mbps) -> {reset_text}, runtime holds {held_kbps} kbps,          BufferSizeInKB={held_cpb} InitialDelayInKB={held_delay} multiplier={held_mult}          ({:.1} frames of CPB at 3 Mbps / {FPS} fps)",
        encoder.slice_mode(),
        cpb_frames(held_cpb, held_mult, 3_000_000, FPS)
    );
    reset.expect("a bitrate Reset under the CBR fallback must be accepted");
}

/// The stream shape the production Quick Sync configuration must produce: one PPS id, carried
/// by every keyframe ahead of its slices. A decoder that builds its format from the first PPS of
/// a keyframe (Safari's) cannot decode a slice naming any other, and `AdaptiveCQM`, on by
/// default under `REMOTE_GAMING`, writes five and switches between them. Checked after
/// packetization, through a strict depacketizer, the way a viewer joining on a keyframe sees it.
///
/// Skips only where there is no Intel adapter. With one, an encoder that cannot be built is a
/// failure: a runtime refusing one of these options must not read as "no hardware".
#[test]
fn intel_quicksync_stream_carries_one_pps_with_every_keyframe() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    if let Err(e) = create_intel_d3d11_device() {
        super::skip("Intel stream-shape test (no Intel adapter)", e);
        return;
    }
    for qp in [Some(15u8), None] {
        let mut encoder = Encoder::new(EncoderConfig {
            width: W,
            height: H,
            fps: 60,
            bitrate_bps: 20_000_000,
            max_bitrate_bps: 20_000_000,
            profile: H264Profile::High,
            qp,
            min_qp: None,
            intra_refresh: false,
        })
        .unwrap_or_else(|e| panic!("an Intel adapter is present but the encoder failed: {e:#}"));
        let mut frame = vec![0u8; (W * H * 4) as usize];
        let mut packetizer = H264Packetizer::new();
        let mut payloads = Vec::new();
        // PPS ids the receiver holds, reset at every keyframe: each one has to stand on its
        // own for a viewer who joins on it
        let mut known: Vec<u32> = Vec::new();
        let mut used: Vec<u32> = Vec::new();
        let mut keyframes = 0;
        for i in 0..20u32 {
            fill_synthetic_bgra(&mut frame, W, H, i);
            let au = encoder
                .encode_bgra(&frame, i == 0 || i == 10)
                .expect("Intel encode_bgra");
            packetizer.packetize(
                RTP_PAYLOAD_MTU_LAN,
                &bytes::Bytes::from(au.clone()),
                &mut payloads,
            );
            let is_key = nal_slices(&au).iter().any(|&(t, _)| t == 5);
            if is_key {
                keyframes += 1;
                known.clear();
            }
            for nal in depacketize(&payloads) {
                let id_fields = leading_ues(&nal[1..], 3);
                match nal[0] & 0x1f {
                    8 => {
                        let id = id_fields.expect("PPS ids")[0];
                        if !known.contains(&id) {
                            known.push(id);
                        }
                    }
                    1 | 5 => {
                        let id = id_fields.expect("slice header")[2];
                        assert!(
                            known.contains(&id),
                            "frame {i} (qp {qp:?}): a slice names PPS {id}, the receiver holds \
                             {known:?}"
                        );
                        if !used.contains(&id) {
                            used.push(id);
                        }
                    }
                    _ => {}
                }
            }
        }
        assert_eq!(
            keyframes, 2,
            "frames 0 and 10 were asked to be IDRs (qp {qp:?})"
        );
        assert_eq!(
            used.len(),
            1,
            "qp {qp:?}: slices name PPS ids {used:?}; one is what every decoder can use"
        );
    }
}

/// NAL units as a receiver reassembles them from single-NAL, STAP-A and FU-A payloads.
fn depacketize(payloads: &[bytes::Bytes]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut fu: Option<Vec<u8>> = None;
    for p in payloads {
        match p[0] & 0x1f {
            24 => {
                let mut i = 1;
                while i + 2 <= p.len() {
                    let n = usize::from(u16::from_be_bytes([p[i], p[i + 1]]));
                    out.push(p[i + 2..i + 2 + n].to_vec());
                    i += 2 + n;
                }
            }
            28 => {
                if p[1] & 0x80 != 0 {
                    fu = Some(vec![(p[0] & 0xe0) | (p[1] & 0x1f)]);
                }
                if let Some(acc) = fu.as_mut() {
                    acc.extend_from_slice(&p[2..]);
                }
                if p[1] & 0x40 != 0 {
                    out.extend(fu.take());
                }
            }
            _ => out.push(p.to_vec()),
        }
    }
    out
}

/// A 100 Mbps session reaches the encoder as 100 Mbps, not 65.
///
/// The multiplier is chosen at `Init`: this runtime answers a `Reset` that changes it with a
/// warning and keeps the old one (measured: 20 Mbps Init, Reset to 100 Mbps at multiplier 2,
/// read back multiplier 1). Later rates are then expressed in those units.
#[test]
fn intel_quicksync_bitrate_above_65_mbps_is_not_clamped() {
    super::show_measurements();
    let mut encoder = match Encoder::new(EncoderConfig {
        width: 1920,
        height: 1080,
        fps: 60,
        bitrate_bps: 100_000_000,
        max_bitrate_bps: 100_000_000,
        profile: H264Profile::High,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("Intel >65 Mbps test (no HW/runtime)", e);
            return;
        }
    };
    let (target, max, mult) = encoder.read_back_brc().expect("GetVideoParam");
    eprintln!(
        "Intel 100 Mbps Init read back: target={target} kbps, max={max} kbps, multiplier={mult}"
    );
    assert!(
        target > 65_000,
        "a 100 Mbps session must not be clamped to 65 Mbps; the runtime holds {target} kbps"
    );

    encoder.set_bitrate(80_000_000).expect("80 Mbps Reset");
    let (target, _, _) = encoder.read_back_brc().expect("GetVideoParam");
    eprintln!("Intel after set_bitrate(80 Mbps): target={target} kbps");
    assert!(
        (79_000..=80_000).contains(&target),
        "set_bitrate(80 Mbps) reported Applied but the runtime holds {target} kbps"
    );

    encoder.set_bitrate(10_000_000).expect("10 Mbps Reset");
    let (target, _, _) = encoder.read_back_brc().expect("GetVideoParam");
    eprintln!("Intel after set_bitrate(10 Mbps): target={target} kbps");
    assert!(
        (9_000..=10_000).contains(&target),
        "after returning to 10 Mbps the runtime holds {target} kbps"
    );
}

/// A sync timeout leaves the task queued; the encoder must not reuse the bitstream it owns
/// until it completes, must recover once it has, and must make the next frame an IDR (the
/// client never received the frame the stale task encoded).
///
/// The budget is shortened to a zero-wait `SyncOperation` so a healthy GPU still "times out" on a
/// frame it has not finished. If the GPU finishes inside the call every time, the test skips.
#[test]
fn intel_quicksync_recovers_from_a_sync_timeout_with_an_idr() {
    super::show_measurements();
    const W: u32 = 1280;
    const H: u32 = 720;
    let mut encoder = match Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: 60,
        bitrate_bps: 6_000_000,
        max_bitrate_bps: 6_000_000,
        profile: H264Profile::Baseline,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("Intel sync-timeout recovery test (no HW/runtime)", e);
            return;
        }
    };
    let mut frame = vec![0u8; (W * H * 4) as usize];
    for i in 0..3u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        encoder.encode_bgra(&frame, i == 0).expect("warm-up encode");
    }

    encoder.set_sync_budget_for_test(0, 0);
    let mut timed_out = None;
    for i in 3..23u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        match encoder.encode_bgra(&frame, false) {
            Ok(_) => continue,
            Err(e) => {
                timed_out = Some(format!("{e:#}"));
                break;
            }
        }
    }
    encoder.set_sync_budget_for_test(100, 2_000);
    let Some(err) = timed_out else {
        crate::test_support::skip_because(
            "Intel sync-timeout recovery test",
            "the GPU finished every frame inside a zero-wait SyncOperation, so no timeout occurred",
        );
        return;
    };
    assert!(
        err.contains("device busy") && !err.contains(REBUILD_MARKER),
        "a sync timeout sheds one frame and is not a rebuild: {err}"
    );
    assert!(
        encoder.has_task_in_flight(),
        "the timed-out task must be remembered, not forgotten"
    );

    // let the GPU finish the stale task; the next frame must then recover and be an IDR
    std::thread::sleep(std::time::Duration::from_millis(200));
    fill_synthetic_bgra(&mut frame, W, H, 99);
    let au = encoder
        .encode_bgra(&frame, false)
        .expect("the encoder must recover once the stale task has completed");
    assert!(!encoder.has_task_in_flight());
    assert!(
        is_keyframe(&au),
        "the first frame after a dropped one must be an IDR, or the client decodes against a \
         reference it never received"
    );
    eprintln!("Intel sync-timeout recovery OK: shed one frame ({err}), next frame was an IDR");
}

#[test]
fn sps_dimensions_reads_a_known_sps() {
    // hand-built Baseline SPS (profile 66, level 31); 1080 needs frame cropping (1088 coded)
    let au = build_sps_au(1280, 720);
    assert_eq!(sps_dimensions(&au), Some((1280, 720)));
    let au = build_sps_au(1920, 1080);
    assert_eq!(sps_dimensions(&au), Some((1920, 1080)));
}

/// A minimal Baseline SPS for `w x h` (multiples of 2), with cropping to the exact size.
fn build_sps_au(w: u32, h: u32) -> Vec<u8> {
    struct W {
        bits: Vec<u8>,
    }
    impl W {
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                self.bits.push(((v >> i) & 1) as u8);
            }
        }
        fn ue(&mut self, v: u32) {
            let x = v + 1;
            let len = 32 - x.leading_zeros();
            self.put(0, len - 1);
            self.put(x, len);
        }
    }
    let mut wr = W { bits: Vec::new() };
    wr.put(66, 8);
    wr.put(0xc0, 8);
    wr.put(31, 8);
    wr.ue(0); // sps id
    wr.ue(0); // log2_max_frame_num_minus4
    wr.ue(2); // poc type 2
    wr.ue(1); // max_num_ref_frames
    wr.put(0, 1);
    let (mw, mh) = (w.div_ceil(16), h.div_ceil(16));
    wr.ue(mw - 1);
    wr.ue(mh - 1);
    wr.put(1, 1); // frame_mbs_only
    wr.put(1, 1); // direct_8x8
    let (cr, cb) = ((mw * 16 - w) / 2, (mh * 16 - h) / 2);
    if cr + cb > 0 {
        wr.put(1, 1);
        wr.ue(0);
        wr.ue(cr);
        wr.ue(0);
        wr.ue(cb);
    } else {
        wr.put(0, 1);
    }
    wr.put(0, 1); // vui
    wr.put(1, 1); // rbsp stop bit
    while !wr.bits.len().is_multiple_of(8) {
        wr.bits.push(0);
    }
    let mut out = vec![0, 0, 0, 1, 0x67];
    for chunk in wr.bits.chunks(8) {
        out.push(chunk.iter().fold(0u8, |a, &b| (a << 1) | b));
    }
    out
}
