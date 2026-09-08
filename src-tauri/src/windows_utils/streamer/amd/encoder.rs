//! AMD AMF H.264 encoder backend — the Radeon sibling of `nvidia/encoder.rs`.
//!
//! **North star: minimum glass-to-glass latency.** Where a knob trades a millisecond for a
//! little PSNR, we spend the PSNR. Every configuration decision below has the same shape as the
//! NVENC path's, and the two files should read as siblings.
//!
//! ## Why it looks like this
//!
//! * **`USAGE = ULTRA_LOW_LATENCY`, set first.** AMD: "Setting USAGE you overwrite most of
//!   previous settings including rate control method", so it must precede everything. It is
//!   also the single biggest lever in the whole file: AMF's own encode documentation says
//!   "In the Transcoding mode the encoder needs to accept at least 3 input frames before any
//!   output is produced. In low latency modes output becomes available as soon as the first
//!   submitted frame is encoded" — three frames, 50 ms at 60 fps, decided by one property.
//!   Both existing Rust AMF crates default to `TRANSCODING`.
//! * **ULL has a mandatory fallback.** `USAGE = ULTRA_LOW_LATENCY` breaks `Init()` outright on
//!   pre-RDNA parts from Adrenalin 23.7.1 onward (AMF issue #410). We probe cheaply first
//!   (set USAGE, then read a property back) and, if `Init()` still fails, re-apply the whole
//!   table under `LOW_LATENCY` and try again — the two usages differ only in rate control, VBV
//!   size and `LowLatencyInternal`, all of which we override explicitly anyway.
//! * **One frame of VBV.** `VBVBufferSize = bitrate / fps`. A rate controller that can bank a
//!   second of bits will, and the frame that spends them stalls the link. Mirrors
//!   `rcParams.vbvBufferSize` on the NVENC side; Sunshine's `rc_buffer_size = bitrate/framerate`
//!   and OBS's latency-VBR branch independently land on the same number.
//! * **No B-frames, no pre-encode, no pre-analysis, no VBAQ.** Each is at least one frame of
//!   algorithmic delay or an extra per-frame analysis pass.
//! * **Infinite GOP, keyframes on demand.** `IDRPeriod = i32::MAX` (not 0 — zero's meaning is
//!   undocumented for AVC), with `ForcePictureType = IDR` plus `InsertSPS`/`InsertPPS` on join,
//!   PLI and FIR. Periodic IDRs are bitrate spikes, and a bitrate spike is a latency spike.
//! * **`QueryTimeout = 1` ms, set *before* `Init()`.** It is filed under "dynamic properties" in
//!   the header but is really static: set afterwards it returns `AMF_OK`, echoes back from
//!   `GetProperty`, and does nothing (AMF issue #516). Honoured, it lets `QueryOutput` block for
//!   a millisecond instead of us sleeping — and `Sleep(1)` on Windows is 1–15 ms depending on
//!   timer resolution, which is not a rounding error at 60 fps.
//! * **One frame in flight, one thread.** Submit exactly one surface, drain its packet, release,
//!   repeat. Submitting ahead grows wall-clock latency by (queue depth x frame time) while
//!   throughput still looks perfect — the classic way an encoder integration measures fine and
//!   feels awful. "Nothing yet" is detected by a NULL out-pointer, not by `AMF_REPEAT`: AMD
//!   confirm `AMF_NEED_MORE_INPUT` is not a reliable signal, and FFmpeg never tests either code.
//! * **`MaxNumRefFrames` and `IntraRefreshMBsNumberPerSlot` are left alone.** ULL already turns
//!   rolling intra-refresh on at 255 MBs/slot (~32-frame wave at 1080p, ~0.53 s at 60 fps), and
//!   the docs make intra-refresh conditional on `MaxOfReferenceFrames > 1`, so pinning one
//!   reference would silently switch off free loss resilience for no latency gain.
//! * **One slice per frame.** Multiple slices only buy latency alongside
//!   `OUTPUT_MODE = SLICE`; without it AMF still returns one buffer per frame and the extra
//!   slices are pure compression cost. The NVENC path uses one slice too.
//!
//! ## What is unverified
//!
//! No machine on this project has an AMD GPU. The FFI is fully asserted by
//! `test/amd_layout.rs` against numbers a C compiler measured from the real headers, and the
//! absence path (no `amfrt64.dll`) is tested — but nothing here has ever encoded a frame. See
//! `docs/manual-tests/amd-amf.md` for the script that validates it on a Radeon box in one
//! sitting.

use std::ffi::c_void;
use std::io::Write as _;
use std::ptr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_FORMAT_P010,
    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use super::super::amd_sys::*;
use super::super::nvidia::encoder::{fill_synthetic_bgra, EncoderConfig};
use super::super::scaler::Scaler;
use super::super::{Vendor, AMD_PCI_VENDOR_ID};
use crate::streamer::config::{Config, H264Profile};
use crate::streamer::platform::BitrateApplied;

/// The DXGI format this backend prefers for its own input textures.
///
/// VCN wants NV12, and WGC/DXGI hand us BGRA. Converting is `scaler.rs`'s job — a
/// `VideoProcessor` blit can scale and BT.709 colour-convert in one pass — but `scaler.rs`
/// currently emits BGRA only, and teaching it NV12 belongs to another workstream. Until it
/// lands, the ring is BGRA and AMF runs its own internal converter, which is logged rather than
/// hidden. See `findings/major-amd-nv12-input-pending-scaler.md`.
///
/// The encoder does not depend on this constant being right: if the first frame arrives in a
/// different format it re-initialises the component to match (see `retune_input_format`). This
/// is the format the ring starts in, so that the common case never pays for the retune.
pub const PREFERRED_INPUT_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;

/// Input textures in the ring. One would do under strict one-in-flight discipline; two means a
/// surface whose release the driver defers by a frame can never be overwritten while AMF is
/// still reading it. This is OBS's shape (`CopyResource` into an encoder-owned pool) without
/// OBS's `AMFSurfaceObserver`, which one-in-flight makes unnecessary.
const NUM_IN_FLIGHT: usize = 2;

/// How long a single submit+collect may take before it is reported as a stalled encoder. At
/// 60 fps a healthy frame completes in single-digit milliseconds; this is a hang detector, not
/// a budget.
const SUBMIT_DEADLINE: Duration = Duration::from_millis(2000);

/// Fallback poll interval used only when the driver does not honour `QueryTimeout`.
const POLL_SLEEP: Duration = Duration::from_millis(1);

/// `IDRPeriod`. Sunshine's `gop_size = INT_MAX` with the comment "Use an infinite GOP length
/// since I-frames are generated on demand", and FFmpeg maps `gop_size` straight onto this
/// property. Zero is *not* documented to mean "never" for AVC.
const INFINITE_IDR_PERIOD: i64 = i32::MAX as i64;

/// `MaxOfLTRFrames` accepts 0..=2, further bounded by the level's DPB limit.
pub const MAX_LTR_FRAMES_SUPPORTED: i64 = 2;

// ===========================================================================================
// Capabilities
// ===========================================================================================

/// What `AMFComponent::GetCaps` reported before `Init()`. Logged in full on construction: on a
/// machine none of us can reach, the log line is the diagnosis.
#[derive(Debug, Clone, Copy)]
pub struct AmfCaps {
    pub acceleration: AMF_ACCELERATION_TYPE,
    pub max_bitrate: i64,
    pub num_of_streams: i64,
    pub max_profile: i64,
    pub max_level: i64,
    pub max_reference_frames: i64,
    pub num_hw_instances: i64,
    pub b_frames: bool,
    pub pre_analysis: bool,
    pub query_timeout_support: bool,
    pub slice_output: bool,
    pub color_conversion: AMF_ACCELERATION_TYPE,
    pub max_throughput: i64,
    pub requested_throughput: i64,
    pub width_range: (i32, i32),
    pub height_range: (i32, i32),
    /// `None` when the input format list could not be read at all.
    pub bgra_native: Option<bool>,
    pub nv12_native: Option<bool>,
}

impl AmfCaps {
    fn unknown() -> Self {
        Self {
            acceleration: AMF_ACCEL_NOT_SUPPORTED,
            max_bitrate: 0,
            num_of_streams: 0,
            max_profile: 0,
            max_level: 0,
            max_reference_frames: 0,
            num_hw_instances: 0,
            b_frames: false,
            pre_analysis: false,
            query_timeout_support: false,
            slice_output: false,
            color_conversion: AMF_ACCEL_NOT_SUPPORTED,
            max_throughput: 0,
            requested_throughput: 0,
            width_range: (0, 0),
            height_range: (0, 0),
            bgra_native: None,
            nv12_native: None,
        }
    }

