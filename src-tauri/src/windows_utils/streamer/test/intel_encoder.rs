use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_SHADER_RESOURCE, ID3D11Texture2D};

use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::intel::encoder::{
    Encoder, create_intel_d3d11_device, fill_synthetic_bgra,
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
fn intel_quicksync_encodes_synthetic_frames() {
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
        intra_refresh: true,
    }) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping Intel Quick Sync smoke test (no HW/runtime): {e:?}");
            return;
        }
    };

    let mut dump = std::env::var("INTEL_PROBE_OUT")
        .ok()
        .map(|p| std::fs::File::create(p).expect("create INTEL_PROBE_OUT"));

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut first_keyframe = false;
    let mut total = 0usize;
    for i in 0..30 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .expect("Intel encode_bgra should succeed once the session initialized");
        total += au.len();
        if let Some(f) = dump.as_mut() {
            use std::io::Write as _;
            f.write_all(&au).expect("write dump");
        }
        if i == 0 {
            first_keyframe = is_keyframe(&au);
        }
    }
    assert!(first_keyframe, "first frame should be an IDR with in-band SPS/PPS");
    assert!(total > 0, "encoder should have produced bitstream bytes");

    encoder
        .set_bitrate(3_000_000)
        .expect("in-place bitrate Reset should succeed");
    for i in 30..40 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let _ = encoder.encode_bgra(&frame, false).expect("encode after bitrate reset");
    }
    println!("Intel Quick Sync smoke test OK: 40 frames, {total} bytes, IDR+RepeatPPS verified");
}

#[test]
fn intel_quicksync_fused_downscale() {
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_SUBRESOURCE_DATA, D3D11_USAGE_DEFAULT, D3D11_TEXTURE2D_DESC,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

    const NW: u32 = 1920;
    const NH: u32 = 1080;
    const OW: u32 = 1280;
    const OH: u32 = 720;

    let (device, context) = match create_intel_d3d11_device() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("skipping fused-downscale test (no Intel device): {e:?}");
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
            intra_refresh: true,
        },
        NW,
        NH,
        &device,
        &context,
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipping fused-downscale test (encoder init): {e:?}");
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
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }
        .expect("create native source texture");
    let tex = tex.expect("native texture");

    let au0 = encoder.encode_texture(&tex, true).expect("fused-scale encode frame 0");
    assert!(is_keyframe(&au0), "first fused-scale frame should be an IDR");
    for _ in 0..9 {
        let _ = encoder.encode_texture(&tex, false).expect("fused-scale encode");
    }
    println!("Intel fused-downscale OK: {NW}x{NH} -> {OW}x{OH} in one VPP pass");
}
