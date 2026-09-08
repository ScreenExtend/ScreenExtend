use std::ffi::c_void;
use std::io::Write as _;
use std::ptr;

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Resource, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_BOX, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use super::super::nvidia::encoder::EncoderConfig;
use super::intel_sys::*;
use crate::streamer::config::H264Profile;
use crate::streamer::platform::BitrateApplied;

const DEVICE_BUSY_MAX_RETRIES: u32 = 30;
const INTEL_FPS_QUERY_FALLBACK: u32 = 60;
const KEY_WRITER: u64 = 0;
const KEY_ENCODER: u64 = 1;
const KEY_TIMEOUT_MS: u32 = 1000;
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1; // `DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE`

#[derive(Clone, Copy)]
enum SurfaceKind {
    VppIn,
    VppOut,
    Encode,
}

#[inline]
fn align_up_u16(v: u32, a: u32) -> u16 {
    (v.div_ceil(a) * a) as u16
}

struct InputBridge {
    enc_tex: ID3D11Texture2D,
    enc_mutex: IDXGIKeyedMutex,
    cap_context: ID3D11DeviceContext,
    cap_tex: ID3D11Texture2D,
    cap_mutex: IDXGIKeyedMutex,
    shared_handle: HANDLE,
}

impl InputBridge {
    fn new(
        enc_device: &ID3D11Device,
        cap_device: &ID3D11Device,
        cap_context: &ID3D11DeviceContext,
        w: u32,
        h: u32,
    ) -> Result<Self> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0
                | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32,
        };
        let mut enc_tex: Option<ID3D11Texture2D> = None;
        unsafe { enc_device.CreateTexture2D(&desc, None, Some(&mut enc_tex)) }
            .context("CreateTexture2D (Quick Sync input bridge, encode side)")?;
        let enc_tex = enc_tex.context("bridge texture was null")?;

        let resource: IDXGIResource1 = enc_tex
            .cast()
            .context("bridge texture as IDXGIResource1 (is it SHARED_NTHANDLE?)")?;
        let shared_handle =
            unsafe { resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_RW, PCWSTR::null()) }
                .context("IDXGIResource1::CreateSharedHandle (Quick Sync input bridge)")?;
        let enc_mutex: IDXGIKeyedMutex = enc_tex
            .cast()
            .context("bridge texture as IDXGIKeyedMutex (encode side)")?;

        let cap_device1: ID3D11Device1 = cap_device
            .cast()
            .context("capture device as ID3D11Device1")?;
        let cap_tex: ID3D11Texture2D = unsafe { cap_device1.OpenSharedResource1(shared_handle) }
            .inspect_err(|_| unsafe {
                let _ = CloseHandle(shared_handle);
            })
            .context("OpenSharedResource1 (Quick Sync input bridge, capture side)")?;
        let cap_mutex: IDXGIKeyedMutex = cap_tex
            .cast()
            .context("bridge texture as IDXGIKeyedMutex (capture side)")?;

        Ok(Self {
            enc_tex,
            enc_mutex,
            cap_context: cap_context.clone(),
            cap_tex,
            cap_mutex,
            shared_handle,
        })
    }

    unsafe fn upload(&self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<()> {
        unsafe { self.cap_mutex.AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS) }
            .context("Quick Sync bridge AcquireSync(writer)")?;
        let copy = (|| -> Result<()> {
            let dst_res: ID3D11Resource = self
                .cap_tex
                .cast()
                .context("bridge texture as ID3D11Resource")?;
            let src_res: ID3D11Resource = src.cast().context("source texture as ID3D11Resource")?;
            let box_ = D3D11_BOX {
                left: 0,
                top: 0,
                front: 0,
                right: w,
                bottom: h,
                back: 1,
            };
            unsafe {
                self.cap_context.CopySubresourceRegion(
                    &dst_res,
                    0,
                    0,
                    0,
                    0,
                    &src_res,
                    0,
                    Some(&box_),
                );
                self.cap_context.Flush();
            }
            Ok(())
        })();
        let _ = unsafe { self.cap_mutex.ReleaseSync(KEY_ENCODER) };
        copy
    }
}