    /// `AMF_ACCEL_HARDWARE` is **0** and `AMF_ACCEL_NOT_SUPPORTED` is **-1**, so this cannot be
    /// written as a truthiness test.
    fn is_accelerated(&self) -> bool {
        self.acceleration == AMF_ACCEL_HARDWARE || self.acceleration == AMF_ACCEL_GPU
    }
}

/// Long-term-reference state. Shipped disabled (`max_frames == 0`): user-controlled LTR is
/// mutually exclusive with the rolling intra-refresh `USAGE = ULTRA_LOW_LATENCY` gives us for
/// free ("With user control of LTR, B-pictures and Intra-refresh features are not supported"),
/// and acting on it needs the WebRTC session to tell the encoder which frames the client
/// acknowledged. The plumbing is here so that change is confined to the caller.
#[derive(Debug, Clone, Copy, Default)]
struct LtrState {
    max_frames: i64,
    /// Set on the next submitted surface, then cleared.
    mark_next: Option<i64>,
    /// Restricts the next frame's prediction to these LTR indices, then cleared.
    force_reference: Option<i64>,
    last_marked: Option<i64>,
    last_referenced: Option<i64>,
}

// ===========================================================================================
// RAII for AMF refcounts
// ===========================================================================================

/// One owned AMF reference, released on drop. `Terminate`-then-`Release` ordering for the
/// component and context is handled explicitly in `Drop for Encoder`; this covers the per-frame
/// surfaces and buffers, where a `?` in the middle of the encode path would otherwise leak.
struct Owned<T>(*mut T);

impl<T> Owned<T> {
    fn as_ptr(&self) -> *mut T {
        self.0
    }
}

impl<T> Drop for Owned<T> {
    fn drop(&mut self) {
        // SAFETY: vtable slot 1 (AMFInterface::Release). Every type wrapped in `Owned` here
        // (AMFSurface, AMFData, AMFBuffer, AMFCaps, AMFIOCaps) begins with the AMFInterface
        // prefix, and we own exactly the one reference being dropped.
        unsafe { release(self.0) }
    }
}

// ===========================================================================================
// The encoder
// ===========================================================================================

pub struct Encoder {
    device: ID3D11Device,
    context: ID3D11DeviceContext,

    /// DLL-owned singleton; never released.
    factory: *mut AMFFactory,
    /// May be null: silencing AMF's console writer is best-effort.
    trace: *mut AMFTrace,
    amf_context: *mut AMFContext,
    component: *mut AMFComponent,

    ring: Vec<ID3D11Texture2D>,
    ring_next: usize,
    last_slot: Option<usize>,
    ring_format: DXGI_FORMAT,
    surface_format: AMF_SURFACE_FORMAT,

    config: EncoderConfig,
    caps: AmfCaps,
    usage: AMF_VIDEO_ENCODER_USAGE,
    rate_control: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
    query_timeout_supported: bool,
    extradata: Vec<u8>,

    frame_index: u64,
    idr_requested: bool,
    /// Whether the last access unit AMF emitted was an IDR, per its `OutputDataType`. Only
    /// `_IDR` marks a stream-startable unit; a plain `_I` does not.
    last_output_idr: bool,
    ltr: LtrState,
    warned_input_full: bool,

    /// Declared **last** so it drops last: dropping the `Library` unmaps the code every vtable
    /// above points into. Same rule as `nvidia/encoder.rs`'s trailing `_lib`.
    amf: Amf,
}

