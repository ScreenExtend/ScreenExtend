use std::ffi::c_void;
use std::io::Write as _;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::ptr;

use anyhow::{anyhow, bail, Context as _, Result};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{HMODULE, LUID};
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
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIKeyedMutex, IDXGIResource1,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
};

use super::super::keyed::{self, Acquired};
use super::super::nvidia::encoder::EncoderConfig;
use super::super::tune;
use super::intel_sys::*;
use crate::streamer::config::H264Profile;
use crate::streamer::platform::BitrateApplied;

/// How long a `MFX_WRN_DEVICE_BUSY` retry loop may run before giving the frame up. A wall-clock
/// budget, not a retry count: 50 ms is three frames at 60 fps, and past that the frame is stale
/// and better shed than delivered late.
const DEVICE_BUSY_BUDGET: std::time::Duration = std::time::Duration::from_millis(50);
const INTEL_FPS_QUERY_FALLBACK: u32 = 60;

/// Slice size to ask oneVPL for, in bytes: one RTP payload. 1200 is the IPv6-safe payload the
/// WebRTC packetizer works to, minus room for the RTP header and the FU-A/STAP-A indicator, so
/// one lost packet damages one slice and no slice is fragmented across packets.
const SLICE_BYTE_TARGET: u32 = 1150;

/// The fallback slice layout when `MaxSliceSize` is refused: roughly one slice per 256 rows,
/// bounded to 4..=8. A single slice per picture makes any loss cost the whole frame, and more
/// than eight costs header overhead for no extra resilience at these resolutions.
fn slice_count_for_height(height: u32) -> u16 {
    (height / 256).clamp(4, 8) as u16
}

/// The `ScenarioInfo` hint and the `AdaptiveCQM` setting that has to go with it.
///
/// `REMOTE_GAMING` on a `LowPower` session makes the runtime default `AdaptiveCQM` to on, which
/// writes four extra PPS (ids 1 to 4, custom scaling lists) beside PPS 0 in every access unit
/// and switches between them per picture. Decoders that keep only the first PPS of a keyframe
/// cannot decode that, so it is turned off. Both are `tune` switches: `qsv_remote_gaming` off is
/// `DISPLAY_REMOTING`, `qsv_adaptive_cqm_off` off leaves the runtime's default.
fn scenario_options() -> (u16, u16) {
    let scenario = if tune::on("qsv_remote_gaming") {
        MFX_SCENARIO_REMOTE_GAMING
    } else {
        MFX_SCENARIO_DISPLAY_REMOTING
    };
    let cqm = if tune::on("qsv_adaptive_cqm_off") {
        MFX_CODINGOPTION_OFF
    } else {
        MFX_CODINGOPTION_UNKNOWN
    };
    (scenario, cqm)
}

/// `RepeatPPS`. The runtime writes SPS and PPS with every IDR whatever this says; on, it also
/// writes the PPS into every other frame, which the packetizer then sends as one extra packet
/// per frame for nothing a receiver needs. `SE_TUNE_OFF=qsv_pps_on_idr_only` puts it back.
fn repeat_pps() -> u16 {
    if tune::on("qsv_pps_on_idr_only") {
        MFX_CODINGOPTION_OFF
    } else {
        MFX_CODINGOPTION_ON
    }
}

/// Text every error carries once this encoder can no longer be used and has to be rebuilt.
///
/// The pipeline classifies encode errors by their text (`is_transient_encode_error` looks for
/// "device busy"), so a device loss needs a stable phrase of its own to be told apart from a
/// one-frame failure. [`needs_rebuild`] is the test.
pub(crate) const REBUILD_MARKER: &str = "Quick Sync encoder must be rebuilt";

/// Whether an error from this encoder means the session is gone (device lost, GPU hang, an
/// abandoned bridge mutex) rather than one frame being lost.
pub(crate) fn needs_rebuild(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains(REBUILD_MARKER)
}

/// The statuses after which the session cannot be used again. oneVPL documents `GPU_HANG` as
/// "close and re-initialise", and `DEVICE_LOST` / `DEVICE_FAILED` as a dead device.
fn is_device_loss_status(st: mfxStatus) -> bool {
    matches!(
        st,
        MFX_ERR_DEVICE_LOST | MFX_ERR_DEVICE_FAILED | MFX_ERR_GPU_HANG
    )
}

/// How long one `SyncOperation` call waits, and how long the frame may wait in total.
const SYNC_SLICE_MS: u32 = 100;
const SYNC_CAP_MS: u32 = 2_000;

/// How long a task left running by a sync timeout may keep the encoder unusable before it is
/// declared lost. Every call in between sheds its frame as "device busy" after a zero-wait check.
const IN_FLIGHT_GIVE_UP: std::time::Duration = std::time::Duration::from_secs(10);

/// What `ENCODE_Query` said to change before `Init`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct QueryFallback {
    /// VCM refused: use CBR and drop the VCM-only fields.
    pub cbr: bool,
    /// `MaxSliceSize` refused: use `NumSlice` instead.
    pub num_slice: bool,
}

/// Decide the rate-control and slice fallbacks from one `ENCODE_Query`.
///
/// `Err` means "give up on Quick Sync" and carries why. A field the runtime cannot correct comes
/// back zeroed under a hard `MFX_ERR_UNSUPPORTED`, which is how a VDEnc part without VCM or
/// without slice-byte control answers. With `fallback_on_unsupported` (the
/// `qsv_query_unsupported_fallback` switch) that status is read for the same two fields instead
/// of failing the whole tier.
pub(crate) fn query_fallback(
    status: mfxStatus,
    constant_qp: bool,
    out_rc: u16,
    out_max_slice: u32,
    fallback_on_unsupported: bool,
) -> std::result::Result<QueryFallback, String> {
    let hard_error = status < MFX_ERR_NONE;
    if hard_error && !(status == MFX_ERR_UNSUPPORTED && fallback_on_unsupported) {
        return Err(format!(
            "MFXVideoENCODE_Query rejected the encode parameters: {} (mfxStatus={status})",
            status_name(status)
        ));
    }
    let fb = QueryFallback {
        cbr: !constant_qp && out_rc != MFX_RATECONTROL_VCM,
        num_slice: out_max_slice != SLICE_BYTE_TARGET,
    };
    if hard_error && fb == QueryFallback::default() {
        return Err(format!(
            "MFXVideoENCODE_Query rejected the encode parameters: {} (mfxStatus={status}), and \
             neither the rate control nor the slice size is what it refused",
            status_name(status)
        ));
    }
    Ok(fb)
}

/// The rate-control fields of `mfxInfoMFX` (and `WinBRCMaxAvgKbps`), which are 16-bit kilobit
/// counts scaled by `BRCParamMultiplier`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Brc {
    /// 0 = not used (the runtime reads it as 1).
    pub mult: u16,
    pub target_kbps: u16,
    pub max_kbps: u16,
    pub buffer_kb: u16,
}

/// The highest value any of the 16-bit rate fields is given. 65000, not 65535, on purpose: it
/// is the 65 Mbps clamp `brc_fields` applies without a multiplier.
const BRC_FIELD_MAX: u32 = 65_000;

/// Fill the 16-bit rate fields for `target_bps` / `max_bps` / a `buffer_kb` of CPB, at `Init`.
///
/// Without a multiplier everything above 65 Mbps is clamped to 65 Mbps. With `use_multiplier`
/// (the `qsv_brc_multiplier` switch) a rate that does not fit is expressed as
/// `value * BRCParamMultiplier`; one that fits leaves the multiplier at 0. The multiplier is an
/// `Init`-time choice: on oneVPL 2.7 (UHD Graphics) a `Reset` that changes it is answered with a
/// warning and the runtime keeps the old one, so `set_bitrate` works in the existing units
/// ([`brc_fields_at`]).
pub(crate) fn brc_fields(
    target_bps: u32,
    max_bps: u32,
    buffer_kb: u32,
    use_multiplier: bool,
) -> Brc {
    let target = (target_bps / 1000).max(1);
    let max = (max_bps / 1000).max(target);
    let buffer = buffer_kb.max(1);
    let biggest = max.max(buffer);
    if !use_multiplier || biggest <= BRC_FIELD_MAX {
        let target_kbps = target.min(BRC_FIELD_MAX) as u16;
        return Brc {
            mult: 0,
            target_kbps,
            max_kbps: max.clamp(target_kbps as u32, BRC_FIELD_MAX) as u16,
            buffer_kb: buffer.min(BRC_FIELD_MAX) as u16,
        };
    }
    let mult = biggest.div_ceil(BRC_FIELD_MAX);
    Brc {
        mult: mult.min(u16::MAX as u32) as u16,
        target_kbps: (target / mult).clamp(1, BRC_FIELD_MAX) as u16,
        max_kbps: max.div_ceil(mult).clamp(1, BRC_FIELD_MAX) as u16,
        buffer_kb: buffer.div_ceil(mult).clamp(1, BRC_FIELD_MAX) as u16,
    }
}