impl Drop for InputBridge {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.shared_handle);
        }
    }
}

pub struct Encoder {
    vpl: Vpl,
    session: mfxSession,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    bridge: Option<InputBridge>,
    /// The dispatcher loader owns the enumerated implementations and must outlive the session;
    /// it is released with `MFXUnload` after `MFXClose`. It used to be dropped on the floor on
    /// the success path, leaking one loader plus its implementation descriptions per encoder
    /// construction — and an encoder is built per streaming session.
    loader: mfxLoader,
    enc_param: mfxVideoParam,
    enc_frame_info: mfxFrameInfo,

    /// The three ext buffers, owned so they can be re-attached to `Reset`.
    ///
    /// oneVPL applies whatever ext buffers are present at `Reset`; ones omitted revert to the
    /// runtime's defaults. `set_bitrate` used to `Reset` with `NumExtParam = 0`, so the first
    /// bitrate change — seconds into a session, from the loss-based driver — restored HRD
    /// conformance, PicTiming SEI, the default `MaxDecFrameBuffering`, B-pyramid, and dropped
    /// `RepeatPPS`, the per-frame size caps and intra refresh. The low-latency configuration
    /// lasted until the first congestion event and never came back.
    ext_co: Box<mfxExtCodingOption>,
    ext_co2: Box<mfxExtCodingOption2>,
    ext_co3: Box<mfxExtCodingOption3>,
    ext_ptrs: Vec<*mut mfxExtBuffer>,

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

        let (src_w, src_h) = (native_w.max(config.width), native_h.max(config.height));

        let creation_flags = unsafe { device.GetCreationFlags() };
        let capture_has_video = creation_flags & D3D11_CREATE_DEVICE_VIDEO_SUPPORT.0 != 0;
        let (enc_device, enc_context, bridge) = if capture_has_video {
            (device.clone(), context.clone(), None)
        } else {
            let (enc_device, enc_context) = create_intel_d3d11_device_for(device).context(
                "creating a dedicated Intel device (capture device lacks VIDEO_SUPPORT)",
            )?;
            let bridge = InputBridge::new(&enc_device, device, context, src_w, src_h)
                .context("building the same-adapter Quick Sync input bridge")?;
            tprintln!(
                "Intel Quick Sync: capture device lacks VIDEO_SUPPORT; routing through a dedicated \
                 Intel device via a same-adapter shared-texture bridge ({src_w}x{src_h})"
            );
            (enc_device, enc_context, Some(bridge))
        };

        let vpl = Vpl::load()?;

        if let Ok(mt) = enc_context.cast::<ID3D11Multithread>() {
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }

        let (session, loader) = create_session(&vpl)?;

        check_strict(
            unsafe {
                (vpl.MFXVideoCORE_SetHandle)(
                    session,
                    MFX_HANDLE_D3D11_DEVICE,
                    enc_device.as_raw() as mfxHDL,
                )
            },
            "MFXVideoCORE_SetHandle(D3D11_DEVICE)",
        )
        .inspect_err(|_| unsafe {
            (vpl.MFXClose)(session);
            (vpl.MFXUnload)(loader);
        })?;

        let requested_fps = config.fps.max(1);
        let max_fps = query_max_supported_fps(&vpl, session, &config, requested_fps);
        let config = {
            let mut c = config;
            if requested_fps > max_fps {
                tprintln!(
                    "Intel Quick Sync: capping encode frame rate {requested_fps} -> {max_fps} fps \
                     (highest rate this device's oneVPL H.264 encoder reports as supported)"
                );
                c.fps = max_fps;
            }
            c
        };

        let fps = config.fps.max(1);
        let coded_w = align_up_u16(config.width, 16);
        let coded_h = align_up_u16(config.height, 16);

