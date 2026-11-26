use std::ffi::c_void;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::windows_utils::streamer::keyed::{self, Acquired};
use crate::windows_utils::streamer::{tune, NVIDIA_PCI_VENDOR_ID};

pub const KEY_WRITER: u64 = 0;
pub const KEY_ENCODER: u64 = 1;
pub const KEY_TIMEOUT_MS: u32 = 1000;

/// How long to wait on a per-picture completion event before declaring the encoder wedged.
/// A hang detector: NVENC normally signals within one frame period.
const ENCODE_EVENT_TIMEOUT_MS: u32 = 2000;

/// How long `Drop` waits for the end-of-stream flush. Nothing is in flight by then, so a
/// healthy driver signals at once; the bound stops a wedged one holding up a renegotiation.
const EOS_FLUSH_TIMEOUT_MS: u32 = 500;

/// Window the arrival rate is measured over, for `nvenc_rate_budget_observed_fps`.
const RATE_WINDOW: Duration = Duration::from_secs(1);

/// The smallest output `Vec` allocated ahead of the bitstream lock under `nvenc_single_copy_au`.
const AU_MIN_CAPACITY: usize = 64 * 1024;

/// Output buffers and completion events per encoder in async mode. Two is the minimum that
/// lets submit(N+1) overlap the hardware's work on N.
const OUTPUT_BUFFERS: usize = 4;

/// The most long-term reference slots to claim, however many the engine advertises.
///
/// Two covers the repair pattern: one the client is known to hold and one being established.
/// Each slot is a picture held in the DPB (VRAM, a wider reference search), and more only
/// widen the window in which the encoder may predict from something stale.
const MAX_LTR_SLOTS: u32 = 2;

/// Check every driver entry point the encode path will unwrap, once, before anything is opened.
///
/// `NV_ENCODE_API_FUNCTION_LIST` is a table of nullable C function pointers, and a vGPU or
/// virtualised NVENC may fill it only partially. Failing here lets `build_backend` fall through
/// to the next vendor with a reason; an `.expect()` on the detached encode thread would be a
/// stream that silently stops.
///
/// `nvEncInvalidateRefFrames` and the async-event pair are optional and gated at their call
/// sites, so they are not listed.
fn require_functions(fns: &NV_ENCODE_API_FUNCTION_LIST) -> Result<()> {
    let required: [(&str, bool); 13] = [
        (
            "nvEncOpenEncodeSessionEx",
            fns.nvEncOpenEncodeSessionEx.is_some(),
        ),
        ("nvEncGetEncodeCaps", fns.nvEncGetEncodeCaps.is_some()),
        (
            "nvEncGetEncodePresetConfigEx",
            fns.nvEncGetEncodePresetConfigEx.is_some(),
        ),
        (
            "nvEncInitializeEncoder",
            fns.nvEncInitializeEncoder.is_some(),
        ),
        ("nvEncRegisterResource", fns.nvEncRegisterResource.is_some()),
        (
            "nvEncUnregisterResource",
            fns.nvEncUnregisterResource.is_some(),
        ),
        (
            "nvEncCreateBitstreamBuffer",
            fns.nvEncCreateBitstreamBuffer.is_some(),
        ),
        (
            "nvEncDestroyBitstreamBuffer",
            fns.nvEncDestroyBitstreamBuffer.is_some(),
        ),
        ("nvEncMapInputResource", fns.nvEncMapInputResource.is_some()),
        (
            "nvEncUnmapInputResource",
            fns.nvEncUnmapInputResource.is_some(),
        ),
        ("nvEncEncodePicture", fns.nvEncEncodePicture.is_some()),
        ("nvEncLockBitstream", fns.nvEncLockBitstream.is_some()),
        ("nvEncUnlockBitstream", fns.nvEncUnlockBitstream.is_some()),
    ];
    let missing: Vec<&str> = required
        .iter()
        .filter(|(_, present)| !present)
        .map(|(name, _)| *name)
        .collect();
    if !missing.is_empty() {
        bail!(
            "NVENC function list is missing {} required entry point(s): {} (driver too old, or a \
             virtualised NVENC that does not implement the full API)",
            missing.len(),
            missing.join(", ")
        );
    }
    Ok(())
}

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

/// Take `key` on an input texture's keyed mutex, or say why the texture must not be touched.
///
/// `IDXGIKeyedMutex::AcquireSync` reports a timeout and an abandoned mutex as success codes, and
/// the `windows` wrapper turns both into `Ok(())`, hence `keyed::acquire`. Only `Ok` here means
/// the caller owns the texture and must release it.
fn acquire_key(mutex: &IDXGIKeyedMutex, key: u64, what: &str) -> Result<()> {
    match keyed::acquire(mutex, key, KEY_TIMEOUT_MS) {
        Ok(Acquired::Owned) => Ok(()),
        Ok(Acquired::TimedOut) => bail!(
            "{what}: AcquireSync(key {key}) timed out after {KEY_TIMEOUT_MS} ms; the other side \
             never released the texture, so it was not touched"
        ),
        Ok(Acquired::Abandoned) => bail!(
            "{what}: AcquireSync(key {key}) found the keyed mutex abandoned (the other device \
             went away); the texture was not touched"
        ),
        Err(e) => Err(anyhow::Error::from(e).context(format!("{what}: AcquireSync(key {key})"))),
    }
}

/// Hand an input texture back at `key`. A failure means the other side will wait on a key that
/// never comes, so the caller must report it rather than recycle the slot.
fn release_key(mutex: &IDXGIKeyedMutex, key: u64, what: &str) -> Result<()> {
    unsafe { mutex.ReleaseSync(key) }.with_context(|| format!("{what}: ReleaseSync(key {key})"))
}

/// Whether NVENC can open a session on a DXGI adapter: hardware, with NVIDIA's PCI vendor id.
/// By id, never by the description string: an OEM-renamed or virtualised part need not say
/// "NVIDIA".
pub(crate) fn is_nvenc_adapter(vendor_id: u32, flags: u32) -> bool {
    let is_software = (flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32) != 0;
    !is_software && vendor_id == NVIDIA_PCI_VENDOR_ID
}

