use anyhow::{bail, Context as _, Result};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
    ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_FLAG_DO_NOT_WAIT, D3D11_MAP_READ,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_COLOR_SPACE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_OPTIMAL_SPEED, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;

use super::tune;

/// The scaler's A/B switches, read once per scaler, never per frame.
#[derive(Clone, Copy, Debug)]
struct ScalerTune {
    /// `scaler_bt709` (ships on): declare full-range RGB in and BT.709 studio-range NV12 out,
    /// which is what the x264 VUI (`i_colmatrix = 1`, `b_fullrange = 0`) and AMF's
    /// `OutMatrixCoeff` signal. Off leaves the documented default: a BT.601 matrix and an
    /// undefined nominal range.
    bt709: bool,
    /// `scaler_view_cache` (ships on): cache a view per rotating texture instead of one view.
    view_cache: bool,
    /// `scaler_no_auto_processing` (ships on): turn off the driver's "video enhancements"
    /// (denoise, edge enhancement, skin tone), which the driver default may apply.
    no_auto_processing: bool,
    /// `scaler_usage_optimal_speed` (candidate, unmeasured): describe the content as
    /// `OPTIMAL_SPEED` rather than `PLAYBACK_NORMAL`.
    optimal_speed: bool,
    /// `scaler_map_poll` (candidate): poll the staging `Map` with `DO_NOT_WAIT` instead of
    /// blocking in it, so the device lock is taken in short slices. Same frame, nothing
    /// dropped; costs a spinning core while the copy retires.
    map_poll: bool,
}

impl ScalerTune {
    fn read() -> Self {
        Self {
            bt709: tune::on("scaler_bt709"),
            view_cache: tune::on("scaler_view_cache"),
            no_auto_processing: tune::on("scaler_no_auto_processing"),
            optimal_speed: tune::candidate("scaler_usage_optimal_speed"),
            map_poll: tune::candidate("scaler_map_poll"),
        }
    }
}

/// Views kept per scaler with the view cache on. A WGC pool, the encode rings and the DXGI
/// stage ring each rotate 3 textures; the headroom covers a frame-pool recreate without churn.
const VIEW_CACHE_ENTRIES: usize = 8;

/// Video-processor views keyed by the raw pointer of the texture they were made for.
///
/// Sound because each cached view holds a reference to its texture, so a cached address cannot
/// be freed and reused by a different texture while it is in here.
pub(crate) struct ViewCache<V> {
    entries: Vec<(*mut core::ffi::c_void, V)>,
    capacity: usize,
    misses: u64,
}

impl<V: Clone> ViewCache<V> {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            misses: 0,
        }
    }

    pub(crate) fn get(&self, key: *mut core::ffi::c_void) -> Option<V> {
        self.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    }

    /// Counts a miss and evicts the oldest view when the cache is full.
    pub(crate) fn insert(&mut self, key: *mut core::ffi::c_void, view: V) {
        self.misses += 1;
        if self.entries.len() >= self.capacity {
            self.entries.remove(0);
        }
        self.entries.push((key, view));
    }

    pub(crate) fn misses(&self) -> u64 {
        self.misses
    }
}

/// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bit layout: `Usage:1`, `RGB_Range:1`, `YCbCr_Matrix:1`,
/// `YCbCr_xvYCC:1`, `Nominal_Range:2`.
const CS_YCBCR_MATRIX_BT709: u32 = 1 << 2;
const CS_NOMINAL_RANGE_SHIFT: u32 = 4;

/// The stream (input) colour space: full-range RGB (`RGB_Range = 0`), which is what DWM
/// composes. The matrix bit means nothing for an RGB input; it is set to match the output.
pub(crate) fn stream_colour_space() -> u32 {
    CS_YCBCR_MATRIX_BT709
}

/// The output colour space for a destination `format`: BT.709 studio range for NV12, which is
/// what the encoders' VUI declares, and full-range RGB for a BGRA destination (a pure scale).
pub(crate) fn output_colour_space(format: ReadbackFormat) -> u32 {
    match format {
        ReadbackFormat::Nv12 => {
            CS_YCBCR_MATRIX_BT709
                | ((D3D11_VIDEO_PROCESSOR_NOMINAL_RANGE_16_235.0 as u32) << CS_NOMINAL_RANGE_SHIFT)
        }
        ReadbackFormat::Bgra8 => CS_YCBCR_MATRIX_BT709,
    }
}

/// How long [`map_for_read`] polls before it gives up and blocks: a copy still pending by then
/// will not retire sooner for being polled.
const MAP_POLL_LIMIT: std::time::Duration = std::time::Duration::from_millis(50);

