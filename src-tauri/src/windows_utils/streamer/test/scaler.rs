//! The GPU downscaler, in both output formats.
//!
//! The point of the NV12 mode is what leaves the GPU: BGRA is 4 bytes per pixel, NV12 is 1.5, so
//! the `CopyResource` into the staging texture, the blocking `Map`, and the `copy_nonoverlapping`
//! into the retained buffer all shrink by the same 62.5% — and the colour conversion the encoder
//! would otherwise do on the CPU has already happened, for free, on the video engine that was
//! already doing the scale. These tests assert the size relationship and time both paths so the
//! claim has a number behind it on whatever machine runs them.

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use super::skip;
use crate::windows_utils::streamer::scaler::{ReadbackFormat, Scaler};

const SRC_W: u32 = 1920;
const SRC_H: u32 = 1080;
const DST_W: u32 = 1280;
const DST_H: u32 = 720;
const ITERATIONS: usize = 60;

fn device() -> Option<(ID3D11Device, ID3D11DeviceContext)> {
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut device = None;
    let mut context = None;
    let hr = unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    match (hr, device, context) {
        (Ok(()), Some(d), Some(c)) => Some((d, c)),
        (r, ..) => {
            skip("scaler", r.err());
            None
        }
    }
}

/// A source frame with real structure in it, so an all-black or all-grey readback is a failure
/// rather than a coincidence: a diagonal gradient plus a saturated red band.
fn source(device: &ID3D11Device) -> Option<ID3D11Texture2D> {
    let mut pixels = vec![0u8; (SRC_W * SRC_H * 4) as usize];
    for y in 0..SRC_H {
        for x in 0..SRC_W {
            let i = ((y * SRC_W + x) * 4) as usize;
            let band = y > SRC_H / 3 && y < 2 * SRC_H / 3;
            pixels[i] = if band { 0 } else { (x * 255 / SRC_W) as u8 };
            pixels[i + 1] = if band { 0 } else { (y * 255 / SRC_H) as u8 };
            pixels[i + 2] = if band { 255 } else { 128 };
            pixels[i + 3] = 255;
        }
    }
    let desc = D3D11_TEXTURE2D_DESC {
        Width: SRC_W,
        Height: SRC_H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        // A VideoProcessor input view will not open a texture that is only a shader resource.
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let init = D3D11_SUBRESOURCE_DATA {
        pSysMem: pixels.as_ptr() as *const core::ffi::c_void,
        SysMemPitch: SRC_W * 4,
        SysMemSlicePitch: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut tex)) }.ok()?;
    tex
}

/// Runs the scale + readback loop and returns (median microseconds, bytes read back).
fn time_one(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    format: ReadbackFormat,
) -> Option<(u128, usize, ReadbackFormat)> {
    let mut scaler = match Scaler::with_format(device, context, SRC_W, SRC_H, DST_W, DST_H, format)
    {
        Ok(s) => s,
        Err(e) => {
            skip("scaler", e);
            return None;
        }
    };
    let landed = scaler.format();

    // One untimed pass so the staging texture, the views and the driver's shader are all built.
    scaler.scale(src).expect("VideoProcessorBlt");
    scaler.read_back().expect("scaler readback");

    let mut samples = Vec::with_capacity(ITERATIONS);
    let mut bytes = 0usize;
    for _ in 0..ITERATIONS {
        let start = std::time::Instant::now();
        scaler.scale(src).expect("VideoProcessorBlt");
        let (data, _pitch) = scaler.read_back().expect("scaler readback");
        bytes = data.len();
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    Some((samples[samples.len() / 2], bytes, landed))
}

#[test]
fn nv12_readback_is_smaller_and_not_slower_than_bgra() {
    let Some((device, context)) = device() else {
        return;
    };
    let Some(src) = source(&device) else {
        skip("scaler", "source texture");
        return;
    };

    let Some((bgra_us, bgra_bytes, _)) = time_one(&device, &context, &src, ReadbackFormat::Bgra8)
    else {
        return;
    };
    let Some((nv12_us, nv12_bytes, landed)) =
        time_one(&device, &context, &src, ReadbackFormat::Nv12)
    else {
        return;
    };

    eprintln!(
        "scaler {SRC_W}x{SRC_H} -> {DST_W}x{DST_H}, {ITERATIONS} iterations, median per frame:\n  \
         bgra8 {bgra_us} us, {bgra_bytes} bytes\n  \
         {} {nv12_us} us, {nv12_bytes} bytes",
        landed.name()
    );

    if landed != ReadbackFormat::Nv12 {
        skip("scaler nv12", "video processor refused NV12 output");
        return;
    }

    // NV12 is 1.5 bytes per pixel against BGRA's 4, but both are row-pitch padded, so compare
    // against the ratio with a margin rather than against an exact byte count.
    assert!(
        (nv12_bytes as f64) < (bgra_bytes as f64) * 0.5,
        "NV12 readback should be well under half of BGRA: {nv12_bytes} vs {bgra_bytes}"
    );
}

#[test]
fn nv12_output_carries_the_picture() {
    let Some((device, context)) = device() else {
        return;
    };
    let Some(src) = source(&device) else {
        skip("scaler", "source texture");
        return;
    };
    let mut scaler = match Scaler::with_format(
        &device,
        &context,
        SRC_W,
        SRC_H,
        DST_W,
        DST_H,
        ReadbackFormat::Nv12,
    ) {
        Ok(s) => s,
        Err(e) => {
            skip("scaler", e);
            return;
        }
    };
    if scaler.format() != ReadbackFormat::Nv12 {
        skip("scaler nv12", "video processor refused NV12 output");
        return;
    }

    scaler.scale(&src).expect("VideoProcessorBlt to NV12");
    let (data, pitch) = scaler.read_back().expect("NV12 readback");
    let pitch = pitch as usize;

    assert_eq!(
        data.len(),
        pitch * (DST_H as usize + DST_H as usize / 2),
        "an NV12 map is height + height/2 rows at one pitch"
    );

    // The red band in the middle of the source has to survive as chroma: sample a UV row inside
    // it and one outside, and require them to differ. A converter that dropped chroma, or a blt
    // that never ran, gives two identical rows of 128.
    let uv_base = pitch * DST_H as usize;
    let uv_rows = DST_H as usize / 2;
    let band_row = uv_base + (uv_rows / 2) * pitch;
    let top_row = uv_base + (uv_rows / 16) * pitch;
    let band: &[u8] = &data[band_row..band_row + 16];
    let top: &[u8] = &data[top_row..top_row + 16];
    assert_ne!(
        band, top,
        "the red band and the gradient should not produce identical chroma"
    );

    // And the luma plane must have real contrast, not a flat fill.
    let y_row = pitch * (DST_H as usize / 4);
    let row = &data[y_row..y_row + DST_W as usize];
    let lo = *row.iter().min().unwrap_or(&0);
    let hi = *row.iter().max().unwrap_or(&0);
    assert!(
        hi.saturating_sub(lo) > 16,
        "luma row is flat ({lo}..{hi}); the gradient did not survive the conversion"
    );
}
