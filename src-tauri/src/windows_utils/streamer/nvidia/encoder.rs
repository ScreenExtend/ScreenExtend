use std::ffi::c_void;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::ptr;

use anyhow::{Context as _, Result, anyhow, bail};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter1,
    IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use crate::streamer::config::{Config, H264Profile};
use super::nvenc_sys::*;

pub const KEY_WRITER: u64 = 0;
pub const KEY_ENCODER: u64 = 1;
pub const KEY_TIMEOUT_MS: u32 = 1000;
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1;

type CreateInstanceFn = unsafe extern "C" fn(*mut NV_ENCODE_API_FUNCTION_LIST) -> NVENCSTATUS;

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub max_bitrate_bps: u32,
    pub profile: H264Profile,
    pub qp: Option<u8>,
    pub intra_refresh: bool,
}

pub struct Encoder {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    fns: NV_ENCODE_API_FUNCTION_LIST,
    encoder: *mut c_void,
    input_texture: ID3D11Texture2D,
    registered_input: NV_ENC_REGISTERED_PTR,
    input_keyed_mutex: Option<IDXGIKeyedMutex>,
    shared_handle: Option<HANDLE>,
    bitstream: NV_ENC_OUTPUT_PTR,
    init_params: NV_ENC_INITIALIZE_PARAMS,
    encode_config: NV_ENC_CONFIG,
    config: EncoderConfig,
    frame_index: u64,
    idr_requested: bool,
    _lib: libloading::Library,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        Self::new_inner(config, false)
    }

    pub fn new_shared(config: EncoderConfig) -> Result<Self> {
        Self::new_inner(config, true)
    }

    fn new_inner(config: EncoderConfig, shared: bool) -> Result<Self> {
        let (device, context) = create_nvidia_d3d11_device()?;

        let lib = unsafe { libloading::Library::new("nvEncodeAPI64.dll") }
            .context("loading nvEncodeAPI64.dll (NVENC driver component)")?;

        let mut fns: NV_ENCODE_API_FUNCTION_LIST = unsafe { zeroed() };
        fns.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        unsafe {
            let create: libloading::Symbol<CreateInstanceFn> = lib
                .get(b"NvEncodeAPICreateInstance\0")
                .context("resolving NvEncodeAPICreateInstance")?;
            check(create(&mut fns), "NvEncodeAPICreateInstance", ptr::null_mut())?;
        }
        if fns.nvEncOpenEncodeSessionEx.is_none() {
            bail!("NVENC function list did not populate (driver too old?)");
        }

        let encoder = unsafe { open_session(&fns, device.as_raw())? };
        let input_texture = create_input_texture(&device, config.width, config.height, shared)?;

        let mut this = Self {
            device,
            context,
            fns,
            encoder,
            input_texture,
            registered_input: ptr::null_mut(),
            input_keyed_mutex: None,
            shared_handle: None,
            bitstream: ptr::null_mut(),
            init_params: unsafe { zeroed() },
            encode_config: unsafe { zeroed() },
            config,
            frame_index: 0,
            idr_requested: false,
            _lib: lib,
        };

        unsafe { this.initialize()? };
        unsafe {
            this.register_input()?;
            this.create_bitstream()?;
        }
        if shared {
            this.setup_shared()?;
        }

        println!(
            "NVENC H.264 encoder initialized (D3D11 / ULL): {}x{}@{}, bitrate_bps={}, zero_copy={}, profile={:?}, rc={}, qp={:?}, intra_refresh={}",
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            shared,
            config.profile,
            if config.qp.is_some() { "constqp" } else { "cbr" },
            config.qp,
            config.intra_refresh,
        );
        Ok(this)
    }

    fn setup_shared(&mut self) -> Result<()> {
        let resource: IDXGIResource1 = self
            .input_texture
            .cast()
            .context("input texture as IDXGIResource1 (is it SHARED?)")?;
        let handle = unsafe {
            resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_RW, PCWSTR::null())
        }
        .context("IDXGIResource1::CreateSharedHandle")?;
        let mutex: IDXGIKeyedMutex = self
            .input_texture
            .cast()
            .context("input texture as IDXGIKeyedMutex")?;
        self.shared_handle = Some(handle);
        self.input_keyed_mutex = Some(mutex);
        Ok(())
    }

    pub fn shared_handle(&self) -> Option<HANDLE> {
        self.shared_handle
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn encode_bgra(&mut self, bgra: &[u8], force_idr: bool) -> Result<Vec<u8>> {
        let expected = (self.config.width as usize) * (self.config.height as usize) * 4;
        if bgra.len() != expected {
            bail!(
                "encode_bgra: expected {expected} bytes ({}x{} BGRA), got {}",
                self.config.width,
                self.config.height,
                bgra.len()
            );
        }

        unsafe {
            self.context.UpdateSubresource(
                &self.input_texture,
                0,
                None,
                bgra.as_ptr() as *const c_void,
                self.config.width * 4,
                0,
            );
        }

        let force_idr = force_idr || self.idr_requested;
        let data = unsafe { self.encode_mapped(force_idr)? };
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(data)
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
        unsafe {
            self.context.UpdateSubresource(
                &self.input_texture,
                0,
                None,
                data.as_ptr() as *const c_void,
                row_pitch,
                0,
            );
        }
        let force_idr = force_idr || self.idr_requested;
        let out = unsafe { self.encode_mapped(force_idr)? };
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_input(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let Some(mutex) = self.input_keyed_mutex.clone() else {
            bail!("encode_input called on a non-shared (CPU-upload) encoder");
        };
        unsafe { mutex.AcquireSync(KEY_ENCODER, KEY_TIMEOUT_MS) }
            .context("keyed mutex AcquireSync(encoder)")?;
        let force_idr = force_idr || self.idr_requested;
        let res = unsafe { self.encode_mapped(force_idr) };
        let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
        let out = res?;
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let force_idr = force_idr || self.idr_requested;
        let out = if let Some(mutex) = self.input_keyed_mutex.clone() {
            unsafe { mutex.AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS) }
                .context("repeat keyed mutex AcquireSync(writer)")?;
            let res = unsafe { self.encode_mapped(force_idr) };
            let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
            res?
        } else {
            unsafe { self.encode_mapped(force_idr)? }
        };
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if self.config.qp.is_some() {
            return Ok(());
        }
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;

        let mut reconfig: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconfig.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        let mut new_config = self.encode_config;
        new_config.rcParams.averageBitRate = bps;
        new_config.rcParams.maxBitRate = bps;
        new_config.rcParams.vbvBufferSize = bps / self.config.fps.max(1);
        new_config.rcParams.vbvInitialDelay = new_config.rcParams.vbvBufferSize;

        reconfig.reInitEncodeParams = self.init_params;
        reconfig.reInitEncodeParams.encodeConfig = &mut new_config;

        unsafe {
            let f = self
                .fns
                .nvEncReconfigureEncoder
                .expect("nvEncReconfigureEncoder present");
            self.check(f(self.encoder, &mut reconfig), "nvEncReconfigureEncoder")?;
        }

        self.encode_config = new_config;
        self.init_params = reconfig.reInitEncodeParams;
        self.init_params.encodeConfig = &mut self.encode_config;
        println!("NVENC bitrate reconfigured (bitrate_bps={bps})");
        Ok(())
    }

    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    pub fn sequence_params(&mut self) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; NV_MAX_SEQ_HDR_LEN as usize];
        let mut out_size: u32 = 0;
        let mut payload: NV_ENC_SEQUENCE_PARAM_PAYLOAD = unsafe { zeroed() };
        payload.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
        payload.inBufferSize = NV_MAX_SEQ_HDR_LEN;
        payload.spsppsBuffer = buf.as_mut_ptr() as *mut c_void;
        payload.outSPSPPSPayloadSize = &mut out_size;

        unsafe {
            let f = self
                .fns
                .nvEncGetSequenceParams
                .expect("nvEncGetSequenceParams present");
            self.check(f(self.encoder, &mut payload), "nvEncGetSequenceParams")?;
        }
        buf.truncate(out_size as usize);
        Ok(buf)
    }

    fn check(&self, status: NVENCSTATUS, what: &str) -> Result<()> {
        if status == NVENCSTATUS::NV_ENC_SUCCESS {
            return Ok(());
        }
        let detail = self
            .fns
            .nvEncGetLastErrorString
            .map(|f| {
                let p = unsafe { f(self.encoder) };
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { std::ffi::CStr::from_ptr(p) }
                        .to_string_lossy()
                        .into_owned()
                }
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        bail!("{what} failed: {status:?}{detail}");
    }

    unsafe fn initialize(&mut self) -> Result<()> {
        let codec = NV_ENC_CODEC_H264_GUID;
        let preset = NV_ENC_PRESET_P1_GUID;
        let tuning = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
        let mut preset_config: NV_ENC_PRESET_CONFIG = unsafe { zeroed() };
        preset_config.version = NV_ENC_PRESET_CONFIG_VER;
        preset_config.presetCfg.version = NV_ENC_CONFIG_VER;
        unsafe {
            let f = self
                .fns
                .nvEncGetEncodePresetConfigEx
                .expect("nvEncGetEncodePresetConfigEx present");
            self.check(
                f(self.encoder, codec, preset, tuning, &mut preset_config),
                "nvEncGetEncodePresetConfigEx",
            )?;
        }

        let mut config: NV_ENC_CONFIG = preset_config.presetCfg;
        config.version = NV_ENC_CONFIG_VER;
        config.profileGUID = match self.config.profile {
            H264Profile::Baseline => NV_ENC_H264_PROFILE_BASELINE_GUID,
            H264Profile::Main => NV_ENC_H264_PROFILE_MAIN_GUID,
            H264Profile::High => NV_ENC_H264_PROFILE_HIGH_GUID,
        };
        config.gopLength = NVENC_INFINITE_GOPLENGTH;
        config.frameIntervalP = 1;

        let fps = self.config.fps.max(1);
        config.rcParams.version = NV_ENC_RC_PARAMS_VER;
        if let Some(q) = self.config.qp {
            let q = q as u32;
            config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
            config.rcParams.constQP = NV_ENC_QP { qpInterP: q, qpInterB: q, qpIntra: q };
        } else {
            config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            config.rcParams.averageBitRate = self.config.bitrate_bps;
            config.rcParams.maxBitRate = self.config.max_bitrate_bps;
            config.rcParams.vbvBufferSize = self.config.bitrate_bps / fps;
            config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
        }
        config.rcParams.set_enableLookahead(0);
        config.rcParams.lookaheadDepth = 0;
        config.rcParams.set_enableAQ(0);
        config.rcParams.set_enableTemporalAQ(0);
        config.rcParams.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;

        let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
        h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        if self.config.intra_refresh {
            let refresh_period = (fps / 2).max(1);
            h264.set_enableIntraRefresh(1);
            h264.intraRefreshPeriod = refresh_period;
            h264.intraRefreshCnt = (fps / 4).max(1);
        }
        let slice_count = (self.config.height / 256).clamp(4, 8);
        h264.sliceMode = 3;
        h264.sliceModeData = slice_count;
        h264.maxNumRefFrames = 1;
        h264.set_repeatSPSPPS(1);

        match self.config.profile {
            H264Profile::Baseline => {
                h264.set_enableConstrainedEncoding(1);
                h264.entropyCodingMode =
                    NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
                h264.adaptiveTransformMode =
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE;
            }
            H264Profile::Main => {
                h264.entropyCodingMode =
                    NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
                h264.adaptiveTransformMode =
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE;
            }
            H264Profile::High => {
                h264.entropyCodingMode =
                    NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
                h264.adaptiveTransformMode =
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_ENABLE;
            }
        }

        let vui = &mut h264.h264VUIParameters;
        vui.videoSignalTypePresentFlag = 1;
        vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT::NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
        vui.videoFullRangeFlag = 1;
        vui.colourDescriptionPresentFlag = 1;
        vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES::NV_ENC_VUI_COLOR_PRIMARIES_BT709;
        vui.transferCharacteristics =
            NV_ENC_VUI_TRANSFER_CHARACTERISTIC::NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
        vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS::NV_ENC_VUI_MATRIX_COEFFS_BT709;

        let mut init: NV_ENC_INITIALIZE_PARAMS = unsafe { zeroed() };
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encodeGUID = codec;
        init.presetGUID = preset;
        init.encodeWidth = self.config.width;
        init.encodeHeight = self.config.height;
        init.darWidth = self.config.width;
        init.darHeight = self.config.height;
        init.frameRateNum = fps;
        init.frameRateDen = 1;
        init.enablePTD = 1;
        init.enableEncodeAsync = 0;
        init.tuningInfo = tuning;
        init.maxEncodeWidth = self.config.width;
        init.maxEncodeHeight = self.config.height;
        init.encodeConfig = &mut config;

        unsafe {
            let f = self
                .fns
                .nvEncInitializeEncoder
                .expect("nvEncInitializeEncoder present");
            self.check(f(self.encoder, &mut init), "nvEncInitializeEncoder")?;
        }

        self.encode_config = config;
        self.init_params = init;
        self.init_params.encodeConfig = &mut self.encode_config;
        Ok(())
    }

    unsafe fn register_input(&mut self) -> Result<()> {
        let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { zeroed() };
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.config.width;
        reg.height = self.config.height;
        reg.pitch = 0;
        reg.resourceToRegister = self.input_texture.as_raw();
        reg.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        reg.bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        unsafe {
            let f = self
                .fns
                .nvEncRegisterResource
                .expect("nvEncRegisterResource present");
            self.check(f(self.encoder, &mut reg), "nvEncRegisterResource")?;
        }
        self.registered_input = reg.registeredResource;
        Ok(())
    }

    unsafe fn create_bitstream(&mut self) -> Result<()> {
        let mut create: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { zeroed() };
        create.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        unsafe {
            let f = self
                .fns
                .nvEncCreateBitstreamBuffer
                .expect("nvEncCreateBitstreamBuffer present");
            self.check(f(self.encoder, &mut create), "nvEncCreateBitstreamBuffer")?;
        }
        self.bitstream = create.bitstreamBuffer;
        Ok(())
    }

    unsafe fn encode_mapped(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { zeroed() };
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = self.registered_input;
        unsafe {
            let f = self
                .fns
                .nvEncMapInputResource
                .expect("nvEncMapInputResource present");
            self.check(f(self.encoder, &mut map), "nvEncMapInputResource")?;
        }
        let mapped = map.mappedResource;
        let result = unsafe { self.encode_locked(mapped, force_idr) };

        unsafe {
            let f = self
                .fns
                .nvEncUnmapInputResource
                .expect("nvEncUnmapInputResource present");
            let st = f(self.encoder, mapped);
            if result.is_ok() {
                self.check(st, "nvEncUnmapInputResource")?;
            }
        }
        result
    }

    unsafe fn encode_locked(&mut self, mapped: NV_ENC_INPUT_PTR, force_idr: bool) -> Result<Vec<u8>> {
        let mut pic: NV_ENC_PIC_PARAMS = unsafe { zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.config.width;
        pic.inputHeight = self.config.height;
        pic.inputPitch = self.config.width * 4;
        pic.inputBuffer = mapped;
        pic.outputBitstream = self.bitstream;
        pic.bufferFmt = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        pic.inputTimeStamp = self.frame_index;
        pic.frameIdx = self.frame_index as u32;
        if force_idr {
            pic.encodePicFlags = (NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32)
                | (NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32);
        }

        unsafe {
            let f = self
                .fns
                .nvEncEncodePicture
                .expect("nvEncEncodePicture present");
            self.check(f(self.encoder, &mut pic), "nvEncEncodePicture")?;
        }

        let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { zeroed() };
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = self.bitstream;
        unsafe {
            let f = self
                .fns
                .nvEncLockBitstream
                .expect("nvEncLockBitstream present");
            self.check(f(self.encoder, &mut lock), "nvEncLockBitstream")?;
        }

        let bytes = unsafe {
            std::slice::from_raw_parts(
                lock.bitstreamBufferPtr as *const u8,
                lock.bitstreamSizeInBytes as usize,
            )
        }
        .to_vec();

        unsafe {
            let f = self
                .fns
                .nvEncUnlockBitstream
                .expect("nvEncUnlockBitstream present");
            self.check(f(self.encoder, self.bitstream), "nvEncUnlockBitstream")?;
        }
        Ok(bytes)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(h) = self.shared_handle.take() {
                let _ = CloseHandle(h);
            }
            if !self.registered_input.is_null() {
                if let Some(f) = self.fns.nvEncUnregisterResource {
                    let st = f(self.encoder, self.registered_input);
                    log_drop_status(st, "nvEncUnregisterResource");
                }
                self.registered_input = ptr::null_mut();
            }
            if !self.bitstream.is_null() {
                if let Some(f) = self.fns.nvEncDestroyBitstreamBuffer {
                    let st = f(self.encoder, self.bitstream);
                    log_drop_status(st, "nvEncDestroyBitstreamBuffer");
                }
                self.bitstream = ptr::null_mut();
            }
            if !self.encoder.is_null() {
                if let Some(f) = self.fns.nvEncDestroyEncoder {
                    let st = f(self.encoder);
                    log_drop_status(st, "nvEncDestroyEncoder");
                }
                self.encoder = ptr::null_mut();
            }
        }
    }
}