/// Maps a staging texture for reading.
///
/// With `poll` the copy is flushed and the map retried with `DO_NOT_WAIT`, so the
/// multithread-protected device lock is held only per attempt. A blocking `Map` holds it for
/// the whole wait, which on the DXGI path stalls the capture thread's next copy. Same frame
/// either way; nothing is dropped.
///
/// # Safety
/// `res` must be a live staging texture with CPU read access on the device behind `ctx`.
unsafe fn map_for_read(
    ctx: &ID3D11DeviceContext,
    res: &ID3D11Texture2D,
    poll: bool,
    what: &'static str,
) -> Result<D3D11_MAPPED_SUBRESOURCE> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    if poll {
        ctx.Flush();
        let started = std::time::Instant::now();
        loop {
            match ctx.Map(
                res,
                0,
                D3D11_MAP_READ,
                D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32,
                Some(&mut mapped),
            ) {
                Ok(()) => return Ok(mapped),
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING => {
                    if started.elapsed() >= MAP_POLL_LIMIT {
                        break;
                    }
                    std::thread::yield_now();
                }
                Err(e) => return Err(e).context(what),
            }
        }
    }
    ctx.Map(res, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        .context(what)?;
    Ok(mapped)
}

/// What a CPU-bridge readback hands the encoder.
///
/// The scale and the colour conversion are the same `VideoProcessorBlt`, so NV12 costs nothing
/// extra and shrinks the readback, the memcpy and the upload from 4 bytes per pixel to 1.5
/// (8.29 MB to 3.11 MB at 1080p), and moves the 4:2:0 conversion off the CPU. NVENC stays on
/// BGRA: `NV_ENC_BUFFER_FORMAT_ARGB` is byte-identical to the captured texture and its internal
/// conversion beats a VideoProcessor pre-pass.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadbackFormat {
    Bgra8,
    Nv12,
}

impl ReadbackFormat {
    fn dxgi(self) -> DXGI_FORMAT {
        match self {
            ReadbackFormat::Bgra8 => DXGI_FORMAT_B8G8R8A8_UNORM,
            ReadbackFormat::Nv12 => DXGI_FORMAT_NV12,
        }
    }

    /// How many mapped rows a surface of `height` occupies. NV12 is one allocation: `height` rows
    /// of luma followed by `height / 2` rows of interleaved chroma at the same row pitch.
    fn mapped_rows(self, height: u32) -> u32 {
        match self {
            ReadbackFormat::Bgra8 => height,
            ReadbackFormat::Nv12 => height + height / 2,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ReadbackFormat::Bgra8 => "bgra8",
            ReadbackFormat::Nv12 => "nv12",
        }
    }
}

/// Whether this GPU's video processor can write `format` as an output surface. Asked rather
/// than assumed: on a WARP or remote-desktop adapter a wrong guess is a black picture, not a
/// fallback.
fn output_format_supported(
    enumerator: &ID3D11VideoProcessorEnumerator,
    format: DXGI_FORMAT,
) -> bool {
    match unsafe { enumerator.CheckVideoProcessorFormat(format) } {
        Ok(flags) => flags & D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0 as u32 != 0,
        Err(_) => false,
    }
}

pub struct Scaler {
    video_device: ID3D11VideoDevice,
    video_ctx: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    dst: ID3D11Texture2D,
    out_view: ID3D11VideoProcessorOutputView,
    immediate: ID3D11DeviceContext,
    staging: Option<ID3D11Texture2D>,
    readback: Vec<u8>,
    /// The row pitch of what `readback` holds, once [`Scaler::read_back`] has filled it.
    readback_pitch: Option<u32>,
    dst_w: u32,
    dst_h: u32,
    format: ReadbackFormat,
    in_views: ViewCache<ID3D11VideoProcessorInputView>,
    out_views: ViewCache<ID3D11VideoProcessorOutputView>,
    map_poll: bool,
    /// The processor's colour spaces as read back after they were set: `(stream, output)`.
    colour_space: (u32, u32),
}

unsafe impl Send for Scaler {}