/// Owns a freshly opened NVENC session until an [`Encoder`] takes it.
///
/// The caps query, the input textures and the clamp can all fail before the `Encoder` exists,
/// and a raw pointer has no `Drop`: each such failure would leak one of the driver's
/// concurrent-session slots (8 on driver 580.97) for the life of the process.
pub(crate) struct SessionGuard {
    encoder: *mut c_void,
    destroy: PNVENCDESTROYENCODER,
}

impl SessionGuard {
    pub(crate) fn new(encoder: *mut c_void, destroy: PNVENCDESTROYENCODER) -> Self {
        Self { encoder, destroy }
    }

    pub(crate) fn raw(&self) -> *mut c_void {
        self.encoder
    }

    /// Give the session to its owner; the guard no longer destroys it.
    pub(crate) fn into_raw(mut self) -> *mut c_void {
        std::mem::replace(&mut self.encoder, ptr::null_mut())
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        if self.encoder.is_null() {
            return;
        }
        if let Some(destroy) = self.destroy {
            log_drop_status(
                unsafe { destroy(self.encoder) },
                "nvEncDestroyEncoder (encoder construction failed)",
            );
        }
        self.encoder = ptr::null_mut();
    }
}

/// The frame rate the CBR budget should be spread over, or `None` to keep the one applied.
///
/// NVENC's rate control has no clock: every picture gets `averageBitRate / frameRateNum`, so
/// pictures arriving faster than the configured rate (a cursor-moved frame is exempt from the
/// capture cadence gate) overspend the target. Never slower than configured, since
/// under-delivery only leaves budget unspent, and a 10 % band stops a rate hovering on a
/// boundary from reconfiguring every window.
pub(crate) fn budget_fps(configured: u32, observed: u32, applied: u32) -> Option<u32> {
    let want = observed.max(configured.max(1));
    let applied = applied.max(1);
    (u64::from(want.abs_diff(applied)) * 10 > u64::from(applied)).then_some(want)
}

/// The `averageBitRate` that makes `budget` pictures a second, each given
/// `averageBitRate / configured` by the rate control, add up to `target`.
pub(crate) fn scaled_bitrate(target: u32, configured: u32, budget: u32) -> u32 {
    let v = u64::from(target) * u64::from(configured.max(1)) / u64::from(budget.max(1));
    v.clamp(1, u64::from(u32::MAX)) as u32
}

/// Capacity to allocate for the next access unit ahead of the bitstream lock: twice the last
/// one, so ordinary frame-to-frame variation fits. A picture that does not fit (an IDR after
/// small P frames) takes the two-copy path instead of allocating inside the lock.
pub(crate) fn next_au_capacity(last_len: usize) -> usize {
    last_len.saturating_mul(2).max(AU_MIN_CAPACITY)
}

/// The `nvenc_rate_budget_observed_fps` measurement state.
struct RateBudget {
    window_start: Instant,
    frames: u32,
    /// The frame rate the budget is currently spread over. Starts at the configured rate.
    applied_fps: u32,
}

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
    shared_handles: Vec<OwnedHandle>,
}

/// What this NVENC engine says it can do, read with `nvEncGetEncodeCaps` once per session
/// between `open_session` and `initialize`.
///
/// Every optional field in `initialize()` is gated on one of these. A refused
/// `nvEncInitializeEncoder` costs the whole NVIDIA path over one unsupported flag, because
/// `build_backend` cannot tell that from "NVENC is unavailable" and falls through to Quick
/// Sync or x264.
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
        rc_mode_supported(self.rc_modes, mode)
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

