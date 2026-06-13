use anyhow::{Context as _, Result};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_STREAM,
    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
    ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
    ID3D11VideoProcessorOutputView,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

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
    dst_w: u32,
    dst_h: u32,
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
        let video_device: ID3D11VideoDevice =
            device.cast().context("capture device as ID3D11VideoDevice")?;
        let video_ctx: ID3D11VideoContext =
            context.cast().context("capture context as ID3D11VideoContext")?;

        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: src_w,
            InputHeight: src_h,
            OutputFrameRate: rate,
            OutputWidth: dst_w,
            OutputHeight: dst_h,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
            .context("CreateVideoProcessorEnumerator")?;
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }
            .context("CreateVideoProcessor")?;

        let dst_desc = D3D11_TEXTURE2D_DESC {
            Width: dst_w,
            Height: dst_h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
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

        println!("GPU downscaler ready: src={src_w}x{src_h}, dst={dst_w}x{dst_h}");

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
            dst_w,
            dst_h,
        })
    }

    pub fn scale(&mut self, src: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
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

        let mut stream = D3D11_VIDEO_PROCESSOR_STREAM::default();
        stream.Enable = true.into();
        stream.pInputSurface = core::mem::ManuallyDrop::new(Some(in_view));

        let result = unsafe {
            self.video_ctx.VideoProcessorBlt(
                &self.processor,
                &self.out_view,
                0,
                std::slice::from_ref(&stream),
            )
        };
        unsafe {
            core::mem::ManuallyDrop::drop(&mut stream.pInputSurface);
        }
        result.context("VideoProcessorBlt")?;

        Ok(&self.dst)
    }

    pub fn read_back(&mut self) -> Result<(&[u8], u32)> {
        if self.staging.is_none() {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: self.dst_w,
                Height: self.dst_h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
        let staging = self.staging.as_ref().unwrap();

        unsafe {
            self.immediate.CopyResource(staging, &self.dst);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.immediate
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(scaler staging)")?;
            let row_pitch = mapped.RowPitch;
            let total = row_pitch as usize * self.dst_h as usize;
            self.readback.resize(total, 0);
            std::ptr::copy_nonoverlapping(
                mapped.pData as *const u8,
                self.readback.as_mut_ptr(),
                total,
            );
            self.immediate.Unmap(staging, 0);
            Ok((&self.readback, row_pitch))
        }
    }
}

pub struct TextureReader {
    ctx: ID3D11DeviceContext,
    staging: ID3D11Texture2D,
    readback: Vec<u8>,
    height: u32,
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
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
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
        })
    }

    pub fn read_back(&mut self, src: &ID3D11Texture2D) -> Result<(&[u8], u32)> {
        unsafe {
            self.ctx.CopyResource(&self.staging, src);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.ctx
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .context("Map(reader staging)")?;
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
}