/// The rate fields in units of an existing multiplier `mult` (0 or 1 = plain kbps), for `Reset`.
/// The flag is true when the rate did not fit and was clamped to `65000 * mult` kbps.
pub(crate) fn brc_fields_at(
    target_bps: u32,
    max_bps: u32,
    buffer_kb: u32,
    mult: u16,
) -> (Brc, bool) {
    let m = u32::from(mult.max(1));
    let target = (target_bps / 1000).max(1);
    let max = (max_bps / 1000).max(target);
    let buffer = buffer_kb.max(1);
    let clamped = max.div_ceil(m) > BRC_FIELD_MAX;
    let target_kbps = (target / m).clamp(1, BRC_FIELD_MAX) as u16;
    (
        Brc {
            mult,
            target_kbps,
            max_kbps: max.div_ceil(m).clamp(target_kbps as u32, BRC_FIELD_MAX) as u16,
            buffer_kb: buffer.div_ceil(m).clamp(1, BRC_FIELD_MAX) as u16,
        },
        clamped,
    )
}

/// Whether the fallback parameter set gets its own `ENCODE_Query` before `Init`.
///
/// Only when the first `Query` was a hard error (which reaches a fallback only through the
/// `qsv_query_unsupported_fallback` switch). A fallback chosen from a warning goes straight to
/// `Init`: asking again would add a failure path to a configuration `Init` usually accepts.
pub(crate) fn requery_fallback_before_init(query_status: mfxStatus, fb: QueryFallback) -> bool {
    query_status < MFX_ERR_NONE && fb != QueryFallback::default()
}

/// The `(BufferSizeInKB, InitialDelayInKB)` pairs a refused `Reset` is retried with, in order,
/// all in the units of the session's `BRCParamMultiplier`.
///
/// `new_cpb` is the one-frame CPB the new rate asks for; `old_cpb` / `old_delay` are what the
/// session holds now. Smallest CPB first, because a CPB larger than one frame is queueing delay
/// on a fixed-rate link, and the full old CPB only as the last resort:
/// 1. the new CPB, the initial delay left to the runtime (0, what `Init` sent);
/// 2. the new CPB, the initial delay cut down to fit inside it;
/// 3. a CPB just large enough to hold the old initial delay, if that is still below the old CPB;
/// 4. the old CPB and initial delay unchanged.
///
/// Duplicates are dropped, so each pair is asked once.
pub(crate) fn reset_retry_ladder(new_cpb: u16, old_cpb: u16, old_delay: u16) -> Vec<(u16, u16)> {
    let new_cpb = new_cpb.max(1);
    let candidates = [
        (new_cpb, 0),
        (new_cpb, old_delay.min(new_cpb)),
        (old_delay.max(new_cpb).min(old_cpb.max(new_cpb)), old_delay),
        (old_cpb.max(1), old_delay),
    ];
    let mut out: Vec<(u16, u16)> = Vec::with_capacity(candidates.len());
    for c in candidates {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// How many frames of buffering a CPB of `cpb_units` (times `mult`, 0 meaning 1) holds at
/// `bps` and `fps`. One is the design point; more is latency a fixed-rate link queues.
pub(crate) fn cpb_frames(cpb_units: u16, mult: u16, bps: u32, fps: u32) -> f64 {
    let cpb_bits = f64::from(cpb_units) * f64::from(mult.max(1)) * 1024.0 * 8.0;
    let frame_bits = f64::from(bps.max(1)) / f64::from(fps.max(1));
    cpb_bits / frame_bits
}

/// Test-only stand-ins for runtime answers a VCM-capable Quick Sync part never gives.
#[cfg(test)]
pub(crate) mod test_hooks {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static REFUSE_VCM: Cell<bool> = const { Cell::new(false) };
    }

    /// Make encoders built on this thread see what a VDEnc part without VCM or slice-byte control
    /// answers: `ENCODE_Query` returns `MFX_ERR_UNSUPPORTED` with both fields zeroed. Everything
    /// after that first answer (the second `Query`, `Init`, `Reset`, the encodes) is the real
    /// runtime's.
    pub(crate) fn set_refuse_vcm(on: bool) {
        REFUSE_VCM.with(|c| c.set(on));
    }

    fn refusing() -> bool {
        REFUSE_VCM.with(|c| c.get())
    }

    pub(super) fn refuse_vcm_query(
        status: mfxStatus,
        out: &mut mfxVideoParam,
        out_co2: &mut mfxExtCodingOption2,
    ) -> mfxStatus {
        if !refusing() || unsafe { out.info.mfx.RateControlMethod } != MFX_RATECONTROL_VCM {
            return status;
        }
        out.info.mfx.RateControlMethod = 0;
        out_co2.MaxSliceSize = 0;
        MFX_ERR_UNSUPPORTED
    }

    pub(super) fn refuse_vcm_fps_query(status: mfxStatus, vcm: bool) -> mfxStatus {
        if refusing() && vcm {
            MFX_ERR_UNSUPPORTED
        } else {
            status
        }
    }
}

/// The lowest rate the fps search asks about; if even this is refused the probe itself failed.
const FPS_PROBE_LOW: u32 = 30;

/// The highest frame rate in `FPS_PROBE_LOW..=requested` that `supported` accepts, or `None`
/// when `FPS_PROBE_LOW` itself is refused. Assumes the accepted rates form one run from the
/// bottom, which is what a level limit produces.
pub(crate) fn search_max_fps(
    requested: u32,
    mut supported: impl FnMut(u32) -> bool,
) -> Option<u32> {
    let requested = requested.max(1);
    if supported(requested) {
        return Some(requested);
    }
    if requested <= FPS_PROBE_LOW || !supported(FPS_PROBE_LOW) {
        return None;
    }
    let (mut lo, mut hi, mut best) = (FPS_PROBE_LOW + 1, requested - 1, FPS_PROBE_LOW);
    while lo <= hi {
        let mid = lo + (hi - lo) / 2;
        if supported(mid) {
            best = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    Some(best)
}

/// Per-stage timings for the same-adapter path, so the full-frame `CopySubresourceRegion` into
/// the VPP input surface can be told apart from the VPP pass. Bounded ring buffers written on
/// the encode thread; they allocate once and never grow.
#[derive(Default)]
struct StageTiming {
    get_surface: Samples,
    copy_in: Samples,
    vpp: Samples,
}

/// A fixed-capacity sample ring. Sorting happens only in `median_us`, which the encode thread
/// never calls (the tests do).
struct Samples {
    buf: Vec<u32>,
    next: usize,
}

impl Default for Samples {
    fn default() -> Self {
        Self {
            buf: Vec::with_capacity(Self::CAP),
            next: 0,
        }
    }
}

impl Samples {
    const CAP: usize = 256;

    fn push(&mut self, d: std::time::Duration) {
        let us = d.as_micros().min(u32::MAX as u128) as u32;
        if self.buf.len() < Self::CAP {
            self.buf.push(us);
        } else {
            self.buf[self.next] = us;
            self.next = (self.next + 1) % Self::CAP;
        }
    }

    fn median_us(&self) -> u128 {
        if self.buf.is_empty() {
            return 0;
        }
        let mut v = self.buf.clone();
        v.sort_unstable();
        v[v.len() / 2] as u128
    }
}

/// How the picture is cut into slices, once the runtime has had its say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SliceMode {
    /// `mfxExtCodingOption2::MaxSliceSize` honoured: slices are bounded in bytes.
    MaxBytes(u32),
    /// `MaxSliceSize` refused; `mfxInfoMFX::NumSlice` slices per picture instead.
    FixedCount(u16),
}
const KEY_WRITER: u64 = 0;
const KEY_ENCODER: u64 = 1;
const KEY_TIMEOUT_MS: u32 = 1000;
const DXGI_SHARED_RESOURCE_RW: u32 = 0x8000_0000 | 0x1; // `DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE`

/// The wait between `MFX_WRN_DEVICE_BUSY` retries. Busy usually clears in microseconds, and even
/// a 1 ms sleep is about a thousand times that, so: spin first, then yield the rest of the
/// timeslice, and only sleep once it is clear the GPU is genuinely backed up.
struct DeviceBusyBackoff {
    attempts: u32,
    what: &'static str,
    started: std::time::Instant,
}

impl DeviceBusyBackoff {
    const SPIN_ATTEMPTS: u32 = 64;
    const YIELD_ATTEMPTS: u32 = 256;

    fn new(what: &'static str) -> Self {
        Self {
            attempts: 0,
            what,
            started: std::time::Instant::now(),
        }
    }

    /// Wait once. `Err` when the budget is exhausted.
    fn wait(&mut self) -> Result<()> {
        self.attempts += 1;
        let elapsed = self.started.elapsed();
        if elapsed >= DEVICE_BUSY_BUDGET {
            bail!(
                "{}: device busy for {:.1} ms over {} retries",
                self.what,
                elapsed.as_secs_f64() * 1e3,
                self.attempts
            );
        }
        if self.attempts <= Self::SPIN_ATTEMPTS {
            std::hint::spin_loop();
        } else if self.attempts <= Self::YIELD_ATTEMPTS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_micros(200));
        }
        Ok(())
    }

    /// Report only when the wait was long enough to have cost a frame, so a contended iGPU can
    /// be told from a fast one.
    fn finish(self) {
        if self.attempts > Self::SPIN_ATTEMPTS {
            let ms = self.started.elapsed().as_secs_f64() * 1e3;
            if ms >= 1.0 {
                teprintln!(
                    "oneVPL: {} waited {ms:.1} ms on MFX_WRN_DEVICE_BUSY ({} retries)",
                    self.what,
                    self.attempts
                );
            }
        }
    }
}