impl Scaler {
    pub fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
    ) -> Result<Self> {
        Self::with_format(
            device,
            context,
            src_w,
            src_h,
            dst_w,
            dst_h,
            ReadbackFormat::Bgra8,
        )
    }

    /// Builds a scaler whose destination surface is `format`.
    ///
    /// An NV12 request that the video processor will not honour falls back to BGRA rather than
    /// failing the session: the caller's encoder accepts both.
    #[allow(clippy::too_many_arguments)]
    pub fn with_format(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src_w: u32,
        src_h: u32,
        dst_w: u32,
        dst_h: u32,
        format: ReadbackFormat,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device
            .cast()
            .context("capture device as ID3D11VideoDevice")?;
        let video_ctx: ID3D11VideoContext = context
            .cast()
            .context("capture context as ID3D11VideoContext")?;
        let switches = ScalerTune::read();

        let rate = DXGI_RATIONAL {
            Numerator: 60,
            Denominator: 1,
        };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: rate,
            OutputWidth: dst_w,
            OutputHeight: dst_h,
            Usage: if switches.optimal_speed {
                D3D11_VIDEO_USAGE_OPTIMAL_SPEED
            } else {
                D3D11_VIDEO_USAGE_PLAYBACK_NORMAL
            },
        };

        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
            .context("CreateVideoProcessorEnumerator")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;

        // NV12 is 4:2:0, so an odd dimension is an invalid surface. `scaled_dims` already rounds
        // to even; this is the last guard before the driver
        let format = if format == ReadbackFormat::Nv12
            && (!dst_w.is_multiple_of(2)
                || !dst_h.is_multiple_of(2)
                || !output_format_supported(&enumerator, DXGI_FORMAT_NV12))
        {
            teprintln!(
                "scaler: NV12 output unavailable for {dst_w}x{dst_h} on this video processor; \
                 falling back to BGRA (the encoder will convert on the CPU)"
            );
            ReadbackFormat::Bgra8
        } else {
            format
        };

        // set once, applies to every blt. colour spaces are read back because a driver may
        // ignore a setting it does not implement
        if switches.no_auto_processing {
            unsafe { video_ctx.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false) };
        }
        let colour_space = if switches.bt709 {
            let stream = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: stream_colour_space(),
            };
            let output = D3D11_VIDEO_PROCESSOR_COLOR_SPACE {
                _bitfield: output_colour_space(format),
            };
            unsafe {
                video_ctx.VideoProcessorSetStreamColorSpace(&processor, 0, &stream);
                video_ctx.VideoProcessorSetOutputColorSpace(&processor, &output);
            }
            let read = unsafe {
                (
                    video_ctx
                        .VideoProcessorGetStreamColorSpace(&processor, 0)
                        ._bitfield,
                    video_ctx
                        .VideoProcessorGetOutputColorSpace(&processor)
                        ._bitfield,
                )
            };
            if read != (stream._bitfield, output._bitfield) {
                teprintln!(
                    "scaler: the video processor did not keep the colour space it was given \
                     (stream {:#x} -> {:#x}, output {:#x} -> {:#x}); the encoders' BT.709 VUI \
                     may not describe its samples",
                    stream._bitfield,
                    read.0,
                    output._bitfield,
                    read.1
                );
            }
            read
        } else {
            unsafe {
                (
                    video_ctx
                        .VideoProcessorGetStreamColorSpace(&processor, 0)
                        ._bitfield,
                    video_ctx
                        .VideoProcessorGetOutputColorSpace(&processor)
                        ._bitfield,
                )
            }
        };

        // several drivers refuse SHADER_RESOURCE alongside RENDER_TARGET on an NV12 surface
        let bind = match format {
            ReadbackFormat::Bgra8 => D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0,
            ReadbackFormat::Nv12 => D3D11_BIND_RENDER_TARGET.0,
        } as u32;
        let dst_desc = D3D11_TEXTURE2D_DESC {
            Width: dst_w,
            Height: dst_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format.dxgi(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut dst: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&dst_desc, None, Some(&mut dst)) }
            .context("CreateTexture2D (scaler destination)")?;
        let dst = dst.context("scaler destination texture was null")?;

        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &dst,
                &enumerator,
                &out_desc,
                Some(&mut out_view),
            )
        }
        .context("CreateVideoProcessorOutputView")?;
        let out_view = out_view.context("scaler output view was null")?;

        tprintln!(
            "GPU downscaler ready: src={src_w}x{src_h}, dst={dst_w}x{dst_h}, out={}, \
             colour space stream={:#x} output={:#x}, auto-processing {}, view cache {}",
            format.name(),
            colour_space.0,
            colour_space.1,
            if switches.no_auto_processing {
                "off"
            } else {
                "driver default"
            },
            if switches.view_cache {
                VIEW_CACHE_ENTRIES
            } else {
                1
            }
        );
        let view_entries = if switches.view_cache {
            VIEW_CACHE_ENTRIES
        } else {
            1
        };

        Ok(Self {
            video_device,
            video_ctx,
            processor,
            enumerator,
            dst,
            out_view,
            immediate: context.clone(),
            staging: None,
            readback: Vec::new(),
            readback_pitch: None,
            dst_w,
            dst_h,
            format,
            in_views: ViewCache::new(view_entries),
            out_views: ViewCache::new(view_entries),
            map_poll: switches.map_poll,
            colour_space,
        })
    }

    /// What `read_back` will actually produce, which is not necessarily what was asked for.
    pub fn format(&self) -> ReadbackFormat {
        self.format
    }

    /// The processor's `(stream, output)` colour spaces as read back after creation, in the
    /// `D3D11_VIDEO_PROCESSOR_COLOR_SPACE` bit layout.
    pub fn colour_space(&self) -> (u32, u32) {
        self.colour_space
    }

    /// How many input and output views this scaler has created, i.e. its cache misses.
    pub fn view_creations(&self) -> (u64, u64) {
        (self.in_views.misses(), self.out_views.misses())
    }

    pub fn scale(&mut self, src: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
        let in_view = self.input_view(src)?;
        self.run_blt(in_view, &self.out_view)?;
        Ok(&self.dst)
    }

    pub fn scale_into(&mut self, src: &ID3D11Texture2D, dst: &ID3D11Texture2D) -> Result<()> {
        let in_view = self.input_view(src)?;
        let out_view = self.output_view_for(dst)?;
        self.run_blt(in_view, &out_view)
    }

    fn input_view(&mut self, src: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorInputView> {
        let key = src.as_raw();
        if let Some(view) = self.in_views.get(key) {
            return Ok(view);
        }
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut in_view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                src,
                &self.enumerator,
                &in_desc,
                Some(&mut in_view),
            )
        }
        .context("CreateVideoProcessorInputView")?;
        let in_view = in_view.context("scaler input view was null")?;
        self.in_views.insert(key, in_view.clone());
        Ok(in_view)
    }

    fn output_view_for(&mut self, dst: &ID3D11Texture2D) -> Result<ID3D11VideoProcessorOutputView> {
        let key = dst.as_raw();
        if let Some(view) = self.out_views.get(key) {
            return Ok(view);
        }
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            ..Default::default()
        };
        let mut out_view: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            self.video_device.CreateVideoProcessorOutputView(
                dst,
                &self.enumerator,
                &out_desc,
                Some(&mut out_view),
            )
        }
        .context("CreateVideoProcessorOutputView (external target)")?;
        let out_view = out_view.context("external scaler output view was null")?;
        self.out_views.insert(key, out_view.clone());
        Ok(out_view)
    }

    fn run_blt(
        &self,
        in_view: ID3D11VideoProcessorInputView,
        out_view: &ID3D11VideoProcessorOutputView,
    ) -> Result<()> {
        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: core::mem::ManuallyDrop::new(Some(in_view)),
            ..Default::default()
        };

        let result = unsafe {
            self.video_ctx.VideoProcessorBlt(
                &self.processor,
                out_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe {
            core::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
        }
        result.context("VideoProcessorBlt")
    }

    /// The staging texture a readback maps, created on first use.
    fn staging_texture(&mut self) -> Result<ID3D11Texture2D> {
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.dst_w,
                Height: self.dst_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: self.format.dxgi(),
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
            };
            let mut staging: Option<ID3D11Texture2D> = None;
            let device = unsafe { self.immediate.GetDevice() }
                .context("staging: capture context had no device")?;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
                .context("CreateTexture2D (scaler staging)")?;
            self.staging = Some(staging.context("scaler staging texture was null")?);
        }
        let Some(staging) = self.staging.as_ref() else {
            bail!("scaler staging texture missing after creation")
        };
        Ok(staging.clone())
    }

    pub fn read_back(&mut self) -> Result<(&[u8], u32)> {
        let staging = self.staging_texture()?;
        let rows = self.format.mapped_rows(self.dst_h);
        unsafe {
            self.immediate.CopyResource(&staging, &self.dst);
            let mapped = map_for_read(
                &self.immediate,
                &staging,
                self.map_poll,
                "Map(scaler staging)",
            )?;
            let row_pitch = mapped.RowPitch;
            let total = row_pitch as usize * rows as usize;
            self.readback.resize(total, 0);
            std::ptr::copy_nonoverlapping(
                mapped.pData as *const u8,
                self.readback.as_mut_ptr(),
                total,
            );
            self.immediate.Unmap(&staging, 0);
            self.readback_pitch = Some(row_pitch);
            Ok((&self.readback, row_pitch))
        }
    }

    /// The NV12 picture the last [`read_back`](Self::read_back) left in this scaler's buffer,
    /// with its row pitch. `None` before the first one, or for a BGRA scaler.
    pub fn last_nv12_readback(&self) -> Option<(&[u8], u32)> {
        if self.format != ReadbackFormat::Nv12 {
            return None;
        }
        Some((&self.readback, self.readback_pitch?))
    }

    /// [`read_back`](Self::read_back) without the retained copy: `f` reads the mapped staging
    /// memory directly and the texture is unmapped when it returns.
    ///
    /// Every CPU-bridge encoder copies the picture into a surface of its own, so this saves a
    /// second full-frame copy (3.1 MB NV12 or 8.3 MB BGRA at 1080p). A held map takes no device
    /// lock, so the capture thread is not held up while `f` runs. `f` must not keep the slice:
    /// it is gone the moment `f` returns.
    pub fn read_back_with<R>(&mut self, f: impl FnOnce(&[u8], u32) -> Result<R>) -> Result<R> {
        let staging = self.staging_texture()?;
        let rows = self.format.mapped_rows(self.dst_h);
        unsafe {
            self.immediate.CopyResource(&staging, &self.dst);
            let mapped = map_for_read(
                &self.immediate,
                &staging,
                self.map_poll,
                "Map(scaler staging)",
            )?;
            let _unmap = Unmap {
                ctx: &self.immediate,
                res: &staging,
            };
            let row_pitch = mapped.RowPitch;
            let total = row_pitch as usize * rows as usize;
            // SAFETY: a successful map of a `rows`-row staging texture at `row_pitch` exposes
            // `row_pitch * rows` readable bytes until the `Unmap` the guard runs after `f`.
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, total);
            f(data, row_pitch)
        }
    }
}