        let mut this = Self {
            vpl,
            session,
            device: enc_device,
            context: enc_context,
            bridge,
            loader,
            enc_param: unsafe { std::mem::zeroed() },
            enc_frame_info: unsafe { std::mem::zeroed() },
            ext_co: Box::new(unsafe { std::mem::zeroed() }),
            ext_co2: Box::new(unsafe { std::mem::zeroed() }),
            ext_co3: Box::new(unsafe { std::mem::zeroed() }),
            ext_ptrs: Vec::new(),
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

        // Dropping `this` runs `Drop for Encoder`, which closes the encode and VPP components,
        // the session and the loader. Closing them here as well double-closed the session
        // (harmless-looking, since every return value was discarded, but a second `MFXClose` on
        // a freed handle is the runtime's problem to survive, and the loader would now be
        // unloaded twice).
        this.init_vpp()?;
        this.init_encode()?;

        // Requested geometry only. What the runtime actually agreed to is the "oneVPL effective"
        // line `init_encode` prints from `GetVideoParam`; this one used to assert `low_power=on,
        // async_depth=1, gop_ref_dist=1` and an `rc=cbr` that was never read back.
        tprintln!(
            "Intel Quick Sync (oneVPL) H.264 encoder initialized (D3D11 / ULL): {}x{} \
             (coded {}x{})@{}, requested bitrate_bps={}, qp={:?}, profile={:?}, intra_refresh={}",
            this.config.width,
            this.config.height,
            this.coded_w,
            this.coded_h,
            fps,
            this.config.bitrate_bps,
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
        let buffer_size_kb = vbv_kb(self.config.bitrate_bps, fps);

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
            // VCM, not CBR. oneVPL documents `LowDelayBRC`, `MaxFrameSizeI` and `MaxFrameSizeP`
            // as applying to VBR/QVBR/VCM only — under CBR, which is what this branch used to
            // select, all three are no-ops, so the Quick Sync path has had no working per-frame
            // size cap at all. The `ENCODE_Query` below asks the runtime before committing, and
            // falls back to CBR (dropping the three dead fields) if VCM is refused, which it is
            // on some generations under `LowPower = ON`.
            mfx.RateControlMethod = MFX_RATECONTROL_VCM;
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
        // Quality ceiling under CBR: a floor on QP can only shrink the stream, so the
        // loss-based controller keeps the top end.
        if let Some(q) = self.config.min_qp.filter(|&q| q > 1) {
            co2.MinQPI = q;
            co2.MinQPP = q;
            co2.MinQPB = q;
        }
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

        // Boxed so the pointers stay valid across every later move of `Encoder`, and owned so
        // `set_bitrate` can hand the same array to `Reset`.
        *self.ext_co = co;
        *self.ext_co2 = co2;
        *self.ext_co3 = co3;
        self.rebuild_ext_ptrs();
        self.attach_ext(&mut par);

        // Ask before committing. `Query` is the API whose job is to report which requested
        // parameters the runtime altered or dropped, and it was not called anywhere in this
        // file — so VCM, LowDelayBRC, the frame-size caps and LowPower were all asserted.
        //
        // In and out must be distinct allocations: the runtime writes the corrected parameters
        // into `out` while reading `in`, and aliasing them (which `query_fps_supported` gets
        // away with because it attaches no ext buffers) faults once ext buffers are involved.
        // `out`'s ext buffers carry headers only, per the API contract.
        let mut out_co: mfxExtCodingOption = unsafe { std::mem::zeroed() };
        out_co.Header = self.ext_co.Header;
        let mut out_co2: mfxExtCodingOption2 = unsafe { std::mem::zeroed() };
        out_co2.Header = self.ext_co2.Header;
        let mut out_co3: mfxExtCodingOption3 = unsafe { std::mem::zeroed() };
        out_co3.Header = self.ext_co3.Header;
        let mut out_ptrs: [*mut mfxExtBuffer; 3] = [
            &mut out_co as *mut _ as *mut mfxExtBuffer,
            &mut out_co2 as *mut _ as *mut mfxExtBuffer,
            &mut out_co3 as *mut _ as *mut mfxExtBuffer,
        ];
        let mut out = par;
        out.ExtParam = out_ptrs.as_mut_ptr();
        out.NumExtParam = out_ptrs.len() as u16;

        let query_status =
            unsafe { (self.vpl.MFXVideoENCODE_Query)(self.session, &mut par, &mut out) };
        if query_status < MFX_ERR_NONE {
            bail!("MFXVideoENCODE_Query rejected the encode parameters: mfxStatus={query_status}");
        }
        if query_status > MFX_ERR_NONE {
            tprintln!(
                "oneVPL: ENCODE_Query returned {} ({query_status}); the runtime changed something",
                warning_name(query_status)
            );
        }

        // `Query` writes the values it will honour into `out`, so a rate control it refused
        // comes back as something else. Drop the fields that only exist for the mode we asked
        // for, rather than leaving them set and claiming a cap that is not applied.
        let effective_rc = unsafe { out.info.mfx.RateControlMethod };
        if self.config.qp.is_none() && effective_rc != MFX_RATECONTROL_VCM {
            teprintln!(
                "oneVPL: VCM refused (runtime chose rc={effective_rc}); falling back to CBR \
                 without LowDelayBRC or the per-frame size caps, which do not apply under CBR"
            );
            par.info.mfx.RateControlMethod = MFX_RATECONTROL_CBR;
            par.info.mfx.TargetKbps = target_kbps;
            par.info.mfx.MaxKbps = max_kbps;
            par.info.mfx.BufferSizeInKB = buffer_size_kb;
            self.ext_co3.LowDelayBRC = MFX_CODINGOPTION_UNKNOWN;
            self.ext_co3.MaxFrameSizeI = 0;
            self.ext_co3.MaxFrameSizeP = 0;
            self.attach_ext(&mut par);
        }

        check_no_software_fallback(
            unsafe { (self.vpl.MFXVideoENCODE_Init)(self.session, &mut par) },
            "MFXVideoENCODE_Init",
        )?;

        // Read back *with* the ext buffers attached, so CO2/CO3 report their effective values.
        // Reading back with `NumExtParam = 0`, as this used to, returns only the base
        // `mfxInfoMFX` — which is why nobody could tell whether the ULL configuration took.
        self.attach_ext(&mut par);
        check(
            unsafe { (self.vpl.MFXVideoENCODE_GetVideoParam)(self.session, &mut par) },
            "MFXVideoENCODE_GetVideoParam",
        )?;

        let eff = unsafe { par.info.mfx };
        // Two different needs that were conflated into one number: the CPB the rate controller
        // may bank into, which must be about one frame, and the output allocation, which must
        // hold the largest access unit. `BufferSizeInKB` is the former; the latter gets its own
        // floor so a large IDR still fits.
        let out_kb = eff.BufferSizeInKB.max(buffer_size_kb).max(512);
        self.bitstream_buf = vec![0u8; (out_kb as usize) * 1024];
        self.enc_param = par;
        self.enc_frame_info = eff.FrameInfo;

        // Effective, not requested. The old line printed `self.config` and asserted
        // "low_power=on, async_depth=1, gop_ref_dist=1" without ever reading any of them back.
        tprintln!(
            "oneVPL effective: rc={}, target_kbps={}, max_kbps={}, buffer_kb={} (requested {}), \
             initial_delay_kb={}, low_power={}, target_usage={}, gop_ref_dist={}, num_ref={}, \
             num_slice={}, async_depth={}, low_delay_brc={}, max_frame_i={}, max_frame_p={}, \
             max_dec_frame_buffering={}, repeat_pps={}, nal_hrd={}, int_ref_type={}, \
             int_ref_cycle={}, out_buf_kb={}",
            eff.RateControlMethod,
            eff.TargetKbps,
            eff.MaxKbps,
            eff.BufferSizeInKB,
            buffer_size_kb,
            eff.InitialDelayInKB,
            eff.LowPower,
            eff.TargetUsage,
            eff.GopRefDist,
            eff.NumRefFrame,
            eff.NumSlice,
            par.AsyncDepth,
            self.ext_co3.LowDelayBRC,
            self.ext_co3.MaxFrameSizeI,
            self.ext_co3.MaxFrameSizeP,
            self.ext_co.MaxDecFrameBuffering,
            self.ext_co2.RepeatPPS,
            self.ext_co.NalHrdConformance,
            self.ext_co2.IntRefType,
            self.ext_co2.IntRefCycleSize,
            out_kb,
        );
        Ok(())
    }

    fn rebuild_ext_ptrs(&mut self) {
        self.ext_ptrs = vec![
            &mut *self.ext_co as *mut _ as *mut mfxExtBuffer,
            &mut *self.ext_co2 as *mut _ as *mut mfxExtBuffer,
            &mut *self.ext_co3 as *mut _ as *mut mfxExtBuffer,
        ];
    }

    fn attach_ext(&mut self, par: &mut mfxVideoParam) {
        par.ExtParam = self.ext_ptrs.as_mut_ptr();
        par.NumExtParam = self.ext_ptrs.len() as u16;
    }

    #[allow(dead_code)]
    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        let kbps = (bps / 1000).clamp(1, 65000) as u16;
        let fps = self.config.fps.max(1);
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;

        // The rate-control fields are the *only* thing that may differ between Init and Reset.
        // Everything else, ext buffers included, is re-sent exactly as Init received it —
        // omitting them at Reset is what silently reverted the whole low-latency posture on the
        // first congestion event.
        let mut par = self.enc_param;
        par.info.mfx.TargetKbps = kbps;
        par.info.mfx.MaxKbps = kbps;
        par.info.mfx.BufferSizeInKB = vbv_kb(bps, fps);
        if self.ext_co3.LowDelayBRC == MFX_CODINGOPTION_ON {
            let avg_frame_bytes = (bps / 8 / fps).max(1);
            self.ext_co3.WinBRCMaxAvgKbps = kbps;
            self.ext_co3.MaxFrameSizeP = avg_frame_bytes.saturating_mul(2);
            self.ext_co3.MaxFrameSizeI = avg_frame_bytes.saturating_mul(6);
        }
        self.attach_ext(&mut par);

        check(
            unsafe { (self.vpl.MFXVideoENCODE_Reset)(self.session, &mut par) },
            "MFXVideoENCODE_Reset",
        )?;
        self.enc_param = par;
        tprintln!(
            "Intel Quick Sync bitrate reconfigured (bitrate_bps={bps}, buffer_kb={}, \
             ext_buffers={} retained)",
            unsafe { par.info.mfx.BufferSizeInKB },
            par.NumExtParam,
        );
        Ok(BitrateApplied::Applied)
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
        let box_ = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: self.src_w,
            bottom: self.src_h,
            back: 1,
        };