/// Whether to `Flush()` the encoder context after copying into the VPP input surface.
///
/// On by default. The copy and the oneVPL VPP submit go to the same immediate context, so the
/// flush adds no ordering, but it lets the GPU start the copy while the CPU is still setting up
/// the VPP call. Measured at 1280x720 on Intel UHD Graphics: p50 6971 us with the flush against
/// 7393 us without, p95 and p99 level. `SE_INTEL_NO_PER_FRAME_FLUSH=1` turns it off.
fn per_frame_flush_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::var("SE_INTEL_NO_PER_FRAME_FLUSH").as_deref() != Ok("1"))
}

/// Make a D3D11 immediate context safe to touch from more than one thread. A failure is an
/// error, because `unsafe impl Send for Encoder` moves the context across threads regardless.
fn set_multithread_protected(context: &ID3D11DeviceContext, what: &str) -> Result<()> {
    let mt: ID3D11Multithread = context
        .cast()
        .with_context(|| format!("{what}: context does not expose ID3D11Multithread"))?;
    // returns the previous setting, not a status, so the read back below is the only way to
    // know it took
    let _previous = unsafe { mt.SetMultithreadProtected(true) };
    if !unsafe { mt.GetMultithreadProtected() }.as_bool() {
        bail!("{what}: SetMultithreadProtected(true) did not take");
    }
    Ok(())
}

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
    /// Closed on drop, on every path out of `new` included.
    _shared_handle: OwnedHandle,
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
        // SAFETY: a fresh handle nothing else owns.
        let owned = unsafe { OwnedHandle::from_raw_handle(shared_handle.0) };
        let enc_mutex: IDXGIKeyedMutex = enc_tex
            .cast()
            .context("bridge texture as IDXGIKeyedMutex (encode side)")?;

        let cap_device1: ID3D11Device1 = cap_device
            .cast()
            .context("capture device as ID3D11Device1")?;
        let cap_tex: ID3D11Texture2D = unsafe { cap_device1.OpenSharedResource1(shared_handle) }
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
            _shared_handle: owned,
        })
    }

    unsafe fn upload(&self, src: &ID3D11Texture2D, w: u32, h: u32) -> Result<()> {
        acquire_bridge_key(&self.cap_mutex, KEY_WRITER, "writer")?;
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
        let released = unsafe { self.cap_mutex.ReleaseSync(KEY_ENCODER) };
        copy?;
        released.with_context(|| format!("Quick Sync bridge ReleaseSync(writer); {REBUILD_MARKER}"))
    }
}

/// What a bridge `AcquireSync` outcome means for the frame.
///
/// Both halves of the bridge run on the encode thread, one after the other, and each releases the
/// key the other waits for before the other asks. So a timeout cannot be a slow peer: it means the
/// key sequence is already out of step (a previous half failed between its acquire and release),
/// and every later frame would wait the full `KEY_TIMEOUT_MS` on it. An abandoned mutex, by the
/// DXGI contract, means the surface must be recreated. Either way nothing may touch the texture or
/// call `ReleaseSync`, and the encoder has to be rebuilt.
pub(crate) fn bridge_acquire_verdict(
    outcome: Acquired,
    side: &str,
) -> std::result::Result<(), String> {
    match outcome {
        Acquired::Owned => Ok(()),
        Acquired::TimedOut => Err(format!(
            "Quick Sync bridge AcquireSync({side}) timed out after {KEY_TIMEOUT_MS} ms, so the \
             keyed-mutex sequence is out of step; {REBUILD_MARKER}"
        )),
        Acquired::Abandoned => Err(format!(
            "Quick Sync bridge keyed mutex abandoned at AcquireSync({side}); {REBUILD_MARKER}"
        )),
    }
}

/// Acquire one side of the bridge, through `keyed::acquire` so a timeout is not read as
/// ownership. On `Err` the caller owns nothing and must neither copy nor `ReleaseSync`.
fn acquire_bridge_key(mutex: &IDXGIKeyedMutex, key: u64, side: &str) -> Result<()> {
    let outcome = keyed::acquire(mutex, key, KEY_TIMEOUT_MS)
        .with_context(|| format!("Quick Sync bridge AcquireSync({side})"))?;
    bridge_acquire_verdict(outcome, side).map_err(|e| anyhow!(e))
}

pub struct Encoder {
    vpl: Vpl,
    session: mfxSession,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    bridge: Option<InputBridge>,
    /// The dispatcher loader owns the enumerated implementations and must outlive the session;
    /// it is released with `MFXUnload` after `MFXClose`.
    loader: mfxLoader,
    enc_param: mfxVideoParam,
    enc_frame_info: mfxFrameInfo,

    /// The three ext buffers, owned so they can be re-attached to `Reset`: oneVPL applies
    /// whatever ext buffers are present at `Reset`, and ones omitted revert to the runtime's
    /// defaults, which would undo the low-latency configuration at the first bitrate change.
    ext_co: Box<mfxExtCodingOption>,
    ext_co2: Box<mfxExtCodingOption2>,
    ext_co3: Box<mfxExtCodingOption3>,
    ext_ptrs: Vec<*mut mfxExtBuffer>,

    bitstream_buf: Vec<u8>,
    /// The `mfxBitstream` and `mfxEncodeCtrl` handed to `EncodeFrameAsync`. The runtime keeps
    /// both pointers in the queued task and writes `DataOffset`/`DataLength` through the
    /// bitstream one when the task completes, which can be after the 2 s sync timeout (on the
    /// next submit or inside `ENCODE_Close`), so they must live as long as the encoder.
    bs: Box<mfxBitstream>,
    ctrl: Box<mfxEncodeCtrl>,
    /// The per-frame QP `set_qp` asked for, when it is not the one the session opened with.
    qp_override: Option<u16>,
    /// A task left running by a sync timeout, and when that happened. Nothing is submitted until
    /// it completes (its output goes nowhere: the frame was already given up), and the next
    /// frame after it is an IDR because the client never received the one it encoded.
    in_flight: Option<(mfxSyncPoint, std::time::Instant)>,
    /// Set once the session is unusable (device lost, GPU hang, a broken bridge). Every later
    /// call fails at once with this text, which carries `REBUILD_MARKER`.
    broken: Option<String>,
    /// `SyncOperation` wait per call and in total. Constants in production; a test shortens them
    /// to force the timeout path.
    sync_slice_ms: u32,
    sync_cap_ms: u32,
    /// Switches, read once at construction.
    use_brc_multiplier: bool,
    unsupported_query_fallback: bool,
    /// `qsv_reset_retry`: retry a refused `Reset` along `reset_retry_ladder`. A rung above the
    /// first may leave a CPB of more than one frame, which is queueing delay; the accepting
    /// retry logs how many frames it holds.
    reset_retry: bool,
    config: EncoderConfig,

    coded_w: u16,
    coded_h: u16,
    src_w: u32,
    src_h: u32,

    upload_texture: Option<ID3D11Texture2D>,
    nv12_upload_texture: Option<ID3D11Texture2D>,
    last_nv12: *mut mfxFrameSurface1,

    /// What the runtime agreed to for slicing, decided by `ENCODE_Query`.
    slice_mode: SliceMode,

    stage_us: StageTiming,

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
        // Quick Sync cannot take a cross-adapter texture: `OpenSharedResource1` on an NT handle
        // needs both devices on the same adapter, so `InputBridge` is same-adapter only and a
        // cross-adapter Intel host goes through `Backend::IntelCpu` and its readback.
        if super::super::device_vendor(device) != super::super::Vendor::Intel {
            bail!(
                "Intel Quick Sync requires an Intel D3D11 device, but the capture device is not \
                 Intel; NT-handle texture sharing is same-adapter only, so this falls to the NV12 \
                 CPU bridge (or to NVENC, which can take a cross-adapter texture)"
            );
        }