fn create_nvidia_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().context("CreateDXGIFactory1")?;

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
            println!("DXGI adapter (index={i}, name={name}, is_software={is_software})");
            if !is_software && name.to_uppercase().contains("NVIDIA") {
                chosen = Some(adapter);
                println!("selected NVIDIA adapter for NVENC (index={i}, name={name})");
                break;
            }
            i += 1;
        }

        let adapter = chosen.ok_or_else(|| {
            anyhow!("no NVIDIA adapter found; NVENC requires NVIDIA GPU")
        })?;

        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice on NVIDIA adapter")?;

        let device = device.context("D3D11CreateDevice returned no device")?;
        let context = context.context("D3D11CreateDevice returned no context")?;
        Ok((device, context))
    }
}

fn create_input_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    shared: bool,
) -> Result<ID3D11Texture2D> {
    let misc = if shared {
        (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0) as u32
    } else {
        0
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: misc,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .context("CreateTexture2D (NVENC input)")?;
    }
    texture.context("CreateTexture2D returned no texture")
}

unsafe fn open_session(
    fns: &NV_ENCODE_API_FUNCTION_LIST,
    device: *mut c_void,
) -> Result<*mut c_void> {
    let mut params: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = unsafe { zeroed() };
    params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    params.deviceType = NV_ENC_DEVICE_TYPE::NV_ENC_DEVICE_TYPE_DIRECTX;
    params.device = device;
    params.apiVersion = NVENCAPI_VERSION;

    let mut encoder: *mut c_void = ptr::null_mut();
    unsafe {
        let f = fns
            .nvEncOpenEncodeSessionEx
            .expect("nvEncOpenEncodeSessionEx present");
        let st = f(&mut params, &mut encoder);
        if st != NVENCSTATUS::NV_ENC_SUCCESS {
            if let Some(d) = fns.nvEncDestroyEncoder {
                let _ = d(encoder);
            }
            return Err(anyhow!("nvEncOpenEncodeSessionEx failed: {st:?}"));
        }
    }
    Ok(encoder)
}

