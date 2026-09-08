use std::ffi::c_void;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use super::nvenc_sys::*;
use crate::streamer::config::{Config, H264Profile};
use crate::streamer::platform::BitrateApplied;

pub const KEY_WRITER: u64 = 0;
pub const KEY_ENCODER: u64 = 1;
pub const KEY_TIMEOUT_MS: u32 = 1000;

/// How long to wait on a per-picture completion event before declaring the encoder wedged.
/// NVENC signals within one frame period in normal operation; this is a hang detector, not a
/// scheduling parameter.
const ENCODE_EVENT_TIMEOUT_MS: u32 = 2000;

/// Output buffers (and, in async mode, completion events) allocated per encoder. With one, a
/// submitted picture must be drained before the next can be handed to the hardware; two is the
/// minimum that lets submit(N+1) overlap the hardware's work on N.
const OUTPUT_BUFFERS: usize = 4;

/// The most long-term reference slots to claim, however many the engine advertises.
///
/// Each slot is a picture held in the DPB for as long as the slot lives, so the cost is VRAM and
/// a slightly wider reference search. Two is enough for the repair pattern LTR exists for here —
/// one the client is known to hold and one being established — and more only widens the window
/// in which the encoder may predict from something stale.
const MAX_LTR_SLOTS: u32 = 2;

pub enum SlotEncodeError {
    Poisoned(anyhow::Error),
    Encode(anyhow::Error),
}

impl std::fmt::Debug for SlotEncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SlotEncodeError::Poisoned(e) => write!(f, "Poisoned: {e:?}"),
            SlotEncodeError::Encode(e) => write!(f, "Encode: {e:?}"),
        }
    }
}
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
    /// Explicit constant-QP rate control. Disables bitrate adaptation entirely.
    pub qp: Option<u8>,
    /// Quality ceiling under CBR: the encoder may not spend bits to go below this QP.
    pub min_qp: Option<u8>,
    pub intra_refresh: bool,
}

struct Ring {
    input_textures: Vec<ID3D11Texture2D>,
    registered_inputs: Vec<NV_ENC_REGISTERED_PTR>,
    keyed_mutexes: Vec<IDXGIKeyedMutex>,
    shared_handles: Vec<HANDLE>,
}

/// What this NVENC engine says it can do, read from the driver with `nvEncGetEncodeCaps`
/// once per session between `open_session` and `initialize`.
///
/// Every optional field in `initialize()` is gated on one of these. The alternative — asking
/// for a feature and letting `nvEncInitializeEncoder` fail — costs the user the whole NVIDIA
/// path over one unsupported flag, because `build_backend` cannot tell "this GPU lacks intra
/// refresh" from "NVENC is unavailable" and falls through to Quick Sync or x264.
#[derive(Debug, Clone, Copy, Default)]
pub struct EncoderCaps {
    pub width_min: u32,
    pub width_max: u32,
    pub height_min: u32,
    pub height_max: u32,
    /// Bitmask of `NV_ENC_PARAMS_RC_MODE` values.
    pub rc_modes: u32,
    pub cabac: bool,
    pub adaptive_transform: bool,
    pub constrained_encoding: bool,
    pub intra_refresh: bool,
    pub single_slice_intra_refresh: bool,
    pub custom_vbv_buf_size: bool,
    pub dyn_bitrate_change: bool,
    pub dyn_rcmode_change: bool,
    pub dynamic_slice_mode: bool,
    pub subframe_readback: bool,
    pub async_encode: bool,
    pub ref_pic_invalidation: bool,
    pub multiple_ref_frames: bool,
    pub max_ltr_frames: u32,
    pub num_encoder_engines: u32,
    pub level_max: u32,
}

impl EncoderCaps {
    fn query(fns: &NV_ENCODE_API_FUNCTION_LIST, encoder: *mut c_void) -> Self {
        let get = |cap: NV_ENC_CAPS| -> i32 {
            let Some(f) = fns.nvEncGetEncodeCaps else {
                return 0;
            };
            let mut param: NV_ENC_CAPS_PARAM = unsafe { zeroed() };
            param.version = NV_ENC_CAPS_PARAM_VER;
            param.capsToQuery = cap;
            let mut value: i32 = 0;
            let st = unsafe { f(encoder, NV_ENC_CODEC_H264_GUID, &mut param, &mut value) };
            if st == NVENCSTATUS::NV_ENC_SUCCESS {
                value
            } else {
                0
            }
        };
        use NV_ENC_CAPS::*;
        Self {
            width_min: get(NV_ENC_CAPS_WIDTH_MIN).max(0) as u32,
            width_max: get(NV_ENC_CAPS_WIDTH_MAX).max(0) as u32,
            height_min: get(NV_ENC_CAPS_HEIGHT_MIN).max(0) as u32,
            height_max: get(NV_ENC_CAPS_HEIGHT_MAX).max(0) as u32,
            rc_modes: get(NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES).max(0) as u32,
            cabac: get(NV_ENC_CAPS_SUPPORT_CABAC) != 0,
            adaptive_transform: get(NV_ENC_CAPS_SUPPORT_ADAPTIVE_TRANSFORM) != 0,
            constrained_encoding: get(NV_ENC_CAPS_SUPPORT_CONSTRAINED_ENCODING) != 0,
            intra_refresh: get(NV_ENC_CAPS_SUPPORT_INTRA_REFRESH) != 0,
            single_slice_intra_refresh: get(NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH) != 0,
            custom_vbv_buf_size: get(NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE) != 0,
            dyn_bitrate_change: get(NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE) != 0,
            dyn_rcmode_change: get(NV_ENC_CAPS_SUPPORT_DYN_RCMODE_CHANGE) != 0,
            dynamic_slice_mode: get(NV_ENC_CAPS_SUPPORT_DYNAMIC_SLICE_MODE) != 0,
            subframe_readback: get(NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK) != 0,
            async_encode: get(NV_ENC_CAPS_ASYNC_ENCODE_SUPPORT) != 0,
            ref_pic_invalidation: get(NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION) != 0,
            multiple_ref_frames: get(NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES) != 0,
            max_ltr_frames: get(NV_ENC_CAPS_NUM_MAX_LTR_FRAMES).max(0) as u32,
            num_encoder_engines: get(NV_ENC_CAPS_NUM_ENCODER_ENGINES).max(0) as u32,
            level_max: get(NV_ENC_CAPS_LEVEL_MAX).max(0) as u32,
        }
    }