// SAFETY: the AMF objects are owned exclusively by this struct and only ever touched from the
// thread that owns it. The pipeline moves an encoder between threads but never shares one.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Builds an encoder on its own D3D11 device, picked from the first non-software adapter
    /// whose PCI vendor id is AMD's. Used by the probe harness and the CPU-bridge fallback.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let (device, context) = create_amd_d3d11_device()?;
        Self::build(config, device, context, 0)
    }

    /// Builds an encoder on the **capture** device, which is what makes
    /// `CreateSurfaceFromDX11Native` a pointer wrap instead of a cross-device copy. This is the
    /// common AMD case: on a Radeon desktop or a Ryzen APU laptop the display, the capture and
    /// the VCN encoder are all one adapter, so the cross-adapter shared-texture dance the NVENC
    /// path needs on a hybrid laptop disappears entirely.
    pub fn new_on_device(
        config: EncoderConfig,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self> {
        Self::new_on_device_with_ltr(config, device, context, 0)
    }

    /// As [`Self::new_on_device`], reserving `max_ltr_frames` long-term references.
    ///
    /// Non-zero switches the encoder from ULL's rolling intra-refresh to user-controlled LTR:
    /// the two are mutually exclusive. Leave it at 0 until the WebRTC session can report which
    /// frames the client acknowledged.
    pub fn new_on_device_with_ltr(
        config: EncoderConfig,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        max_ltr_frames: i64,
    ) -> Result<Self> {
        if super::super::device_vendor(device) != Vendor::Amd {
            bail!(
                "AMD AMF requires an AMD D3D11 device, but the capture device is not AMD; \
                 a cross-adapter AMF bridge is not implemented"
            );
        }
        Self::build(
            config,
            device.clone(),
            context.clone(),
            max_ltr_frames.clamp(0, MAX_LTR_FRAMES_SUPPORTED),
        )
    }

    fn build(
        config: EncoderConfig,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        max_ltr_frames: i64,
    ) -> Result<Self> {
        // AMF's own DeviceDX11.cpp does this unconditionally, and AMD's fix for flicker plus
        // *growing* latency on a Desktop-Duplication device was "do not use
        // D3D11_CREATE_DEVICE_SINGLETHREADED" (AMF issue #92).
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            // SAFETY: `mt` is a live ID3D10Multithread view of our own device.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }

        let amf = Amf::load()?;

        match amf.query_version() {
            Ok(v) => tprintln!("AMF runtime version {}", format_version(v)),
            Err(e) => teprintln!("AMFQueryVersion failed ({e:#}); continuing without it"),
        }

        // Pinned low on purpose — see AMFINIT_VERSION. The runtime's own AMF_RESULT is the gate,
        // not a version comparison.
        let factory = amf.init_factory(AMFINIT_VERSION)?;

        let trace = silence_amf_trace(factory);

        let mut amf_context: *mut AMFContext = ptr::null_mut();
        // SAFETY: vtable slot 0 (AMFFactory::CreateContext) on the factory AMFInit returned.
        let r = unsafe { ((*(*factory).pVtbl).CreateContext)(factory, &mut amf_context) };
        check(r, "AMFFactory::CreateContext")?;
        if amf_context.is_null() {
            bail!("AMFFactory::CreateContext returned AMF_OK with a null context");
        }

        // From here on every early return must release what has been created, so the struct is
        // assembled first and its Drop impl owns the teardown.
        let mut this = Self {
            device,
            context,
            factory,
            trace,
            amf_context,
            component: ptr::null_mut(),
            ring: Vec::new(),
            ring_next: 0,
            last_slot: None,
            ring_format: PREFERRED_INPUT_FORMAT,
            surface_format: amf_surface_format(PREFERRED_INPUT_FORMAT).unwrap_or(AMF_SURFACE_BGRA),
            config,
            caps: AmfCaps::unknown(),
            usage: AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY,
            rate_control: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN,
            query_timeout_supported: false,
            extradata: Vec::new(),
            frame_index: 0,
            idr_requested: false,
            last_output_idr: false,
            ltr: LtrState {
                max_frames: max_ltr_frames,
                ..LtrState::default()
            },
            warned_input_full: false,
            amf,
        };

        // SAFETY: vtable slot 18 (AMFContext::InitDX11). The device outlives the context: it is
        // a field of the same struct declared before `amf_context`'s teardown in Drop.
        let r = unsafe {
            ((*(*this.amf_context).pVtbl).InitDX11)(
                this.amf_context,
                this.device.as_raw(),
                AMF_DX11_1,
            )
        };
        // A context may legitimately be initialised twice; FFmpeg tolerates the same code.
        if r != AMF_ALREADY_INITIALIZED {
            check(r, "AMFContext::InitDX11").context(
                "handing our D3D11 device to AMF (without it every submit is a hidden copy)",
            )?;
        }

        let mut component: *mut AMFComponent = ptr::null_mut();
        // SAFETY: vtable slot 1 (AMFFactory::CreateComponent) with a NUL-terminated component id.
        let r = unsafe {
            ((*(*factory).pVtbl).CreateComponent)(
                factory,
                this.amf_context,
                codec::VIDEO_ENCODER_VCE_AVC.as_ptr(),
                &mut component,
            )
        };
        check(r, "AMFFactory::CreateComponent(AMFVideoEncoderVCE_AVC)")
            .context("this adapter has no AMF H.264 encoder")?;
        if component.is_null() {
            bail!("CreateComponent returned AMF_OK with a null component");
        }
        this.component = component;

        this.caps = this.probe_caps();
        tprintln!(
            "AMF AVC caps: accel={}, max_bitrate={}, streams={}, hw_instances={}, \
             max_profile={}, max_level={}, max_refs={}, b_frames={}, pre_analysis={}, \
             query_timeout={}, slice_output={}, color_conversion={}, \
             throughput={}/{} MB, input={}x{}..{}x{}, bgra_native={:?}, nv12_native={:?}",
            acceleration_name(this.caps.acceleration),
            this.caps.max_bitrate,
            this.caps.num_of_streams,
            this.caps.num_hw_instances,
            this.caps.max_profile,
            this.caps.max_level,
            this.caps.max_reference_frames,
            this.caps.b_frames,
            this.caps.pre_analysis,
            this.caps.query_timeout_support,
            this.caps.slice_output,
            acceleration_name(this.caps.color_conversion),
            this.caps.requested_throughput,
            this.caps.max_throughput,
            this.caps.width_range.0,
            this.caps.height_range.0,
            this.caps.width_range.1,
            this.caps.height_range.1,
            this.caps.bgra_native,
            this.caps.nv12_native,
        );

        if !this.caps.is_accelerated() {
            bail!(
                "AMF reports acceleration type {} for H.264 on this adapter; refusing to encode \
                 on a non-hardware path (software x264 is the better fallback)",
                acceleration_name(this.caps.acceleration)
            );
        }

        this.build_ring(PREFERRED_INPUT_FORMAT)?;
        this.configure_and_init()?;
        this.read_extradata();

        tprintln!(
            "AMD AMF H.264 encoder initialized (D3D11 / {}): {}x{}@{}, bitrate_bps={}, \
             input={}, profile={:?}, rc={}, qp={:?}, ltr_frames={}, query_timeout={}",
            usage_name(this.usage),
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            surface_format_name(this.surface_format),
            config.profile,
            rate_control_name(this.rate_control),
            config.qp,
            this.ltr.max_frames,
            if this.query_timeout_supported {
                "1ms-blocking"
            } else {
                "poll+sleep"
            },
        );

        Ok(this)
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn caps(&self) -> AmfCaps {
        self.caps
    }

    /// The Annex-B SPS+PPS read back from `ExtraData` after `Init()`, for the out-of-band SDP
    /// path. Empty if the property was unreadable.
    pub fn sequence_params(&self) -> Vec<u8> {
        self.extradata.clone()
    }

    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    pub fn ltr_enabled(&self) -> bool {
        self.ltr.max_frames > 0
    }

    /// Marks the next submitted frame as long-term reference `index`. No-op unless the encoder
    /// was built with `max_ltr_frames > 0`.
    pub fn mark_next_frame_as_ltr(&mut self, index: i64) {
        if self.ltr_enabled() {
            self.ltr.mark_next = Some(index.clamp(0, self.ltr.max_frames - 1));
        }
    }

    /// Restricts the next frame's prediction to the long-term references named in `bitfield`.
    /// This is the loss-recovery hook: on a NACK or PLI, point the next frame at a reference the
    /// client is known to hold instead of paying for a full IDR. Note that AMF also *disallows*
    /// LTRs outside the bitfield from current and future reference.
    pub fn force_ltr_reference(&mut self, bitfield: i64) {
        if self.ltr_enabled() {
            self.ltr.force_reference = Some(bitfield);
        }
    }

    /// The LTR index AMF reported on the last output buffer, if any.
    pub fn last_marked_ltr(&self) -> Option<i64> {
        self.ltr.last_marked
    }

    /// The LTR bitfield AMF reported the last output frame referenced, if any.
    pub fn last_referenced_ltr(&self) -> Option<i64> {
        self.ltr.last_referenced
    }

    /// Whether the last access unit AMF emitted was an IDR, per its `OutputDataType`. The
    /// pipeline decides keyframes by scanning the Annex-B stream, so this is the encoder's own
    /// second opinion — useful when a forced IDR does not appear to have taken.
    pub fn last_output_was_idr(&self) -> bool {
        self.last_output_idr
    }

    /// Live bitrate change. Dynamic property writes take effect at the next `SubmitInput` with
    /// no `ReInit` and no `Flush` — OBS's comment says as much, and a flush would only risk
    /// unaligned IDRs. AMD warn "you cannot change it too often", which is why
    /// `streamer::bitrate::BitrateController` stays the only writer.
    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;

        let fps = self.config.fps.max(1) as i64;
        let bps = bps as i64;
        self.set_i64(prop::AMF_VIDEO_ENCODER_TARGET_BITRATE, bps)?;
        self.set_i64(prop::AMF_VIDEO_ENCODER_PEAK_BITRATE, bps)?;
        self.set_i64(prop::AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, (bps / fps).max(1))?;
        tprintln!("AMF bitrate reconfigured (bitrate_bps={bps})");
        Ok(BitrateApplied::Applied)
    }

    /// GPU path: stage the captured texture into the ring (scaling and/or converting through the
    /// `VideoProcessor` when one is supplied), then wrap and submit it.
    pub fn encode_texture(
        &mut self,
        src: &ID3D11Texture2D,
        scaler: Option<&mut Scaler>,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        // Without a scaler the stage is a CopyResource, which demands identical formats. With
        // one, VideoProcessorBlt converts, so any source format is fine.
        if scaler.is_none() {
            let src_format = texture_format(src);
            if src_format != self.ring_format {
                self.retune_input_format(src_format)?;
            }
        }

        let slot = self.ring_next;
        let dst = self
            .ring
            .get(slot)
            .ok_or_else(|| anyhow!("AMF ring slot {slot} out of range"))?
            .clone();

        match scaler {
            Some(s) => s
                .scale_into(src, &dst)
                .context("scaling the captured frame into the AMF input texture")?,
            None => {
                // SAFETY: both textures are live, same device, same format and same size.
                unsafe { self.context.CopyResource(&dst, src) };
            }
        }
        // AMD's fix for flicker and growing latency on a Desktop-Duplication device is an
        // explicit Flush after the copy, before the encoder reads the texture (AMF issue #92).
        // SAFETY: the immediate context is live and owned by this encoder.
        unsafe { self.context.Flush() };

        self.submit_slot(slot, force_idr)
    }

    /// CPU-bridge path: upload a BGRA frame into the ring, then submit it. Only valid while the
    /// ring is BGRA — the bridge exists precisely because there is no GPU path to the encoder.
    pub fn encode_bgra_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        if self.ring_format != DXGI_FORMAT_B8G8R8A8_UNORM {
            bail!(
                "encode_bgra_padded on an AMF encoder whose input ring is {:?}",
                self.ring_format
            );
        }
        let min_len = (self.config.height as usize) * (row_pitch as usize);
        if (row_pitch as usize) < (self.config.width as usize) * 4 || data.len() < min_len {
            bail!(
                "encode_bgra_padded: need row_pitch >= {} and >= {min_len} bytes (got pitch \
                 {row_pitch}, {} bytes)",
                self.config.width * 4,
                data.len()
            );
        }

        let slot = self.ring_next;
        let dst = self
            .ring
            .get(slot)
            .ok_or_else(|| anyhow!("AMF ring slot {slot} out of range"))?
            .clone();
        // SAFETY: `data` holds at least `height * row_pitch` bytes, checked above, and `dst` is
        // a live default-usage texture of exactly the encoder's dimensions.
        unsafe {
            self.context.UpdateSubresource(
                &dst,
                0,
                None,
                data.as_ptr() as *const c_void,
                row_pitch,
                0,
            );
            self.context.Flush();
        }
        self.submit_slot(slot, force_idr)
    }

    /// Static-desktop repeat: re-submit the texture that is already staged. AMF has no
    /// "encode the previous frame again" call, but the input texture is still valid and still
    /// holds the last picture, so a repeat costs one encode and zero copies.
    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let slot = self
            .last_slot
            .ok_or_else(|| anyhow!("encode_repeat called before the first frame"))?;
        self.submit_slot(slot, force_idr)
    }

    // -------------------------------------------------------------------------------------
    // Submit / collect
    // -------------------------------------------------------------------------------------

    fn submit_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>> {
        let force_idr = force_idr || self.idr_requested;
        let texture = self
            .ring
            .get(slot)
            .ok_or_else(|| anyhow!("AMF ring slot {slot} out of range"))?
            .clone();

        let mut raw: *mut AMFSurface = ptr::null_mut();
        // SAFETY: vtable slot 49 (AMFContext::CreateSurfaceFromDX11Native). The texture is a
        // live ring entry that outlives the surface, and the observer parameter is NULL because
        // one-frame-in-flight answers "is AMF done" by the packet arriving.
        let r = unsafe {
            ((*(*self.amf_context).pVtbl).CreateSurfaceFromDX11Native)(
                self.amf_context,
                texture.as_raw(),
                &mut raw,
                ptr::null_mut(),
            )
        };
        check(r, "AMFContext::CreateSurfaceFromDX11Native")?;
        if raw.is_null() {
            bail!("CreateSurfaceFromDX11Native returned AMF_OK with a null surface");
        }
        let surface = Owned(raw);

        let out = self.submit_surface(surface.as_ptr(), force_idr);

        if out.is_ok() {
            if force_idr {
                self.idr_requested = false;
            }
            self.frame_index += 1;
            self.last_slot = Some(slot);
            self.ring_next = (slot + 1) % self.ring.len().max(1);
        }
        out
    }

    fn submit_surface(&mut self, surface: *mut AMFSurface, force_idr: bool) -> Result<Vec<u8>> {
        let fps = self.config.fps.max(1) as i64;
        let pts = self.frame_index as i64 * (AMF_SECOND / fps);

        // SAFETY: vtable slots 28 (SetFrameType), 19 (SetPts) and 21 (SetDuration) on a live
        // surface we own.
        unsafe {
            ((*(*surface).pVtbl).SetFrameType)(surface, AMF_FRAME_PROGRESSIVE);
            ((*(*surface).pVtbl).SetPts)(surface, pts);
            ((*(*surface).pVtbl).SetDuration)(surface, AMF_SECOND / fps);
            // Input hardware surfaces can be vertically aligned up by 16, and our virtual
            // displays are arbitrary client-driven sizes.
            let _ = ((*(*surface).pVtbl).SetCrop)(
                surface,
                0,
                0,
                self.config.width as i32,
                self.config.height as i32,
            );
        }

        // SAFETY: the surface's first 13 vtable slots are the AMFPropertyStorage prefix.
        let storage = unsafe { as_property_storage(surface) };
        // SAFETY: `storage` is the live surface viewed through its property-storage prefix; all
        // names are NUL-terminated statics.
        unsafe {
            // Access-unit delimiters are bytes no browser decoder needs.
            set_property(
                storage,
                prop::AMF_VIDEO_ENCODER_INSERT_AUD,
                AMFVariantStruct::bool(false),
            );
            if force_idr {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE,
                    AMFVariantStruct::int64(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR as i64),
                );
                // Guarantees the IDR is self-contained for a mid-stream joiner.
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_INSERT_SPS,
                    AMFVariantStruct::bool(true),
                );
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_INSERT_PPS,
                    AMFVariantStruct::bool(true),
                );
            }
            if let Some(index) = self.ltr.mark_next.take() {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX,
                    AMFVariantStruct::int64(index),
                );
            }
            if let Some(bitfield) = self.ltr.force_reference.take() {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD,
                    AMFVariantStruct::int64(bitfield),
                );
            }
        }

        let deadline = Instant::now() + SUBMIT_DEADLINE;
        // Bytes harvested while clearing an AMF_INPUT_FULL. Under one-in-flight this stays
        // empty; if it ever fills, concatenating it ahead of this frame's access unit keeps the
        // Annex-B stream complete and in order rather than dropping a frame on the floor.
        let mut carry: Vec<u8> = Vec::new();

        loop {
            // SAFETY: vtable slot 22 (AMFComponent::SubmitInput). An AMFSurface is an AMFData
            // by prefix, which is what the C API's own signature expects.
            let r = unsafe {
                ((*(*self.component).pVtbl).SubmitInput)(self.component, surface.cast::<AMFData>())
            };
            match r {
                AMF_OK | AMF_NEED_MORE_INPUT => break,
                AMF_INPUT_FULL => {
                    // The surface was NOT consumed — re-submit this same pointer. AMD: a full
                    // input queue means the output is not being drained, so drain it.
                    if !self.warned_input_full {
                        self.warned_input_full = true;
                        teprintln!(
                            "AMF SubmitInput returned AMF_INPUT_FULL under one-frame-in-flight \
                             discipline; draining output and retrying (this should not happen)"
                        );
                    }
                    if let Some(bytes) = self.query_once()? {
                        carry.extend_from_slice(&bytes);
                    } else if !self.query_timeout_supported {
                        std::thread::sleep(POLL_SLEEP);
                    }
                    if Instant::now() >= deadline {
                        bail!("AMF SubmitInput stalled on AMF_INPUT_FULL for {SUBMIT_DEADLINE:?}");
                    }
                }
                _ => {
                    return Err(self.amf_error("AMFComponent::SubmitInput", r));
                }
            }
        }

        loop {
            if let Some(bytes) = self.query_once()? {
                if carry.is_empty() {
                    return Ok(bytes);
                }
                carry.extend_from_slice(&bytes);
                return Ok(carry);
            }
            if Instant::now() >= deadline {
                bail!(
                    "AMF QueryOutput produced nothing for {SUBMIT_DEADLINE:?} after a successful \
                     SubmitInput (encoder stalled)"
                );
            }
            // Only sleep if the driver does not wait inside QueryOutput for us.
            if !self.query_timeout_supported {
                std::thread::sleep(POLL_SLEEP);
            }
        }
    }

    /// One `QueryOutput`. Returns the access unit, or `None` for "nothing yet", recording
    /// whether it was an IDR in `last_output_idr`.
    ///
    /// "Nothing yet" is decided by the NULL out-pointer, not by a result code: `QueryOutput` can
    /// return `AMF_OK` with a null `ppData`, and neither `AMF_REPEAT` nor `AMF_NEED_MORE_INPUT`
    /// is a reliable signal. Both FFmpeg and OBS test the pointer.
    fn query_once(&mut self) -> Result<Option<Vec<u8>>> {
        let mut raw: *mut AMFData = ptr::null_mut();
        // SAFETY: vtable slot 23 (AMFComponent::QueryOutput) on a live initialised component.
        let r = unsafe { ((*(*self.component).pVtbl).QueryOutput)(self.component, &mut raw) };

        if raw.is_null() {
            return match r {
                AMF_OK | AMF_REPEAT | AMF_NEED_MORE_INPUT => Ok(None),
                AMF_EOF => Ok(None),
                _ => Err(self.amf_error("AMFComponent::QueryOutput", r)),
            };
        }
        let data = Owned(raw);

        // SAFETY: vtable slot 2 (AMFInterface::QueryInterface) on a live AMFData; on success we
        // own one reference to the buffer view, wrapped in `Owned` immediately.
        let buffer: *mut AMFBuffer = unsafe { query_interface(data.as_ptr(), &IID_AMF_BUFFER) }
            .map_err(|e| self.amf_error("QueryInterface(IID_AMFBuffer) on the output", e))?;
        let buffer = Owned(buffer);

        // SAFETY: vtable slots 25 (GetNative) and 24 (GetSize) on a live buffer. The native
        // pointer is host memory holding the Annex-B access unit and stays valid until the
        // buffer is released, which `Owned` does at the end of this scope.
        let bytes = unsafe {
            let native = ((*(*buffer.as_ptr()).pVtbl).GetNative)(buffer.as_ptr());
            let size = ((*(*buffer.as_ptr()).pVtbl).GetSize)(buffer.as_ptr());
            if native.is_null() || size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(native as *const u8, size).to_vec()
            }
        };
        if bytes.is_empty() {
            return Ok(None);
        }

        // SAFETY: the buffer's first 13 vtable slots are the AMFPropertyStorage prefix.
        let storage = unsafe { as_property_storage(buffer.as_ptr()) };
        // Only `_IDR` marks a stream-startable access unit; a plain `_I` does not, and for a
        // WHEP joiner that distinction is a working join versus a green screen.
        // SAFETY: `storage` is the live output buffer viewed through its storage prefix.
        let is_idr = unsafe { get_property(storage, prop::AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE) }
            .ok()
            .and_then(|v| v.as_i64())
            .map(|v| v == AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR as i64)
            .unwrap_or(false);

        if self.ltr_enabled() {
            // SAFETY: same live storage view.
            unsafe {
                self.ltr.last_marked =
                    get_property(storage, prop::AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX)
                        .ok()
                        .and_then(|v| v.as_i64())
                        .filter(|v| *v >= 0);
                self.ltr.last_referenced = get_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD,
                )
                .ok()
                .and_then(|v| v.as_i64());
            }
        }

        self.last_output_idr = is_idr;
        Ok(Some(bytes))
    }

    // -------------------------------------------------------------------------------------
    // Configuration
    // -------------------------------------------------------------------------------------

    /// Applies the latency property table and calls `Init()`, falling back from
    /// `ULTRA_LOW_LATENCY` to `LOW_LATENCY` if either the usage probe or `Init()` says no.
    fn configure_and_init(&mut self) -> Result<()> {
        let usage = if self.probe_ultra_low_latency() {
            AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY
        } else {
            teprintln!(
                "AMF: ULTRA_LOW_LATENCY usage rejected by this driver/GPU; falling back to \
                 LOW_LATENCY (see AMF issue #410 — pre-RDNA parts on Adrenalin 23.7.1+)"
            );
            AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY
        };

        self.apply_properties(usage)?;
        match self.init_component() {
            Ok(()) => {
                self.usage = usage;
            }
            Err(e) if usage == AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY => {
                teprintln!(
                    "AMF Init() failed under ULTRA_LOW_LATENCY ({e:#}); retrying with \
                     LOW_LATENCY"
                );
                // USAGE re-defaults most of the table, so the whole thing is re-applied rather
                // than patched.
                self.apply_properties(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY)?;
                self.init_component()
                    .context("AMF Init() failed under both ULTRA_LOW_LATENCY and LOW_LATENCY")?;
                self.usage = AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY;
            }
            Err(e) => return Err(e),
        }

        // Read back what actually stuck. On GCN parts (RX 580, Vega) RateControlMethod reportedly
        // cannot be overridden after ULL at all — LatencyVBR with a one-frame VBV is a fine
        // outcome, but believing we are in CBR when we are not is what makes bitrate control
        // look broken.
        self.rate_control = self
            .get_i64(prop::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD)
            .unwrap_or(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN as i64)
            as AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD;
        let usage_readback = self
            .get_i64(prop::AMF_VIDEO_ENCODER_USAGE)
            .unwrap_or(self.usage as i64);
        if usage_readback != self.usage as i64 {
            teprintln!(
                "AMF: Usage read back as {} after setting {}",
                usage_name(usage_readback as AMF_VIDEO_ENCODER_USAGE),
                usage_name(self.usage)
            );
            self.usage = usage_readback as AMF_VIDEO_ENCODER_USAGE;
        }
        Ok(())
    }

    /// AMD's own cheap probe: set `USAGE`, then read a property back. If the read errors, the
    /// usage did not take. Cheaper than Sunshine's full open-retry, and it runs first.
    fn probe_ultra_low_latency(&self) -> bool {
        if self
            .set_i64(
                prop::AMF_VIDEO_ENCODER_USAGE,
                AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY as i64,
            )
            .is_err()
        {
            return false;
        }
        self.get_i64(prop::AMF_VIDEO_ENCODER_PREENCODE_ENABLE)
            .is_some()
    }

    fn apply_properties(&self, usage: AMF_VIDEO_ENCODER_USAGE) -> Result<()> {
        let cfg = &self.config;
        let fps = cfg.fps.max(1) as i64;
        let bitrate = cfg.bitrate_bps.max(1) as i64;

        // 1. USAGE first. Everything below depends on it having already re-defaulted the encoder.
        self.set_i64(prop::AMF_VIDEO_ENCODER_USAGE, usage as i64)
            .context("setting AMF Usage")?;

        // 2. Fastest VCN algorithm path. Note the value is codec-specific: SPEED is 1 on AVC,
        //    10 on HEVC and 100 on AV1.
        self.set_i64(
            prop::AMF_VIDEO_ENCODER_QUALITY_PRESET,
            AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED as i64,
        )?;

        // 3/4. Size and rate. A wrong fps mis-sizes both the VBV and the per-frame bit budget.
        self.set_var(
            prop::AMF_VIDEO_ENCODER_FRAMESIZE,
            AMFVariantStruct::size(cfg.width as i32, cfg.height as i32),
        )?;
        self.set_var(
            prop::AMF_VIDEO_ENCODER_FRAMERATE,
            AMFVariantStruct::rate(cfg.fps.max(1), 1),
        )?;

        // 5. Profile, matching the NVENC mapping.
        let profile = match cfg.profile {
            H264Profile::Baseline => AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE,
            H264Profile::Main => AMF_VIDEO_ENCODER_PROFILE_MAIN,
            H264Profile::High => AMF_VIDEO_ENCODER_PROFILE_HIGH,
        };
        self.set_i64(prop::AMF_VIDEO_ENCODER_PROFILE, profile as i64)?;
        // ProfileLevel is deliberately left at AMF's default: our clients are browsers, which
        // are lenient about levels, and pinning one low enough for every device would cap
        // resolution.

        // 6..10. Rate control. Peak == target so no frame can balloon and head-of-line-block the
        // next; VBV is one frame of bits, the single biggest CBR latency lever.
        if let Some(q) = cfg.qp {
            let q = q as i64;
            self.set_i64(
                prop::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP as i64,
            )?;
            self.set_i64(prop::AMF_VIDEO_ENCODER_QP_I, q)?;
            self.set_i64(prop::AMF_VIDEO_ENCODER_QP_P, q)?;
        } else {
            self.set_i64(
                prop::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR as i64,
            )?;
            self.set_i64(prop::AMF_VIDEO_ENCODER_TARGET_BITRATE, bitrate)?;
            self.set_i64(prop::AMF_VIDEO_ENCODER_PEAK_BITRATE, bitrate)?;
            self.set_i64(
                prop::AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE,
                (bitrate / fps).max(1),
            )?;
            // The quality slider is a floor on QP, i.e. a ceiling on quality: it stops the
            // encoder spending the whole CBR budget on an easy picture. It can only shrink the
            // stream, so it never fights congestion control the way CONSTANT_QP does. Same
            // meaning as `enableMinQP` on NVENC and `rc.i_qp_min` on x264.
            if let Some(q) = cfg.min_qp.filter(|&q| q > 1) {
                self.set_i64(prop::AMF_VIDEO_ENCODER_MIN_QP, q as i64)?;
            }
        }
        // InitialVBVBufferFullness is left at its default of 64 (= 100%). Zero would mean an
        // *empty* buffer, which is the worst case for a rate controller that then has to fill it.

        // 11/12. HRD conformance obeys a buffer model our receiver is not; filler pads bytes
        // that cost transmit time on our link.
        self.set_bool(prop::AMF_VIDEO_ENCODER_ENFORCE_HRD, false)?;
        self.set_bool(prop::AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE, false)?;

        // 13/14. No B-frames. A B-frame needs a future frame, hence at least one frame of
        // reorder delay. Non-negotiable. Consecutive-B is only touched when the hardware admits
        // to having B-frames at all — VCN 1.0–2.x, RDNA1 included, do not.
        self.set_i64(prop::AMF_VIDEO_ENCODER_B_PIC_PATTERN, 0)?;
        if self.caps.b_frames {
            let _ = self.set_i64(prop::AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES, 0);
        }

        // 15. ULL sets this itself; writing it explicitly is belt and braces and not every
        // driver has the property.
        self.set_optional_bool(prop::AMF_VIDEO_ENCODER_LOWLATENCY_MODE, true);

        // 16..19. Every form of lookahead and per-frame analysis, off.
        self.set_i64(
            prop::AMF_VIDEO_ENCODER_PREENCODE_ENABLE,
            AMF_VIDEO_ENCODER_PREENCODE_DISABLED as i64,
        )?;
        self.set_bool(prop::AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE, false)?;
        self.set_bool(prop::AMF_VIDEO_ENCODER_ENABLE_VBAQ, false)?;
        self.set_optional_bool(
            prop::AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE,
            false,
        );

        // 20..22. Infinite GOP; keyframes on demand only. Header spacing 0 because in-band
        // parameter sets ride the per-surface InsertSPS/InsertPPS on forced IDRs instead of
        // being sprayed every N frames.
        self.set_i64(prop::AMF_VIDEO_ENCODER_IDR_PERIOD, INFINITE_IDR_PERIOD)?;
        self.set_i64(prop::AMF_VIDEO_ENCODER_INTRA_PERIOD, 0)?;
        self.set_i64(prop::AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING, 0)?;

        // 23/24. Deblocking is free on the encoder and improves what the receiver shows. One
        // slice per frame: more slices only pay off with OUTPUT_MODE = SLICE, which needs the
        // RTP packetizer to emit partial access units.
        self.set_bool(prop::AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER, true)?;
        self.set_i64(prop::AMF_VIDEO_ENCODER_SLICES_PER_FRAME, 1)?;
        self.set_optional_i64(
            prop::AMF_VIDEO_ENCODER_OUTPUT_MODE,
            AMF_VIDEO_ENCODER_OUTPUT_MODE_FRAME as i64,
        );

        // 25. Baseline forces CAVLC for a cheaper client-side decode, matching NVENC.
        let coding = match cfg.profile {
            H264Profile::Baseline => AMF_VIDEO_ENCODER_CALV,
            _ => AMF_VIDEO_ENCODER_CABAC,
        };
        self.set_i64(prop::AMF_VIDEO_ENCODER_CABAC_ENABLE, coding as i64)?;

        // 26. We feed full-range colour and the NVENC path signals videoFullRangeFlag = 1; keep
        // the two vendors consistent or AMD hosts look washed out next to NVIDIA ones.
        // `FullRangeColor` is the only colour property AMF actually writes into an AVC
        // bitstream — the three below are set anyway in case a future driver honours them, and
        // their failure is logged once rather than fatal.
        self.set_optional_bool(prop::AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR, true);
        self.set_optional_bool(prop::AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR, true);
        self.set_optional_i64(
            prop::AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES,
            COLOR_PRIMARIES_BT709,
        );
        self.set_optional_i64(
            prop::AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
            TRANSFER_CHARACTERISTIC_BT709,
        );

        // 27. SmartAccess Video parallelises *throughput* across VCN instances; splitting one
        // low-latency stream across engines adds coordination, not speed.
        self.set_optional_bool(prop::AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO, false);

        // 28. The default 16-deep input queue is a latency reservoir we never want to fill. With
        // one-in-flight it stays empty anyway, but a hard 1 turns AMF_INPUT_FULL into a real
        // signal instead of a 16-frame cushion. It may be read-only; failure is fine.
        self.set_optional_i64(prop::AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE, 1);

        // 29. Intra-refresh: leave ULL's default (255 MBs/slot) alone, and write 0 only when the
        // config explicitly disables it. MaxNumRefFrames is deliberately never set — forcing 1
        // reference would switch intra-refresh off, because the docs make it conditional on
        // MaxOfReferenceFrames > 1.
        if !cfg.intra_refresh {
            self.set_optional_i64(prop::AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT, 0);
        }

        // 30. LTR is mutually exclusive with intra-refresh and stays off by default.
        if self.ltr.max_frames > 0 {
            self.set_i64(prop::AMF_VIDEO_ENCODER_MAX_LTR_FRAMES, self.ltr.max_frames)?;
        }

        // 31. QueryTimeout, last of the static block and *before* Init: set afterwards it
        // returns AMF_OK, echoes back, and silently does nothing.
        self.set_optional_i64(prop::AMF_VIDEO_ENCODER_QUERY_TIMEOUT, 1);

        Ok(())
    }

    fn init_component(&mut self) -> Result<()> {
        // FFmpeg's exact recipe: write 1 ms, read it back, and trust the read — not the write —
        // for whether QueryOutput will block for us.
        let echoed = self
            .get_i64(prop::AMF_VIDEO_ENCODER_QUERY_TIMEOUT)
            .unwrap_or(0);
        self.query_timeout_supported = echoed != 0;
        if self.query_timeout_supported != self.caps.query_timeout_support {
            tprintln!(
                "AMF QueryTimeout: caps report support={}, the property read back as {}; \
                 trusting the read-back",
                self.caps.query_timeout_support,
                echoed
            );
        }

        // SAFETY: vtable slot 17 (AMFComponent::Init) on a live, configured component.
        let r = unsafe {
            ((*(*self.component).pVtbl).Init)(
                self.component,
                self.surface_format,
                self.config.width as i32,
                self.config.height as i32,
            )
        };
        check(r, "AMFComponent::Init").with_context(|| {
            format!(
                "initializing the AMF H.264 encoder at {}x{} in {}",
                self.config.width,
                self.config.height,
                surface_format_name(self.surface_format)
            )
        })
    }

    /// Reads the Annex-B SPS+PPS out of `ExtraData`, which is only populated after `Init()`.
    fn read_extradata(&mut self) {
        // SAFETY: the component's first 13 vtable slots are the AMFPropertyStorage prefix.
        let storage = unsafe { as_property_storage(self.component) };
        // SAFETY: `storage` is the live component viewed through that prefix.
        let Ok(var) = (unsafe { get_property(storage, prop::AMF_VIDEO_ENCODER_EXTRADATA) }) else {
            return;
        };
        let Some(iface) = var.as_interface() else {
            return;
        };
        // The variant handed us one reference; wrap it so it is released either way.
        let owned = Owned(iface.cast::<AMFBuffer>());
        // SAFETY: vtable slot 2 (AMFInterface::QueryInterface) on the interface the variant
        // carried; on success the buffer view is a second reference we also own.
        let Ok(buffer) =
            (unsafe { query_interface::<AMFBuffer, AMFBuffer>(owned.as_ptr(), &IID_AMF_BUFFER) })
        else {
            return;
        };
        let buffer = Owned(buffer);
        // SAFETY: vtable slots 25/24 on a live buffer whose memory outlives this copy.
        self.extradata = unsafe {
            let native = ((*(*buffer.as_ptr()).pVtbl).GetNative)(buffer.as_ptr());
            let size = ((*(*buffer.as_ptr()).pVtbl).GetSize)(buffer.as_ptr());
            if native.is_null() || size == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(native as *const u8, size).to_vec()
            }
        };
        tprintln!(
            "AMF SPS/PPS ready (extradata_bytes={})",
            self.extradata.len()
        );
    }

    fn probe_caps(&self) -> AmfCaps {
        let mut caps = AmfCaps::unknown();

        let mut raw: *mut AMFCaps = ptr::null_mut();
        // SAFETY: vtable slot 26 (AMFComponent::GetCaps). Documented and demonstrated to work
        // before Init() — this is exactly what OBS's preset-compatibility check does.
        let r = unsafe { ((*(*self.component).pVtbl).GetCaps)(self.component, &mut raw) };
        if r != AMF_OK || raw.is_null() {
            teprintln!(
                "AMFComponent::GetCaps failed ({}); continuing without a capability report",
                result_name(r)
            );
            return caps;
        }
        let owned = Owned(raw);
        let handle = owned.as_ptr();

        // SAFETY: vtable slot 13 (AMFCaps::GetAccelerationType) on a live caps object.
        caps.acceleration = unsafe { ((*(*handle).pVtbl).GetAccelerationType)(handle) };

        // SAFETY: AMFCaps starts with the AMFPropertyStorage prefix, so caps are read as
        // properties; every name below is a NUL-terminated static.
        let storage = unsafe { as_property_storage(handle) };
        let read_i64 = |name: &[u16]| -> Option<i64> {
            // SAFETY: `storage` is the live caps object viewed through its storage prefix.
            unsafe { get_property(storage, name) }
                .ok()
                .and_then(|v| v.as_i64())
        };
        caps.max_bitrate = read_i64(prop::AMF_VIDEO_ENCODER_CAP_MAX_BITRATE).unwrap_or(0);
        caps.num_of_streams = read_i64(prop::AMF_VIDEO_ENCODER_CAP_NUM_OF_STREAMS).unwrap_or(0);
        caps.max_profile = read_i64(prop::AMF_VIDEO_ENCODER_CAP_MAX_PROFILE).unwrap_or(0);
        caps.max_level = read_i64(prop::AMF_VIDEO_ENCODER_CAP_MAX_LEVEL).unwrap_or(0);
        caps.max_reference_frames =
            read_i64(prop::AMF_VIDEO_ENCODER_CAP_MAX_REFERENCE_FRAMES).unwrap_or(0);
        caps.num_hw_instances =
            read_i64(prop::AMF_VIDEO_ENCODER_CAP_NUM_OF_HW_INSTANCES).unwrap_or(0);
        caps.b_frames = read_i64(prop::AMF_VIDEO_ENCODER_CAP_BFRAMES).unwrap_or(0) != 0;
        caps.pre_analysis = read_i64(prop::AMF_VIDEO_ENCODER_CAP_PRE_ANALYSIS).unwrap_or(0) != 0;
        caps.query_timeout_support =
            read_i64(prop::AMF_VIDEO_ENCODER_CAP_QUERY_TIMEOUT_SUPPORT).unwrap_or(0) != 0;
        caps.slice_output =
            read_i64(prop::AMF_VIDEO_ENCODER_CAP_SUPPORT_SLICE_OUTPUT).unwrap_or(0) != 0;
        caps.color_conversion = read_i64(prop::AMF_VIDEO_ENCODER_CAP_COLOR_CONVERSION)
            .unwrap_or(AMF_ACCEL_NOT_SUPPORTED as i64)
            as AMF_ACCELERATION_TYPE;
        caps.max_throughput = read_i64(prop::AMF_VIDEO_ENCODER_CAP_MAX_THROUGHPUT).unwrap_or(0);
        caps.requested_throughput =
            read_i64(prop::AMF_VIDEO_ENCODER_CAP_REQUESTED_THROUGHPUT).unwrap_or(0);

        let mut io_raw: *mut AMFIOCaps = ptr::null_mut();
        // SAFETY: vtable slot 14 (AMFCaps::GetInputCaps) on a live caps object.
        let r = unsafe { ((*(*handle).pVtbl).GetInputCaps)(handle, &mut io_raw) };
        if r == AMF_OK && !io_raw.is_null() {
            let io = Owned(io_raw);
            let io = io.as_ptr();
            // SAFETY: vtable slots 3/4/6/7 on a live AMFIOCaps; all out-parameters are locals.
            unsafe {
                let (mut min_w, mut max_w) = (0i32, 0i32);
                let (mut min_h, mut max_h) = (0i32, 0i32);
                ((*(*io).pVtbl).GetWidthRange)(io, &mut min_w, &mut max_w);
                ((*(*io).pVtbl).GetHeightRange)(io, &mut min_h, &mut max_h);
                caps.width_range = (min_w, max_w);
                caps.height_range = (min_h, max_h);

                let count = ((*(*io).pVtbl).GetNumOfFormats)(io);
                let mut bgra = None;
                let mut nv12 = None;
                for i in 0..count {
                    let mut fmt: AMF_SURFACE_FORMAT = AMF_SURFACE_UNKNOWN;
                    let mut native: amf_bool = 0;
                    if ((*(*io).pVtbl).GetFormatAt)(io, i, &mut fmt, &mut native) != AMF_OK {
                        continue;
                    }
                    match fmt {
                        AMF_SURFACE_BGRA => bgra = Some(native != 0),
                        AMF_SURFACE_NV12 => nv12 = Some(native != 0),
                        _ => {}
                    }
                }
                caps.bgra_native = bgra;
                caps.nv12_native = nv12;
            }
        }

        caps
    }

    // -------------------------------------------------------------------------------------
    // Input ring
    // -------------------------------------------------------------------------------------

    fn build_ring(&mut self, format: DXGI_FORMAT) -> Result<()> {
        let mut ring = Vec::with_capacity(NUM_IN_FLIGHT);
        for _ in 0..NUM_IN_FLIGHT {
            ring.push(create_input_texture(
                &self.device,
                self.config.width,
                self.config.height,
                format,
            )?);
        }
        self.ring = ring;
        self.ring_next = 0;
        self.last_slot = None;
        self.ring_format = format;
        self.surface_format = amf_surface_format(format).ok_or_else(|| {
            anyhow!("no AMF surface format for DXGI format {format:?}; AMF cannot encode it")
        })?;

        if self.surface_format != AMF_SURFACE_NV12 {
            // Not fatal — AMF accepts packed RGB — but AMD state that BGRA input silently
            // invokes an internal GPU colour converter which competes with the foreground
            // application, so it is a pass we neither control nor measure.
            teprintln!(
                "AMF input is {} rather than NV12: AMF's internal colour converter is active \
                 (accel={}). Once scaler.rs can emit NV12 this becomes a single VideoProcessor \
                 pass.",
                surface_format_name(self.surface_format),
                acceleration_name(self.caps.color_conversion),
            );
        }
        Ok(())
    }

    /// Rebuilds the ring and re-initialises the component for a new input format. Runs at most
    /// once per session, when the pipeline turns out to hand us something other than
    /// [`PREFERRED_INPUT_FORMAT`] — which is exactly what happens the day `scaler.rs` starts
    /// emitting NV12.
    fn retune_input_format(&mut self, format: DXGI_FORMAT) -> Result<()> {
        if amf_surface_format(format).is_none() {
            bail!("the pipeline handed AMF a {format:?} texture, which AMF cannot encode");
        }
        tprintln!(
            "AMF re-initializing for a {:?} input (was {:?})",
            format,
            self.ring_format
        );

        // SAFETY: vtable slot 19 (AMFComponent::Terminate) on a live component. Nothing is in
        // flight: this runs before the frame is staged.
        let r = unsafe { ((*(*self.component).pVtbl).Terminate)(self.component) };
        check(r, "AMFComponent::Terminate (input format retune)")?;

        self.build_ring(format)?;
        self.configure_and_init()?;
        self.read_extradata();
        self.idr_requested = true;
        Ok(())
    }

    // -------------------------------------------------------------------------------------
    // Property helpers
    // -------------------------------------------------------------------------------------

    fn set_var(&self, name: &[u16], value: AMFVariantStruct) -> Result<()> {
        // SAFETY: the component's first 13 vtable slots are the AMFPropertyStorage prefix, and
        // `name` is a NUL-terminated static.
        let r = unsafe { set_property(as_property_storage(self.component), name, value) };
        if r == AMF_OK {
            Ok(())
        } else {
            Err(self.amf_error(&format!("SetProperty({})", wide_to_string(name)), r))
        }
    }

    fn set_i64(&self, name: &[u16], v: i64) -> Result<()> {
        self.set_var(name, AMFVariantStruct::int64(v))
    }

    fn set_bool(&self, name: &[u16], v: bool) -> Result<()> {
        self.set_var(name, AMFVariantStruct::bool(v))
    }

    /// A knob that varies by driver and GPU generation. `AMF_NOT_FOUND` / `AMF_NOT_SUPPORTED`
    /// here means "this build of the runtime or this VCN does not have it", which is a log line,
    /// never a failure.
    fn set_optional_i64(&self, name: &[u16], v: i64) {
        if let Err(e) = self.set_i64(name, v) {
            tprintln!("AMF optional property not applied: {e:#}");
        }
    }

    fn set_optional_bool(&self, name: &[u16], v: bool) {
        if let Err(e) = self.set_bool(name, v) {
            tprintln!("AMF optional property not applied: {e:#}");
        }
    }

    fn get_i64(&self, name: &[u16]) -> Option<i64> {
        // SAFETY: as `set_var`.
        unsafe { get_property(as_property_storage(self.component), name) }
            .ok()
            .and_then(|v| v.as_i64())
    }

    /// Formats an `AMF_RESULT` with AMD's own message when `AMFTrace::GetResultText` is bound,
    /// the way the NVENC path uses `nvEncGetLastErrorString`.
    ///
    /// The class hint is load-bearing, not decoration: `pipeline::is_transient_encode_error`
    /// looks for the substring "device busy" to decide whether to drop the frame and carry on
    /// or tear the session down, so a transient AMF result has to say it in those words.
    fn amf_error(&self, what: &str, r: AMF_RESULT) -> anyhow::Error {
        let hint = match classify(r) {
            AmfErrorClass::Transient => " [transient: device busy]",
            AmfErrorClass::Recoverable => {
                " [recoverable: encoder device lost, rebuild the session]"
            }
            AmfErrorClass::Fatal => "",
        };
        match self.result_text(r) {
            Some(d) => anyhow!("{what} failed: {} ({r}) - {d}{hint}", result_name(r)),
            None => anyhow!("{what} failed: {} ({r}){hint}", result_name(r)),
        }
    }

    fn result_text(&self, r: AMF_RESULT) -> Option<String> {
        if self.trace.is_null() {
            return None;
        }
        // SAFETY: vtable slot 18 (AMFTrace::GetResultText) on the trace singleton the factory
        // handed us. It returns a static wide string owned by the runtime.
        let p = unsafe { ((*(*self.trace).pVtbl).GetResultText)(self.trace, r) };
        if p.is_null() {
            return None;
        }
        // SAFETY: `p` points at a NUL-terminated wide string inside amfrt64.dll's own data.
        let s = unsafe { wide_ptr_to_string(p) };
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Terminate down the ownership chain, then release up. Releasing the context before
        // terminating the component, or unloading the library while any AMF object is alive,
        // frees the code the vtables point into. `amf` is the last field, so the Library goes
        // last on its own.
        if !self.component.is_null() {
            // SAFETY: vtable slot 20 (Drain) then 23 (QueryOutput) then 19 (Terminate) then 1
            // (Release) on a component we own. Drain is best-effort: this path also runs after
            // a construction failure, where the component may never have been initialised.
            unsafe {
                let drain = ((*(*self.component).pVtbl).Drain)(self.component);
                if drain == AMF_OK {
                    let deadline = Instant::now() + SUBMIT_DEADLINE;
                    loop {
                        let mut data: *mut AMFData = ptr::null_mut();
                        let r = ((*(*self.component).pVtbl).QueryOutput)(self.component, &mut data);
                        if !data.is_null() {
                            release(data);
                        }
                        if r == AMF_EOF || Instant::now() >= deadline {
                            break;
                        }
                        if data.is_null() {
                            std::thread::sleep(POLL_SLEEP);
                        }
                    }
                }
                let r = ((*(*self.component).pVtbl).Terminate)(self.component);
                // AMF_NOT_INITIALIZED is expected when construction failed between
                // CreateComponent and Init - a normal fall-through, not a teardown bug.
                if r != AMF_OK && r != AMF_NOT_INITIALIZED {
                    teprintln!(
                        "AMFComponent::Terminate returned {} during encoder drop",
                        result_name(r)
                    );
                }
                release(self.component);
            }
            self.component = ptr::null_mut();
        }

        if !self.amf_context.is_null() {
            // SAFETY: vtable slot 13 (AMFContext::Terminate) then 1 (Release) on a context we
            // own, after the component that used it is gone.
            unsafe {
                let r = ((*(*self.amf_context).pVtbl).Terminate)(self.amf_context);
                if r != AMF_OK {
                    teprintln!(
                        "AMFContext::Terminate returned {} during encoder drop",
                        result_name(r)
                    );
                }
                release(self.amf_context);
            }
            self.amf_context = ptr::null_mut();
        }

        // The factory is a DLL-owned singleton and AMFTrace has no Release slot at all; neither
        // is ours to free.
        self.factory = ptr::null_mut();
        self.trace = ptr::null_mut();
    }
}

