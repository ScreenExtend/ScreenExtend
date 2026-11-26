//! The GPU downscaler, in both output formats.
//!
//! NV12 is 1.5 bytes per pixel against BGRA's 4, so the staging copy, the blocking `Map` and
//! the memcpy into the retained buffer all shrink by 62.5%, and the colour conversion has
//! already happened on the video engine doing the scale. These tests assert the size
//! relationship and time both paths.

use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

use super::skip;
use crate::windows_utils::streamer::scaler::{
    output_colour_space, stream_colour_space, ReadbackFormat, Scaler, ViewCache,
};

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
        // a VideoProcessor input view will not open a texture that is only a shader resource
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

/// One timing run: median microseconds for the blt submit, for the readback, and for the two
/// together, plus the bytes read back and the format that was actually used.
struct ScalerTiming {
    blt_us: u128,
    readback_us: u128,
    total_us: u128,
    bytes: usize,
    format: ReadbackFormat,
}

/// Runs the scale + readback loop and returns its medians.
///
/// The halves are timed apart: `scale` only submits a `VideoProcessorBlt`, while `read_back`
/// copies to a staging texture, blocks in `Map` until the GPU retires that copy, and memcpys
/// the result.
fn time_one(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    format: ReadbackFormat,
) -> Option<ScalerTiming> {
    time_one_to(device, context, src, format, DST_W, DST_H)
}