        match &self.bridge {
            None => {
                let src_res: ID3D11Resource =
                    src.cast().context("source texture as ID3D11Resource")?;
                unsafe {
                    self.context.CopySubresourceRegion(
                        &dst_res,
                        0,
                        0,
                        0,
                        0,
                        &src_res,
                        0,
                        Some(&box_),
                    );
                    self.context.Flush();
                }
            }
            Some(bridge) => {
                unsafe { bridge.upload(src, self.src_w, self.src_h)? };
                unsafe { bridge.enc_mutex.AcquireSync(KEY_ENCODER, KEY_TIMEOUT_MS) }
                    .context("Quick Sync bridge AcquireSync(encoder)")?;
                let copy = (|| -> Result<()> {
                    let src_res: ID3D11Resource = bridge
                        .enc_tex
                        .cast()
                        .context("bridge texture as ID3D11Resource")?;
                    unsafe {
                        self.context.CopySubresourceRegion(
                            &dst_res,
                            0,
                            0,
                            0,
                            0,
                            &src_res,
                            0,
                            Some(&box_),
                        );
                        self.context.Flush();
                    }
                    Ok(())
                })();
                let _ = unsafe { bridge.enc_mutex.ReleaseSync(KEY_WRITER) };
                copy?;
            }
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
            let st = unsafe { (self.vpl.MFXVideoCORE_SyncOperation)(self.session, syncp, 100) };
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
            SurfaceKind::VppIn => (
                self.vpl.MFXMemory_GetSurfaceForVPP,
                "MFXMemory_GetSurfaceForVPP",
            ),
            SurfaceKind::VppOut => (
                self.vpl.MFXMemory_GetSurfaceForVPPOut,
                "MFXMemory_GetSurfaceForVPPOut",
            ),
            SurfaceKind::Encode => (
                self.vpl.MFXMemory_GetSurfaceForEncode,
                "MFXMemory_GetSurfaceForEncode",
            ),
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
            if !self.loader.is_null() {
                (self.vpl.MFXUnload)(self.loader);
            }
        }
    }
}