// ===========================================================================================
// Free functions
// ===========================================================================================

/// BT.709 in `AMF_COLOR_PRIMARIES_ENUM` / `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM`
/// (components/ColorSpace.h). Set for completeness; AMF is reported not to write any of the
/// `Out*` colour properties into an AVC bitstream, which is why `FullRangeColor` carries the
/// signalling that actually reaches the client.
const COLOR_PRIMARIES_BT709: i64 = 1;
const TRANSFER_CHARACTERISTIC_BT709: i64 = 1;

fn usage_name(u: AMF_VIDEO_ENCODER_USAGE) -> &'static str {
    match u {
        AMF_VIDEO_ENCODER_USAGE_TRANSCODING => "TRANSCODING",
        AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY => "ULTRA_LOW_LATENCY",
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY => "LOW_LATENCY",
        AMF_VIDEO_ENCODER_USAGE_WEBCAM => "WEBCAM",
        AMF_VIDEO_ENCODER_USAGE_HIGH_QUALITY => "HIGH_QUALITY",
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY_HIGH_QUALITY => "LOW_LATENCY_HIGH_QUALITY",
        _ => "unknown",
    }
}

/// The AMF surface format for a DXGI format, or `None` if AMF has no equivalent.
pub fn amf_surface_format(f: DXGI_FORMAT) -> Option<AMF_SURFACE_FORMAT> {
    match f {
        DXGI_FORMAT_NV12 => Some(AMF_SURFACE_NV12),
        DXGI_FORMAT_B8G8R8A8_UNORM => Some(AMF_SURFACE_BGRA),
        DXGI_FORMAT_R8G8B8A8_UNORM => Some(AMF_SURFACE_RGBA),
        DXGI_FORMAT_P010 => Some(AMF_SURFACE_P010),
        _ => None,
    }
}