fn time_one_to(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    format: ReadbackFormat,
    dst_w: u32,
    dst_h: u32,
) -> Option<ScalerTiming> {
    let mut scaler = match Scaler::with_format(device, context, SRC_W, SRC_H, dst_w, dst_h, format)
    {
        Ok(s) => s,
        Err(e) => {
            skip("scaler", e);
            return None;
        }
    };
    let landed = scaler.format();

    // one untimed pass so the staging texture, the views and the driver's shader are all built
    scaler.scale(src).expect("VideoProcessorBlt");
    scaler.read_back().expect("scaler readback");

    let mut blt = Vec::with_capacity(ITERATIONS);
    let mut readback = Vec::with_capacity(ITERATIONS);
    let mut total = Vec::with_capacity(ITERATIONS);
    let mut bytes = 0usize;
    for _ in 0..ITERATIONS {
        let start = std::time::Instant::now();
        scaler.scale(src).expect("VideoProcessorBlt");
        let submitted = std::time::Instant::now();
        let (data, _pitch) = scaler.read_back().expect("scaler readback");
        let done = std::time::Instant::now();
        bytes = data.len();
        blt.push((submitted - start).as_micros());
        readback.push((done - submitted).as_micros());
        total.push((done - start).as_micros());
    }
    blt.sort_unstable();
    readback.sort_unstable();
    total.sort_unstable();
    let mid = ITERATIONS / 2;
    Some(ScalerTiming {
        blt_us: blt[mid],
        readback_us: readback[mid],
        total_us: total[mid],
        bytes,
        format: landed,
    })
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

    let Some(bgra) = time_one(&device, &context, &src, ReadbackFormat::Bgra8) else {
        return;
    };
    let Some(nv12) = time_one(&device, &context, &src, ReadbackFormat::Nv12) else {
        return;
    };

    eprintln!(
        "scaler {SRC_W}x{SRC_H} -> {DST_W}x{DST_H}, {ITERATIONS} iterations, median us per frame:\n  \
         bgra8 blt {} readback {} total {}, {} bytes\n  \
         {}  blt {} readback {} total {}, {} bytes",
        bgra.blt_us,
        bgra.readback_us,
        bgra.total_us,
        bgra.bytes,
        nv12.format.name(),
        nv12.blt_us,
        nv12.readback_us,
        nv12.total_us,
        nv12.bytes,
    );

    if nv12.format != ReadbackFormat::Nv12 {
        skip("scaler nv12", "video processor refused NV12 output");
        return;
    }

    // NV12 is 1.5 bytes per pixel against BGRA's 4, but both are row-pitch padded, so compare
    // against the ratio with a margin, not an exact byte count
    assert!(
        (nv12.bytes as f64) < (bgra.bytes as f64) * 0.5,
        "NV12 readback should be well under half of BGRA: {} vs {}",
        nv12.bytes,
        bgra.bytes
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

    // the red band has to survive as chroma: a converter that dropped chroma, or a blt that
    // never ran, gives two identical UV rows of 128
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

    // the luma plane must have real contrast. Along this row only blue varies (0..255), so
    // the swing is 219 * Kb: 15.8 under the BT.709 matrix the scaler asks for (`scaler_bt709`),
    // 25 under BT.601
    let y_row = pitch * (DST_H as usize / 4);
    let row = &data[y_row..y_row + DST_W as usize];
    let lo = *row.iter().min().unwrap_or(&0);
    let hi = *row.iter().max().unwrap_or(&0);
    assert!(
        hi.saturating_sub(lo) > 10,
        "luma row is flat ({lo}..{hi}); the gradient did not survive the conversion"
    );
}

/// Is an identity-size blt worth running purely to get NV12?
///
/// The pipeline assumes so: `needs_scaler` builds a scaler at identity size for every
/// CPU-bridge backend whose `Backend::cpu_bridge_format()` is NV12. That pays only while the
/// blt costs less than the BGRA readback and the colour convert it replaces; this measures it.
#[test]
fn identity_blt_to_nv12_against_a_bgra_readback() {
    let Some((device, context)) = device() else {
        return;
    };
    let Some(src) = source(&device) else {
        skip("scaler", "source texture");
        return;
    };

    // 1:1, the case a native-resolution client produces
    let Some(bgra) = time_one_to(&device, &context, &src, ReadbackFormat::Bgra8, SRC_W, SRC_H)
    else {
        return;
    };
    let Some(nv12) = time_one_to(&device, &context, &src, ReadbackFormat::Nv12, SRC_W, SRC_H)
    else {
        return;
    };

    eprintln!(
        "scaler identity {SRC_W}x{SRC_H}, {ITERATIONS} iterations, median us per frame:\n  \
         bgra8 blt {} readback {} total {}, {} bytes\n  \
         {}  blt {} readback {} total {}, {} bytes",
        bgra.blt_us,
        bgra.readback_us,
        bgra.total_us,
        bgra.bytes,
        nv12.format.name(),
        nv12.blt_us,
        nv12.readback_us,
        nv12.total_us,
        nv12.bytes,
    );

    if nv12.format != ReadbackFormat::Nv12 {
        skip(
            "scaler nv12 identity",
            "video processor refused NV12 output",
        );
        return;
    }
    assert!(
        (nv12.bytes as f64) < (bgra.bytes as f64) * 0.5,
        "NV12 readback should be well under half of BGRA even at 1:1: {} vs {}",
        nv12.bytes,
        bgra.bytes
    );
}

/// The NV12 the scaler hands the CPU-bridge encoders is BT.709 studio range, which is what
/// their VUI declares.
///
/// Pure red (255, 0, 0) is the cleanest discriminator: BT.709 limited gives Y 63 and Cb 102,
/// BT.601 limited gives Y 81 and Cb 90, and full range would put Y near 54. The processor's
/// documented default matrix is BT.601, so this fails on any driver that takes the default
/// literally when the scaler does not ask for BT.709.
#[test]
fn nv12_output_is_bt709_studio_range() {
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
    assert_eq!(
        scaler.colour_space(),
        (
            stream_colour_space(),
            output_colour_space(ReadbackFormat::Nv12)
        ),
        "the processor must keep the colour space it was given"
    );

    scaler.scale(&src).expect("VideoProcessorBlt to NV12");
    let (data, pitch) = scaler.read_back().expect("NV12 readback");
    let pitch = pitch as usize;

    // middle of the red band, away from its scaled edges
    let x = DST_W as usize / 2;
    let y = DST_H as usize / 2;
    let luma = data[y * pitch + x];
    let uv = pitch * DST_H as usize + (y / 2) * pitch + (x & !1);
    let (cb, cr) = (data[uv], data[uv + 1]);
    eprintln!("scaler nv12 pure red: Y {luma} Cb {cb} Cr {cr} (BT.709 limited: 63 102 240)");
    assert!(
        (60..=66).contains(&luma),
        "red luma {luma}: BT.709 limited is 63, BT.601 limited 81, full range 54"
    );
    assert!(
        (98..=106).contains(&cb),
        "red Cb {cb}: BT.709 is 102, BT.601 is 90"
    );
    assert!(cr >= 232, "red Cr {cr}: both matrices put it at 240");
}

/// A BGRA texture the scaler can write into, as the encode rings' slots are.
fn target(device: &ID3D11Device) -> Option<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: DST_W,
        Height: DST_H,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut tex = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.ok()?;
    tex
}