    fn log(&self) {
        tprintln!(
            "NVENC caps: {}x{}..{}x{}, rc_modes=0x{:x}, level_max={}, engines={}, \
             cabac={}, adaptive_transform={}, constrained={}, intra_refresh={} \
             (single_slice={}), custom_vbv={}, dyn_bitrate={}, dyn_rcmode={}, \
             dynamic_slice={}, subframe_readback={}, async={}, ref_pic_invalidation={}, \
             multi_ref={}, max_ltr_frames={}",
            self.width_min,
            self.height_min,
            self.width_max,
            self.height_max,
            self.rc_modes,
            self.level_max,
            self.num_encoder_engines,
            self.cabac,
            self.adaptive_transform,
            self.constrained_encoding,
            self.intra_refresh,
            self.single_slice_intra_refresh,
            self.custom_vbv_buf_size,
            self.dyn_bitrate_change,
            self.dyn_rcmode_change,
            self.dynamic_slice_mode,
            self.subframe_readback,
            self.async_encode,
            self.ref_pic_invalidation,
            self.multiple_ref_frames,
            self.max_ltr_frames,
        );
    }

    fn supports_rc(&self, mode: NV_ENC_PARAMS_RC_MODE) -> bool {
        // rc_modes is a bitmask of the NV_ENC_PARAMS_RC_MODE values themselves, and a driver
        // that fails the query reports 0. Treat 0 as "unknown, do not veto".
        self.rc_modes == 0 || (self.rc_modes & mode as u32) != 0
    }

    /// The dimensions actually usable, clamped into the reported range. A zero max means the
    /// query failed, in which case the request stands.
    fn clamp_dims(&self, w: u32, h: u32) -> (u32, u32) {
        let w = if self.width_max > 0 {
            w.clamp(self.width_min.max(1), self.width_max)
        } else {
            w
        };
        let h = if self.height_max > 0 {
            h.clamp(self.height_min.max(1), self.height_max)
        } else {
            h
        };
        (w, h)
    }
}

/// One output bitstream buffer and, under async mode, the event NVENC signals when the picture
/// written into it is complete.
struct Output {
    bitstream: NV_ENC_OUTPUT_PTR,
    event: Option<HANDLE>,
}

/// A picture handed to the hardware and not yet drained.
struct InFlight {
    output: usize,
    mapped: NV_ENC_INPUT_PTR,
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
    ring: Option<Ring>,
    outputs: Vec<Output>,
    inflight: std::collections::VecDeque<InFlight>,
    next_output: usize,
    /// Reused across frames so a steady-state encode allocates nothing. Sized to the largest
    /// access unit seen so far, which after the first IDR is the whole range.
    au_scratch: Vec<u8>,
    async_mode: bool,
    caps: EncoderCaps,
    init_params: NV_ENC_INITIALIZE_PARAMS,
    encode_config: NV_ENC_CONFIG,
    config: EncoderConfig,
    frame_index: u64,
    idr_requested: bool,
    /// Shared with the pipeline so a `Pipeline::request_idr()` reaches the encoder even on the
    /// ring's idle keepalive path, where nothing else consults the pipeline's atomic.
    shared_idr: Option<Arc<AtomicBool>>,
    /// How many long-term reference slots the encoder was opened with. 0 when the engine does
    /// not advertise LTR, which is the signal that the repair API below is inert.
    ltr_slots: u32,
    /// The slot the next `mark_ltr` will claim, rotating over `ltr_slots`.
    next_ltr_slot: u32,
    /// Set by `mark_ltr`, consumed by the next submitted picture.
    pending_ltr_mark: Option<u32>,
    /// Set by `use_ltr`, consumed by the next submitted picture: a bitmap of the LTR slots the
    /// picture may reference, and nothing else.
    pending_ltr_use: Option<u32>,
    _lib: libloading::Library,
}

/// What became of a request to stop referencing a frame the client never received.
///
/// The caller needs to distinguish these, because "not supported here" means the only repair
/// left is a keyframe, and it should stop asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefInvalidation {
    /// The driver dropped the frame from the reference set. The next P frame will not use it.
    Applied,
    /// The engine does not advertise `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`, or the encoder
    /// was opened without long-term references, so there is nothing to invalidate against.
    Unsupported,
    /// The driver refused. Most often the frame has already left the DPB, which is not an error
    /// so much as "too late" — the repair has to be a keyframe.
    Refused,
}

unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        Self::new_inner(config, false)
    }

    pub fn new_shared(config: EncoderConfig) -> Result<Self> {
        Self::new_inner(config, true)
    }

    pub fn new_shared_ring(config: EncoderConfig, ring: usize) -> Result<Self> {
        assert!(ring >= 1, "ring size must be >= 1");
        let (device, context) = create_nvidia_d3d11_device()?;

        let lib = unsafe { libloading::Library::new("nvEncodeAPI64.dll") }
            .context("loading nvEncodeAPI64.dll (NVENC driver component)")?;

        let mut fns: NV_ENCODE_API_FUNCTION_LIST = unsafe { zeroed() };
        fns.version = NV_ENCODE_API_FUNCTION_LIST_VER;
        unsafe {
            let create: libloading::Symbol<CreateInstanceFn> = lib
                .get(b"NvEncodeAPICreateInstance\0")
                .context("resolving NvEncodeAPICreateInstance")?;
            check(
                create(&mut fns),
                "NvEncodeAPICreateInstance",
                ptr::null_mut(),
            )?;
        }
        if fns.nvEncOpenEncodeSessionEx.is_none() {
            bail!("NVENC function list did not populate (driver too old?)");
        }

        let encoder = unsafe { open_session(&fns, device.as_raw())? };
        let caps = EncoderCaps::query(&fns, encoder);
        caps.log();
        let config = clamp_config_to_caps(config, &caps);

        let mut input_textures: Vec<ID3D11Texture2D> = Vec::with_capacity(ring);
        for _ in 0..ring {
            input_textures.push(create_input_texture(
                &device,
                config.width,
                config.height,
                /* shared = */ true,
            )?);
        }
        let input_texture = input_textures[0].clone();

        let mut this = Self {
            device,
            context,
            fns,
            encoder,
            input_texture,
            registered_input: ptr::null_mut(),
            input_keyed_mutex: None,
            shared_handle: None,
            ring: None,
            outputs: Vec::new(),
            inflight: std::collections::VecDeque::new(),
            next_output: 0,
            au_scratch: Vec::new(),
            async_mode: false,
            caps,
            init_params: unsafe { zeroed() },
            encode_config: unsafe { zeroed() },
            config,
            frame_index: 0,
            idr_requested: false,
            shared_idr: None,
            ltr_slots: 0,
            next_ltr_slot: 0,
            pending_ltr_mark: None,
            pending_ltr_use: None,
            _lib: lib,
        };

        unsafe { this.initialize()? };
        unsafe { this.create_outputs()? };

        let mut registered_inputs: Vec<NV_ENC_REGISTERED_PTR> = Vec::with_capacity(ring);
        let mut keyed_mutexes: Vec<IDXGIKeyedMutex> = Vec::with_capacity(ring);
        let mut shared_handles: Vec<HANDLE> = Vec::with_capacity(ring);
        for tex in &input_textures {
            let registered = unsafe { this.register_texture(tex)? };
            registered_inputs.push(registered);
            let resource: IDXGIResource1 = tex
                .cast()
                .context("ring input texture as IDXGIResource1 (is it SHARED?)")?;
            let handle = unsafe {
                resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_RW, PCWSTR::null())
            }
            .context("IDXGIResource1::CreateSharedHandle (ring)")?;
            let mutex: IDXGIKeyedMutex = tex
                .cast()
                .context("ring input texture as IDXGIKeyedMutex")?;
            keyed_mutexes.push(mutex);
            shared_handles.push(handle);
        }

        this.ring = Some(Ring {
            input_textures,
            registered_inputs,
            keyed_mutexes,
            shared_handles,
        });

        tprintln!(
            "NVENC H.264 ring encoder initialized (D3D11 / ULL): {}x{}@{}, bitrate_bps={}, ring={}, profile={:?}, rc={}, qp={:?}, intra_refresh={}",
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            ring,
            config.profile,
            if config.qp.is_some() { "constqp" } else { "cbr" },
            config.qp,
            config.intra_refresh,
        );
        Ok(this)
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
            check(
                create(&mut fns),
                "NvEncodeAPICreateInstance",
                ptr::null_mut(),
            )?;
        }
        if fns.nvEncOpenEncodeSessionEx.is_none() {
            bail!("NVENC function list did not populate (driver too old?)");
        }

        let encoder = unsafe { open_session(&fns, device.as_raw())? };
        let caps = EncoderCaps::query(&fns, encoder);
        caps.log();
        let config = clamp_config_to_caps(config, &caps);
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
            ring: None,
            outputs: Vec::new(),
            inflight: std::collections::VecDeque::new(),
            next_output: 0,
            au_scratch: Vec::new(),
            async_mode: false,
            caps,
            init_params: unsafe { zeroed() },
            encode_config: unsafe { zeroed() },
            config,
            frame_index: 0,
            idr_requested: false,
            shared_idr: None,
            ltr_slots: 0,
            next_ltr_slot: 0,
            pending_ltr_mark: None,
            pending_ltr_use: None,
            _lib: lib,
        };

        unsafe { this.initialize()? };
        unsafe {
            this.register_input()?;
            this.create_outputs()?;
        }
        if shared {
            this.setup_shared()?;
        }

        tprintln!(
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
        let handle =
            unsafe { resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_RW, PCWSTR::null()) }
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

    pub fn shared_handles(&self) -> Vec<HANDLE> {
        match &self.ring {
            Some(ring) => ring.shared_handles.clone(),
            None => Vec::new(),
        }
    }

    pub fn encode_input_slot(
        &mut self,
        slot: usize,
        force_idr: bool,
    ) -> Result<Vec<u8>, SlotEncodeError> {
        let (mutex, registered) = {
            let ring = self
                .ring
                .as_ref()
                .ok_or_else(|| anyhow!("encode_input_slot called on a non-ring encoder"))
                .map_err(SlotEncodeError::Encode)?;
            let mutex = ring
                .keyed_mutexes
                .get(slot)
                .ok_or_else(|| anyhow!("encode_input_slot: slot {slot} out of range"))
                .map_err(SlotEncodeError::Encode)?
                .clone();
            let registered = ring.registered_inputs[slot];
            (mutex, registered)
        };

        unsafe { mutex.AcquireSync(KEY_ENCODER, KEY_TIMEOUT_MS) }
            .context("ring keyed mutex AcquireSync(encoder)")
            .map_err(SlotEncodeError::Poisoned)?;

        let force_idr = self.take_idr_request(force_idr);
        let res = unsafe { self.encode_mapped_resource(registered, force_idr) };
        let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
        let out = res.map_err(SlotEncodeError::Encode)?;
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_repeat_slot(&mut self, slot: usize) -> Result<Vec<u8>> {
        let (mutex, registered) = {
            let ring = self
                .ring
                .as_ref()
                .ok_or_else(|| anyhow!("encode_repeat_slot called on a non-ring encoder"))?;
            let mutex = ring
                .keyed_mutexes
                .get(slot)
                .ok_or_else(|| anyhow!("encode_repeat_slot: slot {slot} out of range"))?
                .clone();
            let registered = ring.registered_inputs[slot];
            (mutex, registered)
        };

        unsafe { mutex.AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS) }
            .context("ring keepalive keyed mutex AcquireSync(writer)")?;
        let force_idr = self.take_idr_request(false);
        let res = unsafe { self.encode_mapped_resource(registered, force_idr) };
        let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
        let out = res?;
        self.frame_index += 1;
        Ok(out)
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

        let force_idr = self.take_idr_request(force_idr);
        let data = unsafe { self.encode_mapped(force_idr)? };
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
        let force_idr = self.take_idr_request(force_idr);
        let out = unsafe { self.encode_mapped(force_idr)? };
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_input(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let Some(mutex) = self.input_keyed_mutex.clone() else {
            bail!("encode_input called on a non-shared (CPU-upload) encoder");
        };
        unsafe { mutex.AcquireSync(KEY_ENCODER, KEY_TIMEOUT_MS) }
            .context("keyed mutex AcquireSync(encoder)")?;
        let force_idr = self.take_idr_request(force_idr);
        let res = unsafe { self.encode_mapped(force_idr) };
        let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
        let out = res?;
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let force_idr = self.take_idr_request(force_idr);
        let out = if let Some(mutex) = self.input_keyed_mutex.clone() {
            unsafe { mutex.AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS) }
                .context("repeat keyed mutex AcquireSync(writer)")?;
            let res = unsafe { self.encode_mapped(force_idr) };
            let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
            res?
        } else {
            unsafe { self.encode_mapped(force_idr)? }
        };
        self.frame_index += 1;
        Ok(out)
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
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
        tprintln!("NVENC bitrate reconfigured (bitrate_bps={bps})");
        Ok(BitrateApplied::Applied)
    }

    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    /// Let the pipeline's IDR atomic reach the encoder directly.
    ///
    /// The ring's idle keepalive re-encodes the held slot from inside the encode thread; it
    /// never touches `AsyncNvencRing::stage`, which is the only place that consumed the
    /// pipeline's `idr_request`. So on a static desktop — a still window, a paused video, a
    /// locked screen — a PLI produced nothing but another P frame and a joining or damaged
    /// client stayed black until something on the host screen moved. With the flag attached,
    /// `take_idr_request` sees it wherever the encode happens.
    pub fn attach_idr_flag(&mut self, flag: Arc<AtomicBool>) {
        self.shared_idr = Some(flag);
    }

    fn take_idr_request(&mut self, force: bool) -> bool {
        let shared = self
            .shared_idr
            .as_ref()
            .is_some_and(|f| f.swap(false, Ordering::Relaxed));
        let own = self.idr_requested;
        self.idr_requested = false;
        force || own || shared
    }

    /// How many long-term reference slots this encoder has. 0 means the repair API is inert and
    /// the only loss recovery available is a keyframe.
    pub fn ltr_slots(&self) -> u32 {
        self.ltr_slots
    }

    /// Mark the next encoded picture as a long-term reference, and say which slot it takes.
    ///
    /// An LTR stays in the DPB until its slot is claimed again, so it is the picture a later
    /// repair frame can point at once the transport knows the client holds it. The caller owns
    /// the policy — how often to mark, and which slots it believes the client has — because only
    /// the transport sees the per-frame receipts. `None` when the engine has no LTR support.
    ///
    /// The mark is consumed by the next submitted picture, so it must be set before the submit
    /// it is meant for and not left standing across frames.
    pub fn mark_ltr(&mut self) -> Option<u32> {
        if self.ltr_slots == 0 {
            return None;
        }
        let slot = self.next_ltr_slot;
        self.next_ltr_slot = (slot + 1) % self.ltr_slots;
        self.pending_ltr_mark = Some(slot);
        Some(slot)
    }

    /// Make the next encoded picture reference only the long-term slots in `bitmap`.
    ///
    /// This is the repair: after a loss burst, a P frame predicted from a picture the client is
    /// known to hold costs a fraction of an IDR and does not reset the rate controller. Bits
    /// outside the encoder's slot count are ignored rather than rejected, and an empty bitmap is
    /// a no-op — asking for "reference nothing" would mean an intra frame, which the caller
    /// should ask for as an IDR instead.
    pub fn use_ltr(&mut self, bitmap: u32) -> bool {
        if self.ltr_slots == 0 {
            return false;
        }
        let mask = if self.ltr_slots >= 32 {
            u32::MAX
        } else {
            (1u32 << self.ltr_slots) - 1
        };
        let bitmap = bitmap & mask;
        if bitmap == 0 {
            return false;
        }
        self.pending_ltr_use = Some(bitmap);
        true
    }

    /// Drop a frame the client never received from the reference set.
    ///
    /// `frame_index` is the value handed to `NV_ENC_PIC_PARAMS::inputTimeStamp`, which this
    /// encoder sets to its own monotonic frame counter — so the transport identifies a lost
    /// frame by the same number the pipeline stamped on it. `nvEncInvalidateRefFrames` then
    /// removes it, and every picture that referenced it, from the DPB, so the next P frame
    /// predicts from something the client actually has.
    ///
    /// Cap-gated on `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`, never on a driver version.
    pub fn invalidate_reference(&mut self, frame_index: u64) -> RefInvalidation {
        if !self.caps.ref_pic_invalidation || self.ltr_slots == 0 {
            return RefInvalidation::Unsupported;
        }
        let Some(f) = self.fns.nvEncInvalidateRefFrames else {
            return RefInvalidation::Unsupported;
        };
        match unsafe { f(self.encoder, frame_index) } {
            NVENCSTATUS::NV_ENC_SUCCESS => RefInvalidation::Applied,
            status => {
                teprintln!(
                    "NVENC: nvEncInvalidateRefFrames(frame={frame_index}) returned {status:?};                      the frame has probably already left the DPB, so this loss needs a keyframe"
                );
                RefInvalidation::Refused
            }
        }
    }

    /// What the driver reported for this engine. Read once at session open.
    pub fn caps(&self) -> EncoderCaps {
        self.caps
    }

    /// How many pictures may be in flight at once. 1 without async support.
    pub fn pipeline_depth(&self) -> usize {
        self.outputs.len()
    }

    /// Submit one ring slot without waiting for it. Pairs with [`Encoder::complete_slot`].
    ///
    /// This is the half of the contract the pipeline needs for real encode pipelining: submit
    /// N+1 while the hardware is still working on N. The caller owns the pairing of each
    /// completed access unit with the capture timestamp and `FrameTiming` of the frame that
    /// produced it, which is why the encoder cannot do the overlap on its own —
    /// `encode_input_slot` returns *this* frame's bytes, and a depth-2 encoder returning the
    /// previous frame's bytes under that name would silently corrupt A/V sync.
    ///
    /// The slot's keyed mutex stays held at `KEY_ENCODER` until `complete_slot`, so the slot
    /// must not be recycled in between.
    pub fn submit_slot(&mut self, slot: usize, force_idr: bool) -> Result<(), SlotEncodeError> {
        let (mutex, registered) = self.ring_slot(slot)?;
        unsafe { mutex.AcquireSync(KEY_ENCODER, KEY_TIMEOUT_MS) }
            .context("ring keyed mutex AcquireSync(encoder)")
            .map_err(SlotEncodeError::Poisoned)?;

        let force_idr = self.take_idr_request(force_idr);
        match unsafe { self.submit_registered(registered, force_idr) } {
            Ok(()) => {
                self.frame_index += 1;
                Ok(())
            }
            Err(e) => {
                let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
                Err(SlotEncodeError::Encode(e))
            }
        }
    }

    /// Drain the oldest picture submitted by [`Encoder::submit_slot`] and release its slot.
    pub fn complete_slot(&mut self, slot: usize) -> Result<Vec<u8>, SlotEncodeError> {
        let (mutex, _) = self.ring_slot(slot)?;
        let out = unsafe { self.complete_next() };
        let _ = unsafe { mutex.ReleaseSync(KEY_WRITER) };
        out.map_err(SlotEncodeError::Encode)
    }

    fn ring_slot(
        &self,
        slot: usize,
    ) -> Result<(IDXGIKeyedMutex, NV_ENC_REGISTERED_PTR), SlotEncodeError> {
        let ring = self
            .ring
            .as_ref()
            .ok_or_else(|| anyhow!("ring slot access on a non-ring encoder"))
            .map_err(SlotEncodeError::Encode)?;
        let mutex = ring
            .keyed_mutexes
            .get(slot)
            .ok_or_else(|| anyhow!("slot {slot} out of range"))
            .map_err(SlotEncodeError::Encode)?
            .clone();
        Ok((mutex, ring.registered_inputs[slot]))
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
        if self.config.qp.is_some()
            && !self
                .caps
                .supports_rc(NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP)
        {
            teprintln!("NVENC: this engine does not advertise CONSTQP; falling back to CBR");
            self.config.qp = None;
        }
        if self.config.qp.is_none()
            && !self
                .caps
                .supports_rc(NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR)
        {
            bail!(
                "NVENC engine advertises no CBR rate control (rc_modes=0x{:x})",
                self.caps.rc_modes
            );
        }
        if let Some(q) = self.config.qp {
            let q = q as u32;
            config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CONSTQP;
            config.rcParams.constQP = NV_ENC_QP {
                qpInterP: q,
                qpInterB: q,
                qpIntra: q,
            };
        } else {
            config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
            config.rcParams.averageBitRate = self.config.bitrate_bps;
            config.rcParams.maxBitRate = self.config.max_bitrate_bps;
            if self.caps.custom_vbv_buf_size {
                config.rcParams.vbvBufferSize = self.config.bitrate_bps / fps;
                config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
                // The VBV is one frame, so an IDR — the only intra content in an infinite GOP —
                // would otherwise have to blow it or be quantised hard. This is the documented
                // allowance: a keyframe may spend two frame budgets. Left inherited from the
                // preset it was an unknown, because nothing read the preset back.
                config.rcParams.lowDelayKeyFrameScale = 2;
            }
            // The quality slider is a floor on QP, i.e. a ceiling on quality: it stops the
            // encoder spending the whole CBR budget on an easy picture. It can only make the
            // stream smaller, so it never fights congestion control the way CONSTQP did.
            if let Some(q) = self.config.min_qp.filter(|&q| q > 1) {
                let q = q as u32;
                config.rcParams.set_enableMinQP(1);
                config.rcParams.minQP = NV_ENC_QP {
                    qpInterP: q,
                    qpInterB: q,
                    qpIntra: q,
                };
            }
        }
        config.rcParams.set_enableLookahead(0);
        config.rcParams.lookaheadDepth = 0;
        config.rcParams.set_enableAQ(0);
        config.rcParams.set_enableTemporalAQ(0);
        config.rcParams.multiPass = NV_ENC_MULTI_PASS::NV_ENC_MULTI_PASS_DISABLED;
        config.rcParams.set_zeroReorderDelay(1);

        let caps = self.caps;
        let h264 = unsafe { &mut config.encodeCodecConfig.h264Config };
        h264.idrPeriod = NVENC_INFINITE_GOPLENGTH;

        // Rolling intra refresh, not a burst. The old parameters swept `fps/2` rows once every
        // `fps*4` frames — 30 rows every 4 seconds at 60 fps — which is a refresh that is
        // absent whenever loss actually happens. A continuously rolling wave
        // (`cnt = period - 1`) repairs loss with no keyframe and no bitrate spike, which is the
        // shape Sunshine ships. Requires the single-slice cap because intra refresh
        // historically forced multiple slices, and we code one slice per picture.
        let intra_refresh = self.config.intra_refresh && caps.intra_refresh;
        if self.config.intra_refresh && !caps.intra_refresh {
            teprintln!(
                "NVENC: intra refresh requested but not supported by this engine; \
                        loss recovery stays IDR-on-PLI"
            );
        }
        if intra_refresh && !caps.single_slice_intra_refresh {
            teprintln!(
                "NVENC: engine needs multiple slices for intra refresh; keeping it off \
                        rather than changing the slice layout mid-configuration"
            );
        }
        if intra_refresh && caps.single_slice_intra_refresh {
            h264.set_enableIntraRefresh(1);
            h264.intraRefreshPeriod = (fps * 5).clamp(2, 1024);
            h264.intraRefreshCnt = h264.intraRefreshPeriod - 1;
        }
        self.config.intra_refresh = h264.enableIntraRefresh() == 1;

        h264.sliceMode = 0;
        h264.sliceModeData = 0;

        // More than one reference frame is the prerequisite for repairing loss with a P frame
        // that skips the damaged picture instead of a full IDR. With `maxNumRefFrames = 1`
        // every P frame references its immediate predecessor and one loss breaks the chain
        // permanently, so the only recovery is an IDR. The DPB cost at these resolutions is a
        // few MB of VRAM.
        let refs = if caps.multiple_ref_frames { 4 } else { 1 };
        h264.maxNumRefFrames = refs;
        h264.numRefL0 = if refs >= 4 {
            NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_4
        } else {
            NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1
        };
        // frameIntervalP = 1 means no B frames, so L1 is never consulted; left at 1.
        h264.numRefL1 = NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;

        // Long-term references, for the NACK-driven repair path: a frame marked LTR stays in
        // the DPB until replaced, so after a loss burst the encoder can emit a P frame
        // referencing a picture the client is known to hold. Marking and selection are
        // per-picture (`ltrMarkFrameIdx` / `ltrUseFrameBitmap`); trust mode is deprecated.
        let ltr = caps.max_ltr_frames >= 2 && caps.multiple_ref_frames;
        if ltr {
            h264.set_enableLTR(1);
            h264.ltrNumFrames = caps.max_ltr_frames.min(MAX_LTR_SLOTS);
            h264.ltrTrustMode = 0;
        }
        self.ltr_slots = if ltr { h264.ltrNumFrames } else { 0 };

        h264.set_repeatSPSPPS(1);
        h264.set_enableFillerDataInsertion(0);
        h264.set_outputBufferingPeriodSEI(0);
        h264.set_outputPictureTimingSEI(0);
        h264.set_outputAUD(0);

        // CABAC and the adaptive 8x8 transform are the whole difference between Main/High and
        // Baseline here, and they are worth 5-15% bitrate at equal quality on screen content.
        // Both are optional NVENC features, so both are gated — an engine without CABAC gets
        // the requested profile with CAVLC rather than losing the NVIDIA path entirely.
        let cabac = if caps.cabac {
            NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CABAC
        } else {
            NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC
        };
        match self.config.profile {
            H264Profile::Baseline => {
                if caps.constrained_encoding {
                    h264.set_enableConstrainedEncoding(1);
                }
                h264.entropyCodingMode =
                    NV_ENC_H264_ENTROPY_CODING_MODE::NV_ENC_H264_ENTROPY_CODING_MODE_CAVLC;
                h264.adaptiveTransformMode =
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE;
            }
            H264Profile::Main => {
                h264.entropyCodingMode = cabac;
                h264.adaptiveTransformMode =
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE;
            }
            H264Profile::High => {
                h264.entropyCodingMode = cabac;
                h264.adaptiveTransformMode = if caps.adaptive_transform {
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_ENABLE
                } else {
                    NV_ENC_H264_ADAPTIVE_TRANSFORM_MODE::NV_ENC_H264_ADAPTIVE_TRANSFORM_DISABLE
                };
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
        // Without this the SPS says nothing about reordering, so a decoder infers the reorder
        // depth from the level and holds min(16, MaxDpbMbs/MBs) frames before showing the
        // first — 33-267 ms at 60 fps. libwebrtc rewrites the SPS on the receive path today, so
        // on the RTP transport this is insurance; it stops being insurance the moment video
        // leaves RTP. `frameIntervalP = 1` makes the values NVENC derives (reorder 0) correct.
        // Sunshine and FFmpeg's NVENC wrapper both set it.
        vui.bitstreamRestrictionFlag = 1;

        let intra_refresh_period = h264.intraRefreshPeriod;
        let intra_refresh_cnt = h264.intraRefreshCnt;
        let entropy = h264.entropyCodingMode;

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
        // Async mode makes `nvEncEncodePicture` a submit rather than a submit-and-wait: the
        // call returns as soon as the picture is queued and a per-output event says when the
        // hardware finished it. Windows/WDDM only, hence the cap.
        //
        // It is *not* here for throughput. Measured on an Ampere part it is a wash
        // (`nvenc_pipelined_submit_is_not_faster_than_submit_and_drain`), because the GPU has
        // one NVENC engine and pictures serialise on it regardless. What it buys is a bounded
        // wait: `WaitForSingleObject` with a timeout turns a wedged encoder into a named error
        // after 2 s, where the synchronous `LockBitstream` blocks the encode thread inside the
        // driver with nothing to interrupt it. `SE_NVENC_NO_ASYNC=1` goes back to that path.
        self.async_mode = caps.async_encode
            && self.fns.nvEncRegisterAsyncEvent.is_some()
            && !async_disabled_by_env();
        init.enableEncodeAsync = u32::from(self.async_mode);
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

        tprintln!(
            "NVENC configured: async={}, outputs={}, max_ref_frames={}, ltr={}, \
             intra_refresh={} (period={}, cnt={}), entropy={:?}, vbv={}B, low_delay_kf_scale={}",
            self.async_mode,
            if self.async_mode { OUTPUT_BUFFERS } else { 1 },
            refs,
            ltr,
            self.config.intra_refresh,
            intra_refresh_period,
            intra_refresh_cnt,
            entropy,
            config.rcParams.vbvBufferSize,
            config.rcParams.lowDelayKeyFrameScale,
        );

        self.encode_config = config;
        self.init_params = init;
        self.init_params.encodeConfig = &mut self.encode_config;
        Ok(())
    }

    unsafe fn register_input(&mut self) -> Result<()> {
        let registered = unsafe { self.register_texture(&self.input_texture.clone())? };
        self.registered_input = registered;
        Ok(())
    }

    unsafe fn register_texture(&self, texture: &ID3D11Texture2D) -> Result<NV_ENC_REGISTERED_PTR> {
        let mut reg: NV_ENC_REGISTER_RESOURCE = unsafe { zeroed() };
        reg.version = NV_ENC_REGISTER_RESOURCE_VER;
        reg.resourceType = NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        reg.width = self.config.width;
        reg.height = self.config.height;
        reg.pitch = 0;
        reg.resourceToRegister = texture.as_raw();
        reg.bufferFormat = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        reg.bufferUsage = NV_ENC_BUFFER_USAGE::NV_ENC_INPUT_IMAGE;
        unsafe {
            let f = self
                .fns
                .nvEncRegisterResource
                .expect("nvEncRegisterResource present");
            self.check(f(self.encoder, &mut reg), "nvEncRegisterResource")?;
        }
        Ok(reg.registeredResource)
    }

    /// Allocate the output ring: one bitstream buffer per slot and, when the encoder was
    /// initialized in async mode, one auto-reset event per slot registered with the driver.
    ///
    /// Async mode is what turns `nvEncEncodePicture` into a submit: it returns as soon as the
    /// picture is queued, and the event tells us when the hardware is done, so `LockBitstream`
    /// with `doNotWait` never blocks. In synchronous mode the driver blocks inside
    /// `LockBitstream` instead, holding the one output buffer for the whole encode.
    unsafe fn create_outputs(&mut self) -> Result<()> {
        let count = if self.async_mode { OUTPUT_BUFFERS } else { 1 };
        for i in 0..count {
            let mut create: NV_ENC_CREATE_BITSTREAM_BUFFER = unsafe { zeroed() };
            create.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            unsafe {
                let f = self
                    .fns
                    .nvEncCreateBitstreamBuffer
                    .expect("nvEncCreateBitstreamBuffer present");
                self.check(f(self.encoder, &mut create), "nvEncCreateBitstreamBuffer")?;
            }

            let event = if self.async_mode {
                let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
                    .context("CreateEventW for an NVENC completion event")?;
                let mut params: NV_ENC_EVENT_PARAMS = unsafe { zeroed() };
                params.version = NV_ENC_EVENT_PARAMS_VER;
                params.completionEvent = handle.0;
                unsafe {
                    let f = self
                        .fns
                        .nvEncRegisterAsyncEvent
                        .expect("nvEncRegisterAsyncEvent present");
                    self.check(f(self.encoder, &mut params), "nvEncRegisterAsyncEvent")
                        .with_context(|| format!("registering completion event {i}"))?;
                }
                Some(handle)
            } else {
                None
            };

            self.outputs.push(Output {
                bitstream: create.bitstreamBuffer,
                event,
            });
        }
        Ok(())
    }

    unsafe fn encode_mapped(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let registered = self.registered_input;
        unsafe { self.encode_mapped_resource(registered, force_idr) }
    }

    unsafe fn encode_mapped_resource(
        &mut self,
        registered: NV_ENC_REGISTERED_PTR,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        unsafe { self.submit_registered(registered, force_idr)? };
        unsafe { self.complete_next() }
    }

    /// Hand one registered input resource to the hardware and return without waiting.
    ///
    /// The mapped resource and the output slot stay owned by `inflight` until the matching
    /// `complete_next`, because NVENC reads the input for the whole encode and writes the
    /// output buffer for the whole encode — neither may be reused or unmapped before then.
    unsafe fn submit_registered(
        &mut self,
        registered: NV_ENC_REGISTERED_PTR,
        force_idr: bool,
    ) -> Result<()> {
        if self.inflight.len() >= self.outputs.len() {
            bail!(
                "NVENC submit with {} pictures already in flight and only {} output buffers; \
                 drain one with complete_next() first",
                self.inflight.len(),
                self.outputs.len()
            );
        }

        let mut map: NV_ENC_MAP_INPUT_RESOURCE = unsafe { zeroed() };
        map.version = NV_ENC_MAP_INPUT_RESOURCE_VER;
        map.registeredResource = registered;
        unsafe {
            let f = self
                .fns
                .nvEncMapInputResource
                .expect("nvEncMapInputResource present");
            self.check(f(self.encoder, &mut map), "nvEncMapInputResource")?;
        }
        let mapped = map.mappedResource;

        let out_idx = self.next_output;
        let output = &self.outputs[out_idx];

        let mut pic: NV_ENC_PIC_PARAMS = unsafe { zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.inputWidth = self.config.width;
        pic.inputHeight = self.config.height;
        pic.inputPitch = self.config.width * 4;
        pic.inputBuffer = mapped;
        pic.outputBitstream = output.bitstream;
        pic.completionEvent = output.event.map(|h| h.0).unwrap_or(ptr::null_mut());
        pic.bufferFmt = NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ARGB;
        pic.pictureStruct = NV_ENC_PIC_STRUCT::NV_ENC_PIC_STRUCT_FRAME;
        pic.inputTimeStamp = self.frame_index;
        pic.frameIdx = self.frame_index as u32;
        if force_idr {
            pic.encodePicFlags = (NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_FORCEIDR as u32)
                | (NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_OUTPUT_SPSPPS as u32);
        }

        // An IDR resets the DPB, so a pending mark or use against it would be meaningless —
        // take them either way so neither leaks into a later picture, but only apply them to a
        // P frame.
        let ltr_mark = self.pending_ltr_mark.take();
        let ltr_use = self.pending_ltr_use.take();
        if !force_idr && self.ltr_slots > 0 {
            let h264 = unsafe { &mut pic.codecPicParams.h264PicParams };
            if let Some(slot) = ltr_mark {
                h264.set_ltrMarkFrame(1);
                h264.ltrMarkFrameIdx = slot;
            }
            if let Some(bitmap) = ltr_use {
                h264.set_ltrUseFrames(1);
                h264.ltrUseFrameBitmap = bitmap;
            }
        }

        let st = unsafe {
            let f = self
                .fns
                .nvEncEncodePicture
                .expect("nvEncEncodePicture present");
            f(self.encoder, &mut pic)
        };
        if let Err(e) = self.check(st, "nvEncEncodePicture") {
            unsafe { self.unmap(mapped) };
            return Err(e);
        }

        self.next_output = (out_idx + 1) % self.outputs.len();
        self.inflight.push_back(InFlight {
            output: out_idx,
            mapped,
        });
        Ok(())
    }

    /// Drain the oldest submitted picture: wait for its completion event (async) or let the
    /// driver block inside `LockBitstream` (sync), copy the access unit out, then release the
    /// output buffer and unmap the input.
    ///
    /// The copy is into a reused scratch buffer rather than a fresh `Vec` per frame; NVENC maps
    /// its bitstream write-combined, so this read is more expensive than its byte count
    /// suggests and it happens with the buffer locked, delaying the next picture that lands on
    /// the same slot.
    unsafe fn complete_next(&mut self) -> Result<Vec<u8>> {
        let Some(pending) = self.inflight.pop_front() else {
            bail!("NVENC complete_next with nothing in flight");
        };
        let bitstream = self.outputs[pending.output].bitstream;
        let event = self.outputs[pending.output].event;

        let waited = match event {
            Some(h) => unsafe { wait_for_event(h) },
            None => Ok(()),
        };
        if let Err(e) = waited {
            unsafe { self.unmap(pending.mapped) };
            return Err(e);
        }

        let mut lock: NV_ENC_LOCK_BITSTREAM = unsafe { zeroed() };
        lock.version = NV_ENC_LOCK_BITSTREAM_VER;
        lock.outputBitstream = bitstream;
        // With a signalled completion event the data is already there, so the driver must not
        // be allowed to block again inside the lock.
        lock.set_doNotWait(u32::from(event.is_some()));
        let st = unsafe {
            let f = self
                .fns
                .nvEncLockBitstream
                .expect("nvEncLockBitstream present");
            f(self.encoder, &mut lock)
        };
        if let Err(e) = self.check(st, "nvEncLockBitstream") {
            unsafe { self.unmap(pending.mapped) };
            return Err(e);
        }

        let len = lock.bitstreamSizeInBytes as usize;
        self.au_scratch.clear();
        self.au_scratch.reserve(len);
        unsafe {
            ptr::copy_nonoverlapping(
                lock.bitstreamBufferPtr as *const u8,
                self.au_scratch.as_mut_ptr(),
                len,
            );
            self.au_scratch.set_len(len);
        }

        let unlock = unsafe {
            let f = self
                .fns
                .nvEncUnlockBitstream
                .expect("nvEncUnlockBitstream present");
            f(self.encoder, bitstream)
        };
        unsafe { self.unmap(pending.mapped) };
        self.check(unlock, "nvEncUnlockBitstream")?;

        Ok(std::mem::take(&mut self.au_scratch))
    }

    unsafe fn unmap(&self, mapped: NV_ENC_INPUT_PTR) {
        if let Some(f) = self.fns.nvEncUnmapInputResource {
            let st = unsafe { f(self.encoder, mapped) };
            if st != NVENCSTATUS::NV_ENC_SUCCESS {
                teprintln!("nvEncUnmapInputResource returned {st:?}");
            }
        }
    }
}

/// Escape hatch back to the pre-async submit-and-block path: an A/B lever for measuring what
/// async mode is worth, and the workaround if a driver revision turns out to mishandle
/// completion events.
fn async_disabled_by_env() -> bool {
    std::env::var("SE_NVENC_NO_ASYNC").as_deref() == Ok("1")
}

/// Block on an NVENC completion event. A timeout here means the hardware never finished the
/// picture, which is a driver fault or a TDR, not congestion — the caller treats it as an
/// encode error so the session can shed the frame rather than wait forever.
unsafe fn wait_for_event(handle: HANDLE) -> Result<()> {
    let rv = unsafe { WaitForSingleObject(handle, ENCODE_EVENT_TIMEOUT_MS) };
    match rv {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => bail!(
            "NVENC completion event not signalled within {ENCODE_EVENT_TIMEOUT_MS} ms \
             (encoder wedged: GPU hang, TDR or driver fault)"
        ),
        other => bail!("WaitForSingleObject on an NVENC completion event returned {other:?}"),
    }
}

/// Fold the driver's reported limits into the requested geometry. The client picks its own
/// viewport, `server.rs` clamps it only to 2..=16384, and nothing before this consulted what
/// the encoder can actually take.
fn clamp_config_to_caps(config: EncoderConfig, caps: &EncoderCaps) -> EncoderConfig {
    let (w, h) = caps.clamp_dims(config.width, config.height);
    if w != config.width || h != config.height {
        teprintln!(
            "NVENC: requested {}x{} is outside this engine's {}x{}..{}x{} range; encoding at {w}x{h}",
            config.width,
            config.height,
            caps.width_min,
            caps.height_min,
            caps.width_max,
            caps.height_max,
        );
    }
    EncoderConfig {
        width: w,
        height: h,
        ..config
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            if let Some(ring) = self.ring.take() {
                for &registered in &ring.registered_inputs {
                    if !registered.is_null() {
                        if let Some(f) = self.fns.nvEncUnregisterResource {
                            let st = f(self.encoder, registered);
                            log_drop_status(st, "nvEncUnregisterResource (ring)");
                        }
                    }
                }
                for &h in &ring.shared_handles {
                    let _ = CloseHandle(h);
                }
                drop(ring);
            }
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
            // Anything still in flight holds a mapped input and an output buffer; the driver
            // must be told to forget them before either can be destroyed.
            while let Some(pending) = self.inflight.pop_front() {
                if let Some(f) = self.fns.nvEncUnmapInputResource {
                    log_drop_status(f(self.encoder, pending.mapped), "nvEncUnmapInputResource");
                }
            }
            for out in std::mem::take(&mut self.outputs) {
                if let Some(h) = out.event {
                    // Unregister before closing: the driver keeps the handle otherwise.
                    if let Some(f) = self.fns.nvEncUnregisterAsyncEvent {
                        let mut params: NV_ENC_EVENT_PARAMS = zeroed();
                        params.version = NV_ENC_EVENT_PARAMS_VER;
                        params.completionEvent = h.0;
                        log_drop_status(f(self.encoder, &mut params), "nvEncUnregisterAsyncEvent");
                    }
                    let _ = CloseHandle(h);
                }
                if !out.bitstream.is_null() {
                    if let Some(f) = self.fns.nvEncDestroyBitstreamBuffer {
                        log_drop_status(
                            f(self.encoder, out.bitstream),
                            "nvEncDestroyBitstreamBuffer",
                        );
                    }
                }
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
            tprintln!("DXGI adapter (index={i}, name={name}, is_software={is_software})");
            if !is_software && name.to_uppercase().contains("NVIDIA") {
                chosen = Some(adapter);
                tprintln!("selected NVIDIA adapter for NVENC (index={i}, name={name})");
                break;
            }
            i += 1;
        }

        let adapter =
            chosen.ok_or_else(|| anyhow!("no NVIDIA adapter found; NVENC requires NVIDIA GPU"))?;

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
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
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
        teprintln!("{what} returned {status:?} during encoder drop");
    }
}

pub fn probe_encode(config: &Config, path: &str) -> Result<()> {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FPS: u32 = 60;
    const FRAMES: u32 = 300;
    const BITRATE: u32 = 10_000_000;

    tprintln!("encoding synthetic pattern to Annex-B: path={path}, {WIDTH}x{HEIGHT}@{FPS}, {FRAMES} frames");

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
            tprintln!(
                "encoded frame={i} (au_bytes={}, total_bytes={total_bytes})",
                au.len()
            );
        }
    }
    file.flush().context("flushing output file")?;

    tprintln!("wrote Annex-B H.264: path={path}, frames={FRAMES}, total_bytes={total_bytes}");
    Ok(())
}

pub(crate) fn fill_synthetic_bgra(buf: &mut [u8], width: u32, height: u32, frame: u32) {
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
