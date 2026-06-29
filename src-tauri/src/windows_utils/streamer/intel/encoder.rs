use std::ffi::c_void;
use std::io::Write as _;
use std::ptr;

use anyhow::{Context as _, Result, anyhow, bail};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Resource, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1, IDXGIFactory1,
};
use windows::core::Interface;

use crate::streamer::config::H264Profile;
use super::super::nvidia::encoder::EncoderConfig;
use super::intel_sys::*;

const DEVICE_BUSY_MAX_RETRIES: u32 = 30;

#[derive(Clone, Copy)]
enum SurfaceKind {
    VppIn,
    VppOut,
    Encode,
}

#[inline]
fn align_up_u16(v: u32, a: u32) -> u16 {
    (((v + a - 1) / a) * a) as u16
}

pub struct Encoder {
    vpl: Vpl,
    session: mfxSession,
    device: ID3D11Device,
    context: ID3D11DeviceContext,

    enc_param: mfxVideoParam,
    enc_frame_info: mfxFrameInfo,

    bitstream_buf: Vec<u8>,
    config: EncoderConfig,

    coded_w: u16,
    coded_h: u16,
    src_w: u32,
    src_h: u32,

    upload_texture: Option<ID3D11Texture2D>,
    last_nv12: *mut mfxFrameSurface1,