/// Unmaps subresource 0 of `res` when dropped, so an early return from the reader cannot leave
/// the staging texture mapped.
struct Unmap<'a> {
    ctx: &'a ID3D11DeviceContext,
    res: &'a ID3D11Texture2D,
}

impl Drop for Unmap<'_> {
    fn drop(&mut self) {
        // SAFETY: constructed only after a successful `Map` of exactly this subresource.
        unsafe { self.ctx.Unmap(self.res, 0) };
    }
}

pub struct TextureReader {
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    readback: Vec<u8>,
    height: u32,
    map_poll: bool,
}

unsafe impl Send for TextureReader {}

impl TextureReader {
    pub fn new(
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }
            .context("CreateTexture2D (reader staging)")?;
        Ok(Self {
            ctx: ctx.clone(),
            staging: staging.context("reader staging texture was null")?,
            readback: Vec::new(),
            height,
            map_poll: tune::candidate("scaler_map_poll"),
        })
    }

    pub fn read_back(&mut self, src: &ID3D11Texture2D) -> Result<(&[u8], u32)> {
        unsafe {
            self.ctx.CopyResource(&self.staging, src);
            let mapped = map_for_read(
                &self.ctx,
                &self.staging,
                self.map_poll,
                "Map(reader staging)",
            )?;
            let row_pitch = mapped.RowPitch;
            let total = row_pitch as usize * self.height as usize;
            self.readback.resize(total, 0);
            std::ptr::copy_nonoverlapping(
                mapped.pData as *const u8,
                self.readback.as_mut_ptr(),
                total,
            );
            self.ctx.Unmap(&self.staging, 0);
            Ok((&self.readback, row_pitch))
        }
    }

    /// [`read_back`](Self::read_back) without the retained copy; see [`Scaler::read_back_with`].
    pub fn read_back_with<R>(
        &mut self,
        src: &ID3D11Texture2D,
        f: impl FnOnce(&[u8], u32) -> Result<R>,
    ) -> Result<R> {
        unsafe {
            self.ctx.CopyResource(&self.staging, src);
            let mapped = map_for_read(
                &self.ctx,
                &self.staging,
                self.map_poll,
                "Map(reader staging)",
            )?;
            let _unmap = Unmap {
                ctx: &self.ctx,
                res: &self.staging,
            };
            let row_pitch = mapped.RowPitch;
            let total = row_pitch as usize * self.height as usize;
            // SAFETY: as in `Scaler::read_back_with`.
            let data = std::slice::from_raw_parts(mapped.pData as *const u8, total);
            f(data, row_pitch)
        }
    }
}