fn texture_format(tex: &ID3D11Texture2D) -> DXGI_FORMAT {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `tex` is a live texture and `desc` is a live local.
    unsafe { tex.GetDesc(&mut desc) };
    desc.Format
}

fn wide_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// # Safety
/// `p` must point at a NUL-terminated UTF-16 string that stays valid for the call.
unsafe fn wide_ptr_to_string(p: *const u16) -> String {
    let mut len = 0usize;
    // SAFETY: the caller guarantees a NUL terminator within a reasonable distance; the bound
    // keeps a corrupt pointer from walking the address space.
    unsafe {
        while len < 1024 && *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

/// Silences AMF's default console writer and lowers its global level, so AMF diagnostics cannot
/// bypass `logbus` and interleave with `attach_console`'s output on a GUI-subsystem binary.
/// Returns the trace singleton (or null) so `GetResultText` stays available for error messages.
fn silence_amf_trace(factory: *mut AMFFactory) -> *mut AMFTrace {
    let mut trace: *mut AMFTrace = ptr::null_mut();
    // SAFETY: vtable slot 5 (AMFFactory::GetTrace) on the factory AMFInit returned.
    let r = unsafe { ((*(*factory).pVtbl).GetTrace)(factory, &mut trace) };
    if r != AMF_OK || trace.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: vtable slots 4 (EnableWriter) and 2 (SetGlobalLevel) on the trace singleton.
    // AMFTrace is not an AMFInterface and is never released.
    unsafe {
        ((*(*trace).pVtbl).EnableWriter)(trace, trace_writer::CONSOLE.as_ptr(), 0);
        ((*(*trace).pVtbl).SetGlobalLevel)(trace, AMF_TRACE_ERROR);
    }
    trace
}

/// Input texture for the ring. `SHADER_RESOURCE` is mandatory: `CreateSurfaceFromDX11Native`
/// does not migrate or re-flag a texture, and one lacking the bind flag fails with
/// `AMF_DIRECTX_FAILED` at the first `QueryOutput` rather than at wrap time.
fn create_input_texture(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut texture: Option<ID3D11Texture2D> = None;
    // SAFETY: `desc` is a live local and `texture` a live out-parameter.
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .with_context(|| format!("CreateTexture2D (AMF input, {format:?})"))?;
    }
    texture.context("CreateTexture2D returned no texture")
}

/// Picks an AMD adapter by **PCI vendor id**, not by description string: descriptions are
/// localized and OEM-rebranded, and WARP's contains none of the vendor names while reporting
/// Microsoft's `0x1414`. Software adapters are rejected outright.
fn create_amd_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // SAFETY: DXGI enumeration and D3D11 device creation; every out-parameter is a live local.
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
                "DXGI adapter (index={i}, name={name}, vendor_id=0x{:04X}, is_software={is_software})",
                desc.VendorId
            );
            if !is_software && desc.VendorId == AMD_PCI_VENDOR_ID {
                chosen = Some(adapter);
                tprintln!("selected AMD adapter for AMF (index={i}, name={name})");
                break;
            }
            i += 1;
        }

        let adapter = chosen
            .ok_or_else(|| anyhow!("no AMD adapter found; AMF requires an AMD Radeon/Ryzen GPU"))?;

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
        .context("D3D11CreateDevice on AMD adapter")?;

        let device = device.context("D3D11CreateDevice returned no device")?;
        let context = context.context("D3D11CreateDevice returned no context")?;
        Ok((device, context))
    }
}

/// `--probe-encode --encoder amd`: encode a synthetic pattern to an Annex-B file.
pub fn probe_encode(config: &Config, path: &str) -> Result<()> {
    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FPS: u32 = 60;
    const FRAMES: u32 = 300;
    const BITRATE: u32 = 10_000_000;

    tprintln!(
        "encoding synthetic pattern to Annex-B via AMD AMF: path={path}, \
         {WIDTH}x{HEIGHT}@{FPS}, {FRAMES} frames"
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
    let mut total_bytes = 0usize;
    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, WIDTH, HEIGHT, i);
        let au = encoder
            .encode_bgra_padded(&frame, WIDTH * 4, i == 0)
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