/// Whether `NV_ENC_CAPS_SUPPORTED_RATECONTROL_MODES` admits `mode`.
///
/// The cap is a bitmask of the `NV_ENC_PARAMS_RC_MODE` values themselves, and
/// `NV_ENC_PARAMS_RC_CONSTQP` is 0: it has no bit, so `mask & CONSTQP` is 0 on every driver
/// (an RTX 3080 Ti reports `0x3f`). Constant QP is the one mode every NVENC engine implements,
/// so it is always admitted; a 0 mask (the query failed) vetoes nothing.
pub(crate) fn rc_mode_supported(mask: u32, mode: NV_ENC_PARAMS_RC_MODE) -> bool {
    let bit = mode as u32;
    bit == 0 || mask == 0 || (mask & bit) != 0
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
    shared_handle: Option<OwnedHandle>,
    ring: Option<Ring>,
    outputs: Vec<Output>,
    inflight: std::collections::VecDeque<InFlight>,
    next_output: usize,
    /// The staging buffer the locked bitstream is read into. Kept across frames and only ever
    /// grown, so the copy out of the driver's write-combined mapping allocates under the
    /// bitstream lock only when an access unit is the largest seen so far.
    au_scratch: Vec<u8>,
    /// `nvenc_single_copy_au`: copy the access unit straight out of the locked bitstream into the
    /// caller's `Vec`, allocated before the lock, instead of through `au_scratch`.
    single_copy_au: bool,
    /// The capacity to allocate ahead of the next bitstream lock, from the last access unit.
    au_capacity_hint: usize,
    /// `nvenc_rate_budget_observed_fps`, when that candidate is on. `None` otherwise.
    rate_budget: Option<RateBudget>,
    /// Set once a live bitrate change has been refused by an engine that does not advertise
    /// `NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE`; later requests are then answered without
    /// asking the driver again.
    bitrate_fixed: bool,
    /// `nvEncInitializeEncoder` succeeded, so `Drop` has an encoder to flush.
    initialized: bool,
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
    /// How many long-term reference slots the encoder was opened with. 0 means the repair API
    /// below is inert.
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
/// `Unsupported` means the only repair left is a keyframe, and the caller should stop asking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefInvalidation {
    /// The driver dropped the frame from the reference set. The next P frame will not use it.
    Applied,
    /// The engine does not advertise `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`, or the encoder
    /// was opened without long-term references, so there is nothing to invalidate against.
    Unsupported,
    /// The driver refused, most often because the frame has already left the DPB. The repair
    /// has to be a keyframe.
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
        let (device, context, fns, lib) = load_driver()?;

        let session = SessionGuard::new(
            unsafe { open_session(&fns, device.as_raw())? },
            fns.nvEncDestroyEncoder,
        );
        let caps = EncoderCaps::query(&fns, session.raw());
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

        // `Self` owns the session from here and each resource is recorded on it the moment it
        // exists, so `Drop` releases a ring that failed part-way
        let mut this = Self::assemble(
            device,
            context,
            fns,
            session,
            input_texture,
            caps,
            config,
            lib,
        );

        unsafe { this.initialize()? };
        unsafe { this.create_outputs()? };

        this.ring = Some(Ring {
            input_textures: input_textures.clone(),
            registered_inputs: Vec::with_capacity(ring),
            keyed_mutexes: Vec::with_capacity(ring),
            shared_handles: Vec::with_capacity(ring),
        });
        for tex in &input_textures {
            let registered = unsafe { this.register_texture(tex)? };
            this.ring_mut().registered_inputs.push(registered);
            let resource: IDXGIResource1 = tex
                .cast()
                .context("ring input texture as IDXGIResource1 (is it SHARED?)")?;
            let handle = unsafe {
                resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_RW, PCWSTR::null())
            }
            .context("IDXGIResource1::CreateSharedHandle (ring)")?;
            // SAFETY: a fresh handle nothing else owns.
            let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
            this.ring_mut().shared_handles.push(handle);
            let mutex: IDXGIKeyedMutex = tex
                .cast()
                .context("ring input texture as IDXGIKeyedMutex")?;
            this.ring_mut().keyed_mutexes.push(mutex);
        }

        tprintln!(
            "NVENC H.264 ring encoder initialized (D3D11 / ULL): {}x{}@{}, bitrate_bps={}, ring={}, profile={:?}, rc={}, qp={:?}, intra_refresh={}",
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            ring,
            this.config.profile,
            if this.config.qp.is_some() { "constqp" } else { "cbr" },
            this.config.qp,
            this.config.intra_refresh,
        );
        Ok(this)
    }

    fn new_inner(config: EncoderConfig, shared: bool) -> Result<Self> {
        let (device, context, fns, lib) = load_driver()?;

        let session = SessionGuard::new(
            unsafe { open_session(&fns, device.as_raw())? },
            fns.nvEncDestroyEncoder,
        );
        let caps = EncoderCaps::query(&fns, session.raw());
        caps.log();
        let config = clamp_config_to_caps(config, &caps);
        let input_texture = create_input_texture(&device, config.width, config.height, shared)?;

        let mut this = Self::assemble(
            device,
            context,
            fns,
            session,
            input_texture,
            caps,
            config,
            lib,
        );

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
            this.config.profile,
            if this.config.qp.is_some() { "constqp" } else { "cbr" },
            this.config.qp,
            this.config.intra_refresh,
        );
        Ok(this)
    }

    /// The one place an `Encoder` is built, from a session that has not been initialized yet.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        fns: NV_ENCODE_API_FUNCTION_LIST,
        session: SessionGuard,
        input_texture: ID3D11Texture2D,
        caps: EncoderCaps,
        config: EncoderConfig,
        lib: libloading::Library,
    ) -> Self {
        let single_copy_au = tune::on("nvenc_single_copy_au");
        let rate_budget = tune::candidate("nvenc_rate_budget_observed_fps").then(|| RateBudget {
            window_start: Instant::now(),
            frames: 0,
            applied_fps: config.fps.max(1),
        });
        Self {
            device,
            context,
            fns,
            encoder: session.into_raw(),
            input_texture,
            registered_input: ptr::null_mut(),
            input_keyed_mutex: None,
            shared_handle: None,
            ring: None,
            outputs: Vec::new(),
            inflight: std::collections::VecDeque::new(),
            next_output: 0,
            au_scratch: Vec::new(),
            single_copy_au,
            au_capacity_hint: AU_MIN_CAPACITY,
            rate_budget,
            bitrate_fixed: false,
            initialized: false,
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
        }
    }

    fn ring_mut(&mut self) -> &mut Ring {
        self.ring
            .as_mut()
            .expect("ring_mut is only called after the ring is installed")
    }

    fn setup_shared(&mut self) -> Result<()> {
        let resource: IDXGIResource1 = self
            .input_texture
            .cast()
            .context("input texture as IDXGIResource1 (is it SHARED?)")?;
        let handle =
            unsafe { resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_RW, PCWSTR::null()) }
                .context("IDXGIResource1::CreateSharedHandle")?;
        // recorded before the next fallible step, so `Drop` closes it if that step fails
        // SAFETY: a fresh handle nothing else owns.
        self.shared_handle = Some(unsafe { OwnedHandle::from_raw_handle(handle.0) });
        let mutex: IDXGIKeyedMutex = self
            .input_texture
            .cast()
            .context("input texture as IDXGIKeyedMutex")?;
        self.input_keyed_mutex = Some(mutex);
        Ok(())
    }

    /// Borrowed: the encoder owns the handle and closes it when it is dropped.
    pub fn shared_handle(&self) -> Option<HANDLE> {
        self.shared_handle
            .as_ref()
            .map(|h| HANDLE(h.as_raw_handle()))
    }

    /// Borrowed, as [`Self::shared_handle`].
    pub fn shared_handles(&self) -> Vec<HANDLE> {
        match &self.ring {
            Some(ring) => ring
                .shared_handles
                .iter()
                .map(|h| HANDLE(h.as_raw_handle()))
                .collect(),
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

        if let Err(e) = acquire_key(&mutex, KEY_ENCODER, "ring slot (encode)") {
            // the keyframe this frame was carrying is still owed to the client
            self.idr_requested |= force_idr;
            return Err(SlotEncodeError::Poisoned(e));
        }

        let force_idr = self.take_idr_request(force_idr);
        let res = unsafe { self.encode_mapped_resource(registered, force_idr) };
        if let Err(e) = release_key(&mutex, KEY_WRITER, "ring slot (encode)") {
            // the slot cannot go back to the writer, so the ring must quarantine it; the picture is
            // not sent, so the next one must not reference it
            if res.is_ok() {
                self.frame_index += 1;
            }
            self.idr_requested = true;
            return Err(SlotEncodeError::Poisoned(e));
        }
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

        acquire_key(&mutex, KEY_WRITER, "ring keepalive")?;
        let force_idr = self.take_idr_request(false);
        let res = unsafe { self.encode_mapped_resource(registered, force_idr) };
        self.finish_keyed(&mutex, KEY_WRITER, "ring keepalive", res)
    }

    /// Release an input texture after an encode and settle the frame counter. A failed release
    /// is an error even when the encode worked: the bytes are not sent, so the next picture is
    /// made an IDR rather than predicted from one the client will never see.
    fn finish_keyed(
        &mut self,
        mutex: &IDXGIKeyedMutex,
        key: u64,
        what: &str,
        res: Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let released = release_key(mutex, key, what);
        if res.is_ok() {
            self.frame_index += 1;
        }
        if let Err(e) = released {
            self.idr_requested = true;
            return Err(e);
        }
        res
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
        if let Err(e) = acquire_key(&mutex, KEY_ENCODER, "shared input (encode)") {
            self.idr_requested |= force_idr;
            return Err(e);
        }
        let force_idr = self.take_idr_request(force_idr);
        let res = unsafe { self.encode_mapped(force_idr) };
        self.finish_keyed(&mutex, KEY_WRITER, "shared input (encode)", res)
    }

    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        if let Some(mutex) = self.input_keyed_mutex.clone() {
            if let Err(e) = acquire_key(&mutex, KEY_WRITER, "shared input (repeat)") {
                self.idr_requested |= force_idr;
                return Err(e);
            }
            let force_idr = self.take_idr_request(force_idr);
            let res = unsafe { self.encode_mapped(force_idr) };
            return self.finish_keyed(&mutex, KEY_WRITER, "shared input (repeat)", res);
        }
        let force_idr = self.take_idr_request(force_idr);
        let out = unsafe { self.encode_mapped(force_idr)? };
        self.frame_index += 1;
        Ok(out)
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        if self.bitrate_fixed {
            // already refused once: the pipeline re-arms a failed request every frame, and asking
            // again only fills the log
            return Ok(BitrateApplied::Unsupported);
        }
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;

        // the observed-rate candidate spreads the target over the rate pictures really arrive at
        let effective = match &self.rate_budget {
            Some(rb) => scaled_bitrate(bps, self.config.fps, rb.applied_fps),
            None => bps,
        };
        match unsafe { self.reconfigure_rate(effective) } {
            Ok(()) => {
                tprintln!("NVENC bitrate reconfigured (bitrate_bps={bps}, applied={effective})");
                Ok(BitrateApplied::Applied)
            }
            // tried, not assumed: the cap reads 0 whenever its query fails, so it may only
            // explain a refusal, never pre-empt the attempt
            Err(e) if !self.caps.dyn_bitrate_change => {
                self.bitrate_fixed = true;
                teprintln!(
                    "NVENC: this engine does not advertise NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE \
                     and refused a live bitrate change ({e:#}); the session keeps the bitrate it \
                     was opened with and congestion control cannot shrink this stream"
                );
                Ok(BitrateApplied::Unsupported)
            }
            Err(e) => Err(e),
        }
    }

    /// Change the constant QP from the next picture: a reconfigure with neither `resetEncoder`
    /// nor `forceIDR`, so no keyframe and no new session. `Ok(false)` outside constant-QP mode.
    ///
    /// Unlike the rate budget's reconfigure it does not wait for an empty pipeline: pictures
    /// already submitted keep the QP they went in with, on the async path and with long-term
    /// references open alike (tested).
    pub fn set_qp(&mut self, qp: u8) -> Result<bool> {
        if self.config.qp.is_none() {
            return Ok(false);
        }
        let q = qp.clamp(1, 51) as u32;
        if self.encode_config.rcParams.constQP.qpInterP == q {
            return Ok(true);
        }
        let Some(f) = self.fns.nvEncReconfigureEncoder else {
            return Ok(false);
        };
        let mut reconfig: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconfig.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        let mut new_config = self.encode_config;
        new_config.rcParams.constQP = NV_ENC_QP {
            qpInterP: q,
            qpInterB: q,
            qpIntra: q,
        };
        reconfig.reInitEncodeParams = self.init_params;
        reconfig.reInitEncodeParams.encodeConfig = &mut new_config;
        // SAFETY: `self.encoder` is the open session, and `new_config` outlives the call that
        // reads it through `reInitEncodeParams.encodeConfig`.
        self.check(
            unsafe { f(self.encoder, &mut reconfig) },
            "nvEncReconfigureEncoder (QP)",
        )?;
        self.encode_config = new_config;
        self.init_params = reconfig.reInitEncodeParams;
        self.init_params.encodeConfig = ptr::null_mut();
        Ok(true)
    }

    /// Reconfigure the CBR rate in place: average = max = `bps`, and, where the engine takes a
    /// custom VBV (the same gate `initialize` uses), a one-frame VBV at that rate. Writing the
    /// VBV fields on an engine without the cap fails the reconfigure with `INVALID_PARAM`.
    unsafe fn reconfigure_rate(&mut self, bps: u32) -> Result<()> {
        let Some(f) = self.fns.nvEncReconfigureEncoder else {
            bail!("nvEncReconfigureEncoder is not in this driver's NVENC function table");
        };
        let mut reconfig: NV_ENC_RECONFIGURE_PARAMS = unsafe { zeroed() };
        reconfig.version = NV_ENC_RECONFIGURE_PARAMS_VER;
        let mut new_config = self.encode_config;
        new_config.rcParams.averageBitRate = bps;
        new_config.rcParams.maxBitRate = bps;
        if self.caps.custom_vbv_buf_size {
            new_config.rcParams.vbvBufferSize = bps / self.config.fps.max(1);
            new_config.rcParams.vbvInitialDelay = new_config.rcParams.vbvBufferSize;
        }

        reconfig.reInitEncodeParams = self.init_params;
        reconfig.reInitEncodeParams.encodeConfig = &mut new_config;
        self.check(
            unsafe { f(self.encoder, &mut reconfig) },
            "nvEncReconfigureEncoder",
        )?;

        self.encode_config = new_config;
        self.init_params = reconfig.reInitEncodeParams;
        self.init_params.encodeConfig = ptr::null_mut();
        Ok(())
    }

    /// `nvenc_rate_budget_observed_fps`: count this picture and, once a window has passed,
    /// re-spread the CBR budget over the rate pictures actually arrived at. A no-op when the
    /// candidate is off. Reconfigures only with nothing in flight, at most once a window.
    fn rate_budget_tick(&mut self) {
        let Some(rb) = self.rate_budget.as_mut() else {
            return;
        };
        rb.frames += 1;
        let elapsed = rb.window_start.elapsed();
        if elapsed < RATE_WINDOW {
            return;
        }
        let observed =
            (u64::from(rb.frames) * 1000 / (elapsed.as_millis() as u64).max(1)).min(10_000) as u32;
        rb.frames = 0;
        rb.window_start = Instant::now();
        let applied = rb.applied_fps;
        if self.config.qp.is_some() || self.bitrate_fixed || !self.inflight.is_empty() {
            return;
        }
        let Some(budget) = budget_fps(self.config.fps, observed, applied) else {
            return;
        };
        let bps = scaled_bitrate(self.config.bitrate_bps, self.config.fps, budget);
        match unsafe { self.reconfigure_rate(bps) } {
            Ok(()) => {
                if let Some(rb) = self.rate_budget.as_mut() {
                    rb.applied_fps = budget;
                }
                tprintln!(
                    "NVENC rate budget: pictures arrive at {observed}/s against {} configured; \
                     budget spread over {budget}/s (average {bps} bps for a {} bps target)",
                    self.config.fps,
                    self.config.bitrate_bps
                );
            }
            Err(e) => teprintln!("NVENC rate budget: reconfigure to {bps} bps failed: {e:#}"),
        }
    }

    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    /// Let the pipeline's IDR atomic reach the encoder directly.
    ///
    /// The ring's idle keepalive re-encodes the held slot from inside the encode thread and never
    /// passes through `AsyncEncodeRing::stage`, the only other place the pipeline's `idr_request`
    /// is consumed. Without the flag a PLI on a static desktop produces only another P frame, and
    /// a joining or damaged client stays black until something on the host screen moves.
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

    /// The index the *next* submitted picture will carry in `NV_ENC_PIC_PARAMS::inputTimeStamp`.
    ///
    /// The transport reads it before the submit: an LTR mark is set on the encoder and then
    /// submitted, so the slot's contents are recorded against the index that submit will use.
    pub fn next_frame_index(&self) -> u64 {
        self.frame_index
    }

    /// How many long-term reference slots this encoder has. 0 means the repair API is inert and
    /// the only loss recovery available is a keyframe.
    pub fn ltr_slots(&self) -> u32 {
        self.ltr_slots
    }

    /// Mark the next encoded picture as a long-term reference, and say which slot it takes.
    /// `None` when the encoder has no LTR slots.
    ///
    /// An LTR stays in the DPB until its slot is claimed again, so a later repair frame can point
    /// at it once the transport knows the client holds it. The caller owns the policy (how often
    /// to mark, which slots the client has), because only the transport sees per-frame receipts.
    ///
    /// The mark is consumed by the next submitted picture: set it right before the submit it is
    /// meant for, never leave it standing across frames.
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
    /// The repair after a loss burst: a P frame predicted from a picture the client is known to
    /// hold costs a fraction of an IDR and does not reset the rate controller. Bits outside the
    /// slot count are ignored, and an empty bitmap is a no-op: "reference nothing" is an intra
    /// frame, which the caller should ask for as an IDR.
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
    /// encoder sets to its own monotonic frame counter, so the transport names a lost frame by
    /// the number the pipeline stamped on it. `nvEncInvalidateRefFrames` removes it, and every
    /// picture that referenced it, from the DPB.
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
                    "NVENC: nvEncInvalidateRefFrames(frame={frame_index}) returned {status:?}; \
                     the frame has probably already left the DPB, so this loss needs a keyframe"
                );
                RefInvalidation::Refused
            }
        }
    }

    /// The constant QP the session was actually opened with, `None` under CBR. Read back from
    /// what `initialize` applied, not from the request.
    pub fn constant_qp(&self) -> Option<u8> {
        self.config.qp
    }

    /// What the driver reported for this engine. Read once at session open.
    pub fn caps(&self) -> EncoderCaps {
        self.caps
    }

    /// Whether the session was opened with rolling intra refresh, and whether it asked for a
    /// single slice during the waves. Read from the configuration handed to the driver.
    #[cfg(test)]
    pub(crate) fn intra_refresh_config(&self) -> (bool, bool) {
        let h264 = unsafe { &self.encode_config.encodeCodecConfig.h264Config };
        (
            h264.enableIntraRefresh() == 1,
            h264.singleSliceIntraRefresh() == 1,
        )
    }

    /// How many pictures may be in flight at once. 1 without async support.
    pub fn pipeline_depth(&self) -> usize {
        self.outputs.len()
    }

    /// Submit one ring slot without waiting for it. Pairs with [`Encoder::complete_slot`].
    ///
    /// Lets the pipeline submit N+1 while the hardware is still working on N. The caller pairs
    /// each completed access unit with the capture timestamp and `FrameTiming` of the frame that
    /// produced it: the encoder cannot overlap on its own, because `encode_input_slot` returns
    /// *this* frame's bytes and handing back the previous frame's would silently corrupt A/V sync.
    ///
    /// The slot's keyed mutex stays held at `KEY_ENCODER` until `complete_slot`, so the slot
    /// must not be recycled in between.
    pub fn submit_slot(&mut self, slot: usize, force_idr: bool) -> Result<(), SlotEncodeError> {
        let (mutex, registered) = self.ring_slot(slot)?;
        if let Err(e) = acquire_key(&mutex, KEY_ENCODER, "ring slot (submit)") {
            self.idr_requested |= force_idr;
            return Err(SlotEncodeError::Poisoned(e));
        }

        let force_idr = self.take_idr_request(force_idr);
        match unsafe { self.submit_registered(registered, force_idr) } {
            Ok(()) => {
                self.frame_index += 1;
                Ok(())
            }
            Err(e) => match release_key(&mutex, KEY_WRITER, "ring slot (submit)") {
                Ok(()) => Err(SlotEncodeError::Encode(e)),
                Err(r) => Err(SlotEncodeError::Poisoned(
                    r.context(format!("after: {e:#}")),
                )),
            },
        }
    }

    /// Drain the oldest picture submitted by [`Encoder::submit_slot`] and release its slot.
    pub fn complete_slot(&mut self, slot: usize) -> Result<Vec<u8>, SlotEncodeError> {
        let (mutex, _) = self.ring_slot(slot)?;
        let out = unsafe { self.complete_next() };
        if let Err(e) = release_key(&mutex, KEY_WRITER, "ring slot (complete)") {
            self.idr_requested = true;
            return Err(SlotEncodeError::Poisoned(e));
        }
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

        let Some(f) = self.fns.nvEncGetSequenceParams else {
            bail!("nvEncGetSequenceParams is not in this driver's NVENC function table");
        };
        self.check(
            unsafe { f(self.encoder, &mut payload) },
            "nvEncGetSequenceParams",
        )?;
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
        // `pipeline::is_transient_encode_error` matches the substring "device busy" to shed the
        // frame instead of tearing the session down, and `NV_ENC_ERR_ENCODER_BUSY` lacks it
        let hint = match status {
            NVENCSTATUS::NV_ENC_ERR_ENCODER_BUSY => " [transient: device busy]",
            _ => "",
        };
        bail!("{what} failed: {status:?}{detail}{hint}");
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
                // the VBV is one frame, so an IDR (the only intra content in an infinite GOP) would
                // have to blow it or be quantised hard: a keyframe may spend two frame budgets
                config.rcParams.lowDelayKeyFrameScale = 2;
            }
            // the quality slider is a floor on QP: it can only shrink the stream (no CBR budget
            // spent on an easy picture), so it never fights congestion control the way CONSTQP does
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

        // A continuously rolling wave (`cnt = period - 1`), not a burst: it repairs loss with no
        // keyframe and no bitrate spike, the shape Sunshine ships. Needs the single-slice cap:
        // intra refresh otherwise forces multiple slices, and this codes one slice per picture.
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
            // the cap is only the permission, this bit is the request: without it NVENC goes back
            // to several slices for the duration of every refresh wave
            if tune::on("nvenc_single_slice_intra_refresh") {
                h264.set_singleSliceIntraRefresh(1);
            }
        }
        self.config.intra_refresh = h264.enableIntraRefresh() == 1;

        h264.sliceMode = 0;
        h264.sliceModeData = 0;

        // One reference, no LTR. `maxNumRefFrames` is the SPS `max_num_ref_frames`: NVENC counts
        // only short-term references against it and keeps `ltrNumFrames` on top, and it cannot
        // exceed the level's DPB (four frames at 1080p). A hardware decoder sized from that SPS
        // evicts a picture it was told to keep: flat green, no decode error, no keyframe request.
        // `SE_NVENC_LTR=1` reopens the wide DPB for measurement only; it is wrong until
        // `max_num_ref_frames` is rewritten on the way out (as `amd/sps.rs` does for the VUI).
        let ltr = caps.max_ltr_frames >= 2
            && caps.multiple_ref_frames
            && crate::envcfg::var("SE_NVENC_LTR").as_deref() == Ok("1");
        let refs = if ltr { 4 } else { 1 };
        h264.maxNumRefFrames = refs;
        h264.numRefL0 = if refs >= 4 {
            NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_4
        } else {
            NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1
        };
        // frameIntervalP = 1 means no B frames, so L1 is never consulted
        h264.numRefL1 = NV_ENC_NUM_REF_FRAMES::NV_ENC_NUM_REF_FRAMES_1;

        // opt-in only (above); marked and selected per picture (`ltrMarkFrameIdx` /
        // `ltrUseFrameBitmap`), trust mode is deprecated
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

        // CABAC and the adaptive 8x8 transform are all Main/High add over Baseline here (5-15%
        // bitrate at equal quality on screen content). Both are optional in NVENC, so both are
        // gated: an engine without CABAC gets the requested profile with CAVLC, not another vendor.
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
        // Without this the SPS says nothing about reordering, so a decoder infers the depth from
        // the level and holds min(16, MaxDpbMbs/MBs) frames before showing the first (33-267 ms
        // at 60 fps). libwebrtc rewrites the SPS on the receive path, so on RTP this is insurance.
        // `frameIntervalP = 1` makes the values NVENC derives (reorder 0) correct.
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
        // Async mode makes `nvEncEncodePicture` return once the picture is queued, with a
        // per-output completion event (Windows/WDDM only, hence the cap). It buys a bounded wait,
        // not throughput: a wedged encoder is a named error after 2 s where the synchronous
        // `LockBitstream` blocks inside the driver, and with one NVENC engine the speed is a wash
        // (`nvenc_pipelined_submit_is_not_faster_than_submit_and_drain`).
        self.async_mode = caps.async_encode
            && self.fns.nvEncRegisterAsyncEvent.is_some()
            && !async_disabled_by_env();
        init.enableEncodeAsync = u32::from(self.async_mode);
        init.tuningInfo = tuning;
        init.maxEncodeWidth = self.config.width;
        init.maxEncodeHeight = self.config.height;
        init.encodeConfig = &mut config;
        // `splitEncodeMode` stays at `NV_ENC_SPLIT_AUTO_MODE` (the zero from `zeroed()`): split
        // encoding needs more than one NVENC engine, P1 enables it implicitly where one exists, and
        // forcing it on a single-engine part adds coordination rather than speed. The caps log
        // prints `engines=` (1 on an RTX 3080 Ti Laptop).

        #[cfg(feature = "bench")]
        crate::bench::win_hooks::nvenc(&self.fns, self.encoder, &mut init);
        unsafe {
            let f = self
                .fns
                .nvEncInitializeEncoder
                .expect("nvEncInitializeEncoder present");
            self.check(f(self.encoder, &mut init), "nvEncInitializeEncoder")?;
        }
        self.initialized = true;

        tprintln!(
            "NVENC configured: rc={:?} (qp={:?}), async={}, outputs={}, max_ref_frames={}, \
             ltr={}, intra_refresh={} (period={}, cnt={}), entropy={:?}, vbv={}B, \
             low_delay_kf_scale={}",
            config.rcParams.rateControlMode,
            self.config.qp,
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
        // Null, not `&mut self.encode_config`: a pointer into its own field would make `Encoder`
        // self-referential, and `unsafe impl Send` moves it across threads. Every user points
        // `encodeConfig` at something live before the call.
        self.init_params.encodeConfig = ptr::null_mut();
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
    /// In async mode the event says when the hardware is done, so `LockBitstream` with
    /// `doNotWait` never blocks. In synchronous mode the driver blocks inside `LockBitstream`,
    /// holding the one output buffer for the whole encode.
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
            // recorded first, so `Drop` destroys the buffer if the event's fallible steps fail
            self.outputs.push(Output {
                bitstream: create.bitstreamBuffer,
                event: None,
            });

            if self.async_mode {
                let handle = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
                    .context("CreateEventW for an NVENC completion event")?;
                let mut params: NV_ENC_EVENT_PARAMS = unsafe { zeroed() };
                params.version = NV_ENC_EVENT_PARAMS_VER;
                params.completionEvent = handle.0;
                let registered = unsafe {
                    let f = self
                        .fns
                        .nvEncRegisterAsyncEvent
                        .expect("nvEncRegisterAsyncEvent present");
                    self.check(f(self.encoder, &mut params), "nvEncRegisterAsyncEvent")
                };
                if let Err(e) = registered {
                    // SAFETY: the event was created above and nothing else holds it.
                    let _ = unsafe { CloseHandle(handle) };
                    return Err(e.context(format!("registering completion event {i}")));
                }
                if let Some(out) = self.outputs.last_mut() {
                    out.event = Some(handle);
                }
            }
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

    /// [`Self::drain_next`], and when a picture NVENC accepted is lost on the way out (a wait
    /// timeout or a failed lock), the next one is made an IDR: the lost picture is in the
    /// encoder's DPB but never reaches the client, and RTP shows no gap for the receiver to ask
    /// about, so a P frame predicted from it decodes as garbage.
    unsafe fn complete_next(&mut self) -> Result<Vec<u8>> {
        let out = unsafe { self.drain_next() };
        if out.is_err() {
            self.idr_requested = true;
        }
        out
    }

    /// Hand one registered input resource to the hardware and return without waiting.
    ///
    /// The mapped resource and the output slot stay owned by `inflight` until the matching
    /// `complete_next`: NVENC reads the input and writes the output buffer for the whole encode,
    /// so neither may be reused or unmapped before then.
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
        self.rate_budget_tick();

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

        // an IDR resets the DPB, so a pending mark or use is meaningless against it: take both
        // either way so neither leaks into a later picture, but apply them only to a P frame
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
    /// NVENC maps its bitstream write-combined, so the read costs more than its byte count
    /// suggests, and it happens with the buffer locked, delaying the next picture on the same
    /// slot. So the copy goes into the reused `au_scratch`, or under `nvenc_single_copy_au`
    /// straight into a `Vec` allocated before the lock and sized from the previous access unit;
    /// one that does not fit takes the two-copy path.
    unsafe fn drain_next(&mut self) -> Result<Vec<u8>> {
        let Some(pending) = self.inflight.pop_front() else {
            bail!("NVENC complete_next with nothing in flight");
        };
        let bitstream = self.outputs[pending.output].bitstream;
        let event = self.outputs[pending.output].event;
        let mut direct: Vec<u8> = if self.single_copy_au {
            Vec::with_capacity(self.au_capacity_hint)
        } else {
            Vec::new()
        };

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
        // with a signalled completion event the data is already there, so the driver must not
        // be allowed to block again inside the lock
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

        // inside the lock: one read out of the driver's write-combined mapping, nothing else;
        // `au_scratch` is kept, not handed out, because a fresh `Vec` would allocate right here
        let len = lock.bitstreamSizeInBytes as usize;
        let copied_direct = self.single_copy_au && len <= direct.capacity();
        if copied_direct {
            unsafe {
                ptr::copy_nonoverlapping(
                    lock.bitstreamBufferPtr as *const u8,
                    direct.as_mut_ptr(),
                    len,
                );
                // the first `len` bytes were just written; `len <= capacity` was checked above
                direct.set_len(len);
            }
        } else {
            if self.au_scratch.len() < len {
                self.au_scratch.resize(len, 0);
            }
            unsafe {
                ptr::copy_nonoverlapping(
                    lock.bitstreamBufferPtr as *const u8,
                    self.au_scratch.as_mut_ptr(),
                    len,
                );
            }
        }

        let unlock = unsafe {
            let f = self
                .fns
                .nvEncUnlockBitstream
                .expect("nvEncUnlockBitstream present");
            f(self.encoder, bitstream)
        };
        unsafe { self.unmap(pending.mapped) };
        // the bytes were copied out while the lock was held, so the picture is whole and the
        // client needs it: dropping it would leave a frame in the DPB the client never saw
        if let Err(e) = self.check(unlock, "nvEncUnlockBitstream") {
            teprintln!("NVENC: {e:#}; the access unit was already copied and is kept");
        }
        self.au_capacity_hint = next_au_capacity(len);

        if copied_direct {
            return Ok(direct);
        }
        // outside the lock: one allocation per access unit, because the callers want an owned `Vec`
        Ok(self.au_scratch[..len].to_vec())
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

/// Escape hatch back to the synchronous submit-and-block path: an A/B lever for measuring
/// what async mode is worth, and the workaround if a driver mishandles completion events.
fn async_disabled_by_env() -> bool {
    crate::envcfg::var("SE_NVENC_NO_ASYNC").as_deref() == Ok("1")
}

/// Block on an NVENC completion event. A timeout means the hardware never finished the
/// picture (a driver fault or a TDR, not congestion); the caller treats it as an encode
/// error so the session sheds the frame rather than waiting forever.
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
/// viewport and `server.rs` clamps it only to 2..=16384.
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

/// What the end-of-stream flush in `Drop` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EosFlush {
    /// No flush: the encoder was never initialized, or the table has no `nvEncEncodePicture`.
    Skipped,
    /// `nvEncEncodePicture` refused the EOS picture.
    Refused,
    /// Submitted with no completion event to wait on (synchronous mode).
    Submitted,
    /// Submitted, and its completion event signalled.
    Signalled,
    /// Submitted, and its completion event did not signal within `EOS_FLUSH_TIMEOUT_MS`.
    TimedOut,
}

/// What the last `Encoder` dropped on this thread did at teardown, so a test can assert the
/// order rather than only how long it took.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TeardownRecord {
    pub async_mode: bool,
    /// Pictures still in flight when `Drop` began.
    pub inflight_at_drop: usize,
    pub eos: EosFlush,
    /// Pictures still in flight when the first resource was unregistered.
    pub inflight_at_unregister: usize,
}