#[inline]
unsafe fn zeroed<T>() -> T {
    unsafe { MaybeUninit::<T>::zeroed().assume_init() }
}

fn check(status: NVENCSTATUS, what: &str, _encoder: *mut c_void) -> Result<()> {
    if status == NVENCSTATUS::NV_ENC_SUCCESS {
        return Ok(());
    }
    bail!("{what} failed: {status:?}");
}

fn log_drop_status(status: NVENCSTATUS, what: &str) {
    if status != NVENCSTATUS::NV_ENC_SUCCESS {
        eprintln!("{what} returned {status:?} during encoder drop");
    }
}

pub fn probe_encode(config: &Config, path: &str) -> Result<()> {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FPS: u32 = 60;
    const FRAMES: u32 = 300;
    const BITRATE: u32 = 10_000_000;

    println!("encoding synthetic pattern to Annex-B: path={path}, {WIDTH}x{HEIGHT}@{FPS}, {FRAMES} frames");

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

    let mut file = std::fs::File::create(path)
        .with_context(|| format!("creating output file {path}"))?;

    let mut frame = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    let mut total_bytes = 0usize;
    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, WIDTH, HEIGHT, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .with_context(|| format!("encoding frame {i}"))?;
        total_bytes += au.len();
        file.write_all(&au)
            .with_context(|| format!("writing frame {i} ({} bytes)", au.len()))?;

        if i % 60 == 0 || i == FRAMES - 1 {
            println!("encoded frame={i} (au_bytes={}, total_bytes={total_bytes})", au.len());
        }
    }
    file.flush().context("flushing output file")?;

    println!("wrote Annex-B H.264: path={path}, frames={FRAMES}, total_bytes={total_bytes}");
    Ok(())
}

fn fill_synthetic_bgra(buf: &mut [u8], width: u32, height: u32, frame: u32) {
    let w = width as usize;
    let h = height as usize;
    let f = frame as usize;
    let box_w = w / 6;
    let box_h = h / 6;
    let span_x = w.saturating_sub(box_w).max(1);
    let span_y = h.saturating_sub(box_h).max(1);
    let box_x = (f * 13) % span_x;
    let box_y = (f * 7) % span_y;

    for y in 0..h {
        let row = y * w * 4;
        for x in 0..w {
            let o = row + x * 4;
            let b = ((x + f * 3) & 0xff) as u8;
            let g = ((y + f * 5) & 0xff) as u8;
            let r = ((x + y + f * 2) & 0xff) as u8;
            let in_box = x >= box_x && x < box_x + box_w && y >= box_y && y < box_y + box_h;
            if in_box {
                buf[o] = 0;
                buf[o + 1] = 255;
                buf[o + 2] = 255;
            } else {
                buf[o] = b;
                buf[o + 1] = g;
                buf[o + 2] = r;
            }
            buf[o + 3] = 255;
        }
    }
}
