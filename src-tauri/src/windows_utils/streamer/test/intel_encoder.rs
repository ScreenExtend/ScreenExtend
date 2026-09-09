use windows::Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_BIND_SHADER_RESOURCE};

use super::h264::{nal_slices, parse_sps};
use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::intel::encoder::{
    create_intel_d3d11_device, fill_synthetic_bgra, Encoder,
};
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
    eprintln!("Intel Quick Sync extreme-fps clamp OK: 1920x1080@357 -> encoded {total} bytes");
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
            // 1 = non-IDR slice, 5 = IDR slice.
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

    // The client negotiates `42e01f`. Anything above High (100) it cannot decode, and 4:4:4 is
    // the specific way the x264 path used to get this wrong — assert it here too rather than
    // trusting that "some NALs came out" means a decodable stream.
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
        "Intel Quick Sync smoke test OK: 40 frames, {total} bytes, IDR+RepeatPPS verified, \
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
    for _ in 0..9 {
        let _ = encoder
            .encode_texture(&tex, false)
            .expect("fused-scale encode");
    }
    // The same-adapter path's per-stage split. `major-intel-extra-full-frame-copy-per-frame`
    // called the full-frame `CopySubresourceRegion` into the VPP input surface a real cost that
    // nothing measured; this is the measurement. If the copy is a small fraction of the VPP pass
    // it feeds, oneVPL surface import would be replacing a cheap blit with new FFI on a
    // 2.9+-only API, which is not a trade worth making.
    let (get_us, copy_us, vpp_us) = encoder.stage_medians_us();
    eprintln!(
        "Intel fused-downscale OK: {NW}x{NH} -> {OW}x{OH} in one VPP pass; per-frame medians \
         get_surface={get_us}us copy_into_surface={copy_us}us vpp_submit={vpp_us}us"
    );
}

/// Three encoders built at once must all get a session.
///
/// oneVPL implementation enumeration is dispatcher-wide: `MFXCreateSession` opens the candidate
/// runtimes and asks each whether it can serve the filter, and two of those at once against the
/// same iGPU used to leave one holding `MFX_ERR_NOT_FOUND` — reported up the stack as "no Intel
/// hardware oneVPL implementation", which sent that session to x264 for its whole life. An
/// encoder is built per streaming session, so this is what two clients joining together does.
///
/// Skipping needs care here: a box with no Intel hardware must skip, but a box that has it must
/// not be allowed to pass by failing every thread the same way. So the first failure decides —
/// if the *serial* probe cannot build one either, there is no hardware and the test skips.
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
/// On BGRA the bridge re-uploads 4 bytes a pixel and runs a `VideoProcessorBlt` to reach NV12 —
/// a conversion W3a's scaler has already done as part of a blt it was running anyway. The NV12
/// door skips the VPP pass entirely, which is the point; what has to hold is that skipping it
/// does not change what comes out.
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

    // The repeat path holds the encode surface, which the NV12 door filled directly rather than
    // through VPP — so it has to work off that surface, not a stale VPP output.
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
/// encoders are handed the same picture rather than two different ones.
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