/// Serializes oneVPL implementation enumeration across the process.
///
/// `MFXLoad` + `MFXSetConfigFilterProperty` + `MFXCreateSession` is a dispatcher-wide
/// enumeration: it opens the candidate runtimes and asks each one whether it can serve the
/// filter. Run two of them at once against the same iGPU and one loses — measured on this box
/// as `MFXCreateSession` returning `MFX_ERR_NOT_FOUND` in roughly half of concurrent pairs,
/// which the old code reported as "no Intel hardware oneVPL implementation". An encoder is
/// built per streaming session, so two clients joining together on an Intel host is exactly the
/// concurrent case, and the loser did not retry — it fell to x264 for the life of that session.
static SESSION_CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How many times to re-enumerate before believing `MFX_ERR_NOT_FOUND`.
///
/// The lock removes the contention this process creates; the retry covers the rest — another
/// process using the same iGPU, or a runtime still tearing down the previous session's device.
const SESSION_CREATE_ATTEMPTS: u32 = 4;
const SESSION_CREATE_BACKOFF_MS: u64 = 25;

fn create_session(vpl: &Vpl) -> Result<(mfxSession, mfxLoader)> {
    // A poisoned lock here means another thread panicked mid-enumeration. The data is `()`, so
    // there is nothing to be inconsistent about, and refusing to encode over it would be worse.
    let _serialize = SESSION_CREATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..SESSION_CREATE_ATTEMPTS {
        match create_session_once(vpl) {
            Ok(v) => {
                if attempt > 0 {
                    tprintln!(
                        "oneVPL: session created on attempt {} of {SESSION_CREATE_ATTEMPTS}",
                        attempt + 1
                    );
                }
                return Ok(v);
            }
            Err(e) if is_transient_session_error(&e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(
                    SESSION_CREATE_BACKOFF_MS * u64::from(attempt + 1),
                ));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last
        .expect("the retry loop only exits empty-handed after recording an error")
        .context(format!(
            "no Intel hardware oneVPL implementation after {SESSION_CREATE_ATTEMPTS} attempts \
             (would fail over to next vendor)"
        )))
}

/// `MFX_ERR_NOT_FOUND` and `MFX_ERR_DEVICE_*` from enumeration are "not right now", not
/// "not on this machine". Anything else is a real refusal and retrying only delays the failover.
fn is_transient_session_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("MFX_ERR_NOT_FOUND")
        || s.contains("MFX_ERR_DEVICE_LOST")
        || s.contains("MFX_ERR_DEVICE_FAILED")
}