#[cfg(test)]
thread_local! {
    pub(crate) static LAST_TEARDOWN: std::cell::Cell<Option<TeardownRecord>> =
        const { std::cell::Cell::new(None) };
}

impl Encoder {
    /// The teardown order `nvEncDestroyEncoder` documents: every submitted picture completed,
    /// the encoder flushed with an end-of-stream picture, and only then are resources
    /// unregistered and freed. Unregistering first could destroy a bitstream buffer the hardware
    /// is still writing.
    unsafe fn drain_and_flush(&mut self) -> EosFlush {
        while !self.inflight.is_empty() {
            if let Err(e) = unsafe { self.drain_next() } {
                teprintln!("NVENC teardown: an in-flight picture did not complete: {e:#}");
            }
        }
        if !self.initialized {
            return EosFlush::Skipped;
        }
        let Some(encode) = self.fns.nvEncEncodePicture else {
            return EosFlush::Skipped;
        };
        let mut pic: NV_ENC_PIC_PARAMS = unsafe { zeroed() };
        pic.version = NV_ENC_PIC_PARAMS_VER;
        pic.encodePicFlags = NV_ENC_PIC_FLAGS::NV_ENC_PIC_FLAG_EOS as u32;
        // nothing is in flight now, so the first output's event is free to carry the flush
        let event = self.outputs.first().and_then(|o| o.event);
        pic.completionEvent = event.map(|h| h.0).unwrap_or(ptr::null_mut());
        let st = unsafe { encode(self.encoder, &mut pic) };
        if st != NVENCSTATUS::NV_ENC_SUCCESS {
            log_drop_status(st, "nvEncEncodePicture (EOS flush)");
            return EosFlush::Refused;
        }
        let Some(h) = event else {
            return EosFlush::Submitted;
        };
        let rv = unsafe { WaitForSingleObject(h, EOS_FLUSH_TIMEOUT_MS) };
        if rv != WAIT_OBJECT_0 {
            teprintln!(
                "NVENC teardown: the end-of-stream flush did not signal within \
                 {EOS_FLUSH_TIMEOUT_MS} ms ({rv:?}); freeing the session anyway"
            );
            return EosFlush::TimedOut;
        }
        EosFlush::Signalled
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            #[cfg(test)]
            let inflight_at_drop = self.inflight.len();
            let _eos = self.drain_and_flush();
            #[cfg(test)]
            LAST_TEARDOWN.with(|r| {
                r.set(Some(TeardownRecord {
                    async_mode: self.async_mode,
                    inflight_at_drop,
                    eos: _eos,
                    inflight_at_unregister: self.inflight.len(),
                }))
            });
            if let Some(ring) = self.ring.take() {
                for &registered in &ring.registered_inputs {
                    if !registered.is_null() {
                        if let Some(f) = self.fns.nvEncUnregisterResource {
                            let st = f(self.encoder, registered);
                            log_drop_status(st, "nvEncUnregisterResource (ring)");
                        }
                    }
                }
                // closes the shared handles
                drop(ring);
            }
            self.shared_handle = None;
            if !self.registered_input.is_null() {
                if let Some(f) = self.fns.nvEncUnregisterResource {
                    let st = f(self.encoder, self.registered_input);
                    log_drop_status(st, "nvEncUnregisterResource");
                }
                self.registered_input = ptr::null_mut();
            }
            // `drain_and_flush` already completed and unmapped everything in flight; this only
            // catches a picture it could not pop
            while let Some(pending) = self.inflight.pop_front() {
                if let Some(f) = self.fns.nvEncUnmapInputResource {
                    log_drop_status(f(self.encoder, pending.mapped), "nvEncUnmapInputResource");
                }
            }
            for out in std::mem::take(&mut self.outputs) {
                if let Some(h) = out.event {
                    // unregister before closing: the driver keeps the handle otherwise
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

/// The NVIDIA device, the driver's NVENC library and its checked function table: everything an
/// encoder needs before it can open a session.
fn load_driver() -> Result<(
    ID3D11Device,
    ID3D11DeviceContext,
    NV_ENCODE_API_FUNCTION_LIST,
    libloading::Library,
)> {
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
    require_functions(&fns)?;
    Ok((device, context, fns, lib))
}

pub(crate) fn create_nvidia_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
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
            tprintln!(
                "DXGI adapter (index={i}, name={name}, vendor=0x{:04x}, is_software={is_software})",
                desc.VendorId
            );
            if is_nvenc_adapter(desc.VendorId, desc.Flags) {
                chosen = Some(adapter);
                tprintln!("selected NVIDIA adapter for NVENC (index={i}, name={name})");
                break;
            }
            i += 1;
        }

        let adapter = chosen.ok_or_else(|| {
            anyhow!("no NVIDIA adapter found (PCI vendor 0x10de); NVENC requires an NVIDIA GPU")
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

/// Two plain 2048x2048 textures for [`super::clock_hold`] to copy between.
pub(crate) fn create_hold_textures(
    device: &ID3D11Device,
) -> Result<(ID3D11Texture2D, ID3D11Texture2D)> {
    Ok((
        create_input_texture(device, 2048, 2048, false)?,
        create_input_texture(device, 2048, 2048, false)?,
    ))
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
            // Consumer drivers cap concurrent NVENC sessions per system (8 on an RTX 3080 Ti Laptop
            // with driver 580.97) and report the cap as `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY` or, on
            // other branches, `OUT_OF_MEMORY`. Found by attempting, never by counting: the limit
            // varies by branch and by what else on the machine is encoding.
            let detail = match st {
                NVENCSTATUS::NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY
                | NVENCSTATUS::NV_ENC_ERR_OUT_OF_MEMORY => {
                    " (the driver's concurrent-session limit: this system already holds as many \
                     NVENC sessions as it allows, so this client encodes on Quick Sync or x264 \
                     instead)"
                }
                // `apiVersion` is a floor: the driver is older than the API these bindings target
                NVENCSTATUS::NV_ENC_ERR_INVALID_VERSION => {
                    " (this build asks for NVENC API 12.1, which the installed NVIDIA driver \
                     does not implement; updating the driver is what fixes it)"
                }
                _ => "",
            };
            return Err(anyhow!("nvEncOpenEncodeSessionEx failed: {st:?}{detail}"));
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