    frame_index: u64,
    idr_requested: bool,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let (device, context) = create_intel_d3d11_device()?;
        Self::new_on_device(config, config.width, config.height, &device, &context)
    }

    pub fn new_on_device(
        config: EncoderConfig,
        native_w: u32,
        native_h: u32,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self> {
        if super::super::device_vendor(device) != super::super::Vendor::Intel {
            bail!(
                "Intel Quick Sync requires an Intel D3D11 device, but the capture device is not Intel; \
                 use NVENC for this adapter (cross-adapter Quick Sync bridge is not implemented)"
            );
        }

        let creation_flags = unsafe { device.GetCreationFlags() };
        if creation_flags & D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 as u32 == 0 {
            bail!(
                "capture D3D11 device was created without VIDEO_SUPPORT; the oneVPL same-adapter \
                 Quick Sync path would fault in MFXVideoVPP_Init — use a dedicated Intel device"
            );
        }

        let vpl = Vpl::load()?;

        if let Ok(mt) = context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }

        let session = create_session(&vpl)?;

        check_strict(
            unsafe {
                (vpl.MFXVideoCORE_SetHandle)(
                    session,
                    MFX_HANDLE_D3D11_DEVICE,
                    device.as_raw() as mfxHDL,
                )
            },
            "MFXVideoCORE_SetHandle(D3D11_DEVICE)",
        )
        .inspect_err(|_| unsafe {
            (vpl.MFXClose)(session);
        })?;

        let fps = config.fps.max(1);
        let coded_w = align_up_u16(config.width, 16);
        let coded_h = align_up_u16(config.height, 16);
        let (src_w, src_h) = (native_w.max(config.width), native_h.max(config.height));

        let mut this = Self {
            vpl,
            session,
            device: device.clone(),
            context: context.clone(),
            enc_param: unsafe { std::mem::zeroed() },
            enc_frame_info: unsafe { std::mem::zeroed() },
            bitstream_buf: Vec::new(),
            config,
            coded_w,
            coded_h,
            src_w,
            src_h,
            upload_texture: None,
            last_nv12: ptr::null_mut(),
            frame_index: 0,
            idr_requested: false,
        };

        if let Err(e) = this.init_vpp() {
            unsafe { (this.vpl.MFXClose)(this.session) };
            return Err(e);
        }
        if let Err(e) = this.init_encode() {
            unsafe {
                (this.vpl.MFXVideoVPP_Close)(this.session);
                (this.vpl.MFXClose)(this.session);
            }
            return Err(e);
        }

        tprintln!(
            "Intel Quick Sync (oneVPL) H.264 encoder initialized (D3D11 / ULL): {}x{} (coded {}x{})@{}, \
             bitrate_bps={}, rc={}, qp={:?}, profile={:?}, intra_refresh={}, low_power=on, async_depth=1, gop_ref_dist=1",
            this.config.width,
            this.config.height,
            this.coded_w,
            this.coded_h,
            fps,
            this.config.bitrate_bps,
            if this.config.qp.is_some() { "cqp" } else { "cbr" },
            this.config.qp,
            this.config.profile,
            this.config.intra_refresh,
        );

        Ok(this)
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    fn frame_info(
        &self,
        fourcc: u32,
        coded_w: u16,
        coded_h: u16,
        crop_w: u16,
        crop_h: u16,
    ) -> mfxFrameInfo {
        let fps = self.config.fps.max(1);
        let is_nv12 = fourcc == MFX_FOURCC_NV12;
        mfxFrameInfo {
            FourCC: fourcc,
            ChromaFormat: MFX_CHROMAFORMAT_YUV420,
            BitDepthLuma: if is_nv12 { 8 } else { 0 },
            BitDepthChroma: if is_nv12 { 8 } else { 0 },
            PicStruct: MFX_PICSTRUCT_PROGRESSIVE,
            FrameRateExtN: fps,
            FrameRateExtD: 1,
            Width: coded_w,
            Height: coded_h,
            CropX: 0,
            CropY: 0,
            CropW: crop_w,
            CropH: crop_h,
            ..Default::default()
        }
    }

    fn init_vpp(&mut self) -> Result<()> {
        let in_coded_w = align_up_u16(self.src_w, 16);
        let in_coded_h = align_up_u16(self.src_h, 16);

        let mut par: mfxVideoParam = unsafe { std::mem::zeroed() };
        par.AsyncDepth = 1;
        par.IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY | MFX_IOPATTERN_OUT_VIDEO_MEMORY;
        let vpp = mfxInfoVPP {
            reserved: [0; 8],
            In: self.frame_info(
                MFX_FOURCC_RGB4,
                in_coded_w,
                in_coded_h,
                self.src_w as u16,
                self.src_h as u16,
            ),
            Out: self.frame_info(
                MFX_FOURCC_NV12,
                self.coded_w,
                self.coded_h,
                self.config.width as u16,
                self.config.height as u16,
            ),
        };
        par.info = mfxInfoUnion { vpp };

        check(
            unsafe { (self.vpl.MFXVideoVPP_Init)(self.session, &mut par) },
            "MFXVideoVPP_Init",
        )
    }

    fn init_encode(&mut self) -> Result<()> {
        let fps = self.config.fps.max(1);
        self.enc_frame_info = self.frame_info(
            MFX_FOURCC_NV12,
            self.coded_w,
            self.coded_h,
            self.config.width as u16,
            self.config.height as u16,
        );

        let mut par: mfxVideoParam = unsafe { std::mem::zeroed() };
        par.AsyncDepth = 1;
        par.IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY;

        let target_kbps = (self.config.bitrate_bps / 1000).clamp(1, 65000) as u16;
        let max_kbps = (self.config.max_bitrate_bps / 1000).clamp(target_kbps as u32, 65000) as u16;
        let buffer_size_kb =
            ((self.coded_w as u32 * self.coded_h as u32 * 4) / 1024).clamp(512, 65000) as u16;

        let mut mfx: mfxInfoMFX = unsafe { std::mem::zeroed() };
        mfx.FrameInfo = self.enc_frame_info;
        mfx.CodecId = MFX_CODEC_AVC;
        mfx.CodecProfile = match self.config.profile {
            H264Profile::Baseline => MFX_PROFILE_AVC_CONSTRAINED_BASELINE,
            H264Profile::Main => MFX_PROFILE_AVC_MAIN,
            H264Profile::High => MFX_PROFILE_AVC_HIGH,
        };
        mfx.LowPower = MFX_CODINGOPTION_ON;
        mfx.TargetUsage = MFX_TARGETUSAGE_BEST_SPEED;
        mfx.GopPicSize = 0xFFFF;
        mfx.GopRefDist = 1;
        mfx.IdrInterval = 0;
        mfx.NumSlice = (self.config.height / 256).clamp(4, 8) as u16;
        mfx.NumRefFrame = 1;
        mfx.BufferSizeInKB = buffer_size_kb;

        if let Some(qp) = self.config.qp {
            let q = qp as u16;
            mfx.RateControlMethod = MFX_RATECONTROL_CQP;
            mfx.InitialDelayInKB = q;
            mfx.TargetKbps = q;
            mfx.MaxKbps = q;
        } else {
            mfx.RateControlMethod = MFX_RATECONTROL_CBR;
            mfx.TargetKbps = target_kbps;
            mfx.MaxKbps = max_kbps;
            mfx.InitialDelayInKB = 0;
        }
        par.info = mfxInfoUnion { mfx };

        let mut co: mfxExtCodingOption = unsafe { std::mem::zeroed() };
        co.Header.BufferId = MFX_EXTBUFF_CODING_OPTION;
        co.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption>() as u32;
        co.MaxDecFrameBuffering = 1;
        co.NalHrdConformance = MFX_CODINGOPTION_OFF;
        co.VuiNalHrdParameters = MFX_CODINGOPTION_OFF;
        co.VuiVclHrdParameters = MFX_CODINGOPTION_OFF;
        co.PicTimingSEI = MFX_CODINGOPTION_OFF;
        co.AUDelimiter = MFX_CODINGOPTION_OFF;

        let mut co2: mfxExtCodingOption2 = unsafe { std::mem::zeroed() };
        co2.Header.BufferId = MFX_EXTBUFF_CODING_OPTION2;
        co2.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption2>() as u32;
        co2.RepeatPPS = MFX_CODINGOPTION_ON;
        co2.BRefType = MFX_B_REF_OFF;
        co2.AdaptiveI = MFX_CODINGOPTION_OFF;
        co2.AdaptiveB = MFX_CODINGOPTION_OFF;
        if self.config.intra_refresh {
            co2.IntRefType = MFX_REFRESH_HORIZONTAL;
            co2.IntRefCycleSize = fps.clamp(2, u16::MAX as u32) as u16;
            co2.IntRefQPDelta = 0;
        }

        let mut co3: mfxExtCodingOption3 = unsafe { std::mem::zeroed() };
        co3.Header.BufferId = MFX_EXTBUFF_CODING_OPTION3;
        co3.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption3>() as u32;
        co3.ScenarioInfo = MFX_SCENARIO_DISPLAY_REMOTING;
        co3.GPB = MFX_CODINGOPTION_OFF;
        if self.config.intra_refresh {
            co3.IntRefCycleDist = (fps * 4).clamp(2, u16::MAX as u32) as u16;
        }
        if self.config.qp.is_none() {
            co3.LowDelayBRC = MFX_CODINGOPTION_ON;
            co3.WinBRCSize = 1;
            co3.WinBRCMaxAvgKbps = max_kbps;
            let fps = self.config.fps.max(1);
            let avg_frame_bytes = (self.config.bitrate_bps / 8 / fps).max(1);
            co3.MaxFrameSizeP = avg_frame_bytes.saturating_mul(2);
            co3.MaxFrameSizeI = avg_frame_bytes.saturating_mul(6);
        }

        let mut ext: [*mut mfxExtBuffer; 3] = [
            &mut co as *mut _ as *mut mfxExtBuffer,
            &mut co2 as *mut _ as *mut mfxExtBuffer,
            &mut co3 as *mut _ as *mut mfxExtBuffer,
        ];
        par.ExtParam = ext.as_mut_ptr();
        par.NumExtParam = ext.len() as u16;

        check(
            unsafe { (self.vpl.MFXVideoENCODE_Init)(self.session, &mut par) },
            "MFXVideoENCODE_Init",
        )?;

        par.ExtParam = ptr::null_mut();
        par.NumExtParam = 0;
        check(
            unsafe { (self.vpl.MFXVideoENCODE_GetVideoParam)(self.session, &mut par) },
            "MFXVideoENCODE_GetVideoParam",
        )?;

        let eff_buf_kb = unsafe { par.info.mfx.BufferSizeInKB }.max(buffer_size_kb);
        self.bitstream_buf = vec![0u8; (eff_buf_kb as usize) * 1024];
        self.enc_param = par;
        self.enc_frame_info = unsafe { par.info.mfx.FrameInfo };
        Ok(())
    }

    #[allow(dead_code)]
    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if self.config.qp.is_some() {
            return Ok(());
        }
        let kbps = (bps / 1000).clamp(1, 65000) as u16;
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;

        let mut par = self.enc_param;
        par.info.mfx.TargetKbps = kbps;
        par.info.mfx.MaxKbps = kbps;
        par.ExtParam = ptr::null_mut();
        par.NumExtParam = 0;

        check(
            unsafe { (self.vpl.MFXVideoENCODE_Reset)(self.session, &mut par) },
            "MFXVideoENCODE_Reset",
        )?;
        self.enc_param = par;
        tprintln!("Intel Quick Sync bitrate reconfigured (bitrate_bps={bps})");
        Ok(())
    }

    pub fn encode_texture(&mut self, src: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<u8>> {
        let force_idr = force_idr || self.idr_requested;
        let nv12 = self.vpp_convert(src)?;
        let out = unsafe { self.encode_surface(nv12, force_idr) };
        self.replace_last_nv12(nv12);
        let out = out?;
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_bgra(&mut self, bgra: &[u8], force_idr: bool) -> Result<Vec<u8>> {
        self.encode_bgra_padded(bgra, self.config.width * 4, force_idr)
    }

    pub fn encode_bgra_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let min_len = (self.config.height as usize) * (row_pitch as usize);
        if (row_pitch as usize) < (self.config.width as usize) * 4 || data.len() < min_len {
            bail!(
                "encode_bgra_padded: need row_pitch >= {} and >= {min_len} bytes (got pitch {row_pitch}, {} bytes)",
                self.config.width * 4,
                data.len()
            );
        }
        let tex = self.ensure_upload_texture()?;
        unsafe {
            self.context.UpdateSubresource(
                &tex,
                0,
                None,
                data.as_ptr() as *const c_void,
                row_pitch,
                0,
            );
        }
        self.encode_texture(&tex, force_idr)
    }

    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        if self.last_nv12.is_null() {
            return Ok(Vec::new());
        }
        let force_idr = force_idr || self.idr_requested;
        let out = unsafe { self.encode_surface(self.last_nv12, force_idr) }?;
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    fn ensure_upload_texture(&mut self) -> Result<ID3D11Texture2D> {
        if let Some(t) = &self.upload_texture {
            return Ok(t.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.config.width,
            Height: self.config.height,
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
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("CreateTexture2D (Intel CPU-upload staging)")?;
        let tex = tex.context("CreateTexture2D returned no texture")?;
        self.upload_texture = Some(tex.clone());
        Ok(tex)
    }

    fn vpp_convert(&mut self, src: &ID3D11Texture2D) -> Result<*mut mfxFrameSurface1> {
        let in_surf = self.get_surface(SurfaceKind::VppIn)?;
        let copy_res = unsafe { self.copy_into_surface(in_surf, src) };
        let nv12 = match copy_res.and_then(|()| self.get_surface(SurfaceKind::VppOut)) {
            Ok(s) => s,
            Err(e) => {
                self.release_surface(in_surf);
                return Err(e);
            }
        };

        let mut syncp: mfxSyncPoint = ptr::null_mut();
        let mut attempts = 0u32;
        let st = loop {
            let st = unsafe {
                (self.vpl.MFXVideoVPP_RunFrameVPPAsync)(
                    self.session,
                    in_surf,
                    nv12,
                    ptr::null_mut(),
                    &mut syncp,
                )
            };
            if st == MFX_WRN_DEVICE_BUSY {
                attempts += 1;
                if attempts >= DEVICE_BUSY_MAX_RETRIES {
                    self.release_surface(in_surf);
                    self.release_surface(nv12);
                    bail!("MFXVideoVPP_RunFrameVPPAsync: device busy after {attempts} retries");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            break st;
        };
        self.release_surface(in_surf);
        match check(st, "MFXVideoVPP_RunFrameVPPAsync") {
            Ok(()) => Ok(nv12),
            Err(e) => {
                self.release_surface(nv12);
                Err(e)
            }
        }
    }

    unsafe fn copy_into_surface(
        &self,
        surf: *mut mfxFrameSurface1,
        src: &ID3D11Texture2D,
    ) -> Result<()> {
        let iface = unsafe { (*surf).FrameInterface };
        if iface.is_null() {
            bail!("VPP input surface has no FrameInterface");
        }
        let get_native = unsafe { (*iface).GetNativeHandle }
            .ok_or_else(|| anyhow!("FrameInterface::GetNativeHandle is null"))?;
        let mut res: mfxHDL = ptr::null_mut();
        let mut res_type: mfxResourceType = 0;
        check_strict(
            unsafe { get_native(surf, &mut res, &mut res_type) },
            "FrameInterface::GetNativeHandle",
        )?;
        if res_type != MFX_RESOURCE_DX11_TEXTURE || res.is_null() {
            bail!("VPP input surface is not a D3D11 texture (resource_type={res_type})");
        }
        let dst_ptr = res;
        let dst = unsafe { ID3D11Texture2D::from_raw_borrowed(&dst_ptr) }
            .ok_or_else(|| anyhow!("GetNativeHandle returned null texture"))?;
        let dst_res: ID3D11Resource = dst.cast().context("VPP input texture as ID3D11Resource")?;
        let src_res: ID3D11Resource = src.cast().context("source texture as ID3D11Resource")?;
        let box_ = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: self.src_w,
            bottom: self.src_h,
            back: 1,
        };
        unsafe {
            self.context
                .CopySubresourceRegion(&dst_res, 0, 0, 0, 0, &src_res, 0, Some(&box_));
            self.context.Flush();
        }
        Ok(())
    }

    unsafe fn encode_surface(
        &mut self,
        surf: *mut mfxFrameSurface1,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let mut ctrl: mfxEncodeCtrl = unsafe { std::mem::zeroed() };
        let ctrl_ptr = if force_idr {
            ctrl.FrameType = MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_REF;
            &mut ctrl as *mut mfxEncodeCtrl
        } else {
            ptr::null_mut()
        };

        let mut bs: mfxBitstream = unsafe { std::mem::zeroed() };
        bs.Data = self.bitstream_buf.as_mut_ptr();
        bs.MaxLength = self.bitstream_buf.len() as u32;
        bs.DataOffset = 0;
        bs.DataLength = 0;

        let mut syncp: mfxSyncPoint = ptr::null_mut();
        let mut attempts = 0u32;
        loop {
            let st = unsafe {
                (self.vpl.MFXVideoENCODE_EncodeFrameAsync)(
                    self.session,
                    ctrl_ptr,
                    surf,
                    &mut bs,
                    &mut syncp,
                )
            };
            if st == MFX_WRN_DEVICE_BUSY {
                attempts += 1;
                if attempts >= DEVICE_BUSY_MAX_RETRIES {
                    bail!("MFXVideoENCODE_EncodeFrameAsync: device busy after {attempts} retries");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            if st == MFX_ERR_MORE_DATA {
                return Ok(Vec::new());
            }
            check(st, "MFXVideoENCODE_EncodeFrameAsync")?;
            break;
        }

        if syncp.is_null() {
            return Ok(Vec::new());
        }
        let mut waited_ms = 0u32;
        loop {
            let st =
                unsafe { (self.vpl.MFXVideoCORE_SyncOperation)(self.session, syncp, 100) };
            if st == MFX_WRN_IN_EXECUTION {
                waited_ms += 100;
                if waited_ms >= 2_000 {
                    bail!("MFXVideoCORE_SyncOperation(encode): GPU made no progress for {waited_ms}ms");
                }
                continue;
            }
            check(st, "MFXVideoCORE_SyncOperation(encode)")?;
            break;
        }

        let start = bs.DataOffset as usize;
        let end = start + bs.DataLength as usize;
        let bytes = self.bitstream_buf[start..end].to_vec();
        Ok(bytes)
    }

    fn get_surface(&self, kind: SurfaceKind) -> Result<*mut mfxFrameSurface1> {
        let mut surf: *mut mfxFrameSurface1 = ptr::null_mut();
        let (f, what) = match kind {
            SurfaceKind::VppIn => (self.vpl.MFXMemory_GetSurfaceForVPP, "MFXMemory_GetSurfaceForVPP"),
            SurfaceKind::VppOut => {
                (self.vpl.MFXMemory_GetSurfaceForVPPOut, "MFXMemory_GetSurfaceForVPPOut")
            }
            SurfaceKind::Encode => {
                (self.vpl.MFXMemory_GetSurfaceForEncode, "MFXMemory_GetSurfaceForEncode")
            }
        };
        check_strict(unsafe { f(self.session, &mut surf) }, what)?;
        if surf.is_null() {
            bail!("{what} returned a null surface");
        }
        Ok(surf)
    }

    fn release_surface(&self, surf: *mut mfxFrameSurface1) {
        if surf.is_null() {
            return;
        }
        unsafe {
            let iface = (*surf).FrameInterface;
            if !iface.is_null() {
                if let Some(release) = (*iface).Release {
                    let _ = release(surf);
                }
            }
        }
    }

    fn replace_last_nv12(&mut self, surf: *mut mfxFrameSurface1) {
        if self.last_nv12 != surf {
            self.release_surface(self.last_nv12);
            self.last_nv12 = surf;
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        self.release_surface(self.last_nv12);
        self.last_nv12 = ptr::null_mut();
        unsafe {
            (self.vpl.MFXVideoENCODE_Close)(self.session);
            (self.vpl.MFXVideoVPP_Close)(self.session);
            (self.vpl.MFXClose)(self.session);
        }
    }
}

fn create_session(vpl: &Vpl) -> Result<mfxSession> {
    let loader = unsafe { (vpl.MFXLoad)() };
    if loader.is_null() {
        bail!("MFXLoad returned null (oneVPL dispatcher unavailable)");
    }

    let set_u32 = |name: &[u8], value: u32| -> Result<()> {
        let cfg = unsafe { (vpl.MFXCreateConfig)(loader) };
        if cfg.is_null() {
            bail!("MFXCreateConfig returned null");
        }
        let mut var: mfxVariant = unsafe { std::mem::zeroed() };
        var.Type = MFX_VARIANT_TYPE_U32;
        var.Data.U32 = value;
        check_strict(
            unsafe { (vpl.MFXSetConfigFilterProperty)(cfg, name.as_ptr(), var) },
            "MFXSetConfigFilterProperty",
        )
    };

    let build = || -> Result<mfxSession> {
        set_u32(b"mfxImplDescription.Impl\0", MFX_IMPL_TYPE_HARDWARE as u32)?;
        set_u32(
            b"mfxImplDescription.AccelerationMode\0",
            MFX_ACCEL_MODE_VIA_D3D11 as u32,
        )?;
        let mut session: mfxSession = ptr::null_mut();
        check_strict(
            unsafe { (vpl.MFXCreateSession)(loader, 0, &mut session) },
            "MFXCreateSession (Intel HW + D3D11)",
        )?;
        Ok(session)
    };

    match build() {
        Ok(session) => {
            let mut ver: u32 = 0;
            let _ = unsafe { (vpl.MFXQueryVersion)(session, &mut ver) };
            tprintln!(
                "oneVPL session created: HW impl via D3D11, api_version={}.{}",
                ver >> 16,
                ver & 0xFFFF
            );
            Ok(session)
        }
        Err(e) => {
            unsafe { (vpl.MFXUnload)(loader) };
            Err(e.context("no Intel hardware oneVPL implementation (would fail over to next vendor)"))
        }
    }
}

pub(crate) fn create_intel_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut chosen: Option<IDXGIAdapter1> = None;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let desc = adapter.GetDesc1().context("IDXGIAdapter1::GetDesc1")?;
            let is_software =
                (DXGI_ADAPTER_FLAG(desc.Flags as i32).0 & DXGI_ADAPTER_FLAG_SOFTWARE.0) != 0;
            let name = String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(desc.Description.len())],
            );
            if !is_software && name.to_uppercase().contains("INTEL") {
                tprintln!("selected Intel adapter for Quick Sync (index={i}, name={name})");
                chosen = Some(adapter);
                break;
            }
            i += 1;
        }
        let adapter = chosen
            .ok_or_else(|| anyhow!("no Intel adapter found; Quick Sync requires an Intel GPU"))?;

        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice on Intel adapter")?;
        Ok((
            device.context("D3D11CreateDevice returned no device")?,
            context.context("D3D11CreateDevice returned no context")?,
        ))
    }
}

pub fn probe_encode(config: &crate::streamer::config::Config, path: &str) -> Result<()> {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FPS: u32 = 60;
    const FRAMES: u32 = 300;
    const BITRATE: u32 = 10_000_000;

    tprintln!(
        "Intel Quick Sync: encoding synthetic pattern to Annex-B: path={path}, {WIDTH}x{HEIGHT}@{FPS}, {FRAMES} frames"
    );

    let mut encoder = Encoder::new(EncoderConfig {
        width: WIDTH,
        height: HEIGHT,
        fps: FPS,
        bitrate_bps: BITRATE,
        max_bitrate_bps: BITRATE,
        profile: config.h264_profile,
        qp: config.qp,
        intra_refresh: config.intra_refresh,
    })?;

    let mut file =
        std::fs::File::create(path).with_context(|| format!("creating output file {path}"))?;
    let mut frame = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    let mut total = 0usize;
    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, WIDTH, HEIGHT, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .with_context(|| format!("encoding frame {i}"))?;
        total += au.len();
        file.write_all(&au)
            .with_context(|| format!("writing frame {i}"))?;
        if i % 60 == 0 || i == FRAMES - 1 {
            tprintln!("Intel encoded frame={i} (au_bytes={}, total_bytes={total})", au.len());
        }
    }
    file.flush().context("flushing output file")?;
    tprintln!("Intel wrote Annex-B H.264: path={path}, frames={FRAMES}, total_bytes={total}");
    Ok(())
}

pub(crate) fn fill_synthetic_bgra(buf: &mut [u8], width: u32, height: u32, frame: u32) {
    let (w, h, f) = (width as usize, height as usize, frame as usize);
    let box_w = w / 6;
    let box_h = h / 6;
    let box_x = (f * 13) % w.saturating_sub(box_w).max(1);
    let box_y = (f * 7) % h.saturating_sub(box_h).max(1);
    for y in 0..h {
        let row = y * w * 4;
        for x in 0..w {
            let o = row + x * 4;
            let in_box = x >= box_x && x < box_x + box_w && y >= box_y && y < box_y + box_h;
            if in_box {
                buf[o] = 0;
                buf[o + 1] = 255;
                buf[o + 2] = 255;
            } else {
                buf[o] = ((x + f * 3) & 0xff) as u8;
                buf[o + 1] = ((y + f * 5) & 0xff) as u8;
                buf[o + 2] = ((x + y + f * 2) & 0xff) as u8;
            }
            buf[o + 3] = 255;
        }
    }
}