fn create_session_once(vpl: &Vpl) -> Result<(mfxSession, mfxLoader)> {
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
            let st = unsafe { (vpl.MFXQueryVersion)(session, &mut ver) };
            let (major, minor) = (ver >> 16, ver & 0xFFFF);
            tprintln!(
                "oneVPL session created: HW impl via D3D11, api_version={major}.{minor}, \
                 dispatcher={}",
                vpl.library_path().display(),
            );
            // The encoder depends on 2.x-only internal allocation
            // (`MFXMemory_GetSurfaceForVPP`/`Encode`, `mfxFrameSurfaceInterface::GetNativeHandle`).
            // A 1.x runtime returns MFX_ERR_UNSUPPORTED from those on the *first frame*, long
            // after `build_backend` committed to this vendor and logged "live capture ready" —
            // so the check has to happen here, where failing over is still possible.
            if st < MFX_ERR_NONE {
                unsafe { (vpl.MFXClose)(session) };
                unsafe { (vpl.MFXUnload)(loader) };
                bail!("MFXQueryVersion failed (mfxStatus={st}); cannot confirm a 2.x runtime");
            }
            if major < 2 {
                unsafe { (vpl.MFXClose)(session) };
                unsafe { (vpl.MFXUnload)(loader) };
                bail!(
                    "oneVPL runtime reports API {major}.{minor}; this encoder needs 2.0+ for \
                     internal surface allocation, and a 1.x runtime would fail at the first frame"
                );
            }
            Ok((session, loader))
        }
        Err(e) => {
            unsafe { (vpl.MFXUnload)(loader) };
            // No blanket "no Intel hardware" here — `create_session` decides that, after the
            // retries, and only for statuses that mean it.
            Err(e.context(format!(
                "oneVPL implementation enumeration failed (dispatcher {})",
                vpl.library_path().display()
            )))
        }
    }
}