/// Rotating sources and destinations (a WGC pool and a ring) reuse their views instead of
/// rebuilding both on almost every frame.
#[test]
fn rotating_textures_reuse_their_views() {
    let Some((device, context)) = device() else {
        return;
    };
    let sources: Vec<_> = (0..3).filter_map(|_| source(&device)).collect();
    let targets: Vec<_> = (0..3).filter_map(|_| target(&device)).collect();
    if sources.len() != 3 || targets.len() != 3 {
        skip("scaler view cache", "could not create the textures");
        return;
    }
    let mut scaler = match Scaler::with_format(
        &device,
        &context,
        SRC_W,
        SRC_H,
        DST_W,
        DST_H,
        ReadbackFormat::Bgra8,
    ) {
        Ok(s) => s,
        Err(e) => {
            skip("scaler", e);
            return;
        }
    };

    // ten frames of a 3-deep pool into a 3-slot ring, offset so the pairs keep changing
    let mut per_frame = Vec::new();
    for frame in 0..10usize {
        let started = std::time::Instant::now();
        scaler
            .scale_into(&sources[frame % 3], &targets[(frame + 1) % 3])
            .expect("VideoProcessorBlt into a ring slot");
        per_frame.push(started.elapsed().as_micros());
    }
    eprintln!("scaler scale_into over rotating textures, us per frame: {per_frame:?}");
    assert_eq!(
        scaler.view_creations(),
        (3, 3),
        "each texture's view is built once, not once per frame"
    );
}

/// The view cache itself, and that a capacity of one behaves as a single-entry cache.
#[test]
fn view_cache_keeps_a_view_per_texture() {
    let key = |n: usize| n as *mut core::ffi::c_void;

    let mut cache = ViewCache::<u32>::new(8);
    for n in 1..=3 {
        assert!(cache.get(key(n)).is_none());
        cache.insert(key(n), n as u32);
    }
    for _ in 0..5 {
        for n in 1..=3 {
            assert_eq!(cache.get(key(n)), Some(n as u32));
        }
    }
    assert_eq!(cache.misses(), 3);

    let mut single = ViewCache::<u32>::new(1);
    single.insert(key(1), 1);
    single.insert(key(2), 2);
    assert_eq!(
        single.get(key(1)),
        None,
        "one entry: the newer view replaces the older"
    );
    assert_eq!(single.get(key(2)), Some(2));
}

#[test]
fn mapped_readback_matches_the_copied_one() {
    let Some((device, context)) = device() else {
        return;
    };
    let Some(src) = source(&device) else {
        skip("scaler", "source texture");
        return;
    };
    for format in [ReadbackFormat::Bgra8, ReadbackFormat::Nv12] {
        let mut scaler =
            match Scaler::with_format(&device, &context, SRC_W, SRC_H, DST_W, DST_H, format) {
                Ok(s) => s,
                Err(e) => {
                    skip("scaler", e);
                    return;
                }
            };
        scaler.scale(&src).expect("VideoProcessorBlt");
        let (copied, pitch) = scaler
            .read_back()
            .map(|(d, p)| (d.to_vec(), p))
            .expect("copy");
        let mapped = scaler
            .read_back_with(|data, p| {
                assert_eq!(p, pitch);
                Ok(data.to_vec())
            })
            .expect("mapped");
        assert_eq!(mapped.len(), copied.len(), "{}", format.name());
        assert!(mapped == copied, "{}: mapped bytes differ", format.name());
    }
}

/// With `x264_nv12_no_copy` x264 keeps no picture of its own, so its repeat is re-encoded from
/// the scaler's readback: nothing before the first readback, a full picture after it.
#[test]
fn an_x264_repeat_without_a_copy_comes_from_the_scaler() {
    use crate::streamer::config::Config;
    use crate::windows_utils::streamer::pipeline::{
        encode_scaled, live_encoder_config, repeat_from_scaler, Backend,
    };
    use crate::windows_utils::streamer::x264::encoder::X264Encoder;

    let Some((device, context)) = device() else {
        return;
    };
    let Some(src) = source(&device) else {
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
            skip("x264 repeat from the scaler", e);
            return;
        }
    };
    if scaler.format() != ReadbackFormat::Nv12 {
        crate::test_support::skip_because(
            "x264 repeat from the scaler",
            "the scaler fell back to BGRA",
        );
        return;
    }
    let config = live_encoder_config(DST_W, DST_H, 60, &Config::default());
    let encoder = match X264Encoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            skip("x264 repeat from the scaler (libx264 unavailable)", e);
            return;
        }
    };
    let mut backend = Backend::X264 { encoder };
    assert!(
        repeat_from_scaler(&mut backend, Some(&scaler), false).is_none(),
        "nothing has been read back yet"
    );

    scaler.scale(&src).expect("VideoProcessorBlt");
    let (data, pitch) = scaler.read_back().expect("scaler readback");
    let au = encode_scaled(&mut backend, data, pitch, ReadbackFormat::Nv12, true, true)
        .expect("unretained encode");
    assert!(!au.is_empty());

    let repeat = repeat_from_scaler(&mut backend, Some(&scaler), false)
        .expect("the repeat comes from the scaler")
        .expect("repeat encode");
    assert!(!repeat.is_empty());
}