        // The VPP input crop and the `D3D11_BOX` copied out of the captured texture, so the
        // source's extent and nothing larger: D3D11 drops a `CopySubresourceRegion` whose source
        // box is out of range with no HRESULT, leaving a frozen or black picture. Scaling is the
        // VPP output stage's job.
        let (src_w, src_h) = (native_w, native_h);

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
            // the bridge drives the capture device's immediate context from the encode thread,
            // and an immediate context is not free-threaded: the capture device needs the
            // protection too, and not getting it is a correctness problem, hence the hard error
            set_multithread_protected(context, "capture device (Quick Sync input bridge)")?;
            tprintln!(
                "Intel Quick Sync: capture device lacks VIDEO_SUPPORT; routing through a dedicated \
                 Intel device via a same-adapter shared-texture bridge ({src_w}x{src_h})"
            );
            (enc_device, enc_context, Some(bridge))
        };

        let vpl = Vpl::load()?;

        set_multithread_protected(&enc_context, "Quick Sync encode device")?;

        // pin the session to the adapter `SetHandle` is about to give it, so implementation 0 on
        // a host with two Intel adapters cannot be the other one
        let pin_luid = if tune::on("qsv_session_pin_luid") {
            adapter_luid(&enc_device)
        } else {
            None
        };
        let use_brc_multiplier = tune::on("qsv_brc_multiplier");
        let unsupported_query_fallback = tune::on("qsv_query_unsupported_fallback");

        let (session, loader) = create_session(&vpl, pin_luid)?;

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
        let max_fps = query_max_supported_fps(
            &vpl,
            session,
            &config,
            requested_fps,
            use_brc_multiplier,
            unsupported_query_fallback,
        );
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
            bs: Box::new(unsafe { std::mem::zeroed() }),
            ctrl: Box::new(unsafe { std::mem::zeroed() }),
            qp_override: None,
            in_flight: None,
            broken: None,
            sync_slice_ms: SYNC_SLICE_MS,
            sync_cap_ms: SYNC_CAP_MS,
            use_brc_multiplier,
            unsupported_query_fallback,
            reset_retry: tune::on("qsv_reset_retry"),
            config,
            coded_w,
            coded_h,
            src_w,
            src_h,
            upload_texture: None,
            nv12_upload_texture: None,
            last_nv12: ptr::null_mut(),
            slice_mode: SliceMode::FixedCount(0),
            stage_us: StageTiming::default(),
            frame_index: 0,
            idr_requested: false,
        };

        // on failure, dropping `this` closes the encode and VPP components, the session and the
        // loader: do not close them here as well
        this.init_vpp()?;
        this.init_encode()?;

        // requested values only: what the runtime agreed to is the "oneVPL effective" line
        // `init_encode` prints from `GetVideoParam`
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

    /// The slice layout the runtime agreed to, not the one that was requested.
    pub(crate) fn slice_mode(&self) -> SliceMode {
        self.slice_mode
    }

    /// Per-stage medians for the same-adapter path, in microseconds:
    /// `(get_surface, copy_into_surface, VPP submit)`.
    pub(crate) fn stage_medians_us(&self) -> (u128, u128, u128) {
        (
            self.stage_us.get_surface.median_us(),
            self.stage_us.copy_in.median_us(),
            self.stage_us.vpp.median_us(),
        )
    }

    /// `(FrameRateExtN, FrameRateExtD)` as the runtime reported them after `Init`.
    #[cfg(test)]
    pub(crate) fn effective_frame_rate(&self) -> (u32, u32) {
        let fi = unsafe { self.enc_param.info.mfx.FrameInfo };
        (fi.FrameRateExtN, fi.FrameRateExtD)
    }

    /// The rate control the runtime reported after `Init`.
    #[cfg(test)]
    pub(crate) fn rate_control(&self) -> u16 {
        unsafe { self.enc_param.info.mfx.RateControlMethod }
    }

    /// Ask the runtime, now, for `(TargetKbps, MaxKbps, BRCParamMultiplier)`, with the target and
    /// maximum already multiplied out.
    #[cfg(test)]
    pub(crate) fn read_back_brc(&mut self) -> Result<(u32, u32, u16)> {
        let mut par = self.enc_param;
        self.attach_ext(&mut par);
        check(
            unsafe { (self.vpl.MFXVideoENCODE_GetVideoParam)(self.session, &mut par) },
            "MFXVideoENCODE_GetVideoParam",
        )?;
        let m = unsafe { par.info.mfx };
        let mult = u32::from(m.BRCParamMultiplier.max(1));
        Ok((
            u32::from(m.TargetKbps) * mult,
            u32::from(m.MaxKbps) * mult,
            m.BRCParamMultiplier,
        ))
    }

    /// `(BufferSizeInKB, InitialDelayInKB, BRCParamMultiplier)` as the runtime holds them.
    #[cfg(test)]
    pub(crate) fn read_back_cpb(&mut self) -> Result<(u16, u16, u16)> {
        let mut par = self.enc_param;
        self.attach_ext(&mut par);
        check(
            unsafe { (self.vpl.MFXVideoENCODE_GetVideoParam)(self.session, &mut par) },
            "MFXVideoENCODE_GetVideoParam",
        )?;
        let m = unsafe { par.info.mfx };
        Ok((m.BufferSizeInKB, m.InitialDelayInKB, m.BRCParamMultiplier))
    }

    /// Shorten the `SyncOperation` budget so a test can drive the timeout path on a healthy GPU.
    #[cfg(test)]
    pub(crate) fn set_sync_budget_for_test(&mut self, slice_ms: u32, cap_ms: u32) {
        self.sync_slice_ms = slice_ms;
        self.sync_cap_ms = cap_ms;
    }

    #[cfg(test)]
    pub(crate) fn has_task_in_flight(&self) -> bool {
        self.in_flight.is_some()
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

        let brc = brc_fields(
            self.config.bitrate_bps,
            self.config.max_bitrate_bps,
            vbv_kb_raw(self.config.bitrate_bps, fps),
            self.use_brc_multiplier && self.config.qp.is_none(),
        );
        let target_kbps = brc.target_kbps;
        let max_kbps = brc.max_kbps;
        let buffer_size_kb = brc.buffer_kb;

        let mut mfx: mfxInfoMFX = unsafe { std::mem::zeroed() };
        mfx.BRCParamMultiplier = brc.mult;
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
        // 0 for the first Query: `MaxSliceSize` overrides every slice-count control when it is
        // honoured, and sending both is ambiguous
        mfx.NumSlice = 0;
        mfx.NumRefFrame = 1;
        mfx.BufferSizeInKB = buffer_size_kb;

        if let Some(qp) = self.config.qp {
            let q = qp as u16;
            mfx.RateControlMethod = MFX_RATECONTROL_CQP;
            mfx.InitialDelayInKB = q;
            mfx.TargetKbps = q;
            mfx.MaxKbps = q;
        } else {
            // VCM, not CBR: oneVPL documents `LowDelayBRC`, `MaxFrameSizeI` and `MaxFrameSizeP`
            // as applying to VBR/QVBR/VCM only, so under CBR there is no per-frame size cap.
            // Some generations refuse VCM under `LowPower = ON`; the `ENCODE_Query` below then
            // falls back to CBR.
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
        co2.RepeatPPS = repeat_pps();
        co2.BRefType = MFX_B_REF_OFF;
        co2.AdaptiveI = MFX_CODINGOPTION_OFF;
        co2.AdaptiveB = MFX_CODINGOPTION_OFF;
        // Slices sized to the network, so one lost packet damages one slice. `LowPower = ON`
        // selects the VDEnc fixed-function path, which has more parameter restrictions than PAK,
        // so the `ENCODE_Query` below decides whether this survives.
        co2.MaxSliceSize = SLICE_BYTE_TARGET;
        // quality ceiling under CBR: a floor on QP can only shrink the stream, so the
        // loss-based controller keeps the top end
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
        // remote gaming, not display remoting: on Intel UHD Graphics (Gen12) at 1080p60, capture
        // to bitstream p50/p95/p99 goes from 6.81/11.03/12.19 to 3.95/5.82/6.41 ms at the same
        // bitrate and PSNR. A hint: a runtime may ignore it.
        (co3.ScenarioInfo, co3.AdaptiveCQM) = scenario_options();
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

        // boxed so the pointers stay valid across every later move of `Encoder`, and owned so
        // `set_bitrate` can hand the same array to `Reset`
        *self.ext_co = co;
        *self.ext_co2 = co2;
        *self.ext_co3 = co3;
        self.rebuild_ext_ptrs();
        self.attach_ext(&mut par);

        // `Query` reports which requested parameters the runtime altered or dropped. In and out
        // must be distinct allocations: the runtime writes the corrected parameters into `out`
        // while reading `in`, and aliasing them faults once ext buffers are attached. `out`'s
        // ext buffers carry headers only, per the API contract.
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
        #[cfg(test)]
        let query_status = test_hooks::refuse_vcm_query(query_status, &mut out, &mut out_co2);
        if query_status > MFX_ERR_NONE {
            tprintln!(
                "oneVPL: ENCODE_Query returned {} ({query_status}); the runtime changed something",
                warning_name(query_status)
            );
        }
        // read from `out` before judging the status: a hard `MFX_ERR_UNSUPPORTED` zeroes the
        // fields the runtime cannot honour, and the two fallbacks below are written for that
        let effective_rc = unsafe { out.info.mfx.RateControlMethod };
        let fallback = query_fallback(
            query_status,
            self.config.qp.is_some(),
            effective_rc,
            out_co2.MaxSliceSize,
            self.unsupported_query_fallback,
        )
        .map_err(|e| anyhow!(e))?;
        if query_status < MFX_ERR_NONE {
            teprintln!(
                "oneVPL: ENCODE_Query returned {} ({query_status}); trying the fallbacks it \
                 points at ({fallback:?}) before giving up on Quick Sync",
                status_name(query_status)
            );
        }

        // `Query` zeroes or shrinks `MaxSliceSize` when the runtime will not honour it (common
        // on `LowPower = ON` parts), and the picture would then be coded as a single slice, so
        // fall back to a slice count
        self.slice_mode = if !fallback.num_slice {
            SliceMode::MaxBytes(SLICE_BYTE_TARGET)
        } else {
            self.ext_co2.MaxSliceSize = 0;
            let n = slice_count_for_height(self.config.height);
            par.info.mfx.NumSlice = n;
            self.attach_ext(&mut par);
            SliceMode::FixedCount(n)
        };

        // a rate control `Query` refused comes back in `out` as something else (or 0): drop the
        // VCM-only fields rather than claim a cap that is not applied
        if fallback.cbr {
            teprintln!(
                "oneVPL: VCM refused (runtime chose rc={effective_rc}); falling back to CBR \
                 without LowDelayBRC or the per-frame size caps, which do not apply under CBR"
            );
            par.info.mfx.RateControlMethod = MFX_RATECONTROL_CBR;
            par.info.mfx.TargetKbps = target_kbps;
            par.info.mfx.MaxKbps = max_kbps;
            par.info.mfx.BufferSizeInKB = buffer_size_kb;
            par.info.mfx.BRCParamMultiplier = brc.mult;
            self.ext_co3.LowDelayBRC = MFX_CODINGOPTION_UNKNOWN;
            self.ext_co3.MaxFrameSizeI = 0;
            self.ext_co3.MaxFrameSizeP = 0;
            self.attach_ext(&mut par);
        }

        // a fallback reached from a hard `Query` error is a new parameter set, so ask about it
        // before `Init`: a refusal is then reported as that, not as an `Init` failure with the
        // first refusal's reason lost (see `requery_fallback_before_init`)
        if requery_fallback_before_init(query_status, fallback) {
            let mut out2_co: mfxExtCodingOption = unsafe { std::mem::zeroed() };
            out2_co.Header = self.ext_co.Header;
            let mut out2_co2: mfxExtCodingOption2 = unsafe { std::mem::zeroed() };
            out2_co2.Header = self.ext_co2.Header;
            let mut out2_co3: mfxExtCodingOption3 = unsafe { std::mem::zeroed() };
            out2_co3.Header = self.ext_co3.Header;
            let mut out2_ptrs: [*mut mfxExtBuffer; 3] = [
                &mut out2_co as *mut _ as *mut mfxExtBuffer,
                &mut out2_co2 as *mut _ as *mut mfxExtBuffer,
                &mut out2_co3 as *mut _ as *mut mfxExtBuffer,
            ];
            let mut out2 = par;
            out2.ExtParam = out2_ptrs.as_mut_ptr();
            out2.NumExtParam = out2_ptrs.len() as u16;
            let st2 = unsafe { (self.vpl.MFXVideoENCODE_Query)(self.session, &mut par, &mut out2) };
            if st2 < MFX_ERR_NONE {
                bail!(
                    "MFXVideoENCODE_Query rejected the fallback parameters too ({fallback:?}): {} \
                     (mfxStatus={st2}); the first Query returned {} (mfxStatus={query_status})",
                    status_name(st2),
                    status_name(query_status)
                );
            }
            if st2 > MFX_ERR_NONE {
                tprintln!(
                    "oneVPL: ENCODE_Query on the fallback parameters returned {} ({st2})",
                    warning_name(st2)
                );
            }
            // `Query` may have handed the in-parameter's ext buffers back changed; re-point at
            // the owned ones before `Init`
            self.attach_ext(&mut par);
        }

        #[cfg(feature = "bench")]
        crate::bench::win_hooks::qsv(
            self.session,
            self.vpl.MFXVideoENCODE_GetVideoParam,
            &mut par,
            &mut self.ext_co,
            &mut self.ext_co2,
            &mut self.ext_co3,
        );
        check_no_software_fallback(
            unsafe { (self.vpl.MFXVideoENCODE_Init)(self.session, &mut par) },
            "MFXVideoENCODE_Init",
        )?;

        // read back with the ext buffers attached so CO2/CO3 report their effective values:
        // `NumExtParam = 0` returns only the base `mfxInfoMFX`
        self.attach_ext(&mut par);
        check(
            unsafe { (self.vpl.MFXVideoENCODE_GetVideoParam)(self.session, &mut par) },
            "MFXVideoENCODE_GetVideoParam",
        )?;

        let eff = unsafe { par.info.mfx };
        // `BufferSizeInKB` is the CPB the rate controller may bank into (about one frame); the
        // output allocation must hold the largest access unit, so it gets its own 512 KB floor.
        // Every rate field is in units of `BRCParamMultiplier` (0 reads as 1).
        let eff_mult = u32::from(eff.BRCParamMultiplier.max(1));
        let out_kb = (u32::from(eff.BufferSizeInKB) * eff_mult)
            .max(u32::from(buffer_size_kb) * u32::from(brc.mult.max(1)))
            .max(512);
        self.bitstream_buf = vec![0u8; (out_kb as usize) * 1024];
        self.enc_param = par;
        self.enc_frame_info = eff.FrameInfo;

        // effective values read back from the runtime, not the requested ones
        tprintln!(
            "oneVPL effective: rc={}, target_kbps={}, max_kbps={}, buffer_kb={} (requested {}), \
             brc_multiplier={}, initial_delay_kb={}, low_power={}, target_usage={}, \
             gop_ref_dist={}, num_ref={}, num_slice={}, slices={:?}, async_depth={}, \
             low_delay_brc={}, max_frame_i={}, max_frame_p={}, max_dec_frame_buffering={}, \
             repeat_pps={}, nal_hrd={}, int_ref_type={}, int_ref_cycle={}, out_buf_kb={}, \
             scenario={}, adaptive_cqm={}",
            eff.RateControlMethod,
            u32::from(eff.TargetKbps) * eff_mult,
            u32::from(eff.MaxKbps) * eff_mult,
            u32::from(eff.BufferSizeInKB) * eff_mult,
            u32::from(buffer_size_kb) * u32::from(brc.mult.max(1)),
            eff.BRCParamMultiplier,
            eff.InitialDelayInKB,
            eff.LowPower,
            eff.TargetUsage,
            eff.GopRefDist,
            eff.NumRefFrame,
            eff.NumSlice,
            self.slice_mode,
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
            self.ext_co3.ScenarioInfo,
            self.ext_co3.AdaptiveCQM,
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

    /// Change the constant QP from the next frame: `mfxEncodeCtrl::QP` overrides the session's QP
    /// for one frame in CQP, so nothing is reset and no keyframe is forced. At the QP the session
    /// opened with, no control is passed at all. `Ok(false)` outside CQP.
    pub fn set_qp(&mut self, qp: u8) -> Result<bool> {
        let Some(base) = self.config.qp else {
            return Ok(false);
        };
        let q = qp.clamp(1, 51);
        self.qp_override = (q != base).then_some(q as u16);
        Ok(true)
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        self.usable()?;
        let fps = self.config.fps.max(1);
        // in the units `Init` chose: the runtime does not take a new multiplier at `Reset`
        let init_mult = unsafe { self.enc_param.info.mfx.BRCParamMultiplier };
        let (brc, clamped) = brc_fields_at(
            bps,
            bps,
            vbv_kb_raw(bps, fps),
            if self.use_brc_multiplier {
                init_mult
            } else {
                0
            },
        );
        // reported by the pipeline once per change, with the rate it was held to
        let clamped_kbps = u32::from(brc.max_kbps) * u32::from(init_mult.max(1));
        let kbps = brc.max_kbps;

        // only the rate-control fields may differ between Init and Reset: everything else, ext
        // buffers included, is re-sent as Init received it (omitted ones revert to defaults)
        let mut par = self.enc_param;
        par.info.mfx.TargetKbps = brc.target_kbps;
        par.info.mfx.MaxKbps = brc.max_kbps;
        par.info.mfx.BufferSizeInKB = brc.buffer_kb;
        if self.ext_co3.LowDelayBRC == MFX_CODINGOPTION_ON {
            let avg_frame_bytes = (bps / 8 / fps).max(1);
            self.ext_co3.WinBRCMaxAvgKbps = kbps;
            self.ext_co3.MaxFrameSizeP = avg_frame_bytes.saturating_mul(2);
            self.ext_co3.MaxFrameSizeI = avg_frame_bytes.saturating_mul(6);
        }
        self.attach_ext(&mut par);

        let mut st = unsafe { (self.vpl.MFXVideoENCODE_Reset)(self.session, &mut par) };
        // under the CBR fallback `Reset` refuses a lower rate with
        // MFX_ERR_INCOMPATIBLE_VIDEO_PARAM, probably because the `InitialDelayInKB` that
        // `GetVideoParam` returned after `Init` no longer fits the smaller CPB; only then is
        // `reset_retry_ladder` tried
        if self.reset_retry
            && matches!(
                st,
                MFX_ERR_INCOMPATIBLE_VIDEO_PARAM | MFX_ERR_INVALID_VIDEO_PARAM
            )
        {
            let first = st;
            let (old_cpb, old_delay) = unsafe {
                (
                    self.enc_param.info.mfx.BufferSizeInKB,
                    self.enc_param.info.mfx.InitialDelayInKB,
                )
            };
            let ladder = reset_retry_ladder(brc.buffer_kb, old_cpb, old_delay);
            for (rung, &(cpb, delay)) in ladder.iter().enumerate() {
                let mut retry = par;
                retry.info.mfx.BufferSizeInKB = cpb;
                retry.info.mfx.InitialDelayInKB = delay;
                self.attach_ext(&mut retry);
                st = unsafe { (self.vpl.MFXVideoENCODE_Reset)(self.session, &mut retry) };
                if st >= MFX_ERR_NONE {
                    let mult = unsafe { retry.info.mfx.BRCParamMultiplier };
                    let frames = cpb_frames(cpb, mult, bps, fps);
                    let msg = format!(
                        "Intel Quick Sync: MFXVideoENCODE_Reset refused the new rate with {} and \
                         accepted it on retry {} of {} (BufferSizeInKB={cpb}, \
                         InitialDelayInKB={delay}, asked BufferSizeInKB={}, multiplier={}): the \
                         CPB now holds {frames:.1} frames at {bps} bps / {fps} fps",
                        status_name(first),
                        rung + 1,
                        ladder.len(),
                        brc.buffer_kb,
                        mult.max(1),
                    );
                    if cpb > brc.buffer_kb.max(1) {
                        teprintln!(
                            "{msg}; the extra frames are queueing delay on a fixed-rate link"
                        );
                    } else {
                        tprintln!("{msg}");
                    }
                    par = retry;
                    break;
                }
            }
        }
        check(st, "MFXVideoENCODE_Reset")?;
        self.enc_param = par;
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;
        tprintln!(
            "Intel Quick Sync bitrate reconfigured (bitrate_bps={bps}, buffer_kb={}, \
             brc_multiplier={}, ext_buffers={} retained)",
            unsafe { par.info.mfx.BufferSizeInKB },
            unsafe { par.info.mfx.BRCParamMultiplier },
            par.NumExtParam,
        );
        if clamped {
            return Ok(BitrateApplied::Clamped {
                applied_bps: clamped_kbps.saturating_mul(1000),
            });
        }
        Ok(BitrateApplied::Applied)
    }

    pub fn encode_texture(&mut self, src: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<u8>> {
        self.usable()?;
        let r = self.encode_texture_inner(src, force_idr);
        self.note(r)
    }

    fn encode_texture_inner(&mut self, src: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<u8>> {
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
        self.usable()?;
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
        let r = self.encode_texture_inner(&tex, force_idr);
        self.note(r)
    }

    /// Encode a frame the GPU's video processor already scaled and converted to NV12, for the
    /// CPU-bridge path: 1.5 bytes a pixel instead of 4 of BGRA, no VPP pass, and the upload goes
    /// straight into the encode surface.
    ///
    /// Layout per `Scaler::read_back` with `ReadbackFormat::Nv12`: `height` rows of luma then
    /// `height / 2` rows of interleaved chroma (U then V), all at one `row_pitch`. That is
    /// exactly one D3D11 NV12 subresource, so `UpdateSubresource` takes it in a single call.
    pub fn encode_nv12_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let (w, h) = (self.config.width as usize, self.config.height as usize);
        let rows = h + h / 2;
        let need = rows * row_pitch as usize;
        if (row_pitch as usize) < w || data.len() < need {
            bail!(
                "encode_nv12_padded: need row_pitch >= {w} and >= {need} bytes for {w}x{h} NV12 \
                 (got pitch {row_pitch}, {} bytes)",
                data.len()
            );
        }
        self.usable()?;
        let r = self.encode_nv12_inner(data, row_pitch, force_idr);
        self.note(r)
    }

    fn encode_nv12_inner(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let staging = self.ensure_nv12_upload_texture()?;
        unsafe {
            self.context.UpdateSubresource(
                &staging,
                0,
                None,
                data.as_ptr() as *const c_void,
                row_pitch,
                0,
            );
        }

        let force_idr = force_idr || self.idr_requested;
        let surf = self.get_surface(SurfaceKind::Encode)?;
        if let Err(e) = unsafe { self.copy_into_nv12_surface(surf, &staging) } {
            self.release_surface(surf);
            return Err(e);
        }
        let out = unsafe { self.encode_surface(surf, force_idr) };
        self.replace_last_nv12(surf);
        let out = out?;
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        if self.last_nv12.is_null() {
            return Ok(Vec::new());
        }
        self.usable()?;
        let force_idr = force_idr || self.idr_requested;
        let out = unsafe { self.encode_surface(self.last_nv12, force_idr) };
        let out = self.note(out)?;
        if force_idr {
            self.idr_requested = false;
        }
        self.frame_index += 1;
        Ok(out)
    }

    /// The NV12 upload texture the CPU-bridge NV12 path writes into, created once. Separate from
    /// `upload_texture`, which is BGRA: a session uses one or the other for its whole life.
    fn ensure_nv12_upload_texture(&mut self) -> Result<ID3D11Texture2D> {
        if let Some(t) = &self.nv12_upload_texture {
            return Ok(t.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.config.width,
            Height: self.config.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: 0,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut tex: Option<ID3D11Texture2D> = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }
            .context("CreateTexture2D (Intel NV12 CPU-upload staging)")?;
        let tex = tex.context("CreateTexture2D returned no NV12 texture")?;
        self.nv12_upload_texture = Some(tex.clone());
        Ok(tex)
    }

    /// Copy a whole NV12 texture into a oneVPL surface's own D3D11 texture.
    ///
    /// No `D3D11_BOX`, and the two are not the same size: the upload texture is
    /// `config.width x config.height`, the encode surface is the 16-aligned coded size. A box-less
    /// `CopySubresourceRegion` copies the whole (smaller) source to the destination's origin,
    /// which is the crop rectangle `enc_frame_info` describes; the padding is outside the crop.
    /// A box on an NV12 resource would need luma coordinates with even bounds. `Flush` for the
    /// same reason `copy_into_surface` flushes.
    unsafe fn copy_into_nv12_surface(
        &self,
        surf: *mut mfxFrameSurface1,
        src: &ID3D11Texture2D,
    ) -> Result<()> {
        let iface = unsafe { (*surf).FrameInterface };
        if iface.is_null() {
            bail!("encode surface has no FrameInterface");
        }
        let get_native = unsafe { (*iface).GetNativeHandle }
            .ok_or_else(|| anyhow!("FrameInterface::GetNativeHandle is null"))?;
        let mut res: mfxHDL = ptr::null_mut();
        let mut res_type: mfxResourceType = 0;
        check_strict(
            unsafe { get_native(surf, &mut res, &mut res_type) },
            "FrameInterface::GetNativeHandle (encode surface)",
        )?;
        if res_type != MFX_RESOURCE_DX11_TEXTURE || res.is_null() {
            bail!("encode surface is not a D3D11 texture (resource_type={res_type})");
        }
        let dst_ptr = res;
        let dst = unsafe { ID3D11Texture2D::from_raw_borrowed(&dst_ptr) }
            .ok_or_else(|| anyhow!("GetNativeHandle returned null texture"))?;
        let dst_res: ID3D11Resource = dst
            .cast()
            .context("encode surface texture as ID3D11Resource")?;
        let src_res: ID3D11Resource = src
            .cast()
            .context("NV12 upload texture as ID3D11Resource")?;
        unsafe {
            self.context
                .CopySubresourceRegion(&dst_res, 0, 0, 0, 0, &src_res, 0, None);
            if per_frame_flush_enabled() {
                self.context.Flush();
            }
        }
        Ok(())
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
        let t_surface = std::time::Instant::now();
        let in_surf = self.get_surface(SurfaceKind::VppIn)?;
        let t_copy = std::time::Instant::now();
        self.stage_us.get_surface.push(t_copy - t_surface);
        let copy_res = unsafe { self.copy_into_surface(in_surf, src) };
        self.stage_us.copy_in.push(t_copy.elapsed());
        let t_vpp = std::time::Instant::now();
        let nv12 = match copy_res.and_then(|()| self.get_surface(SurfaceKind::VppOut)) {
            Ok(s) => s,
            Err(e) => {
                self.release_surface(in_surf);
                return Err(e);
            }
        };

        // `syncp` is deliberately never passed to `MFXVideoCORE_SyncOperation`. ENCODE consumes
        // this surface in the same session, so the runtime carries the dependency itself and
        // synchronising here would cost a frame of pipelining for nothing. Do not "fix" it.
        let mut syncp: mfxSyncPoint = ptr::null_mut();
        let mut busy = DeviceBusyBackoff::new("MFXVideoVPP_RunFrameVPPAsync");
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
                if let Err(e) = busy.wait() {
                    self.release_surface(in_surf);
                    self.release_surface(nv12);
                    return Err(e);
                }
                continue;
            }
            break st;
        };
        busy.finish();
        self.stage_us.vpp.push(t_vpp.elapsed());
        self.release_surface(in_surf);
        match self.mfx_check(st, "MFXVideoVPP_RunFrameVPPAsync") {
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

        // No oneVPL surface import (`mfxSurfaceD3D11Tex2D`, oneVPL 2.9+) to delete this copy: it
        // is 136 us of a ~7000 us frame on Intel UHD Graphics, and most of that is the `Flush`
        // below, which measured as a p50 win.
        let flush = per_frame_flush_enabled();
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
                    if flush {
                        self.context.Flush();
                    }
                }
            }
            Some(bridge) => {
                unsafe { bridge.upload(src, self.src_w, self.src_h)? };
                acquire_bridge_key(&bridge.enc_mutex, KEY_ENCODER, "encoder")?;
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
                        if flush {
                            self.context.Flush();
                        }
                    }
                    Ok(())
                })();
                let released = unsafe { bridge.enc_mutex.ReleaseSync(KEY_WRITER) };
                copy?;
                released.with_context(|| {
                    format!("Quick Sync bridge ReleaseSync(encoder); {REBUILD_MARKER}")
                })?;
            }
        }
        Ok(())
    }

    /// `check`, except that a status meaning the session is gone produces an error carrying
    /// `REBUILD_MARKER` (and says whether the D3D11 device was removed).
    fn mfx_check(&self, st: mfxStatus, what: &str) -> Result<()> {
        if is_device_loss_status(st) {
            let removed = match unsafe { self.device.GetDeviceRemovedReason() } {
                Ok(()) => String::new(),
                Err(e) => format!(", D3D11 device removed: {e}"),
            };
            bail!(
                "{what} failed: {} (mfxStatus={st}{removed}); {REBUILD_MARKER}",
                status_name(st)
            );
        }
        check(st, what)
    }

    /// Mark the encoder unusable for good; every later call returns this error at once instead
    /// of submitting into a dead session (and, for the repeater, waiting on it).
    fn latch_broken(&mut self, e: &anyhow::Error) {
        if self.broken.is_none() {
            let text = format!("{e:#}");
            let text = if text.contains(REBUILD_MARKER) {
                text
            } else {
                format!("{text}; {REBUILD_MARKER}")
            };
            teprintln!("Intel Quick Sync: encoder is no longer usable: {text}");
            self.broken = Some(text);
        }
    }

    /// Latch on errors that mean the session is gone, and pass every result through.
    fn note<T>(&mut self, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            if needs_rebuild(e) {
                self.latch_broken(e);
            }
        }
        r
    }

    /// Whether a frame may be submitted now.
    ///
    /// Fails at once when the encoder is latched broken. Otherwise, if a task was left running by
    /// a sync timeout, asks it (without waiting) whether it has finished: until it has, the
    /// bitstream it will write into is still the runtime's, so this frame is shed as "device
    /// busy"; once it has, its output is discarded and the next frame is forced to an IDR.
    fn usable(&mut self) -> Result<()> {
        if let Some(reason) = &self.broken {
            bail!("{reason}");
        }
        let Some((syncp, since)) = self.in_flight else {
            return Ok(());
        };
        let st = unsafe { (self.vpl.MFXVideoCORE_SyncOperation)(self.session, syncp, 0) };
        if st == MFX_WRN_IN_EXECUTION {
            if since.elapsed() >= IN_FLIGHT_GIVE_UP {
                let e = anyhow!(
                    "MFXVideoCORE_SyncOperation(encode): a timed-out encode task is still \
                     executing after {:.1} s; {REBUILD_MARKER}",
                    since.elapsed().as_secs_f64()
                );
                self.latch_broken(&e);
                return Err(e);
            }
            bail!(
                "MFXVideoCORE_SyncOperation(encode): the encode task from a sync timeout is still \
                 executing; frame shed (device busy)"
            );
        }
        self.in_flight = None;
        self.idr_requested = true;
        if let Err(e) = self.mfx_check(st, "MFXVideoCORE_SyncOperation(timed-out encode)") {
            if needs_rebuild(&e) {
                self.latch_broken(&e);
                return Err(e);
            }
            teprintln!("Intel Quick Sync: the timed-out encode task ended in error: {e:#}");
        } else {
            tprintln!(
                "Intel Quick Sync: the timed-out encode task completed after {:.0} ms; its output \
                 is discarded and the next frame is an IDR",
                since.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(())
    }

    unsafe fn encode_surface(
        &mut self,
        surf: *mut mfxFrameSurface1,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        // `ctrl` and `bs` live in boxes: the runtime may write through them until the task is
        // synchronised, which can be after this function returns (see `in_flight`)
        *self.ctrl = unsafe { std::mem::zeroed() };
        if let Some(q) = self.qp_override {
            self.ctrl.QP = q;
        }
        let ctrl_ptr = if force_idr {
            self.ctrl.FrameType = MFX_FRAMETYPE_I | MFX_FRAMETYPE_IDR | MFX_FRAMETYPE_REF;
            &mut *self.ctrl as *mut mfxEncodeCtrl
        } else if self.qp_override.is_some() {
            &mut *self.ctrl as *mut mfxEncodeCtrl
        } else {
            ptr::null_mut()
        };

        *self.bs = unsafe { std::mem::zeroed() };
        self.bs.Data = self.bitstream_buf.as_mut_ptr();
        self.bs.MaxLength = self.bitstream_buf.len() as u32;
        let bs_ptr = &mut *self.bs as *mut mfxBitstream;

        let mut syncp: mfxSyncPoint = ptr::null_mut();
        let mut busy = DeviceBusyBackoff::new("MFXVideoENCODE_EncodeFrameAsync");
        loop {
            let st = unsafe {
                (self.vpl.MFXVideoENCODE_EncodeFrameAsync)(
                    self.session,
                    ctrl_ptr,
                    surf,
                    bs_ptr,
                    &mut syncp,
                )
            };
            if st == MFX_WRN_DEVICE_BUSY {
                busy.wait()?;
                continue;
            }
            busy.finish();
            if st == MFX_ERR_MORE_DATA {
                return Ok(Vec::new());
            }
            self.mfx_check(st, "MFXVideoENCODE_EncodeFrameAsync")?;
            break;
        }

        if syncp.is_null() {
            return Ok(Vec::new());
        }
        let mut waited_ms = 0u32;
        loop {
            let st = unsafe {
                (self.vpl.MFXVideoCORE_SyncOperation)(self.session, syncp, self.sync_slice_ms)
            };
            if st == MFX_WRN_IN_EXECUTION {
                waited_ms += self.sync_slice_ms;
                if waited_ms >= self.sync_cap_ms {
                    // the task is still queued and still owns `bs` and `ctrl`: keep it so nothing
                    // reuses them before it completes, and make the frame after it an IDR
                    self.in_flight = Some((syncp, std::time::Instant::now()));
                    self.idr_requested = true;
                    bail!(
                        "MFXVideoCORE_SyncOperation(encode): GPU made no progress for \
                         {waited_ms}ms; frame shed, the task stays queued (device busy)"
                    );
                }
                continue;
            }
            self.mfx_check(st, "MFXVideoCORE_SyncOperation(encode)")?;
            break;
        }

        let start = self.bs.DataOffset as usize;
        let end = start + self.bs.DataLength as usize;
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
        let st = unsafe { f(self.session, &mut surf) };
        if is_device_loss_status(st) {
            self.mfx_check(st, what)?;
        }
        check_strict(st, what)?;
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
/// enumeration, and two at once against the same iGPU make one lose: `MFXCreateSession`
/// returned `MFX_ERR_NOT_FOUND` in roughly half of concurrent pairs. An encoder is built per
/// streaming session, so two clients joining together is exactly that case.
static SESSION_CREATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// How many times to re-enumerate before believing `MFX_ERR_NOT_FOUND`. The lock removes the
/// contention this process creates; the retry covers another process using the same iGPU, or a
/// runtime still tearing down the previous session's device.
const SESSION_CREATE_ATTEMPTS: u32 = 4;
const SESSION_CREATE_BACKOFF_MS: u64 = 25;

fn create_session(vpl: &Vpl, pin_luid: Option<LUID>) -> Result<(mfxSession, mfxLoader)> {
    // a poisoned lock guards `()`, so there is nothing inconsistent to refuse over
    let _serialize = SESSION_CREATE_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    let mut last: Option<anyhow::Error> = None;
    let mut pin = pin_luid;
    let mut last_pin_error: Option<anyhow::Error> = None;
    for attempt in 0..SESSION_CREATE_ATTEMPTS {
        // The adapter-pinned enumeration first. `mfxExtendedDeviceId.DeviceLUID` needs a runtime
        // that reports extended device ids (API 2.6+); one that does not answers
        // `MFX_ERR_NOT_FOUND` to the filter. A pinned refusal is only believed after the unpinned
        // enumeration succeeds, and then pinning is not tried again for this encoder.
        if let Some(luid) = pin {
            match create_session_once(vpl, Some(luid)) {
                Ok(v) => {
                    tprintln!(
                        "oneVPL: session pinned to the encode device's adapter (LUID {:08x}:{:08x})",
                        luid.HighPart,
                        luid.LowPart
                    );
                    return Ok(v);
                }
                // whatever refused the pinned enumeration (an unknown property on an older
                // dispatcher, no match), the unpinned one decides whether there is Quick Sync
                Err(e) => last_pin_error = Some(e),
            }
        }
        match create_session_once(vpl, None) {
            Ok(v) => {
                if attempt > 0 {
                    tprintln!(
                        "oneVPL: session created on attempt {} of {SESSION_CREATE_ATTEMPTS}",
                        attempt + 1
                    );
                }
                if pin.take().is_some() {
                    tprintln!(
                        "oneVPL: the adapter-pinned enumeration failed ({}); using the first \
                         hardware D3D11 implementation, unpinned",
                        last_pin_error
                            .as_ref()
                            .map(|e| format!("{e:#}"))
                            .unwrap_or_default()
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

fn create_session_once(vpl: &Vpl, pin_luid: Option<LUID>) -> Result<(mfxSession, mfxLoader)> {
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

    // The dispatcher copies the eight bytes at `MFXSetConfigFilterProperty` time
    // (`ConfigCtxVPL::SetFilterProperty`, `ePropExtDev_DeviceLUID`), so a local is enough. Byte
    // order is the in-memory `LUID` (LowPart little-endian, then HighPart), which is how the
    // dispatcher itself fills `DeviceLUID` from a DXGI adapter.
    let luid_bytes: Option<[u8; 8]> = pin_luid.map(luid_to_bytes);
    let build = || -> Result<mfxSession> {
        set_u32(b"mfxImplDescription.Impl\0", MFX_IMPL_TYPE_HARDWARE as u32)?;
        set_u32(
            b"mfxImplDescription.AccelerationMode\0",
            MFX_ACCEL_MODE_VIA_D3D11 as u32,
        )?;
        if let Some(bytes) = luid_bytes.as_ref() {
            let cfg = unsafe { (vpl.MFXCreateConfig)(loader) };
            if cfg.is_null() {
                bail!("MFXCreateConfig returned null");
            }
            let mut var: mfxVariant = unsafe { std::mem::zeroed() };
            var.Type = MFX_VARIANT_TYPE_PTR;
            var.Data.Ptr = bytes.as_ptr() as mfxHDL;
            check_strict(
                unsafe {
                    (vpl.MFXSetConfigFilterProperty)(
                        cfg,
                        c"mfxExtendedDeviceId.DeviceLUID".as_ptr().cast(),
                        var,
                    )
                },
                "MFXSetConfigFilterProperty(mfxExtendedDeviceId.DeviceLUID)",
            )?;
        }
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
            // A 1.x runtime returns MFX_ERR_UNSUPPORTED from those on the first frame, after
            // `build_backend` has committed to this vendor, so check here, where failing over is
            // still possible.
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
            // no blanket "no Intel hardware" here: `create_session` decides that, after the
            // retries, and only for statuses that mean it
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
        let mut first_intel: Option<(u32, String, IDXGIAdapter1)> = None;
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
            // by PCI vendor id, never by the description string, which can be localised,
            // rebranded or empty
            if first_intel.is_none()
                && super::super::Vendor::from_pci_id(desc.VendorId) == super::super::Vendor::Intel
            {
                if prefer_luid.is_none() {
                    tprintln!(
                        "selected Intel adapter for Quick Sync by vendor id (index={idx}, name={name})"
                    );
                    chosen = Some(adapter);
                    break;
                }
                first_intel = Some((idx, name, adapter));
            }
        }
        let adapter = match chosen {
            Some(a) => a,
            None => {
                let (idx, name, a) = first_intel.ok_or_else(|| {
                    anyhow!(
                        "no adapter with Intel's PCI vendor id (0x8086); Quick Sync requires an \
                         Intel GPU"
                    )
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

/// `LUID` as the eight bytes `mfxExtendedDeviceId::DeviceLUID` holds.
pub(crate) fn luid_to_bytes(luid: LUID) -> [u8; 8] {
    let v = (u64::from(luid.HighPart as u32) << 32) | u64::from(luid.LowPart);
    v.to_le_bytes()
}

/// The coded-picture buffer the rate controller may bank into, in KB: about one frame of bits.
/// A larger CPB lets the encoder smooth excursions over a long window instead of clipping them
/// per frame, which on a fixed-rate link becomes queueing delay and then loss. The output
/// allocation is a separate, larger number.
fn vbv_kb_raw(bitrate_bps: u32, fps: u32) -> u32 {
    ((bitrate_bps / 8 / fps.max(1)) / 1024).max(1)
}

/// Whether `ENCODE_Query` accepts `fps` unchanged, under rate control `rc` (VCM, or the CBR
/// fallback; ignored in constant-QP mode). On a part that refuses VCM every rate fails under it,
/// so the probe has to ask under the rate control the encoder will actually run.
fn query_fps_supported(
    vpl: &Vpl,
    session: mfxSession,
    config: &EncoderConfig,
    fps: u32,
    rc: u16,
    use_brc_multiplier: bool,
) -> bool {
    let fps = fps.max(1);
    let coded_w = align_up_u16(config.width, 16);
    let coded_h = align_up_u16(config.height, 16);
    let brc = brc_fields(
        config.bitrate_bps,
        config.max_bitrate_bps,
        vbv_kb_raw(config.bitrate_bps, fps),
        use_brc_multiplier && config.qp.is_none(),
    );
    let buffer_size_kb = brc.buffer_kb;
    let target_kbps = brc.target_kbps;
    let max_kbps = brc.max_kbps;

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
    mfx.NumSlice = slice_count_for_height(config.height);
    mfx.NumRefFrame = 1;
    mfx.BufferSizeInKB = buffer_size_kb;
    mfx.BRCParamMultiplier = brc.mult;
    let vcm = config.qp.is_none() && rc == MFX_RATECONTROL_VCM;
    if let Some(qp) = config.qp {
        let q = qp as u16;
        mfx.RateControlMethod = MFX_RATECONTROL_CQP;
        mfx.InitialDelayInKB = q;
        mfx.TargetKbps = q;
        mfx.MaxKbps = q;
    } else {
        mfx.RateControlMethod = rc;
        mfx.TargetKbps = target_kbps;
        mfx.MaxKbps = max_kbps;
    }
    mfx.IdrInterval = 0;

    let mut par: mfxVideoParam = unsafe { std::mem::zeroed() };
    par.AsyncDepth = 1;
    par.IOPattern = MFX_IOPATTERN_IN_VIDEO_MEMORY;
    par.info = mfxInfoUnion { mfx };

    // the ext buffers matter to the answer: a rate the runtime accepts bare can be refused under
    // `LowDelayBRC`, the per-frame size caps and intra refresh, and `Init` would then fail at a
    // rate this probe accepted
    let mut co: mfxExtCodingOption = unsafe { std::mem::zeroed() };
    co.Header.BufferId = MFX_EXTBUFF_CODING_OPTION;
    co.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption>() as u32;
    co.MaxDecFrameBuffering = 1;
    co.NalHrdConformance = MFX_CODINGOPTION_OFF;
    co.PicTimingSEI = MFX_CODINGOPTION_OFF;

    let mut co2: mfxExtCodingOption2 = unsafe { std::mem::zeroed() };
    co2.Header.BufferId = MFX_EXTBUFF_CODING_OPTION2;
    co2.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption2>() as u32;
    co2.RepeatPPS = repeat_pps();
    co2.BRefType = MFX_B_REF_OFF;
    if config.intra_refresh {
        co2.IntRefType = MFX_REFRESH_HORIZONTAL;
        co2.IntRefCycleSize = fps.clamp(2, u16::MAX as u32) as u16;
    }

    let mut co3: mfxExtCodingOption3 = unsafe { std::mem::zeroed() };
    co3.Header.BufferId = MFX_EXTBUFF_CODING_OPTION3;
    co3.Header.BufferSz = std::mem::size_of::<mfxExtCodingOption3>() as u32;
    (co3.ScenarioInfo, co3.AdaptiveCQM) = scenario_options();
    co3.GPB = MFX_CODINGOPTION_OFF;
    if config.qp.is_none() {
        co3.WinBRCSize = 1;
        co3.WinBRCMaxAvgKbps = max_kbps;
    }
    if vcm {
        // VCM-only: `init_encode` drops them under its CBR fallback
        co3.LowDelayBRC = MFX_CODINGOPTION_ON;
        let avg_frame_bytes = (config.bitrate_bps / 8 / fps).max(1);
        co3.MaxFrameSizeP = avg_frame_bytes.saturating_mul(2);
        co3.MaxFrameSizeI = avg_frame_bytes.saturating_mul(6);
    }

    let mut in_ptrs: [*mut mfxExtBuffer; 3] = [
        &mut co as *mut _ as *mut mfxExtBuffer,
        &mut co2 as *mut _ as *mut mfxExtBuffer,
        &mut co3 as *mut _ as *mut mfxExtBuffer,
    ];
    par.ExtParam = in_ptrs.as_mut_ptr();
    par.NumExtParam = in_ptrs.len() as u16;

    // in and out must be distinct once ext buffers are attached: the runtime writes the
    // corrected parameters into `out` while reading `in`
    let mut out_co: mfxExtCodingOption = unsafe { std::mem::zeroed() };
    out_co.Header = co.Header;
    let mut out_co2: mfxExtCodingOption2 = unsafe { std::mem::zeroed() };
    out_co2.Header = co2.Header;
    let mut out_co3: mfxExtCodingOption3 = unsafe { std::mem::zeroed() };
    out_co3.Header = co3.Header;
    let mut out_ptrs: [*mut mfxExtBuffer; 3] = [
        &mut out_co as *mut _ as *mut mfxExtBuffer,
        &mut out_co2 as *mut _ as *mut mfxExtBuffer,
        &mut out_co3 as *mut _ as *mut mfxExtBuffer,
    ];
    let mut out = par;
    out.ExtParam = out_ptrs.as_mut_ptr();
    out.NumExtParam = out_ptrs.len() as u16;

    let status = unsafe { (vpl.MFXVideoENCODE_Query)(session, &mut par, &mut out) };
    #[cfg(test)]
    let status = test_hooks::refuse_vcm_fps_query(status, vcm);
    let out_fps = unsafe { out.info.mfx.FrameInfo.FrameRateExtN };
    status >= MFX_ERR_NONE && out_fps == fps
}

fn query_max_supported_fps(
    vpl: &Vpl,
    session: mfxSession,
    config: &EncoderConfig,
    requested: u32,
    use_brc_multiplier: bool,
    cbr_fallback: bool,
) -> u32 {
    let requested = requested.max(1);
    let probe = |rc: u16| {
        search_max_fps(requested, |fps| {
            query_fps_supported(vpl, session, config, fps, rc, use_brc_multiplier)
        })
    };
    let mut found = probe(MFX_RATECONTROL_VCM);
    // the rate control `init_encode` falls back to when VCM is refused: without asking under
    // it, a VCM-refusing part fails every rate and lands on the guess below
    if found.is_none() && cbr_fallback && config.qp.is_none() {
        found = probe(MFX_RATECONTROL_CBR);
        if let Some(fps) = found {
            tprintln!(
                "Intel Quick Sync: the frame-rate probe is refused under VCM and accepted under the \
                 CBR fallback (up to {fps} fps)"
            );
        }
    }
    match found {
        Some(fps) => fps,
        None => {
            // the probe itself failed, so the cap is a guess, not "this device tops out at 60"
            teprintln!(
                "Intel Quick Sync: the {FPS_PROBE_LOW} fps probe failed, so the supported \
                 frame-rate range is unknown; capping at {INTEL_FPS_QUERY_FALLBACK} fps as a guess"
            );
            requested.min(INTEL_FPS_QUERY_FALLBACK)
        }
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