pub(crate) fn create_intel_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    create_intel_d3d11_device_inner(None)
}

fn create_intel_d3d11_device_for(
    capture: &ID3D11Device,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    create_intel_d3d11_device_inner(adapter_luid(capture))
}

fn create_intel_d3d11_device_inner(
    prefer_luid: Option<LUID>,
) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;
        let mut chosen: Option<IDXGIAdapter1> = None;
        let mut by_name: Option<(u32, String, IDXGIAdapter1)> = None;
        let mut i = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            let idx = i;
            i += 1;
            let desc = adapter.GetDesc1().context("IDXGIAdapter1::GetDesc1")?;
            let is_software =
                (DXGI_ADAPTER_FLAG(desc.Flags as i32).0 & DXGI_ADAPTER_FLAG_SOFTWARE.0) != 0;
            if is_software {
                continue;
            }
            let name = adapter_name(&desc);
            if let Some(want) = prefer_luid {
                if luid_eq(desc.AdapterLuid, want) {
                    tprintln!(
                        "selected Intel adapter for Quick Sync by LUID match (index={idx}, name={name})"
                    );
                    chosen = Some(adapter);
                    break;
                }
            }
            if by_name.is_none() && name.to_uppercase().contains("INTEL") {
                if prefer_luid.is_none() {
                    tprintln!("selected Intel adapter for Quick Sync (index={idx}, name={name})");
                    chosen = Some(adapter);
                    break;
                }
                by_name = Some((idx, name, adapter));
            }
        }
        let adapter = match chosen {
            Some(a) => a,
            None => {
                let (idx, name, a) = by_name.ok_or_else(|| {
                    anyhow!("no Intel adapter found; Quick Sync requires an Intel GPU")
                })?;
                tprintln!("selected Intel adapter for Quick Sync (index={idx}, name={name})");
                a
            }
        };

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

fn adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
    let end = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..end])
}

fn adapter_luid(device: &ID3D11Device) -> Option<LUID> {
    let dxgi: IDXGIDevice = device.cast().ok()?;
    let adapter = unsafe { dxgi.GetAdapter() }.ok()?;
    let desc = unsafe { adapter.GetDesc() }.ok()?;
    Some(desc.AdapterLuid)
}

#[inline]
fn luid_eq(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

/// The coded-picture buffer the rate controller may bank into: about one frame of bits.
///
/// This used to be `(coded_w * coded_h * 4) / 1024` — the size of one *uncompressed* BGRA
/// frame, 8160 KB at 1920x1088, which is ~65 Mbit, i.e. about 6.5 seconds of buffer at a
/// 10 Mbps target. The encoder was free to bank several seconds of bits and smooth excursions
/// over a long window instead of clipping them per frame, which on a fixed-rate link becomes
/// queueing delay and then loss. The output *allocation* still needs to be large; that is a
/// separate number now.
fn vbv_kb(bitrate_bps: u32, fps: u32) -> u16 {
    ((bitrate_bps / 8 / fps.max(1)) / 1024).clamp(1, 65000) as u16
}

fn query_fps_supported(vpl: &Vpl, session: mfxSession, config: &EncoderConfig, fps: u32) -> bool {
    let fps = fps.max(1);
    let coded_w = align_up_u16(config.width, 16);
    let coded_h = align_up_u16(config.height, 16);
    let buffer_size_kb = vbv_kb(config.bitrate_bps, fps);
    let target_kbps = (config.bitrate_bps / 1000).clamp(1, 65000) as u16;
    let max_kbps = (config.max_bitrate_bps / 1000).clamp(target_kbps as u32, 65000) as u16;

    let mut mfx: mfxInfoMFX = unsafe { std::mem::zeroed() };
    mfx.FrameInfo = mfxFrameInfo {
        FourCC: MFX_FOURCC_NV12,
        ChromaFormat: MFX_CHROMAFORMAT_YUV420,
        BitDepthLuma: 8,
        BitDepthChroma: 8,
        PicStruct: MFX_PICSTRUCT_PROGRESSIVE,
        FrameRateExtN: fps,
        FrameRateExtD: 1,
        Width: coded_w,
        Height: coded_h,
        CropW: config.width as u16,
        CropH: config.height as u16,
        ..Default::default()
    };
    mfx.CodecId = MFX_CODEC_AVC;
    mfx.CodecProfile = match config.profile {
        H264Profile::Baseline => MFX_PROFILE_AVC_CONSTRAINED_BASELINE,
        H264Profile::Main => MFX_PROFILE_AVC_MAIN,
        H264Profile::High => MFX_PROFILE_AVC_HIGH,
    };
    mfx.LowPower = MFX_CODINGOPTION_ON;
    mfx.TargetUsage = MFX_TARGETUSAGE_BEST_SPEED;
    mfx.GopPicSize = 0xFFFF;
    mfx.GopRefDist = 1;
    mfx.NumSlice = (config.height / 256).clamp(4, 8) as u16;
    mfx.NumRefFrame = 1;
    mfx.BufferSizeInKB = buffer_size_kb;
    if let Some(qp) = config.qp {
        let q = qp as u16;
        mfx.RateControlMethod = MFX_RATECONTROL_CQP;
        mfx.InitialDelayInKB = q;
        mfx.TargetKbps = q;
        mfx.MaxKbps = q;
    } else {
        mfx.RateControlMethod = MFX_RATECONTROL_VCM;
        mfx.TargetKbps = target_kbps;
        mfx.MaxKbps = max_kbps;
    }
    mfx.IdrInterval = 0;

    let mut par: mfxVideoParam = unsafe { std::mem::zeroed() };
    par.AsyncDepth = 1;
    par.IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY;
    par.info = mfxInfoUnion { mfx };

    let p: *mut mfxVideoParam = &mut par;
    let status = unsafe { (vpl.MFXVideoENCODE_Query)(session, p, p) };
    let out_fps = unsafe { par.info.mfx.FrameInfo.FrameRateExtN };
    status >= MFX_ERR_NONE && out_fps == fps
}

fn query_max_supported_fps(
    vpl: &Vpl,
    session: mfxSession,
    config: &EncoderConfig,
    requested: u32,
) -> u32 {
    let requested = requested.max(1);
    if query_fps_supported(vpl, session, config, requested) {
        return requested;
    }
    const PROBE_LOW: u32 = 30;
    if !query_fps_supported(vpl, session, config, PROBE_LOW) {
        // Not "this device tops out at 60" — the probe itself failed, for a reason we have not
        // established, and the cap is a guess. It used to be applied silently, so a host whose
        // Query fails for an unrelated reason quietly lost every rate above 60.
        teprintln!(
            "Intel Quick Sync: the {PROBE_LOW} fps probe failed, so the supported frame-rate \
             range is unknown; capping at {INTEL_FPS_QUERY_FALLBACK} fps as a guess"
        );
        return requested.min(INTEL_FPS_QUERY_FALLBACK);
    }
    let (mut lo, mut hi, mut best) = (PROBE_LOW, requested, PROBE_LOW);
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        if query_fps_supported(vpl, session, config, mid) {
            best = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    best
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
        min_qp: config.quality_qp,
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
            tprintln!(
                "Intel encoded frame={i} (au_bytes={}, total_bytes={total})",
                au.len()
            );
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
