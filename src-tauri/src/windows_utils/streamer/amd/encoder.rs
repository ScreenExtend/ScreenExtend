//! AMD AMF H.264 encoder backend, the Radeon sibling of `nvidia/encoder.rs`.
//!
//! Tuned for latency over PSNR: `ULTRA_LOW_LATENCY` usage (`TRANSCODING` holds 3 frames, 50 ms at
//! 60 fps, before any output), one surface in flight on one thread (submitting ahead adds queue
//! depth x frame time), and no periodic IDR. The reason for each property sits beside its write
//! in `apply_properties`.
//!
//! Nothing here has encoded a frame on a Radeon yet: the FFI is asserted by `test/amd_layout.rs`
//! and the behaviour by the fake runtime in `test/amd_fake.rs`.

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

/// The DXGI format the input ring starts in.
///
/// VCN wants NV12, but an unscaled capture hands over BGRA with no blt at all, so the GPU ring
/// starts there. AMD state that packed-RGB input runs an internal GPU colour converter that
/// competes with the foreground application, so the encoder logs when it is active. A first frame
/// in another format re-initialises the component to match (`retune_input_format`).
pub const PREFERRED_INPUT_FORMAT: DXGI_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM;

/// Input textures in the ring. One would do with one frame in flight; two means a surface whose
/// release the driver defers by a frame is never overwritten while AMF still reads it.
const NUM_IN_FLIGHT: usize = 2;

/// How long one submit plus collect may take before it is a stalled encoder. A healthy frame
/// takes single-digit milliseconds: this is a hang detector, not a budget.
const SUBMIT_DEADLINE: Duration = Duration::from_millis(2000);

/// The poll interval with `amf_poll_backoff` off. Only reached when the driver does not honour
/// `QueryTimeout`.
const POLL_SLEEP: Duration = Duration::from_millis(1);

/// How long teardown waits for `Drain` to reach `AMF_EOF`. Shutdown does not need the drained
/// bytes and `Terminate` alone is legal, so this only bounds the courtesy of letting AMF finish,
/// while the session teardown and a rejoin wait behind it.
const DRAIN_DEADLINE: Duration = Duration::from_millis(100);

/// `IDRPeriod` = 0: AMF's encode documentation, "IDRPeriod = 0 turns IDR off" (range 0-1000),
/// and the value `ForcePictureType` asks for.
const IDR_PERIOD_OFF: i64 = 0;

/// `IDRPeriod` = `INT_MAX`: what Sunshine and FFmpeg write for an infinite GOP. Outside the
/// documented range, so it is the fallback, not the first choice.
const INFINITE_IDR_PERIOD: i64 = i32::MAX as i64;

/// `FrameRate`'s documented range is `1*den .. 120*den` (AMF encode doc, every version since
/// 1.4.29). A runtime that enforces it refuses a 144/165/240 Hz client's rate outright.
const FRAMERATE_DOC_MAX: u32 = 120;

/// `MaxOfLTRFrames` accepts 0..=2, further bounded by the level's DPB limit.
pub const MAX_LTR_FRAMES_SUPPORTED: i64 = 2;

/// Every behaviour choice this backend makes that a measurement may want to flip, read once per
/// encoder in `open` and kept as plain bools.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AmfSwitches {
    /// ON: `IDRPeriod` 0 first, `i32::MAX` if refused. OFF: `i32::MAX` first, 0 if refused.
    pub(crate) idr_period_zero: bool,
    /// Candidate: once unrequested IDRs arrive at a regular interval, write the other "no
    /// periodic IDR" value live. OFF: log the period and never write `IDRPeriod` after `Init`.
    pub(crate) idr_period_flip: bool,
    /// ON: a `FrameRate` above AMF's documented 120 that the runtime refuses is retried at 120.
    /// OFF: the real rate is the only one tried, and a refusal is fatal.
    pub(crate) framerate_fallback: bool,
    /// ON: while AMF holds a clamped `FrameRate`, Target/Peak are scaled by `held / real` so the
    /// bits per second still match the target. OFF: the session bitrate is written as is.
    pub(crate) framerate_rc_scale: bool,
    /// ON: `ProfileLevel` from the geometry and frame rate, clamped to the adapter's cap. OFF:
    /// AMF's default (4.2, raised by frame size only).
    pub(crate) profile_level: bool,
    /// ON: `LTRMode = KEEP_UNUSED` whenever LTR is on. OFF: AMF's `RESET_UNUSED` default.
    pub(crate) ltr_keep_unused: bool,
    /// ON: spin, yield, then 200 us sleeps between empty polls. OFF: `Sleep(1)`.
    pub(crate) poll_backoff: bool,
    /// ON: time the first `NotReady` `QueryOutput`s and stop trusting a `QueryTimeout` that
    /// returns early. OFF: trust the property read-back alone.
    pub(crate) measure_query_timeout: bool,
    /// ON: when `RateControlMethod = CONSTANT_QP` does not stick, apply the bitrate block so the
    /// session is not left at the usage's default bitrate. OFF: log it only.
    pub(crate) cqp_fallback_rate: bool,
    /// ON: the bitrate tier writes `EnforceHRD = true`, which is what makes AMF hold the one-frame
    /// VBV, and the constant-QP tier leaves it alone. OFF: `false` on every tier.
    pub(crate) enforce_hrd: bool,
    /// Candidate: cap every access unit at three frames' worth of bits with `MaxAUSize`.
    pub(crate) max_au_size: bool,
    /// ON: `RateControlSkipFrameEnable = false`, so the rate controller can never answer a hard
    /// frame by skipping it. OFF: the usage's default, which the header leaves unspecified.
    pub(crate) rc_skip_off: bool,
    /// Candidate: ask for per-frame statistics and keep the frame QP AMF reports, for a link
    /// governor and the telemetry. Costs one property read per frame and an unknown amount of
    /// VCN work.
    pub(crate) statistics_feedback: bool,
    /// Candidate: `InputQueueSize = 1`. No reference streamer writes it, AMF documents it as a
    /// storage setting, and if the runtime counts a surface until its output is released it
    /// would make every submit meet `AMF_INPUT_FULL`.
    pub(crate) input_queue_one: bool,
    /// ON: `set_qp` writes `QPI`/`QPP`/`QPB` live (the header files them as dynamic). OFF: it
    /// flushes and re-initialises with the new QP, which is what OBS does and costs an IDR.
    pub(crate) qp_live: bool,
}

impl AmfSwitches {
    fn read() -> Self {
        use super::super::tune;
        Self {
            idr_period_zero: tune::on("amf_idr_period_zero"),
            idr_period_flip: tune::candidate("amf_idr_period_flip"),
            framerate_fallback: tune::on("amf_framerate_fallback"),
            framerate_rc_scale: tune::on("amf_framerate_rc_scale"),
            profile_level: tune::on("amf_profile_level"),
            ltr_keep_unused: tune::on("amf_ltr_keep_unused"),
            poll_backoff: tune::on("amf_poll_backoff"),
            measure_query_timeout: tune::on("amf_query_timeout_measure"),
            cqp_fallback_rate: tune::on("amf_cqp_fallback_rate"),
            enforce_hrd: tune::on("amf_enforce_hrd"),
            max_au_size: tune::candidate("amf_max_au_size"),
            rc_skip_off: tune::on("amf_rc_skip_off"),
            statistics_feedback: tune::candidate("amf_statistics_feedback"),
            input_queue_one: tune::candidate("amf_input_queue_one"),
            qp_live: tune::on("amf_qp_live"),
        }
    }

    /// What ships: every `tune::on` switch on and every candidate off, without reading the
    /// environment.
    #[cfg(test)]
    pub(crate) const fn shipped() -> Self {
        Self {
            idr_period_zero: true,
            idr_period_flip: false,
            framerate_fallback: true,
            framerate_rc_scale: true,
            profile_level: true,
            ltr_keep_unused: true,
            poll_backoff: true,
            measure_query_timeout: true,
            cqp_fallback_rate: true,
            enforce_hrd: true,
            max_au_size: false,
            rc_skip_off: true,
            statistics_feedback: false,
            input_queue_one: false,
            qp_live: true,
        }
    }
}

/// The usage to configure after AMD's cheap ULTRA_LOW_LATENCY probe (set `Usage`, read a
/// property back): ULL when the read worked, LOW_LATENCY when it did not. ULL breaks `Init()` on
/// pre-RDNA parts from Adrenalin 23.7.1 (AMF issue #410); AMD suggest TRANSCODING there, but it
/// holds three frames.
pub(crate) fn usage_after_probe(probe_ok: bool) -> AMF_VIDEO_ENCODER_USAGE {
    if probe_ok {
        AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY
    } else {
        AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY
    }
}

/// The usage to retry `Init()` under after it failed under `failed`, if any: ULL falls back to
/// LOW_LATENCY (Sunshine's open-retry), and nothing falls back further. The two differ only in
/// rate control, VBV size and `LowLatencyInternal`, all of which are written explicitly anyway.
pub(crate) fn retry_usage_after_init_failure(
    failed: AMF_VIDEO_ENCODER_USAGE,
) -> Option<AMF_VIDEO_ENCODER_USAGE> {
    (failed == AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY)
        .then_some(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY)
}

/// `IDRPeriod` values in the order they are tried. Both mean "no periodic IDR"; which one a
/// runtime accepts is not established, so a refusal of the first is never fatal.
pub(crate) fn idr_period_candidates(zero_first: bool) -> [i64; 2] {
    if zero_first {
        [IDR_PERIOD_OFF, INFINITE_IDR_PERIOD]
    } else {
        [INFINITE_IDR_PERIOD, IDR_PERIOD_OFF]
    }
}

/// The other "no periodic IDR" value, for a runtime that turned out to emit periodic IDRs under
/// `current`. `None` when `current` is neither candidate (the write was refused and the usage's
/// own period is in force, which neither candidate was able to replace).
pub(crate) fn other_idr_period(current: i64) -> Option<i64> {
    match current {
        IDR_PERIOD_OFF => Some(INFINITE_IDR_PERIOD),
        INFINITE_IDR_PERIOD => Some(IDR_PERIOD_OFF),
        _ => None,
    }
}

/// `FrameRate` numerators in the order they are tried: the real rate, then, only when it is
/// above AMF's documented ceiling and the fallback is on, the ceiling.
pub(crate) fn frame_rate_candidates(fps: u32, fallback: bool) -> Vec<u32> {
    let fps = fps.max(1);
    if fallback && fps > FRAMERATE_DOC_MAX {
        vec![fps, FRAMERATE_DOC_MAX]
    } else {
        vec![fps]
    }
}

/// The Target/Peak bitrate to write when AMF's rate controller holds `rc_fps` but pictures arrive
/// at `fps`. AMF budgets a frame as bitrate / FrameRate, so at 240 fps under a held 120 the
/// unscaled target would put twice the session's bits on the wire. Scaled by `rc_fps / fps`, one
/// frame's budget is one real frame of the target, the same as the one-frame VBV beside it.
pub(crate) fn rc_target_bitrate(bps: i64, fps: u32, rc_fps: u32) -> i64 {
    let bps = bps.max(1);
    let (fps, rc_fps) = (fps.max(1) as i128, rc_fps.max(1) as i128);
    if rc_fps >= fps {
        return bps;
    }
    ((bps as i128 * rc_fps / fps) as i64).max(1)
}

/// Whether an `IDRPeriod` write that returned `AMF_OK` actually took. A read-back that differs
/// (a runtime that clamps `i32::MAX` to its own maximum, say) is a refusal while another candidate
/// is left to try; on the last candidate it is accepted and reported. An unreadable read-back is
/// not evidence against the write.
pub(crate) fn idr_period_took(asked: i64, read: Option<i64>, last_candidate: bool) -> bool {
    last_candidate || read.is_none_or(|v| v == asked)
}

/// Whether the gap since the previous IDR (of any kind) matches the gap before the previous
/// unrequested one, within a frame: the shape of a period, rather than one IDR the runtime
/// inserted for its own reasons (a resync, a forced bitfield naming an empty slot).
pub(crate) fn is_periodic_idr(prev_unrequested_gap: Option<u64>, gap: u64) -> bool {
    gap > 1 && matches!(prev_unrequested_gap, Some(p) if p.abs_diff(gap) <= 1)
}

/// Whether to write the other `IDRPeriod` value live: only behind the switch, only once, and only
/// on evidence of a period.
pub(crate) fn should_flip_idr_period(switch_on: bool, already: bool, periodic: bool) -> bool {
    switch_on && !already && periodic
}

/// The `ProfileLevel` to write, as an `AMF_H264_LEVEL` (whose values are `level_idc`), or `None`
/// to leave AMF's default. It is the level the SDP answer advertises (`negotiate::level_for`), so
/// the SPS and the SDP agree. Written only when the adapter reported its maximum, and clamped to
/// it: a level AMF cannot reach fails `Init()`, where AMF's own default is at worst a mislabel.
pub(crate) fn profile_level_for(width: u32, height: u32, fps: u32, max_level: i64) -> Option<i64> {
    if max_level <= 0 {
        return None;
    }
    let want = crate::streamer::negotiate::level_for(width, height, fps) as i64;
    Some(want.min(max_level))
}

/// Whether a CONSTANT_QP request was read back as some other rate control. An unreadable
/// read-back (`UNKNOWN`) is not evidence either way and is left alone.
pub(crate) fn cqp_was_refused(
    qp_requested: bool,
    readback: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
) -> bool {
    qp_requested
        && readback != AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN
        && readback != AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP
}

/// The rate-control tier a session runs, decided from what was asked for and what
/// `RateControlMethod` read back after `Init()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateTier {
    /// Constant QP: the quality is pinned and the bitrate follows the content. The tier for a fast
    /// link, and what a default join asks for. A link governor moves it with
    /// [`Encoder::set_constant_qp`], which takes effect on the next picture with no IDR.
    ConstantQp,
    /// Constant QP was asked for and the driver kept its usage's own rate control (reported on
    /// GCN after ULL). With `amf_cqp_fallback_rate` it runs at the session bitrate with the
    /// requested QP as a floor, so [`Encoder::set_bitrate`] acts and
    /// [`Encoder::set_constant_qp`] moves the floor.
    ConstantQpRefused,
    /// CBR, peak equal to target, a one-frame VBV: the tier for a link the stream has to fit. A
    /// link governor moves it with [`Encoder::set_bitrate`].
    LinkCbr,
}

pub(crate) fn select_rate_tier(
    qp_requested: bool,
    readback: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
) -> RateTier {
    if !qp_requested {
        RateTier::LinkCbr
    } else if cqp_was_refused(qp_requested, readback) {
        RateTier::ConstantQpRefused
    } else {
        RateTier::ConstantQp
    }
}

/// What a QP change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QpApplied {
    /// The new QP holds from the next submitted picture, with no IDR: `QPI`/`QPP` under constant
    /// QP, `MinQP` when the driver refused constant QP and the session runs at a bitrate.
    Applied,
    /// The session runs CBR, or a refused constant QP was left at the usage's own rate control;
    /// its control input is the bitrate.
    NotConstantQp,
}

/// Whether an SPS that declares `max_num_ref_frames` can carry one short-term reference plus
/// `ltr` long-term ones. H.264 counts both against that field, and a decoder sizes its sliding
/// window from it: a stream whose SPS leaves the long-term pictures out is evicted from under
/// the repair that references them, which paints garbage with no decode error.
pub(crate) fn ltr_fits_sps(max_num_ref_frames: u32, ltr: i64) -> bool {
    // one more than the long-term count: the short-term reference
    i64::from(max_num_ref_frames) > ltr.max(0)
}

/// The first candidate `attempt` accepts, or every reason they were refused. Used to walk the AMD
/// adapters: on a Ryzen APU with a Radeon RX 6400/6500 (Navi 24, which has no encoder) the first
/// AMD adapter DXGI lists may be the one that cannot encode, while the APU next to it can.
pub(crate) fn first_that_works<C, T>(
    candidates: Vec<(String, C)>,
    mut attempt: impl FnMut(C) -> Result<T>,
) -> Result<(String, T)> {
    let mut refused = Vec::new();
    for (name, candidate) in candidates {
        match attempt(candidate) {
            Ok(v) => return Ok((name, v)),
            Err(e) => refused.push(format!("{name}: {e:#}")),
        }
    }
    if refused.is_empty() {
        bail!("no AMD adapter found; AMF requires an AMD Radeon/Ryzen GPU");
    }
    bail!("no AMD adapter could run AMF ({})", refused.join("; "))
}

/// The documented range of `PeakBitrate` (AMF encode documentation: 10 000 - 100 000 000 bit/s).
/// `TargetBitrate` is held to the same range, which the documentation does not state for it.
const BITRATE_DOC_MIN: i64 = 10_000;
const BITRATE_DOC_MAX: i64 = 100_000_000;

/// A bitrate the runtime can take: inside the documented range and under the adapter's
/// `MaxBitrate` cap when it reported one. Above the range `Init()` would fail for the whole tier,
/// and a live raise would be refused on every retry.
pub(crate) fn clamp_bitrate(bps: i64, cap: i64) -> i64 {
    let hi = if cap > 0 {
        cap.min(BITRATE_DOC_MAX)
    } else {
        BITRATE_DOC_MAX
    };
    bps.clamp(BITRATE_DOC_MIN, hi.max(BITRATE_DOC_MIN))
}

/// Where an output AMF handed back belongs, judged by the identity this encoder wrote on the
/// input surface and AMF carried to the output buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputFate {
    /// The picture being collected.
    Mine,
    /// A second output for a picture already delivered: the same surface was encoded twice.
    Duplicate,
    /// An earlier picture that was never delivered (it was given up on or harvested).
    Stale,
    /// No identity on the buffer: this runtime does not carry surface properties to the output.
    Unlabelled,
}

/// Where a picture's identity is read from on the output buffer, learnt from the first output
/// of the session, which can only be picture 1's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentitySource {
    /// No output seen yet.
    Unknown,
    /// The private property set on the input surface came back on the output buffer. This is
    /// how OBS (`L"PTS"`) and FFmpeg (`amfenc.c`) match outputs to pictures, so it is the
    /// better-evidenced carrier.
    Property,
    /// The property did not survive, but the PTS set on the surface did (`SetPts` in, `GetPts`
    /// out). OBS treats the output PTS as a decode time, so it follows submission order; with no
    /// B-frames and one picture in flight that is the same order.
    Pts,
    /// Neither came back. Outputs are matched by position, and a surface is never submitted
    /// twice, because a second encode could not be told from the next picture's output.
    Unavailable,
}

/// Which source picture `expected`'s output proves: the property first, the PTS second.
/// Picture ids start at 1, so a runtime that zeroes the PTS cannot pass for one that keeps it.
pub(crate) fn learn_identity_source(
    expected: i64,
    pts: Option<i64>,
    prop: Option<i64>,
    pts_step: i64,
) -> IdentitySource {
    if prop == Some(expected) {
        IdentitySource::Property
    } else if pts == Some(expected * pts_step) {
        IdentitySource::Pts
    } else {
        IdentitySource::Unavailable
    }
}

/// The PTS of an output as a picture id, when it can be one.
fn pts_identity(pts: Option<i64>, pts_step: i64) -> Option<i64> {
    pts.filter(|&p| p > 0 && pts_step > 0 && p % pts_step == 0)
        .map(|p| p / pts_step)
}

/// The picture an output belongs to, read per output: the property, then the PTS. The learnt
/// source only says whether identities can be trusted at all; one output missing the property
/// (a runtime might leave it off a forced IDR carrying parameter sets) still has its PTS, and
/// dropping it as stale would turn that picture into a stall and a re-init.
pub(crate) fn resolve_identity(
    source: IdentitySource,
    pts: Option<i64>,
    prop: Option<i64>,
    pts_step: i64,
) -> Option<i64> {
    match source {
        IdentitySource::Property | IdentitySource::Pts => {
            prop.or_else(|| pts_identity(pts, pts_step))
        }
        IdentitySource::Unknown | IdentitySource::Unavailable => None,
    }
}

/// What `AMF_NEED_MORE_INPUT` from `SubmitInput` has shown it means on this runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NeedMoreInputMeaning {
    Unknown,
    /// The surface was taken (OBS's reading): its output follows.
    Taken,
    /// The surface was not taken (AMD's samples' reading): it must be submitted again.
    NotTaken,
}

/// How many times evidence may reverse a learnt meaning. A runtime that contradicts itself more
/// often ends at "taken": a wrong "taken" costs the stall path (a 2 s wait and a re-init with an
/// IDR) on each occurrence, a wrong "not taken" a duplicate encode, a damaged picture and an IDR.
pub(crate) const NEED_MORE_INPUT_FLIPS: u32 = 2;

/// The meaning after a piece of evidence, and the flips spent. Learning from `Unknown` is free;
/// a reversal costs one flip; once they are spent the meaning settles on "taken" and stays.
pub(crate) fn revise_need_more_input(
    current: NeedMoreInputMeaning,
    evidence: NeedMoreInputMeaning,
    flips: u32,
) -> (NeedMoreInputMeaning, u32) {
    use NeedMoreInputMeaning::*;
    match (current, evidence) {
        (_, Unknown) => (current, flips),
        (Unknown, e) => (e, flips),
        (c, e) if c == e => (c, flips),
        _ if flips >= NEED_MORE_INPUT_FLIPS => (Taken, flips),
        (_, e) => (e, flips + 1),
    }
}

/// What to do on `AMF_NEED_MORE_INPUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NeedMoreInputAction {
    /// Treat the surface as taken and collect its output.
    Accept,
    /// Not known yet: look for this picture's output for a window, and submit once more if it
    /// has not come. Happens at most once per session.
    Probe,
    /// Known not to be taken: submit the same surface again at once.
    ResubmitNow,
}

pub(crate) fn need_more_input_action(
    source: IdentitySource,
    meaning: NeedMoreInputMeaning,
    resubmits: u32,
) -> NeedMoreInputAction {
    // without an identity a resubmitted copy would pass for the next picture's output
    if !matches!(source, IdentitySource::Pts | IdentitySource::Property) {
        return NeedMoreInputAction::Accept;
    }
    match meaning {
        NeedMoreInputMeaning::Taken => NeedMoreInputAction::Accept,
        NeedMoreInputMeaning::NotTaken if resubmits < NEED_MORE_INPUT_RESUBMITS => {
            NeedMoreInputAction::ResubmitNow
        }
        NeedMoreInputMeaning::NotTaken => NeedMoreInputAction::Accept,
        NeedMoreInputMeaning::Unknown if resubmits == 0 => NeedMoreInputAction::Probe,
        NeedMoreInputMeaning::Unknown => NeedMoreInputAction::Accept,
    }
}

/// The order target and peak are written in, so that target never exceeds peak at any step:
/// raising, peak first; lowering, target first.
pub(crate) fn rate_write_order(from_target: Option<i64>, to_target: i64) -> [&'static str; 2] {
    match from_target {
        Some(from) if to_target > from => ["PeakBitrate", "TargetBitrate"],
        _ => ["TargetBitrate", "PeakBitrate"],
    }
}

pub(crate) fn output_fate(
    reported: Option<i64>,
    current: i64,
    delivered: Option<i64>,
) -> OutputFate {
    match reported {
        None => OutputFate::Unlabelled,
        Some(id) if delivered.is_some_and(|d| id <= d) => OutputFate::Duplicate,
        Some(id) if id == current => OutputFate::Mine,
        Some(_) => OutputFate::Stale,
    }
}

/// How long to look for a picture's own output after `SubmitInput` answered
/// `AMF_NEED_MORE_INPUT`, before deciding the surface was not taken: three times the session's
/// measured encode p99, floored at two frame periods and 4 ms, within the stall deadline. Sized
/// from the encoder so a slow 4K or 240 Hz encode is not mistaken for a refused frame.
pub(crate) fn need_more_input_window(
    fps: u32,
    encode_p99: Option<Duration>,
    deadline: Duration,
) -> Duration {
    let period = Duration::from_micros(1_000_000 / u64::from(fps.max(1)));
    let measured = encode_p99.map_or(Duration::ZERO, |p| p * 3);
    (period * 2)
        .max(measured)
        .max(Duration::from_millis(4))
        .min(deadline)
}

/// Encode times kept for the window above.
const ENCODE_TIME_SAMPLES: usize = 120;

/// How often one picture is submitted again after `AMF_NEED_MORE_INPUT` with no output, before
/// the loop falls back to waiting for the stall deadline.
const NEED_MORE_INPUT_RESUBMITS: u32 = 3;

/// What to pass `AMFInit` when [`AMFINIT_VERSION`] was refused: the runtime's own version, when
/// it is known and lower. `None` means there is nothing better to try.
pub(crate) fn init_version_retry(runtime: Option<amf_uint64>) -> Option<amf_uint64> {
    runtime.filter(|&v| v > 0 && v < AMFINIT_VERSION)
}

/// What the poll loop does after a `QueryOutput` that found nothing, on a runtime that does not
/// block inside `QueryOutput` for us. `attempt` counts from 1 within one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollWait {
    Spin,
    Yield,
    Sleep(Duration),
}

impl PollWait {
    const SPIN_ATTEMPTS: u32 = 64;
    const YIELD_ATTEMPTS: u32 = 256;
    const BACKOFF_SLEEP: Duration = Duration::from_micros(200);

    /// A frame microseconds from done is caught by the spin; only a slow one reaches a sleep, of
    /// 200 us, not the 1-2 ms `Sleep(1)` really is under `timeBeginPeriod(1)`.
    pub(crate) fn after(attempt: u32, backoff: bool) -> Self {
        if !backoff {
            return Self::Sleep(POLL_SLEEP);
        }
        if attempt <= Self::SPIN_ATTEMPTS {
            Self::Spin
        } else if attempt <= Self::YIELD_ATTEMPTS {
            Self::Yield
        } else {
            Self::Sleep(Self::BACKOFF_SLEEP)
        }
    }

    fn wait(self) {
        match self {
            Self::Spin => std::hint::spin_loop(),
            Self::Yield => std::thread::yield_now(),
            Self::Sleep(d) => std::thread::sleep(d),
        }
    }
}

/// Decides, from how long the first `NotReady` `QueryOutput` calls took, whether the runtime
/// really waits for `QueryTimeout`. A property echo cannot tell (AMF #516's report is exactly a
/// runtime that echoes and ignores), and a runtime that ignores it turns the collect loop into a
/// hot spin on a vtable call.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct QueryTimeoutProbe {
    samples: u32,
    fast: u32,
}

impl QueryTimeoutProbe {
    const SAMPLES: u32 = 16;
    /// Half the 1 ms that is asked for: a call that waited for the timeout cannot be this quick.
    const FAST: Duration = Duration::from_micros(500);

    /// Record one `NotReady` call. `Some(honoured)` once enough have been seen.
    pub(crate) fn record(&mut self, elapsed: Duration) -> Option<bool> {
        self.samples += 1;
        if elapsed < Self::FAST {
            self.fast += 1;
        }
        (self.samples >= Self::SAMPLES).then_some(self.fast * 2 < self.samples)
    }

    pub(crate) fn fast(&self) -> u32 {
        self.fast
    }

    pub(crate) fn samples(&self) -> u32 {
        self.samples
    }
}

/// Whether the teardown drain should stop after one `QueryOutput` that returned `r`.
/// `AMF_EOF` is the normal end; any result that is not "nothing yet" means the component cannot
/// drain (a lost device answers every call with an error); and the deadline bounds the rest.
pub(crate) fn drain_should_stop(r: AMF_RESULT, elapsed: Duration) -> bool {
    !matches!(r, AMF_OK | AMF_REPEAT | AMF_NEED_MORE_INPUT) || elapsed >= DRAIN_DEADLINE
}

/// What the picture type AMF reported says about the IDR it was or was not asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdrCheck {
    /// As asked, or nothing to judge.
    Fine,
    /// An IDR nobody requested: a periodic IDR the `IDRPeriod` write did not switch off.
    Unrequested,
    /// A forced IDR that came out as something else: a joiner stays black until the next one.
    NotHonoured,
}

/// `is_idr` is `None` when `OutputDataType` could not be read. The first output after `Init()`
/// is an IDR whatever was asked, so it is never "unrequested".
pub(crate) fn check_idr(forced: bool, is_idr: Option<bool>, first_after_init: bool) -> IdrCheck {
    match (forced, is_idr) {
        (_, None) => IdrCheck::Fine,
        (true, Some(false)) => IdrCheck::NotHonoured,
        (false, Some(true)) if !first_after_init => IdrCheck::Unrequested,
        _ => IdrCheck::Fine,
    }
}

/// Whether a count has just reached a power of two: 1, 2, 4, 8 ..., the log cadence for an event
/// that, if it happens at all on a driver, happens on every frame.
fn log_now(count: u64) -> bool {
    count.is_power_of_two()
}

/// Whether to keep inspecting every in-band SPS after reading `ExtraData`. Only an SPS that was
/// read and found already correct lets the per-frame check stand down; an `ExtraData` that is
/// missing, empty or unparseable says nothing about what the in-band SPS of an IDR will carry,
/// and `patch_annexb` is a no-op per SPS when it is right.
pub(crate) fn keep_patching_sps(inspection: &super::sps::SpsInspection) -> bool {
    !matches!(inspection, super::sps::SpsInspection::AlreadyCorrect)
}

/// What one submit took from the encoder's pending requests, so a failed submit can give them
/// back: a transient `SubmitInput` error on a join or PLI frame must not swallow the IDR, and an
/// LTR mark must not be lost while `RepairState` believes it was made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TakenRequests {
    pub(crate) idr: bool,
    pub(crate) mark: Option<i64>,
    pub(crate) force_reference: Option<i64>,
}

/// What of a failed submit's requests may be given back. A submit that failed because the
/// component stalled and was re-`Init`ed has lost every long-term reference, so a forced bitfield
/// naming them would point the next picture at slots the fresh component does not hold. The IDR
/// and the mark survive: the IDR is what the re-init needs, and a mark names a slot to fill, not
/// one to read.
pub(crate) fn requests_after_failure(taken: TakenRequests, reinitialised: bool) -> TakenRequests {
    if reinitialised {
        TakenRequests {
            force_reference: None,
            ..taken
        }
    } else {
        taken
    }
}

/// Puts back what a failed submit took. A request made since (a newer mark or bitfield) wins.
pub(crate) fn restore_requests(
    taken: TakenRequests,
    idr_requested: &mut bool,
    mark_next: &mut Option<i64>,
    force_reference: &mut Option<i64>,
) {
    if taken.idr {
        *idr_requested = true;
    }
    if mark_next.is_none() {
        *mark_next = taken.mark;
    }
    if force_reference.is_none() {
        *force_reference = taken.force_reference;
    }
}

/// What `AMFComponent::GetCaps` reported before `Init()`. Logged in full on construction: for a
/// machine nobody can reach, the log line is the diagnosis.
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

/// Long-term-reference state. `max_frames` is 0 on the inline paths, which have no repair loop;
/// on the async ring it is what `build_amf_ring` asks for, clamped by the adapter and then by
/// AMF's read-back. User LTR excludes intra-refresh and B-frames (AMF's encode documentation).
#[derive(Debug, Clone, Copy, Default)]
struct LtrState {
    max_frames: i64,
    /// Set on the next submitted surface, then cleared.
    mark_next: Option<i64>,
    /// Restricts the next frame's prediction to these LTR indices, then cleared.
    force_reference: Option<i64>,
    /// The slot `mark_ltr` will claim next, rotating over `max_frames`.
    next_slot: i64,
    /// Slots a successfully submitted picture has marked since the last `Init()`. A re-`Init`
    /// empties AMF's slots, and `use_ltr` never names one that is not in here.
    live_slots: u32,
    /// Logged once each: AMF reporting a different mark than asked, or referencing a slot the
    /// forced bitfield excluded. Either is `RepairState` drifting away from the encoder.
    warned_mark_mismatch: bool,
    warned_reference_mismatch: bool,
}

/// The per-session tally behind the unrequested / not-honoured IDR log lines.
#[derive(Debug, Clone, Copy, Default)]
struct IdrAudit {
    /// Outputs delivered since the last `Init()`; the first is an IDR whatever was asked.
    outputs_since_init: u64,
    /// `outputs_since_init` at the newest IDR of any kind.
    last_idr_at: u64,
    /// Outputs between the previous unrequested IDR and the IDR before it.
    last_unrequested_gap: Option<u64>,
    unrequested: u64,
    not_honoured: u64,
}

/// What one `AMFComponent::QueryOutput` produced, as a fact about the call rather than as bytes.
///
/// A **NULL out-pointer** means the picture is not finished and will be handed over later. A
/// **non-NULL** `AMFData` whose buffer is empty has already been consumed and will never be
/// produced again: waiting for it is a poll to `SUBMIT_DEADLINE` and a torn-down session, when
/// the honest outcome is one dropped frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryClass {
    /// NULL out-pointer with a benign result code. Poll again.
    NotReady,
    /// A real output object carrying no bytes. Consumed; do not wait for it.
    Empty,
    /// A real output object with an access unit in it.
    Data,
    /// NULL out-pointer with a result code that is not benign.
    Error,
}

/// The pure half of `Encoder::query_once`, testable with no AMD hardware.
///
/// `AMF_EOF` counts as `NotReady` on a NULL pointer: the only caller that can see it is the
/// teardown drain, which tests the result code itself.
pub(crate) fn classify_query_output(
    data_null: bool,
    buffer_empty: bool,
    r: AMF_RESULT,
) -> QueryClass {
    if data_null {
        return match r {
            AMF_OK | AMF_REPEAT | AMF_NEED_MORE_INPUT | AMF_EOF => QueryClass::NotReady,
            _ => QueryClass::Error,
        };
    }
    if buffer_empty {
        QueryClass::Empty
    } else {
        QueryClass::Data
    }
}

/// [`QueryClass`] plus the bytes, for the caller that has to act on it.
enum QueryOutcome {
    NotReady,
    /// A consumed output with no bytes, and the PTS and identity property it carried.
    Empty(Option<i64>, Option<i64>),
    Au(AuOut),
}

/// What the identity checks have seen. Logged at powers of two; read by the tests.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IdentityCounts {
    /// `SubmitInput` answered `AMF_NEED_MORE_INPUT` and the picture's own output followed.
    pub(crate) need_more_input_consumed: u64,
    /// ... and nothing followed within the window, so the surface was submitted again.
    pub(crate) need_more_input_resubmitted: u64,
    /// Second outputs for an already delivered picture, dropped.
    pub(crate) duplicates_dropped: u64,
    /// Outputs of earlier pictures that were never delivered, dropped.
    pub(crate) stale_dropped: u64,
    /// Outputs with no identity on them.
    pub(crate) unlabelled: u64,
    /// Foreign outputs that carried no bytes, counted and otherwise ignored.
    pub(crate) stray_empty: u64,
    /// IDRs this encoder wanted on its own account that its keyframe gate held back.
    pub(crate) own_idrs_suppressed: u64,
    /// Outputs whose PTS and private property named different pictures (the property wins).
    pub(crate) identity_mismatch: u64,
    /// Outputs that lacked the property in a session that otherwise carries it, matched by
    /// their PTS instead.
    pub(crate) property_missing: u64,
    /// Pictures that came back as several buffers (slice output) and were joined.
    pub(crate) multi_buffer_pictures: u64,
}

/// One access unit and what AMF said about it on the output buffer.
struct AuOut {
    bytes: Vec<u8>,
    /// `GetPts` on the buffer: the PTS set on the input surface, if the runtime carried it.
    pts: Option<i64>,
    /// The private identity property, as the output buffer reports it.
    prop_id: Option<i64>,
    /// `OutputBufferType`: FRAME, or SLICE / SLICE_LAST when a picture comes back in parts.
    buffer_type: Option<i64>,
    /// `StatisticsFeedbackFrameQP` (the rate controller's QP for the picture), else
    /// `StatisticsFeedbackAvgQP`, when statistics were asked for.
    qp: Option<u8>,
    /// `OutputDataType == IDR`; `None` when the property could not be read.
    idr: Option<bool>,
    /// `MarkedLTRIndex`, read only when this picture was asked to mark one.
    marked: Option<i64>,
    /// `ReferencedLTRIndexBitfield`, read only when this picture was given a forced bitfield.
    referenced: Option<i64>,
}

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
    /// How many textures `build_ring` makes. [`NUM_IN_FLIGHT`] for a self-driven encoder; the
    /// pipeline's async ring asks for its own K and then owns slot selection.
    ring_len: usize,
    ring_next: usize,
    last_slot: Option<usize>,
    ring_format: DXGI_FORMAT,
    surface_format: AMF_SURFACE_FORMAT,

    config: EncoderConfig,
    caps: AmfCaps,
    usage: AMF_VIDEO_ENCODER_USAGE,
    rate_control: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
    query_timeout_supported: bool,
    /// `Some` until the first sixteen `NotReady` returns have said whether `QueryTimeout` is
    /// really honoured (`amf_query_timeout_measure`).
    query_timeout_probe: Option<QueryTimeoutProbe>,
    /// The CQP request did not stick, so `set_bitrate` drives the bitrate after all.
    cqp_not_honoured: bool,
    rate_tier: RateTier,
    switches: AmfSwitches,
    /// How long one submit and collect may take before it is a stall. `SUBMIT_DEADLINE`, except
    /// in the fake-runtime tests, which cannot wait two seconds per scripted stall.
    submit_deadline: Duration,
    /// No picture has been submitted since the last `Init()`: the next one is an IDR whatever is
    /// asked, and it must carry SPS and PPS, which AMF only inserts on request when
    /// `HeaderInsertionSpacing` is 0.
    fresh_init: bool,
    /// The QP AMF reported for the newest picture that asked for statistics.
    last_qp: Option<u8>,
    /// The identity written on the next picture's surface; one per picture, kept across its
    /// resubmissions and never reused.
    next_picture_id: i64,
    /// The identity of the newest picture delivered.
    delivered_id: Option<i64>,
    identity: IdentityCounts,
    /// Where outputs carry their picture's identity, learnt from the first output.
    identity_source: IdentitySource,
    nmi_meaning: NeedMoreInputMeaning,
    /// The picture submitted a second time to learn what `AMF_NEED_MORE_INPUT` means.
    nmi_probe: Option<i64>,
    /// The newest picture submitted a second time, under any meaning: a second output for it
    /// proves "taken" whatever was learnt before.
    last_resubmitted: Option<i64>,
    nmi_flips: u32,
    /// A resync IDR the keyframe limiter held back, retried at every submit until admitted.
    own_idr_deferred: bool,
    /// When the current picture's surface was taken, for the encode time.
    accepted_at: Instant,
    encode_times: std::collections::VecDeque<Duration>,
    /// The same window and bounds the transport's keyframe gate uses, for the IDRs this encoder
    /// asks for on its own account (a stray or duplicate output), so they cannot storm.
    own_idr_gate: crate::streamer::keyframe::KeyframeGate,
    warned_multi_buffer: bool,
    warned_identity_mismatch: bool,
    /// A live QP change waiting for the next picture's statistics to confirm it.
    qp_check: Option<u8>,
    qp_unconfirmed: u64,
    qp_unchecked: u64,
    /// `video_full_range_flag` for the VUI: what `FullRangeColor` read back as after `Init()`.
    vui_full_range: bool,
    /// Target, peak and VBV as last written successfully, so a refused write can be undone.
    written_rate: std::cell::Cell<Option<(i64, i64, i64)>>,

    frame_index: u64,
    idr_requested: bool,
    /// What the picture in flight asked for, so the output can be judged against it.
    in_flight: TakenRequests,
    idr_audit: IdrAudit,
    /// Submits that produced nothing for `SUBMIT_DEADLINE` and cost a re-`Init`.
    stalls: u64,
    /// A periodic IDR has been acted on, by the live `IDRPeriod` write or by the log line that
    /// stands in for it with `amf_idr_period_flip` off (once per encoder).
    idr_period_flipped: bool,
    /// The `FrameRate` AMF's rate controller holds: the real rate, or 120 after a refusal.
    rc_fps: std::cell::Cell<u32>,
    ltr: LtrState,
    /// The pipeline's IDR atomic, so a PLI reaches the encoder wherever the encode happens,
    /// including the ring's idle keepalive, which never goes through the capture-side stage.
    shared_idr: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    warned_input_full: bool,
    warned_empty_output: bool,

    /// Retained, grown-only copy target for the access unit. The copy out of AMF's buffer
    /// happens while the driver still owns the mapping, so it must not allocate; the `Vec` the
    /// pipeline wants is built afterwards.
    au_scratch: Vec<u8>,

    /// `(texture pointer, format)` of the last source texture `encode_texture` looked at, so the
    /// no-scaler path does not call `GetDesc` per frame for a value that only changes with the
    /// display mode.
    src_format_cache: Option<(usize, DXGI_FORMAT)>,

    /// Whether every in-band SPS is still inspected (and rewritten when it lacks the VUI we
    /// want). Cleared only when `ExtraData`'s SPS was read and found already correct
    /// (`keep_patching_sps`).
    patch_sps_vui: bool,

    /// Declared **last** so it drops last: dropping the `Library` unmaps the code every vtable
    /// above points into. `None` only under the test suite's fake runtime, which has no DLL.
    amf: Option<Amf>,
}

// SAFETY: the AMF objects are owned exclusively by this struct and only ever touched from the
// thread that owns it. The pipeline moves an encoder between threads but never shares one.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Builds an encoder on its own D3D11 device, picked from the first non-software adapter
    /// whose PCI vendor id is AMD's. Used by the probe harness and the CPU-bridge fallback.
    pub fn new(config: EncoderConfig) -> Result<Self> {
        Self::new_with_input_format(config, PREFERRED_INPUT_FORMAT)
    }

    /// As [`Self::new`], with the input ring built in `format` from the start.
    ///
    /// The CPU bridge (`Backend::AmfCpu`) is fed NV12 by the scaler (an NV12 readback is 740 us
    /// against BGRA's 1192 at 1920x1080 -> 1280x720); built in the default BGRA, its first frame
    /// would pay `retune_input_format` and a second `Init()`. A format AMF has no surface type
    /// for is refused here rather than at the first frame.
    pub fn new_with_input_format(config: EncoderConfig, format: DXGI_FORMAT) -> Result<Self> {
        if amf_surface_format(format).is_none() {
            bail!("no AMF surface format for DXGI format {format:?}; AMF cannot encode it");
        }
        let (name, encoder) = first_that_works(amd_adapters()?, |adapter| {
            let (device, context) = create_device_on(&adapter)?;
            Self::build(config, device, context, 0, NUM_IN_FLIGHT, format)
        })?;
        tprintln!("selected AMD adapter for AMF ({name})");
        Ok(encoder)
    }

    /// Builds an encoder on the **capture** device, which is what makes
    /// `CreateSurfaceFromDX11Native` a pointer wrap instead of a cross-device copy. This is the
    /// common AMD case: on a Radeon desktop or a Ryzen APU laptop the display, the capture and
    /// the VCN encoder are all one adapter.
    pub fn new_on_device(
        config: EncoderConfig,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<Self> {
        Self::new_on_device_with_ltr(config, device, context, 0, NUM_IN_FLIGHT)
    }

    /// As [`Self::new_on_device`], with `slots` input textures and up to `max_ltr_frames`
    /// long-term references.
    ///
    /// A non-zero LTR budget switches the encoder from ULL's rolling intra-refresh to
    /// user-controlled LTR. The two are mutually exclusive, so only a caller that acts on
    /// receipts (the async ring, driving `streamer::repair::RepairState`) should ask for it.
    /// What AMF granted is [`Self::ltr_slots`].
    ///
    /// `slots` is [`NUM_IN_FLIGHT`] for an encoder that picks its own slot; the async ring asks
    /// for its own K and then owns slot selection through [`Self::encode_slot`].
    pub fn new_on_device_with_ltr(
        config: EncoderConfig,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        max_ltr_frames: i64,
        slots: usize,
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
            slots.max(1),
            PREFERRED_INPUT_FORMAT,
        )
    }

    fn build(
        config: EncoderConfig,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        max_ltr_frames: i64,
        slots: usize,
        format: DXGI_FORMAT,
    ) -> Result<Self> {
        Self::open(config, device, context, max_ltr_frames)?.finish(slots, format)
    }

    /// [`Self::build`] on a factory that is already in hand, with the switches given rather than
    /// read from the environment: the entry point of the fake-runtime tests.
    #[cfg(test)]
    pub(crate) fn build_on_factory(
        factory: *mut AMFFactory,
        switches: AmfSwitches,
        config: EncoderConfig,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        max_ltr_frames: i64,
        ring: (usize, DXGI_FORMAT),
    ) -> Result<Self> {
        Self::open_on_factory(
            None,
            factory,
            switches,
            config,
            device,
            context,
            max_ltr_frames.clamp(0, MAX_LTR_FRAMES_SUPPORTED),
        )?
        .finish(ring.0, ring.1)
    }

    fn finish(mut self, slots: usize, format: DXGI_FORMAT) -> Result<Self> {
        self.ring_len = slots.max(1);

        self.build_ring(format)?;
        self.configure_and_init()?;
        self.read_extradata();

        let config = self.config;
        tprintln!(
            "AMD AMF H.264 encoder initialized (D3D11 / {}): {}x{}@{}, bitrate_bps={}, \
             input={}, profile={:?}, rc={}, tier={:?}, qp={:?}, ltr_frames={}, query_timeout={}",
            usage_name(self.usage),
            config.width,
            config.height,
            config.fps,
            config.bitrate_bps,
            surface_format_name(self.surface_format),
            config.profile,
            rate_control_name(self.rate_control),
            self.rate_tier,
            config.qp,
            self.ltr.max_frames,
            if self.query_timeout_supported {
                "1ms-blocking"
            } else {
                "poll+sleep"
            },
        );

        Ok(self)
    }

    /// Everything up to and including the capability report: load the runtime, take a context on
    /// `device`, create the AVC component and refuse a non-hardware one. Separate from
    /// [`Self::build`] so [`probe_availability`] can stop here, without paying for the property
    /// table, the input ring or `Init()`.
    fn open(
        config: EncoderConfig,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        max_ltr_frames: i64,
    ) -> Result<Self> {
        let amf = Amf::load()?;

        let runtime_version = match amf.query_version() {
            Ok(v) => {
                tprintln!("AMF runtime version {}", format_version(v));
                Some(v)
            }
            Err(e) => {
                teprintln!("AMFQueryVersion failed ({e:#}); continuing without it");
                None
            }
        };

        // pinned (AMFINIT_VERSION); the runtime's version is only used for one lower retry
        let factory = match amf.init_factory(AMFINIT_VERSION) {
            Ok(f) => f,
            Err(e) => match init_version_retry(runtime_version) {
                Some(v) => {
                    teprintln!(
                        "AMFInit({}) refused ({e:#}); retrying with the runtime's own version {}",
                        format_version(AMFINIT_VERSION),
                        format_version(v)
                    );
                    amf.init_factory(v)
                        .with_context(|| format!("AMFInit({}) also refused", format_version(v)))?
                }
                None => return Err(e),
            },
        };
        Self::open_on_factory(
            Some(amf),
            factory,
            AmfSwitches::read(),
            config,
            device,
            context,
            max_ltr_frames,
        )
    }

    /// Everything after the runtime is loaded, on the factory it handed over. The fake runtime
    /// in the test suite enters here with no DLL behind it.
    fn open_on_factory(
        amf: Option<Amf>,
        factory: *mut AMFFactory,
        switches: AmfSwitches,
        config: EncoderConfig,
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        max_ltr_frames: i64,
    ) -> Result<Self> {
        // a single-threaded device gave flicker and growing latency on Desktop Duplication
        // (AMF issue #92); AMF's own DeviceDX11.cpp sets this unconditionally
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            // SAFETY: `mt` is a live ID3D10Multithread view of our own device.
            let _ = unsafe { mt.SetMultithreadProtected(true) };
        }

        let trace = silence_amf_trace(factory);

        let mut amf_context: *mut AMFContext = ptr::null_mut();
        // SAFETY: vtable slot 0 (AMFFactory::CreateContext) on the factory AMFInit returned.
        let r = unsafe { ((*(*factory).pVtbl).CreateContext)(factory, &mut amf_context) };
        check(r, "AMFFactory::CreateContext")?;
        if amf_context.is_null() {
            bail!("AMFFactory::CreateContext returned AMF_OK with a null context");
        }

        // assembled first so its Drop impl releases everything on any early return below
        let mut this = Self {
            device,
            context,
            factory,
            trace,
            amf_context,
            component: ptr::null_mut(),
            ring: Vec::new(),
            ring_len: NUM_IN_FLIGHT,
            ring_next: 0,
            last_slot: None,
            ring_format: PREFERRED_INPUT_FORMAT,
            surface_format: amf_surface_format(PREFERRED_INPUT_FORMAT).unwrap_or(AMF_SURFACE_BGRA),
            config,
            caps: AmfCaps::unknown(),
            usage: AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY,
            rate_control: AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN,
            query_timeout_supported: false,
            query_timeout_probe: None,
            cqp_not_honoured: false,
            rate_tier: RateTier::LinkCbr,
            switches,
            submit_deadline: SUBMIT_DEADLINE,
            fresh_init: false,
            last_qp: None,
            // from 1, so a PTS of 0 (a runtime that drops it) never matches a picture
            next_picture_id: 1,
            delivered_id: None,
            identity: IdentityCounts::default(),
            identity_source: IdentitySource::Unknown,
            nmi_meaning: NeedMoreInputMeaning::Unknown,
            nmi_probe: None,
            last_resubmitted: None,
            nmi_flips: 0,
            own_idr_deferred: false,
            accepted_at: Instant::now(),
            encode_times: std::collections::VecDeque::with_capacity(ENCODE_TIME_SAMPLES),
            own_idr_gate: crate::streamer::keyframe::KeyframeGate::new(),
            warned_multi_buffer: false,
            warned_identity_mismatch: false,
            qp_check: None,
            qp_unconfirmed: 0,
            qp_unchecked: 0,
            vui_full_range: true,
            written_rate: std::cell::Cell::new(None),
            frame_index: 0,
            idr_requested: false,
            in_flight: TakenRequests::default(),
            idr_audit: IdrAudit::default(),
            stalls: 0,
            idr_period_flipped: false,
            rc_fps: std::cell::Cell::new(config.fps.max(1)),
            ltr: LtrState {
                max_frames: max_ltr_frames,
                ..LtrState::default()
            },
            shared_idr: None,
            warned_input_full: false,
            warned_empty_output: false,
            au_scratch: Vec::new(),
            src_format_cache: None,
            patch_sps_vui: true,
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
        // a context may legitimately be initialised twice; FFmpeg tolerates the same code
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

        // LTR switches rolling intra-refresh off in AMF, so it is only taken when the adapter holds
        // more than one reference and the runtime knows the property
        this.ltr.max_frames = this.usable_ltr_frames(max_ltr_frames);

        Ok(this)
    }

    /// How many LTR slots to ask for, given what the caller wants and what the adapter reported.
    /// An adapter that reports a single reference frame cannot do user-controlled LTR at all.
    fn usable_ltr_frames(&self, requested: i64) -> i64 {
        if requested <= 0 {
            return 0;
        }
        if self.caps.max_reference_frames <= 1 {
            tprintln!(
                "AMF LTR: not enabled, this adapter reports max_refs={} (need > 1); \
                 keeping the usage's rolling intra-refresh as the loss-recovery mechanism",
                self.caps.max_reference_frames
            );
            return 0;
        }
        // SAFETY: the component's first 13 vtable slots are the AMFPropertyStorage prefix, and
        // the name is a NUL-terminated static.
        if !unsafe {
            has_property(
                as_property_storage(self.component),
                prop::AMF_VIDEO_ENCODER_MAX_LTR_FRAMES,
            )
        } {
            tprintln!(
                "AMF LTR: not enabled, this runtime has no MaxOfLTRFrames property; \
                 keeping the usage's rolling intra-refresh as the loss-recovery mechanism"
            );
            return 0;
        }
        requested.clamp(0, MAX_LTR_FRAMES_SUPPORTED)
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn caps(&self) -> AmfCaps {
        self.caps
    }

    pub fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    /// Let the pipeline's IDR atomic reach the encoder directly.
    ///
    /// The async ring's idle keepalive re-encodes the held slot from inside the encode thread and
    /// never touches the capture-side stage, which is the only other consumer of the pipeline's
    /// `idr_request`. Without this, a PLI on a static desktop produces another P frame and a
    /// joining client stays black until something on the host screen moves.
    pub fn attach_idr_flag(&mut self, flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
        self.shared_idr = Some(flag);
    }

    fn take_idr_request(&mut self, force: bool) -> bool {
        let shared = self
            .shared_idr
            .as_ref()
            .is_some_and(|f| f.swap(false, std::sync::atomic::Ordering::Relaxed));
        let own = self.idr_requested;
        self.idr_requested = false;
        force || own || shared
    }

    /// The input textures, for a caller that stages into them itself.
    ///
    /// AMF's ring lives on the **capture** device, so, unlike the NVENC cross-adapter ring,
    /// there is no shared handle to open and no keyed mutex to acquire. Slot ownership is
    /// entirely the caller's: hand a slot to `encode_slot` only when nothing else is writing it.
    pub fn ring_textures(&self) -> Vec<ID3D11Texture2D> {
        self.ring.clone()
    }

    /// Encode the texture already staged in `slot`. Pairs with [`Self::ring_textures`].
    pub fn encode_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>> {
        self.submit_slot(slot, force_idr)
    }

    /// Re-encode `slot` for a static desktop: zero copies, one encode.
    pub fn encode_repeat_slot(&mut self, slot: usize) -> Result<Vec<u8>> {
        self.submit_slot(slot, false)
    }

    /// The index the *next* submitted picture will carry, for the repair map.
    pub fn next_frame_index(&self) -> u64 {
        self.frame_index
    }

    /// How many long-term reference slots this encoder has. 0 means the repair API is inert and
    /// the only loss recovery available is a keyframe, which is the case on the inline paths.
    /// On the async ring this is what AMF granted of what `build_amf_ring` asked for.
    pub fn ltr_slots(&self) -> u32 {
        self.ltr.max_frames.max(0) as u32
    }

    /// Mark the next encoded picture as a long-term reference, and say which slot it takes.
    ///
    /// The caller owns the policy, how often to mark, and which slots it believes the client
    /// holds, because only the transport sees the per-frame receipts. `None` when this adapter
    /// gave us no slots.
    pub fn mark_ltr(&mut self) -> Option<u32> {
        if !self.ltr_enabled() {
            return None;
        }
        let slot = self.ltr.next_slot;
        self.ltr.next_slot = (slot + 1) % self.ltr.max_frames;
        self.ltr.mark_next = Some(slot);
        Some(slot as u32)
    }

    /// Make the next encoded picture reference only the long-term slots in `bitmap`.
    ///
    /// This is the repair: after a loss burst, a P frame predicted from a picture the client is
    /// known to hold costs a fraction of an IDR and does not reset the rate controller. Bits
    /// outside the encoder's slot count are ignored rather than rejected, and an empty bitmap is
    /// a no-op, "reference nothing" is an intra frame, which the caller should ask for as an
    /// IDR instead.
    pub fn use_ltr(&mut self, bitmap: u32) -> bool {
        if !self.ltr_enabled() {
            return false;
        }
        let mask = (1u32 << self.ltr.max_frames.min(31)) - 1;
        let bitmap = bitmap & mask & self.ltr.live_slots;
        if bitmap == 0 {
            return false;
        }
        self.ltr.force_reference = Some(bitmap as i64);
        true
    }

    pub fn ltr_enabled(&self) -> bool {
        self.ltr.max_frames > 0
    }

    /// Live bitrate change. Dynamic property writes take effect at the next `SubmitInput` with
    /// no `ReInit` and no `Flush`. AMD warn "you cannot change it too often", which is why
    /// `streamer::bitrate::BitrateController` stays the only writer.
    ///
    /// Under a constant-QP request the bitrate is not ours to move, unless the driver refused
    /// `CONSTANT_QP` and kept its usage's own rate control, in which case leaving the bitrate
    /// alone would pin the session at the usage's default (`amf_cqp_fallback_rate`).
    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() && !self.cqp_not_honoured {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        // written first: a refused write leaves `config` saying what the encoder still runs
        let bps = clamp_bitrate(i64::from(bps), self.caps.max_bitrate) as u32;
        self.write_bitrate(i64::from(bps))?;
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;
        tprintln!("AMF bitrate reconfigured (bitrate_bps={bps})");
        Ok(BitrateApplied::Applied)
    }

    /// The rate-control tier this session settled on after `Init()`.
    pub fn rate_tier(&self) -> RateTier {
        self.rate_tier
    }

    /// The QP a link governor is driving: the constant QP, or the floor that stands in for it when
    /// the driver refused constant QP. `None` under CBR.
    pub fn constant_qp(&self) -> Option<u8> {
        match self.rate_tier {
            RateTier::ConstantQp => self.config.qp,
            RateTier::ConstantQpRefused if self.cqp_not_honoured => self.config.qp,
            _ => None,
        }
    }

    /// The QP control for either constant-QP tier. Under [`RateTier::ConstantQp`] it is
    /// [`Self::set_qp`]. When the driver refused constant QP and the session runs at a bitrate,
    /// it moves the `MinQP` floor instead, which only shrinks the stream; `set_qp` answers false
    /// there.
    pub fn set_constant_qp(&mut self, qp: u8) -> Result<QpApplied> {
        let qp = qp.min(51);
        match self.rate_tier {
            RateTier::LinkCbr => Ok(QpApplied::NotConstantQp),
            RateTier::ConstantQpRefused if !self.cqp_not_honoured => Ok(QpApplied::NotConstantQp),
            RateTier::ConstantQp => Ok(if self.set_qp(qp)? {
                QpApplied::Applied
            } else {
                QpApplied::NotConstantQp
            }),
            RateTier::ConstantQpRefused => {
                self.set_i64(prop::AMF_VIDEO_ENCODER_MIN_QP, i64::from(qp))?;
                self.config.qp = Some(qp);
                Ok(QpApplied::Applied)
            }
        }
    }

    /// The link governor's QP input: constant-QP encoding uses `qp` from the next submitted
    /// picture. `Ok(true)` when it is in effect, `Ok(false)` when this session cannot (it runs
    /// CBR, the driver refused constant QP, or `qp` is outside 0..=51), and then nothing changed.
    /// An unchanged QP writes nothing.
    ///
    /// Live by default (`amf_qp_live`): `QPI`, `QPP` and `QPB` are written with nothing else.
    /// AMF's header files all three under dynamic properties, applied before the next
    /// `SubmitInput`, but **whether a real runtime applies them mid-stream with no IDR is
    /// unverified**: OBS flushes and re-initialises instead ([`Self::set_qp_reinit`], one IDR). A
    /// change that did not take is counted by [`Self::qp_change_unconfirmed`] when the runtime
    /// reports statistics.
    pub fn set_qp(&mut self, qp: u8) -> Result<bool> {
        if self.rate_tier != RateTier::ConstantQp || qp > 51 {
            return Ok(false);
        }
        if self.config.qp == Some(qp) {
            return Ok(true);
        }
        if !self.switches.qp_live {
            return self.set_qp_reinit(qp);
        }
        self.write_qp(qp)?;
        self.config.qp = Some(qp);
        // the next picture asks for statistics regardless, so the change is checked at once
        self.qp_check = Some(qp);
        Ok(true)
    }

    /// The constant-QP change OBS makes (`amf_avc_update_data`: write the QPs, `Flush`,
    /// `ReInit`), for a runtime that does not honour a live write. The next picture is an IDR
    /// carrying its parameter sets. Same answers as [`Self::set_qp`].
    pub fn set_qp_reinit(&mut self, qp: u8) -> Result<bool> {
        if self.rate_tier != RateTier::ConstantQp || qp > 51 {
            return Ok(false);
        }
        if self.config.qp == Some(qp) {
            return Ok(true);
        }
        self.write_qp(qp)?;
        self.config.qp = Some(qp);
        self.qp_check = None;
        // SAFETY: vtable slot 21 (Flush) on a live initialised component; nothing is in flight
        // between two pictures.
        let flushed = unsafe { ((*(*self.component).pVtbl).Flush)(self.component) };
        let r = if flushed == AMF_OK {
            // SAFETY: vtable slot 18 (ReInit) on the same component.
            unsafe {
                ((*(*self.component).pVtbl).ReInit)(
                    self.component,
                    self.config.width as i32,
                    self.config.height as i32,
                )
            }
        } else {
            flushed
        };
        if r == AMF_OK {
            self.after_init();
        } else {
            // OBS treats this as fatal; here a full terminate and init is tried before giving up
            teprintln!(
                "AMF {} for a QP change returned {}; terminating and initialising again",
                if flushed == AMF_OK { "ReInit" } else { "Flush" },
                result_name(r)
            );
            self.terminate_component("for a QP change");
            self.configure_and_init()?;
            self.read_extradata();
        }
        Ok(true)
    }

    /// `QPI`, `QPP` and `QPB` together, or none of them: a refused write puts back the ones
    /// already made, so the picture types never disagree about the QP in force.
    fn write_qp(&self, qp: u8) -> Result<()> {
        let names = [
            prop::AMF_VIDEO_ENCODER_QP_I,
            prop::AMF_VIDEO_ENCODER_QP_P,
            prop::AMF_VIDEO_ENCODER_QP_B,
        ];
        let old: Vec<Option<i64>> = names.iter().map(|n| self.get_i64(n)).collect();
        for (i, name) in names.iter().enumerate() {
            // QPB is meaningless with no B-frames and a runtime may not know it
            let required = i < 2;
            if let Err(e) = self.set_i64(name, i64::from(qp)) {
                if !required {
                    break;
                }
                for (n, v) in names.iter().zip(&old).take(i) {
                    if let Some(v) = v {
                        let _ = self.set_raw(n, AMFVariantStruct::int64(*v));
                    }
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// The QP AMF reported for the newest picture that asked for statistics: the rate
    /// controller's frame QP (`StatisticsFeedbackFrameQP`), or, where only that is reported, the
    /// **average** over the picture's encoded macroblocks (`StatisticsFeedbackAvgQP`), which AMF's
    /// header warns can differ from the bitstream when macroblocks are skipped. Pictures ask for
    /// statistics under `amf_statistics_feedback`, and the first one after a live QP change.
    pub fn last_frame_qp(&self) -> Option<u8> {
        self.last_qp
    }

    /// Live QP changes whose first picture reported a QP other than the one set: evidence that
    /// this runtime does not honour the live write, in which case `amf_qp_live` should be off.
    pub fn qp_change_unconfirmed(&self) -> u64 {
        self.qp_unconfirmed
    }

    /// Live QP changes that could not be checked because the runtime reported no statistics
    /// (anything before RX 5000 / Ryzen 2000). The caller has only frame sizes to judge by.
    pub fn qp_change_unchecked(&self) -> u64 {
        self.qp_unchecked
    }

    pub(crate) fn identity_counts(&self) -> IdentityCounts {
        self.identity
    }

    /// Target, peak and a one-frame VBV, plus the `MaxAUSize` candidate when it is on. VBV is
    /// sized from the *real* frame rate even when AMF holds a clamped `FrameRate`, because the
    /// point of it is one frame of the stream we actually send.
    /// The three are written together or not at all: a refused write puts back what was last
    /// written successfully, so target and peak are never left apart.
    fn write_bitrate(&self, bps: i64) -> Result<()> {
        let fps = self.config.fps.max(1) as i64;
        let bps = clamp_bitrate(bps, self.caps.max_bitrate);
        let target = if self.switches.framerate_rc_scale {
            rc_target_bitrate(bps, self.config.fps, self.rc_fps.get())
        } else {
            bps
        };
        let vbv = (bps / fps).max(1);
        let prev = self.written_rate.get();
        let write_pair = |from: Option<i64>, to: i64| -> Result<()> {
            for name in rate_write_order(from, to) {
                let wide = if name == "PeakBitrate" {
                    prop::AMF_VIDEO_ENCODER_PEAK_BITRATE
                } else {
                    prop::AMF_VIDEO_ENCODER_TARGET_BITRATE
                };
                self.set_i64(wide, to)?;
            }
            Ok(())
        };
        let written = write_pair(prev.map(|(t, _, _)| t), target)
            .and_then(|()| self.set_i64(prop::AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE, vbv));
        if let Err(e) = written {
            if let Some((t, _, v)) = prev {
                // Restored through `rate_write_order` too, so target never passes peak. Every write
                // is tried: stopping at the first refusal could leave the pair further apart.
                for name in rate_write_order(Some(target), t) {
                    let wide = if name == "PeakBitrate" {
                        prop::AMF_VIDEO_ENCODER_PEAK_BITRATE
                    } else {
                        prop::AMF_VIDEO_ENCODER_TARGET_BITRATE
                    };
                    let _ = self.set_raw(wide, AMFVariantStruct::int64(t));
                }
                let _ = self.set_raw(
                    prop::AMF_VIDEO_ENCODER_VBV_BUFFER_SIZE,
                    AMFVariantStruct::int64(v),
                );
                let now = (
                    self.get_i64(prop::AMF_VIDEO_ENCODER_TARGET_BITRATE),
                    self.get_i64(prop::AMF_VIDEO_ENCODER_PEAK_BITRATE),
                );
                if now != (Some(t), Some(t)) {
                    teprintln!(
                        "AMF: restoring bitrate {t} after a refused change left target/peak at \
                         {now:?}"
                    );
                }
            }
            return Err(e);
        }
        self.written_rate.set(Some((target, target, vbv)));
        if self.switches.max_au_size {
            // best effort: a hard cap on one access unit, three frames of bits
            let _ = self.set_raw(
                prop::AMF_VIDEO_ENCODER_MAX_AU_SIZE,
                AMFVariantStruct::int64((3 * bps / fps).max(1)),
            );
        }
        Ok(())
    }

    /// GPU path: stage the captured texture into the ring (scaling and/or converting through the
    /// `VideoProcessor` when one is supplied), then wrap and submit it.
    pub fn encode_texture(
        &mut self,
        src: &ID3D11Texture2D,
        scaler: Option<&mut Scaler>,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        // Without a scaler the stage is a CopyResource, which demands identical formats. The
        // format is cached by texture pointer to skip a per-frame `GetDesc`: WGC frames come from
        // a fixed pool, rebuilt on a mode change.
        if scaler.is_none() {
            let key = src.as_raw() as usize;
            let src_format = match self.src_format_cache {
                Some((cached_key, format)) if cached_key == key => format,
                _ => {
                    let format = texture_format(src);
                    self.src_format_cache = Some((key, format));
                    format
                }
            };
            if src_format != self.ring_format {
                self.src_format_cache = None;
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
    /// ring is BGRA, the bridge exists precisely because there is no GPU path to the encoder.
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

    /// Encode a frame the GPU's video processor already scaled and converted to NV12.
    ///
    /// NV12 is what VCN wants: packed-RGB input runs AMF's internal GPU colour converter, which
    /// competes with the foreground application, while the scaler's `VideoProcessorBlt` converts
    /// in a pass it had to run anyway.
    ///
    /// Layout per `Scaler::read_back` with `ReadbackFormat::Nv12`: `height` rows of luma then
    /// `height / 2` rows of interleaved chroma (U then V), all at one `row_pitch`. That is one
    /// D3D11 NV12 subresource, so `UpdateSubresource` takes it in a single call.
    ///
    /// **UNVERIFIED** on AMD hardware.
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
        // BGRA until the first NV12 frame arrives; the retune runs at most once per session
        if self.ring_format != DXGI_FORMAT_NV12 {
            self.retune_input_format(DXGI_FORMAT_NV12)?;
        }

        let slot = self.ring_next;
        let dst = self
            .ring
            .get(slot)
            .ok_or_else(|| anyhow!("AMF ring slot {slot} out of range"))?
            .clone();
        // SAFETY: `data` holds at least `(height + height / 2) * row_pitch` bytes, checked
        // above, and `dst` is a live default-usage NV12 texture of exactly the encoder's
        // dimensions, one subresource covering both planes.
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

    fn submit_slot(&mut self, slot: usize, force_idr: bool) -> Result<Vec<u8>> {
        self.retry_deferred_idr();
        // taken for this picture, and given back if the submit fails (see `TakenRequests`)
        let taken = TakenRequests {
            idr: self.take_idr_request(force_idr) || self.fresh_init,
            mark: self.ltr.mark_next.take(),
            force_reference: self.ltr.force_reference.take(),
        };
        let stalls_before = self.stalls;
        let out = self.wrap_and_submit(slot, taken);
        match &out {
            Ok(_) => {
                if taken.idr {
                    self.idr_requested = false;
                }
                self.fresh_init = false;
                if taken.idr {
                    // whatever resync was waiting has just been done
                    self.own_idr_deferred = false;
                }
                // a key frame clears every long-term slot, so only a mark made on it survives
                if taken.idr {
                    self.ltr.live_slots = 0;
                }
                if let Some(marked) = taken.mark {
                    self.ltr.live_slots |= 1u32 << marked.clamp(0, 31);
                }
                self.frame_index += 1;
                self.last_slot = Some(slot);
                self.ring_next = (slot + 1) % self.ring.len().max(1);
            }
            Err(_) => restore_requests(
                requests_after_failure(taken, self.stalls != stalls_before),
                &mut self.idr_requested,
                &mut self.ltr.mark_next,
                &mut self.ltr.force_reference,
            ),
        }
        out
    }

    fn wrap_and_submit(&mut self, slot: usize, taken: TakenRequests) -> Result<Vec<u8>> {
        let surface = self.wrap_slot(slot)?;
        self.submit_surface(surface.as_ptr(), taken)
    }

    /// One ring texture as an `AMFSurface` this call owns.
    fn wrap_slot(&self, slot: usize) -> Result<Owned<AMFSurface>> {
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
        Ok(Owned(raw))
    }

    /// Settles what `AMF_NEED_MORE_INPUT` means on a runtime, under the TRANSCODING usage, which
    /// needs several inputs before any output. A picture whose submit answered
    /// `AMF_NEED_MORE_INPUT` and whose identity comes back was taken; one that never comes back
    /// was not. The encoder is not usable for streaming afterwards.
    #[cfg(test)]
    pub(crate) fn need_more_input_probe(&mut self) -> Result<Vec<String>> {
        let mut log = Vec::new();
        self.terminate_component("for the AMF_NEED_MORE_INPUT probe");
        self.apply_properties(AMF_VIDEO_ENCODER_USAGE_TRANSCODING)?;
        self.init_component()?;
        let step = self.pts_step();
        for k in 0..4i64 {
            let id = 1000 + k;
            let surface = self.wrap_slot(k as usize % self.ring.len().max(1))?;
            let raw = surface.as_ptr();
            // SAFETY: slots 19 (SetPts) and the property-storage prefix on a live surface; slot
            // 22 (SubmitInput) on the initialised component.
            let r = unsafe {
                ((*(*raw).pVtbl).SetPts)(raw, id * step);
                set_property(
                    as_property_storage(raw),
                    prop::FRAME_IDENTITY,
                    AMFVariantStruct::int64(id),
                );
                ((*(*self.component).pVtbl).SubmitInput)(self.component, raw.cast::<AMFData>())
            };
            log.push(format!("submit picture {id}: {}", result_name(r)));
        }
        // SAFETY: slot 20 (Drain) on the initialised component.
        let r = unsafe { ((*(*self.component).pVtbl).Drain)(self.component) };
        log.push(format!("drain: {}", result_name(r)));
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            match self.query_once() {
                Ok(QueryOutcome::Au(au)) => log.push(format!(
                    "output: pts picture {:?}, property picture {:?}, {} bytes",
                    au.pts.map(|p| p / step),
                    au.prop_id,
                    au.bytes.len()
                )),
                Ok(QueryOutcome::Empty(pts, id)) => log.push(format!(
                    "empty output: pts picture {:?}, property picture {id:?}",
                    pts.map(|p| p / step)
                )),
                Ok(QueryOutcome::NotReady) => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => {
                    log.push(format!("query ended: {e:#}"));
                    break;
                }
            }
        }
        Ok(log)
    }

    fn submit_surface(
        &mut self,
        surface: *mut AMFSurface,
        taken: TakenRequests,
    ) -> Result<Vec<u8>> {
        let fps = self.config.fps.max(1) as i64;
        let id = self.next_picture_id;
        self.next_picture_id += 1;
        // the PTS carries the identity too, as a cross-check or where a runtime drops the property
        let pts = id * self.pts_step();

        // SAFETY: vtable slots 28 (SetFrameType), 19 (SetPts) and 21 (SetDuration) on a live
        // surface we own.
        unsafe {
            ((*(*surface).pVtbl).SetFrameType)(surface, AMF_FRAME_PROGRESSIVE);
            ((*(*surface).pVtbl).SetPts)(surface, pts);
            ((*(*surface).pVtbl).SetDuration)(surface, AMF_SECOND / fps);
            // surfaces can be vertically aligned up by 16, and display sizes here are arbitrary
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
            // identity as a private property, as OBS (`L"PTS"`) and FFmpeg (`amfenc.c`) carry it
            set_property(storage, prop::FRAME_IDENTITY, AMFVariantStruct::int64(id));
            // access-unit delimiters are bytes no browser decoder needs
            set_property(
                storage,
                prop::AMF_VIDEO_ENCODER_INSERT_AUD,
                AMFVariantStruct::bool(false),
            );
            if self.switches.statistics_feedback || self.qp_check.is_some() {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_STATISTICS_FEEDBACK,
                    AMFVariantStruct::bool(true),
                );
            }
            if taken.idr {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_FORCE_PICTURE_TYPE,
                    AMFVariantStruct::int64(AMF_VIDEO_ENCODER_PICTURE_TYPE_IDR as i64),
                );
                // makes the IDR self-contained for a mid-stream joiner
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
            if let Some(index) = taken.mark {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_MARK_CURRENT_WITH_LTR_INDEX,
                    AMFVariantStruct::int64(index),
                );
            }
            if let Some(bitfield) = taken.force_reference {
                set_property(
                    storage,
                    prop::AMF_VIDEO_ENCODER_FORCE_LTR_REFERENCE_BITFIELD,
                    AMFVariantStruct::int64(bitfield),
                );
            }
        }
        self.in_flight = taken;

        let stall_after = self.submit_deadline;
        let deadline = Instant::now() + stall_after;
        // empty polls this frame, for the backoff when `QueryOutput` does not wait for us
        let mut polls = 0u32;
        let mut resubmits = 0u32;
        // this picture's surface was taken on the word of AMF_NEED_MORE_INPUT
        let mut nmi_accepted = false;

        loop {
            // SAFETY: vtable slot 22 (AMFComponent::SubmitInput). An AMFSurface is an AMFData
            // by prefix, which is what the C API's own signature expects.
            let r = unsafe {
                ((*(*self.component).pVtbl).SubmitInput)(self.component, surface.cast::<AMFData>())
            };
            if r == AMF_OK || r == AMF_NEED_MORE_INPUT {
                self.accepted_at = Instant::now();
            }
            match r {
                AMF_OK => break,
                // OBS treats the surface as taken, AMD's samples resubmit it, FFmpeg calls it an
                // error. Learnt once per session: resubmit after a window, then a second output
                // for that picture proves "taken" and the next picture's alone proves "not taken".
                AMF_NEED_MORE_INPUT => {
                    match need_more_input_action(self.identity_source, self.nmi_meaning, resubmits)
                    {
                        NeedMoreInputAction::Accept => {
                            nmi_accepted = true;
                            break;
                        }
                        NeedMoreInputAction::ResubmitNow => {
                            resubmits += 1;
                            self.identity.need_more_input_resubmitted += 1;
                            self.last_resubmitted = Some(id);
                        }
                        NeedMoreInputAction::Probe => {
                            let window = need_more_input_window(
                                self.config.fps,
                                self.encode_p99(),
                                stall_after,
                            );
                            let until = (Instant::now() + window).min(deadline);
                            if let Some(bytes) = self.collect(id, until, &mut polls)? {
                                self.identity.need_more_input_consumed += 1;
                                self.learn_need_more_input(NeedMoreInputMeaning::Taken);
                                return Ok(bytes);
                            }
                            resubmits += 1;
                            self.identity.need_more_input_resubmitted += 1;
                            self.nmi_probe = Some(id);
                            self.last_resubmitted = Some(id);
                            teprintln!(
                                "AMF SubmitInput answered AMF_NEED_MORE_INPUT and no output \
                                 followed within {window:?}; submitting picture {id} once more to \
                                 learn what the code means on this runtime"
                            );
                        }
                    }
                }
                AMF_INPUT_FULL => {
                    // not consumed: drain the output (AMD's advice) and resubmit the same pointer
                    if !self.warned_input_full {
                        self.warned_input_full = true;
                        teprintln!(
                            "AMF SubmitInput returned AMF_INPUT_FULL under one-frame-in-flight \
                             discipline; draining output and retrying (this should not happen)"
                        );
                    }
                    // an earlier picture's output: dropped, never joined to this access unit (two
                    // pictures under one RTP timestamp are undecodable), and answered with an IDR
                    match self.query_once()? {
                        QueryOutcome::Au(au) => {
                            let reported = self.identity_of(au.pts, au.prop_id);
                            self.drop_foreign(reported, id, true);
                        }
                        QueryOutcome::Empty(..) => self.identity.stray_empty += 1,
                        QueryOutcome::NotReady => self.wait_after_not_ready(&mut polls),
                    }
                    if Instant::now() >= deadline {
                        // nothing is left inside AMF: the frame is simply not encoded
                        bail!("AMF SubmitInput stalled on AMF_INPUT_FULL for {stall_after:?}");
                    }
                }
                _ => {
                    return Err(self.amf_error("AMFComponent::SubmitInput", r));
                }
            }
        }

        if let Some(bytes) = self.collect(id, deadline, &mut polls)? {
            return Ok(bytes);
        }
        // taken on the word of AMF_NEED_MORE_INPUT but never came out: evidence for "not taken"
        if nmi_accepted && self.nmi_meaning == NeedMoreInputMeaning::Taken {
            self.learn_need_more_input(NeedMoreInputMeaning::NotTaken);
        }
        // The picture is still inside AMF: left there it would come out as the next frame's and
        // shift every later frame by one, and the freed slot could be overwritten while AMF still
        // reads it. Re-initialising drops it, and the forced IDR keeps the client off it.
        self.idr_requested = true;
        match self.recover_from_stall() {
            Ok(()) => bail!(
                "AMF QueryOutput produced nothing for {stall_after:?} after a \
                 successful SubmitInput (encoder stalled); the component was \
                 re-initialised and the next frame is an IDR"
            ),
            Err(e) => bail!(
                "AMF QueryOutput produced nothing for {stall_after:?} after a \
                 successful SubmitInput (encoder stalled), and re-initialising the \
                 component failed too: {e:#}"
            ),
        }
    }

    /// Polls until picture `id`'s own output arrives (`Some`, empty for a consumed output with no
    /// bytes) or `until` passes (`None`). Outputs of other pictures are dropped on the way. A
    /// runtime that does not carry the identity is judged by position.
    fn collect(&mut self, id: i64, until: Instant, polls: &mut u32) -> Result<Option<Vec<u8>>> {
        // slice output is turned off but a runtime might ignore that: parts are joined here, and
        // only from buffers carrying this picture's identity
        let mut parts: Option<Vec<u8>> = None;
        loop {
            match self.query_once()? {
                QueryOutcome::Au(au) => {
                    if self.identity_source == IdentitySource::Unknown {
                        self.learn_identity(id, au.pts, au.prop_id);
                    }
                    let reported = self.identity_of(au.pts, au.prop_id);
                    let fate = self.fate_of(reported, id);
                    if fate == OutputFate::Unlabelled {
                        self.identity.unlabelled += 1;
                    }
                    if matches!(fate, OutputFate::Mine | OutputFate::Unlabelled) {
                        let more = au.buffer_type
                            == Some(AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE_SLICE as i64);
                        let first_part = parts.is_none();
                        let mut bytes = parts.take().unwrap_or_default();
                        bytes.extend_from_slice(&au.bytes);
                        if more {
                            if first_part {
                                self.note_multi_buffer();
                            }
                            parts = Some(bytes);
                            continue;
                        }
                        self.audit_output(&au);
                        self.note_delivered(id);
                        return Ok(Some(bytes));
                    }
                    if fate == OutputFate::Duplicate
                        && reported.is_some()
                        && (reported == self.nmi_probe || reported == self.last_resubmitted)
                    {
                        // submitted twice and came out twice: the code means "taken"
                        self.learn_need_more_input(NeedMoreInputMeaning::Taken);
                    }
                    self.drop_foreign(reported, id, false);
                    continue;
                }
                // a consumed output with no bytes is never produced again (see `QueryClass`):
                // take the frame loss, an empty access unit, which the pipeline drops
                QueryOutcome::Empty(pts, prop_id) => {
                    if self.identity_source == IdentitySource::Unknown {
                        self.learn_identity(id, pts, prop_id);
                    }
                    let reported = self.identity_of(pts, prop_id);
                    let fate = self.fate_of(reported, id);
                    if matches!(fate, OutputFate::Mine | OutputFate::Unlabelled) {
                        self.note_empty_output();
                        self.note_delivered(id);
                        return Ok(Some(parts.take().unwrap_or_default()));
                    }
                    // another picture's, with nothing in it: nothing entered the stream
                    self.identity.stray_empty += 1;
                    continue;
                }
                QueryOutcome::NotReady => self.wait_after_not_ready(polls),
            }
            if Instant::now() >= until {
                return Ok(None);
            }
        }
    }

    /// An output that is not the picture being collected: counted, logged at powers of two,
    /// dropped, and answered with an IDR on the next picture, because the encoder's reference
    /// chain now runs through a picture the client never receives.
    fn drop_foreign(&mut self, reported: Option<i64>, current: i64, harvested: bool) {
        let fate = if harvested && reported.is_none() {
            OutputFate::Stale
        } else {
            output_fate(reported, current, self.delivered_id)
        };
        let count = match fate {
            OutputFate::Duplicate => {
                self.identity.duplicates_dropped += 1;
                self.identity.duplicates_dropped
            }
            _ => {
                self.identity.stale_dropped += 1;
                self.identity.stale_dropped
            }
        };
        let idr = self.request_own_idr();
        if log_now(count) {
            teprintln!(
                "AMF output for picture {reported:?} arrived while collecting picture {current} \
                 ({fate:?}, {count} so far); dropped{}",
                if idr {
                    ", and the next picture is an IDR"
                } else {
                    "; the keyframe gate held this one's IDR back"
                }
            );
        }
    }

    /// An IDR this encoder wants on its own account, through the same window the transport's
    /// keyframe requests pass (`streamer::keyframe::KeyframeGate`, its default bounds), so a
    /// runtime that keeps producing strays cannot turn them into a keyframe on every frame.
    /// A held-back request is kept and retried at every submit: the reference chain runs through
    /// a picture the client never received, and nothing else will ask again.
    fn request_own_idr(&mut self) -> bool {
        if self
            .own_idr_gate
            .allow(crate::streamer::audio::host_now_ns())
        {
            self.idr_requested = true;
            self.own_idr_deferred = false;
            true
        } else {
            if !self.own_idr_deferred {
                self.identity.own_idrs_suppressed += 1;
            }
            self.own_idr_deferred = true;
            false
        }
    }

    fn retry_deferred_idr(&mut self) {
        if self.own_idr_deferred
            && self
                .own_idr_gate
                .allow(crate::streamer::audio::host_now_ns())
        {
            self.own_idr_deferred = false;
            self.idr_requested = true;
        }
    }

    /// Where an output belongs. Once the session knows where identities come from, an output
    /// whose identity cannot be read is another picture's (stale), never taken for this one.
    fn fate_of(&self, reported: Option<i64>, id: i64) -> OutputFate {
        if reported.is_none()
            && matches!(
                self.identity_source,
                IdentitySource::Property | IdentitySource::Pts
            )
        {
            return OutputFate::Stale;
        }
        output_fate(reported, id, self.delivered_id)
    }

    fn pts_step(&self) -> i64 {
        (AMF_SECOND / i64::from(self.config.fps.max(1))).max(1)
    }

    fn identity_of(&mut self, pts: Option<i64>, prop: Option<i64>) -> Option<i64> {
        let step = self.pts_step();
        let id = resolve_identity(self.identity_source, pts, prop, step);
        if self.identity_source == IdentitySource::Property && prop.is_none() && id.is_some() {
            self.identity.property_missing += 1;
        }
        if prop.is_some() {
            if let (Some(a), Some(b)) = (id, pts_identity(pts, step)) {
                if a != b {
                    self.identity.identity_mismatch += 1;
                    if !self.warned_identity_mismatch {
                        self.warned_identity_mismatch = true;
                        teprintln!(
                            "AMF output's identity property names picture {a} but its PTS names \
                             {b}; the property is trusted (logged once)"
                        );
                    }
                }
            }
        }
        id
    }

    /// Decides, from picture `expected`'s output (the session's first), where identities come
    /// from, and says so once when neither survives.
    fn learn_identity(&mut self, expected: i64, pts: Option<i64>, prop: Option<i64>) {
        self.identity_source = learn_identity_source(expected, pts, prop, self.pts_step());
        match self.identity_source {
            IdentitySource::Property => {}
            IdentitySource::Pts => teprintln!(
                "AMF did not carry the identity property to the output; pictures are matched by \
                 the PTS instead (got {pts:?})"
            ),
            _ => teprintln!(
                "AMF carried neither the input PTS nor the identity property to the output; \
                 outputs are matched by position and AMF_NEED_MORE_INPUT is taken as accepted, \
                 since a resubmitted copy could not be told from the next picture"
            ),
        }
    }

    /// Records what the evidence says `AMF_NEED_MORE_INPUT` means on this runtime, revising an
    /// earlier answer within `NEED_MORE_INPUT_FLIPS`.
    fn learn_need_more_input(&mut self, evidence: NeedMoreInputMeaning) {
        let (meaning, flips) = revise_need_more_input(self.nmi_meaning, evidence, self.nmi_flips);
        self.nmi_probe = None;
        if meaning == self.nmi_meaning {
            return;
        }
        self.nmi_meaning = meaning;
        self.nmi_flips = flips;
        teprintln!(
            "AMF: AMF_NEED_MORE_INPUT from SubmitInput means {} on this runtime; {}",
            if meaning == NeedMoreInputMeaning::Taken {
                "the surface was taken"
            } else {
                "the surface was not taken"
            },
            if meaning == NeedMoreInputMeaning::Taken {
                "it is accepted from now on"
            } else {
                "it is resubmitted at once from now on"
            }
        );
    }

    /// Picture `id` was delivered. Its encode time feeds the window, and a delivered picture
    /// after the one submitted twice, with no second output in between, proves "not taken".
    fn note_delivered(&mut self, id: i64) {
        self.delivered_id = Some(id);
        if self.encode_times.len() == ENCODE_TIME_SAMPLES {
            self.encode_times.pop_front();
        }
        self.encode_times.push_back(self.accepted_at.elapsed());
        if self.nmi_probe.is_some_and(|p| id > p) {
            self.learn_need_more_input(NeedMoreInputMeaning::NotTaken);
        }
    }

    fn encode_p99(&self) -> Option<Duration> {
        if self.encode_times.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.encode_times.iter().copied().collect();
        sorted.sort_unstable();
        Some(sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)])
    }

    fn note_multi_buffer(&mut self) {
        self.identity.multi_buffer_pictures += 1;
        if !self.warned_multi_buffer {
            self.warned_multi_buffer = true;
            teprintln!(
                "AMF returned a picture as several output buffers although OutputMode is FRAME; \
                 the parts of each picture are joined by its identity (logged once)"
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn identity_source_for_test(&self) -> IdentitySource {
        self.identity_source
    }

    #[cfg(test)]
    pub(crate) fn set_need_more_input_meaning_for_test(&mut self, meaning: NeedMoreInputMeaning) {
        self.nmi_meaning = meaning;
    }

    #[cfg(test)]
    pub(crate) fn need_more_input_meaning_for_test(&self) -> NeedMoreInputMeaning {
        self.nmi_meaning
    }

    #[cfg(test)]
    pub(crate) fn bitrate_for_test(&self) -> u32 {
        self.config.bitrate_bps
    }

    /// Between two `QueryOutput` calls that found nothing. Nothing when the driver waits inside
    /// `QueryOutput` for us; otherwise the `amf_poll_backoff` ladder, or `Sleep(1)` with it off.
    fn wait_after_not_ready(&self, polls: &mut u32) {
        if self.query_timeout_supported {
            return;
        }
        *polls = polls.saturating_add(1);
        PollWait::after(*polls, self.switches.poll_backoff).wait();
    }

    /// Judge the picture AMF returned against what it was asked for.
    fn audit_output(&mut self, au: &AuOut) {
        let frame = self.frame_index;
        let a = &mut self.idr_audit;
        a.outputs_since_init += 1;
        let check = check_idr(self.in_flight.idr, au.idr, a.outputs_since_init == 1);
        let mut period = None;
        if au.idr == Some(true) {
            let gap = a.outputs_since_init - a.last_idr_at;
            if check == IdrCheck::Unrequested {
                if is_periodic_idr(a.last_unrequested_gap, gap) {
                    period = Some(gap);
                }
                a.last_unrequested_gap = Some(gap);
            }
            a.last_idr_at = a.outputs_since_init;
        }
        match check {
            IdrCheck::Fine => {}
            IdrCheck::Unrequested => {
                // an IDR AMF chose for itself cleared the long-term slots all the same
                self.ltr.live_slots &= self.in_flight.mark.map_or(0, |m| 1u32 << m.clamp(0, 31));
                let a = &mut self.idr_audit;
                a.unrequested += 1;
                if log_now(a.unrequested) {
                    teprintln!(
                        "AMF emitted an IDR nobody requested (frame {frame}, {} so far): this \
                         runtime is not honouring the IDRPeriod write, and every one is a bitrate \
                         spike",
                        a.unrequested
                    );
                }
            }
            IdrCheck::NotHonoured => {
                let a = &mut self.idr_audit;
                a.not_honoured += 1;
                if log_now(a.not_honoured) {
                    teprintln!(
                        "AMF ignored a forced IDR (frame {frame}, {} so far): OutputDataType is \
                         not IDR, so a joining client waits for the next keyframe",
                        a.not_honoured
                    );
                }
            }
        }
        if let Some(gap) = period {
            if should_flip_idr_period(self.switches.idr_period_flip, self.idr_period_flipped, true)
            {
                self.flip_idr_period(gap);
            } else if !self.idr_period_flipped {
                self.idr_period_flipped = true;
                teprintln!(
                    "AMF: unrequested IDRs every {gap} outputs, so the IDRPeriod write is not \
                     being honoured; amf_idr_period_flip is off, so IDRPeriod is left as Init \
                     set it"
                );
            }
        }
        if au.qp.is_some() {
            self.last_qp = au.qp;
        }
        if let Some(want) = self.qp_check.take() {
            match au.qp {
                Some(got) if got == want => {}
                Some(got) => {
                    self.qp_unconfirmed += 1;
                    if log_now(self.qp_unconfirmed) {
                        teprintln!(
                            "AMF: a live QP change to {want} came out at QP {got} ({} so far); \
                             this runtime may not honour a live QPI/QPP write \
                             (SE_TUNE_OFF=amf_qp_live re-initialises instead)",
                            self.qp_unconfirmed
                        );
                    }
                }
                None => self.qp_unchecked += 1,
            }
        }
        if let (Some(want), Some(got)) = (self.in_flight.mark, au.marked) {
            if want != got && !self.ltr.warned_mark_mismatch {
                self.ltr.warned_mark_mismatch = true;
                teprintln!(
                    "AMF LTR: asked to mark frame {frame} as slot {want}, the output reports \
                     {got}; the repair map no longer matches the encoder (logged once)"
                );
            }
        }
        if let (Some(forced), Some(referenced)) = (self.in_flight.force_reference, au.referenced) {
            if referenced & !forced != 0 && !self.ltr.warned_reference_mismatch {
                self.ltr.warned_reference_mismatch = true;
                teprintln!(
                    "AMF LTR: frame {frame} was restricted to slots {forced:#b} but references \
                     {referenced:#b} (logged once)"
                );
            }
        }
    }

    /// Unrequested IDRs at a regular interval say the "no periodic IDR" value this runtime
    /// accepted does not mean that to it. `IDRPeriod` is a dynamic property, so under
    /// `amf_idr_period_flip` the other candidate is written once, live, rather than living with a
    /// keyframe every few seconds for the session.
    fn flip_idr_period(&mut self, gap: u64) {
        self.idr_period_flipped = true;
        let Some(current) = self.get_i64(prop::AMF_VIDEO_ENCODER_IDR_PERIOD) else {
            return;
        };
        let Some(other) = other_idr_period(current) else {
            return;
        };
        let r = self.set_raw(
            prop::AMF_VIDEO_ENCODER_IDR_PERIOD,
            AMFVariantStruct::int64(other),
        );
        teprintln!(
            "AMF: IDRPeriod={current} still produced an IDR every {gap} outputs; switching to \
             {other} live ({})",
            result_name(r)
        );
    }

    /// `Terminate` and a fresh `Init()` after a stalled submit, so the picture stuck inside AMF
    /// can never come out as a later frame's. Counted, and logged at powers of two.
    fn recover_from_stall(&mut self) -> Result<()> {
        self.stalls += 1;
        if log_now(self.stalls) {
            teprintln!(
                "AMF encoder stalled for {:?} ({} so far); re-initialising the component",
                self.submit_deadline,
                self.stalls
            );
        }
        self.terminate_component("after a stalled submit");
        // the fresh component holds no long-term references: nothing pending may name one
        self.ltr.force_reference = None;
        self.ltr.next_slot = 0;
        self.ltr.live_slots = 0;
        self.configure_and_init()?;
        self.read_extradata();
        Ok(())
    }

    /// An output object that carried no bytes. Logged once per encoder, because if a driver ever
    /// does this it will do it often and this is a per-frame path.
    fn note_empty_output(&mut self) {
        if !self.warned_empty_output {
            self.warned_empty_output = true;
            teprintln!(
                "AMF QueryOutput returned an output buffer of zero bytes; treating it as a \
                 dropped frame rather than waiting {SUBMIT_DEADLINE:?} for an output that has \
                 already been consumed"
            );
        }
    }

    /// One `NotReady` return timed, toward deciding whether `QueryTimeout` is real.
    fn record_query_timeout(&mut self, elapsed: Duration) {
        let Some(probe) = self.query_timeout_probe.as_mut() else {
            return;
        };
        let Some(honoured) = probe.record(elapsed) else {
            return;
        };
        let (fast, samples) = (probe.fast(), probe.samples());
        self.query_timeout_probe = None;
        if honoured {
            tprintln!(
                "AMF QueryTimeout: measured; QueryOutput waits inside the driver \
                 ({fast}/{samples} empty returns under 0.5 ms)"
            );
        } else {
            self.query_timeout_supported = false;
            teprintln!(
                "AMF QueryTimeout: read back as set, but {fast}/{samples} empty QueryOutput \
                 returns came back in under 0.5 ms, so the driver is not waiting; polling with \
                 backoff instead of spinning on QueryOutput"
            );
        }
    }

    /// One `QueryOutput`, classified.
    ///
    /// "Nothing yet" is decided by the NULL out-pointer, not by a result code: `QueryOutput` can
    /// return `AMF_OK` with a null `ppData`, and neither `AMF_REPEAT` nor `AMF_NEED_MORE_INPUT`
    /// is a reliable signal. Both FFmpeg and OBS test the pointer. A **non**-null pointer whose
    /// buffer is empty is a different thing entirely; see [`QueryClass`].
    fn query_once(&mut self) -> Result<QueryOutcome> {
        let started =
            (self.query_timeout_supported && self.query_timeout_probe.is_some()).then(Instant::now);
        let mut raw: *mut AMFData = ptr::null_mut();
        // SAFETY: vtable slot 23 (AMFComponent::QueryOutput) on a live initialised component.
        let r = unsafe { ((*(*self.component).pVtbl).QueryOutput)(self.component, &mut raw) };

        if raw.is_null() {
            return match classify_query_output(true, true, r) {
                QueryClass::NotReady => {
                    if let Some(started) = started {
                        self.record_query_timeout(started.elapsed());
                    }
                    Ok(QueryOutcome::NotReady)
                }
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
        // buffer is released, which `Owned` does after this copy. The copy goes into the retained
        // scratch buffer, so the only work done while the driver still owns the mapping is the
        // memcpy.
        let len = unsafe {
            let native = ((*(*buffer.as_ptr()).pVtbl).GetNative)(buffer.as_ptr());
            let size = ((*(*buffer.as_ptr()).pVtbl).GetSize)(buffer.as_ptr());
            if native.is_null() || size == 0 {
                0
            } else {
                if self.au_scratch.len() < size {
                    self.au_scratch.resize(size, 0);
                }
                ptr::copy_nonoverlapping(native as *const u8, self.au_scratch.as_mut_ptr(), size);
                size
            }
        };
        // SAFETY: the buffer's first 13 vtable slots are the AMFPropertyStorage prefix.
        let storage = unsafe { as_property_storage(buffer.as_ptr()) };
        let read = |name: &[u16]| -> Option<i64> {
            // SAFETY: `storage` is the live output buffer viewed through its storage prefix.
            unsafe { get_property(storage, name) }
                .ok()
                .and_then(|v| v.as_i64())
        };
        let prop_id = read(prop::FRAME_IDENTITY);
        // SAFETY: vtable slot 20 (AMFData::GetPts) on the live buffer.
        let pts = Some(unsafe { ((*(*buffer.as_ptr()).pVtbl).GetPts)(buffer.as_ptr()) });
        if classify_query_output(false, len == 0, r) == QueryClass::Empty {
            return Ok(QueryOutcome::Empty(pts, prop_id));
        }
        let buffer_type = read(prop::AMF_VIDEO_ENCODER_OUTPUT_BUFFER_TYPE);
        // Only `_IDR` is stream-startable (a plain `_I` leaves a WHEP joiner on a green screen).
        // `OutputDataType` is its own enum (IDR = 0), not the picture-type one.
        let idr = read(prop::AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE)
            .map(|v| v == AMF_VIDEO_ENCODER_OUTPUT_DATA_TYPE_IDR as i64);
        // the LTR read-backs only matter on a frame that asked for something
        let marked = self
            .in_flight
            .mark
            .and_then(|_| read(prop::AMF_VIDEO_ENCODER_OUTPUT_MARKED_LTR_INDEX));
        let referenced = self
            .in_flight
            .force_reference
            .and_then(|_| read(prop::AMF_VIDEO_ENCODER_OUTPUT_REFERENCED_LTR_INDEX_BITFIELD));
        let qp = if self.switches.statistics_feedback || self.qp_check.is_some() {
            read(prop::AMF_VIDEO_ENCODER_STATISTIC_FRAME_QP)
                .or_else(|| read(prop::AMF_VIDEO_ENCODER_STATISTIC_AVERAGE_QP))
                .map(|q| q.clamp(0, 51) as u8)
        } else {
            None
        };

        // release the driver's buffer before allocating the owned `Vec`
        drop(buffer);
        drop(data);

        // only a forced IDR carries parameter sets; on a P frame `patch_annexb` stops at the first
        // slice NAL, reading one NAL header and allocating nothing
        let patched = if self.patch_sps_vui {
            super::sps::patch_annexb(&self.au_scratch[..len], self.vui_full_range)
        } else {
            None
        };
        let bytes = patched.unwrap_or_else(|| self.au_scratch[..len].to_vec());
        Ok(QueryOutcome::Au(AuOut {
            bytes,
            pts,
            prop_id,
            buffer_type,
            qp,
            idr,
            marked,
            referenced,
        }))
    }

    /// Applies the latency property table and calls `Init()`, falling back from
    /// `ULTRA_LOW_LATENCY` to `LOW_LATENCY` if either the usage probe or `Init()` says no.
    fn configure_and_init(&mut self) -> Result<()> {
        let probe_ok = self.probe_ultra_low_latency();
        let usage = usage_after_probe(probe_ok);
        if !probe_ok {
            teprintln!(
                "AMF: ULTRA_LOW_LATENCY usage rejected by this driver/GPU; falling back to \
                 LOW_LATENCY (see AMF issue #410: pre-RDNA parts on Adrenalin 23.7.1+)"
            );
        }

        self.apply_properties(usage)?;
        match self.init_component() {
            Ok(()) => {
                self.usage = usage;
            }
            Err(e) => {
                let Some(retry) = retry_usage_after_init_failure(usage) else {
                    return Err(e);
                };
                teprintln!(
                    "AMF Init() failed under {} ({e:#}); retrying with {}",
                    usage_name(usage),
                    usage_name(retry)
                );
                // re-`Init` after a failed `Init` is undocumented (Sunshine and FFmpeg reopen), so
                // terminate first; AMF_NOT_INITIALIZED is the expected answer
                self.terminate_component("before the LOW_LATENCY retry");
                // USAGE re-defaults most of the table, so all of it is re-applied
                self.apply_properties(retry)?;
                self.init_component()
                    .context("AMF Init() failed under both ULTRA_LOW_LATENCY and LOW_LATENCY")?;
                self.usage = retry;
            }
        }

        // read back what stuck: on GCN parts (RX 580, Vega) RateControlMethod reportedly cannot be
        // overridden after ULL, and assuming CBR when it is not makes bitrate control look broken
        self.rate_control = self
            .get_i64(prop::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD)
            .unwrap_or(AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN as i64)
            as AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD;
        self.cqp_not_honoured = false;
        if cqp_was_refused(self.config.qp.is_some(), self.rate_control) {
            // QPI/QPP mean nothing under the usage's own rate control, and its default bitrate is
            // nobody's choice: run at the session bitrate with a QP floor at the requested QP
            if self.switches.cqp_fallback_rate {
                let qp = self.config.qp.unwrap_or(0) as i64;
                let applied = self
                    .write_bitrate(self.config.bitrate_bps as i64)
                    .and_then(|()| self.set_i64(prop::AMF_VIDEO_ENCODER_MIN_QP, qp.clamp(0, 51)));
                match applied {
                    Ok(()) => {
                        self.cqp_not_honoured = true;
                        teprintln!(
                            "AMF: CONSTANT_QP did not stick (rate control reads {}); running \
                             at bitrate_bps={} with MinQP={qp} and letting congestion control \
                             adjust it",
                            rate_control_name(self.rate_control),
                            self.config.bitrate_bps
                        );
                    }
                    Err(e) => teprintln!(
                        "AMF: CONSTANT_QP did not stick (rate control reads {}) and the bitrate \
                         fallback was refused ({e:#}); the stream runs at the usage's default",
                        rate_control_name(self.rate_control)
                    ),
                }
            } else {
                teprintln!(
                    "AMF: CONSTANT_QP did not stick (rate control reads {}); amf_cqp_fallback_rate \
                     is off, so the stream runs at the usage's default bitrate",
                    rate_control_name(self.rate_control)
                );
            }
        }
        self.rate_tier = select_rate_tier(self.config.qp.is_some(), self.rate_control);
        // announce the range AMF says it encodes: a runtime that refused `FullRangeColor` encodes
        // limited range from BGRA all the same
        self.vui_full_range = self
            .get_i64(prop::AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR)
            .map_or_else(|| self.full_range_input(), |v| v != 0);
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

        // the LTR budget is read back too: a slot the driver did not grant would make a "repair"
        // frame reference a picture that was never long-term, worse than the IDR it replaced
        if self.ltr.max_frames > 0 {
            let granted = self
                .get_i64(prop::AMF_VIDEO_ENCODER_MAX_LTR_FRAMES)
                .unwrap_or(0)
                .clamp(0, MAX_LTR_FRAMES_SUPPORTED);
            if granted != self.ltr.max_frames {
                teprintln!(
                    "AMF LTR: asked for {} slot(s), the encoder reports {}; using {}",
                    self.ltr.max_frames,
                    granted,
                    granted
                );
                self.ltr.max_frames = granted;
            }
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
        let fps = cfg.fps.max(1);
        let bitrate = cfg.bitrate_bps.max(1) as i64;
        // every optional property's gate result and read-back, printed as one line at the end
        let mut report: Vec<String> = Vec::with_capacity(32);

        // only Usage, FrameSize, Profile and rate control with its bitrate/VBV/QP are fatal; a
        // refusal of anything else must not cost the AMD path, so it goes through `set_gated_*`

        // USAGE first: setting it overwrites most other settings, rate control included
        self.set_i64(prop::AMF_VIDEO_ENCODER_USAGE, usage as i64)
            .context("setting AMF Usage")?;

        // the ULL and LL default already; SPEED is 1 on AVC but 10 on HEVC and 100 on AV1
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_QUALITY_PRESET,
            AMF_VIDEO_ENCODER_QUALITY_PRESET_SPEED as i64,
        );

        self.set_var(
            prop::AMF_VIDEO_ENCODER_FRAMESIZE,
            AMFVariantStruct::size(cfg.width as i32, cfg.height as i32),
        )?;
        // a wrong fps mis-sizes both the VBV and the per-frame bit budget
        self.apply_frame_rate(&mut report, fps)?;

        let profile = match cfg.profile {
            H264Profile::Baseline => AMF_VIDEO_ENCODER_PROFILE_CONSTRAINED_BASELINE,
            H264Profile::Main => AMF_VIDEO_ENCODER_PROFILE_MAIN,
            H264Profile::High => AMF_VIDEO_ENCODER_PROFILE_HIGH,
        };
        self.set_i64(prop::AMF_VIDEO_ENCODER_PROFILE, profile as i64)?;
        // AMF's default level is 4.2 (1080p up to 64 fps), raised only for frame size; writing the
        // SDP's level makes SPS and SDP agree at 1080p120, 1440p60 and above
        if self.switches.profile_level {
            match profile_level_for(cfg.width, cfg.height, fps, self.caps.max_level) {
                Some(level) => {
                    self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_PROFILE_LEVEL, level)
                }
                None => report.push("ProfileLevel=default(no MaxLevel cap)".to_string()),
            }
        }

        if let Some(q) = cfg.qp {
            let q = q as i64;
            self.set_i64(
                prop::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP as i64,
            )?;
            self.set_i64(prop::AMF_VIDEO_ENCODER_QP_I, q)?;
            self.set_i64(prop::AMF_VIDEO_ENCODER_QP_P, q)?;
            // QPB too, as OBS does; and the range opened to 0..51 (as FFmpeg), so a narrower usage
            // default cannot clamp the QP asked for or a later `set_qp`
            self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_QP_B, q);
            self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_MIN_QP, 0);
            self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_MAX_QP, 51);
        } else {
            self.set_i64(
                prop::AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD,
                AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR as i64,
            )?;
            // peak == target so no frame can balloon and head-of-line-block the next; VBV is one
            // frame of bits, the single biggest CBR latency lever
            self.write_bitrate(bitrate)?;
            // a QP floor keeps the encoder from spending the whole CBR budget on an easy picture;
            // it only shrinks the stream, so it never fights congestion control as CONSTANT_QP does
            if let Some(q) = cfg.min_qp.filter(|&q| q > 1) {
                self.set_i64(prop::AMF_VIDEO_ENCODER_MIN_QP, q as i64)?;
            }
        }
        // InitialVBVBufferFullness keeps its default of 64 (= 100%): 0 would be an *empty* buffer

        // Without `EnforceHRD` the one-frame VBV bounds nothing (AMF's doc ties the VBV constraint
        // to it); ULL defaults to it, LL does not. Constant QP has no rate model to enforce.
        if !self.switches.enforce_hrd {
            self.set_gated_bool(&mut report, prop::AMF_VIDEO_ENCODER_ENFORCE_HRD, false);
        } else if cfg.qp.is_none() {
            self.set_gated_bool(&mut report, prop::AMF_VIDEO_ENCODER_ENFORCE_HRD, true);
        }
        // filler pads bytes that cost transmit time
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_FILLER_DATA_ENABLE,
            false,
        );
        // a skipped frame is one the client never paints; the default "depends on USAGE" and is
        // undocumented per usage, so it is written (as FFmpeg and Sunshine do)
        if self.switches.rc_skip_off {
            self.set_gated_bool(
                &mut report,
                prop::AMF_VIDEO_ENCODER_RATE_CONTROL_SKIP_FRAME_ENABLE,
                false,
            );
        }

        // A B-frame needs a future frame, i.e. a frame of delay. 0 is the documented default, so a
        // refused write (Annex A: needs MaxOfReferenceFrames > 1 and no LTR) still leaves none.
        // Consecutive-B is only touched where B-frames exist (not VCN 1.0-2.x, RDNA1 included).
        self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_B_PIC_PATTERN, 0);
        if self.caps.b_frames {
            self.set_gated_i64(
                &mut report,
                prop::AMF_VIDEO_ENCODER_MAX_CONSECUTIVE_BPICTURES,
                0,
            );
        }

        // ULL sets this itself, and not every driver has the property
        self.set_gated_bool(&mut report, prop::AMF_VIDEO_ENCODER_LOWLATENCY_MODE, true);

        // lookahead and per-frame analysis off: each is a frame of delay or an extra pass
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_PREENCODE_ENABLE,
            AMF_VIDEO_ENCODER_PREENCODE_DISABLED as i64,
        );
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_PRE_ANALYSIS_ENABLE,
            false,
        );
        self.set_gated_bool(&mut report, prop::AMF_VIDEO_ENCODER_ENABLE_VBAQ, false);
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_HIGH_MOTION_QUALITY_BOOST_ENABLE,
            false,
        );

        // No periodic IDR (each is a bitrate spike). Header spacing 0: parameter sets ride the
        // per-surface InsertSPS/InsertPPS on forced IDRs. IntraPeriod first appears in the 1.4.30
        // headers; it and HeaderInsertionSpacing default to 0, so a refusal costs nothing.
        self.apply_idr_period(&mut report);
        self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_INTRA_PERIOD, 0);
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_HEADER_INSERTION_SPACING,
            0,
        );

        // Deblocking is free on the encoder and the usage default. One slice: more only pay off
        // with OUTPUT_MODE = SLICE, which needs the packetizer to emit partial access units.
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_DE_BLOCKING_FILTER,
            true,
        );
        self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_SLICES_PER_FRAME, 1);
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_OUTPUT_MODE,
            AMF_VIDEO_ENCODER_OUTPUT_MODE_FRAME as i64,
        );

        // CAVLC for Baseline, a cheaper client decode. The default UNDEFINED picks the same, so a
        // refusal changes nothing.
        let coding = match cfg.profile {
            H264Profile::Baseline => AMF_VIDEO_ENCODER_CALV,
            _ => AMF_VIDEO_ENCODER_CABAC,
        };
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_CABAC_ENABLE,
            coding as i64,
        );

        // Range follows the input format (`full_range_input`). `FullRangeColor` is reportedly the
        // only colour property AMF writes into an AVC bitstream; `amd/sps.rs` adds the rest.
        let full_range = self.full_range_input();
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_INPUT_FULL_RANGE_COLOR,
            full_range,
        );
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_OUTPUT_FULL_RANGE_COLOR,
            full_range,
        );
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_OUTPUT_COLOR_PRIMARIES,
            COLOR_PRIMARIES_BT709,
        );
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_OUTPUT_TRANSFER_CHARACTERISTIC,
            TRANSFER_CHARACTERISTIC_BT709,
        );
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_OUTPUT_MATRIX_COEFF,
            MATRIX_COEFF_BT709,
        );
        self.set_gated_i64(
            &mut report,
            prop::AMF_VIDEO_ENCODER_OUTPUT_COLOR_PROFILE,
            if full_range {
                COLOR_PROFILE_FULL_709
            } else {
                COLOR_PROFILE_709
            },
        );

        // SmartAccess Video buys throughput across VCN instances, not latency on one stream
        self.set_gated_bool(
            &mut report,
            prop::AMF_VIDEO_ENCODER_ENABLE_SMART_ACCESS_VIDEO,
            false,
        );

        // candidate only: the queue (default 16) stays empty under one-in-flight, and a runtime
        // counting a surface until its output is released would answer every submit AMF_INPUT_FULL
        if self.switches.input_queue_one {
            self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_INPUT_QUEUE_SIZE, 1);
        }

        // Intra-refresh is 0 on every live join (a refresh wave paints visible bands in flat
        // regions) and under LTR, where AMF turns it off anyway and the 0 shows that in the
        // read-back. Otherwise (`SE_INTRA_REFRESH=1`) ULL's 255 MBs/slot stays. MaxNumRefFrames is
        // never set: 1 reference would switch intra-refresh off (it needs more than one).
        if !cfg.intra_refresh || self.ltr.max_frames > 0 {
            self.set_gated_i64(
                &mut report,
                prop::AMF_VIDEO_ENCODER_INTRA_REFRESH_NUM_MBS_PER_SLOT,
                0,
            );
        }

        // the default `LTRMode = RESET_UNUSED` discards every slot a forced bitfield did not name,
        // while the repair map still believes it holds them; `KEEP_UNUSED` (AMF 1.4.21+) keeps them
        if self.ltr.max_frames > 0 {
            self.set_gated_i64(
                &mut report,
                prop::AMF_VIDEO_ENCODER_MAX_LTR_FRAMES,
                self.ltr.max_frames,
            );
            if self.switches.ltr_keep_unused {
                self.set_gated_i64(
                    &mut report,
                    prop::AMF_VIDEO_ENCODER_LTR_MODE,
                    AMF_VIDEO_ENCODER_LTR_MODE_KEEP_UNUSED as i64,
                );
            }
        }

        // QueryTimeout goes before Init: set afterwards it reportedly returns AMF_OK, echoes back
        // and does nothing (AMF issue #516). Honoured, `QueryOutput` blocks for 1 ms.
        self.set_gated_i64(&mut report, prop::AMF_VIDEO_ENCODER_QUERY_TIMEOUT, 1);

        // `name=v(asked w)`: the driver kept its own value. `rejected(CODE)`: a known name it
        // would not take. `unknown`: `HasProperty` denied it. `unknown-but-set:`: denied, then
        // the write was accepted.
        tprintln!("AMF optional properties: {}", report.join(", "));

        Ok(())
    }

    /// `FrameRate` at the real rate, or, when that is above AMF's documented 120 fps and the
    /// runtime refuses it, at 120. Fatal only when even that is refused, because then the rate
    /// controller's per-frame budget would be the usage default of 30 fps. VBV stays sized from
    /// the real rate either way (`write_bitrate`), which is one frame of what is actually sent.
    fn apply_frame_rate(&self, report: &mut Vec<String>, fps: u32) -> Result<()> {
        let mut refused: Vec<String> = Vec::new();
        for rate in frame_rate_candidates(fps, self.switches.framerate_fallback) {
            let r = self.set_raw(
                prop::AMF_VIDEO_ENCODER_FRAMERATE,
                AMFVariantStruct::rate(rate, 1),
            );
            if r == AMF_OK {
                self.rc_fps.set(rate);
                if rate != fps {
                    let target = if self.switches.framerate_rc_scale {
                        format!("Target/Peak scaled by {rate}/{fps}")
                    } else {
                        format!(
                            "Target/Peak unscaled (amf_framerate_rc_scale off: up to \
                             {fps}/{rate} of the target on the wire)"
                        )
                    };
                    teprintln!(
                        "AMF refused FrameRate={fps} ({}); running the rate controller at {rate} \
                         fps with a VBV sized for {fps} and {target}",
                        refused.join(", ")
                    );
                    report.push(format!("FrameRate={rate}(asked {fps})"));
                }
                return Ok(());
            }
            refused.push(format!("{rate}: {}", result_name(r)));
        }
        Err(anyhow!(
            "SetProperty(FrameRate) refused every rate tried ({})",
            refused.join(", ")
        ))
    }

    /// No periodic IDR. Both candidates mean "never" to someone (AMF's doc: 0; Sunshine and FFmpeg:
    /// `INT_MAX`) and the first the runtime takes wins. If it takes neither, the usage's own period
    /// stays (a keyframe every few seconds, which the unrequested-IDR count reports): never fatal.
    fn apply_idr_period(&self, report: &mut Vec<String>) {
        let mut refused: Vec<String> = Vec::new();
        let candidates = idr_period_candidates(self.switches.idr_period_zero);
        for (i, &period) in candidates.iter().enumerate() {
            let r = self.set_raw(
                prop::AMF_VIDEO_ENCODER_IDR_PERIOD,
                AMFVariantStruct::int64(period),
            );
            if r == AMF_OK {
                let read = self.get_i64(prop::AMF_VIDEO_ENCODER_IDR_PERIOD);
                if !idr_period_took(period, read, i + 1 == candidates.len()) {
                    refused.push(format!("{period}:read-back {}", read.unwrap_or_default()));
                    continue;
                }
                report.push(match read {
                    Some(v) if v == period => format!("IDRPeriod={v}"),
                    Some(v) => format!("IDRPeriod={v}(asked {period})"),
                    None => format!("IDRPeriod=set({period}),unreadable"),
                });
                if !refused.is_empty() {
                    report.push(format!("IDRPeriod-refused=[{}]", refused.join(";")));
                }
                return;
            }
            refused.push(format!("{period}:{}", result_name(r)));
        }
        teprintln!(
            "AMF refused every IDRPeriod tried ({}); the usage's periodic IDR stays on",
            refused.join(", ")
        );
        report.push(format!("IDRPeriod=rejected[{}]", refused.join(";")));
    }

    /// `Terminate` on the component, for a re-`Init`. Tolerates `AMF_NOT_INITIALIZED`, the
    /// expected answer after a failed `Init`; anything else is logged, never fatal, the `Init`
    /// that follows is the real test.
    fn terminate_component(&self, why: &str) {
        // SAFETY: vtable slot 19 (AMFComponent::Terminate) on a live component.
        let r = unsafe { ((*(*self.component).pVtbl).Terminate)(self.component) };
        if r != AMF_OK && r != AMF_NOT_INITIALIZED {
            teprintln!(
                "AMFComponent::Terminate {why} returned {}; continuing",
                result_name(r)
            );
        }
    }

    fn init_component(&mut self) -> Result<()> {
        // A read-back of 0 means the write did not take (FFmpeg's test). Non-zero is only an echo,
        // which a runtime ignoring the property also gives, so it is then timed on the first polls.
        let echoed = self
            .get_i64(prop::AMF_VIDEO_ENCODER_QUERY_TIMEOUT)
            .unwrap_or(0);
        self.query_timeout_supported = echoed != 0;
        self.query_timeout_probe = (self.query_timeout_supported
            && self.switches.measure_query_timeout)
            .then(QueryTimeoutProbe::default);
        if self.query_timeout_supported != self.caps.query_timeout_support {
            tprintln!(
                "AMF QueryTimeout: caps report support={}, the property read back as {}; \
                 trusting the read-back{}",
                self.caps.query_timeout_support,
                echoed,
                if self.query_timeout_probe.is_some() {
                    " until the first empty polls are timed"
                } else {
                    ""
                }
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
        })?;
        self.after_init();
        Ok(())
    }

    /// The first output of a fresh (or re-initialised) component is an IDR whatever was asked,
    /// it holds no long-term references yet, and nothing submitted before can come out of it.
    fn after_init(&mut self) {
        self.idr_audit.outputs_since_init = 0;
        self.idr_audit.last_idr_at = 0;
        self.idr_audit.last_unrequested_gap = None;
        self.ltr.live_slots = 0;
        self.fresh_init = true;
        self.qp_check = None;
        // a copy the re-init removed must not vote "not taken" when the next picture arrives
        self.nmi_probe = None;
        self.last_resubmitted = None;
    }

    /// The Annex-B SPS+PPS out of `ExtraData`, which is only populated after `Init()`. `Err` is
    /// the reason it could not be had, for the log.
    fn fetch_extradata(&self) -> std::result::Result<Vec<u8>, String> {
        // SAFETY: the component's first 13 vtable slots are the AMFPropertyStorage prefix.
        let storage = unsafe { as_property_storage(self.component) };
        // SAFETY: `storage` is the live component viewed through that prefix.
        let var = unsafe { get_property(storage, prop::AMF_VIDEO_ENCODER_EXTRADATA) }
            .map_err(|r| format!("GetProperty(ExtraData) returned {}", result_name(r)))?;
        let iface = var.as_interface().ok_or_else(|| {
            format!(
                "ExtraData is not an interface (variant type {})",
                var.r#type
            )
        })?;
        // the variant handed us one reference; wrap it so it is released either way
        let owned = Owned(iface.cast::<AMFBuffer>());
        // SAFETY: vtable slot 2 (AMFInterface::QueryInterface) on the interface the variant
        // carried; on success the buffer view is a second reference we also own.
        let buffer =
            unsafe { query_interface::<AMFBuffer, AMFBuffer>(owned.as_ptr(), &IID_AMF_BUFFER) }
                .map_err(|r| {
                    format!("QueryInterface(IID_AMFBuffer) returned {}", result_name(r))
                })?;
        let buffer = Owned(buffer);
        // SAFETY: vtable slots 25/24 on a live buffer whose memory outlives this copy.
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
            return Err("ExtraData is empty".to_string());
        }
        Ok(bytes)
    }

    /// Decides, once per `Init()`, whether the per-frame path has to keep inspecting in-band SPSs.
    ///
    /// AMF is reported never to write `bitstream_restriction_flag` into an AVC SPS, which is
    /// what tells a decoder `max_num_reorder_frames = 0`, nor the `Out*` colour description.
    /// Without the restriction a decoder infers the reorder depth from the level and holds frames
    /// before showing the first. Only an `ExtraData` SPS that was read and is already right lets
    /// the per-frame check stand down (`keep_patching_sps`).
    fn read_extradata(&mut self) {
        use super::sps::{inspect_annexb, summarize_annexb, SpsInspection};

        let full_range = self.vui_full_range;
        let extradata = match self.fetch_extradata() {
            Ok(bytes) => bytes,
            Err(reason) => {
                self.patch_sps_vui = true;
                teprintln!(
                    "AMF ExtraData unavailable ({reason}); the SPS of every forced IDR is \
                     inspected and given the BT.709 / max_num_reorder_frames=0 VUI in band"
                );
                self.check_ltr_against_sps(None);
                return;
            }
        };
        self.check_ltr_against_sps(summarize_annexb(&extradata).map(|s| s.max_num_ref_frames));
        let inspection = inspect_annexb(&extradata, full_range);
        self.patch_sps_vui = keep_patching_sps(&inspection);
        let sps = match &inspection {
            SpsInspection::Patched(patched) => summarize_annexb(patched),
            _ => summarize_annexb(&extradata),
        };
        let sps = match sps {
            Some(s) => format!(
                "profile_idc={}, level_idc={}, max_num_ref_frames={}",
                s.profile_idc, s.level_idc, s.max_num_ref_frames
            ),
            None => "no readable SPS".to_string(),
        };
        let range = if full_range { "full" } else { "limited" };
        match inspection {
            SpsInspection::Patched(_) => tprintln!(
                "AMF SPS/PPS ready (extradata_bytes={}, {sps}); SPS VUI rewritten for BT.709 \
                 {range} range and max_num_reorder_frames=0",
                extradata.len()
            ),
            SpsInspection::AlreadyCorrect => tprintln!(
                "AMF SPS/PPS ready (extradata_bytes={}, {sps}); this driver already writes the \
                 colour description and the bitstream restriction, so the SPS is left alone",
                extradata.len()
            ),
            SpsInspection::NoSps => teprintln!(
                "AMF ExtraData carries no SPS (extradata_bytes={}); the SPS of every forced IDR \
                 is inspected and patched in band",
                extradata.len()
            ),
            SpsInspection::Unparseable => teprintln!(
                "AMF ExtraData's SPS could not be parsed (extradata_bytes={}); the SPS of every \
                 forced IDR is inspected and patched in band where it can be",
                extradata.len()
            ),
        }
    }

    /// Long-term references are only safe when the SPS declares room for them next to the
    /// short-term one. An SPS that cannot be read, or that under-declares, turns them off: every
    /// repair is then an IDR, which is what every other tier does, instead of a P frame the
    /// client may no longer hold a reference for. The component stays in user-LTR mode, so it
    /// also keeps rolling intra refresh off; that is what live joins want anyway, but a probe run
    /// with `SE_INTRA_REFRESH=1` and LTR gets no refresh.
    fn check_ltr_against_sps(&mut self, max_num_ref_frames: Option<u32>) {
        if self.ltr.max_frames <= 0 {
            return;
        }
        match max_num_ref_frames {
            Some(refs) if ltr_fits_sps(refs, self.ltr.max_frames) => {}
            Some(refs) => {
                teprintln!(
                    "AMF LTR: the SPS declares max_num_ref_frames={refs}, which cannot hold one \
                     short-term plus {} long-term reference(s); long-term repair is off and every \
                     loss is repaired with an IDR",
                    self.ltr.max_frames
                );
                self.ltr.max_frames = 0;
            }
            None => {
                teprintln!(
                    "AMF LTR: the SPS could not be read, so its reference count is unknown; \
                     long-term repair is off and every loss is repaired with an IDR"
                );
                self.ltr.max_frames = 0;
            }
        }
        if self.ltr.max_frames == 0 {
            self.ltr.mark_next = None;
            self.ltr.force_reference = None;
            self.ltr.live_slots = 0;
        }
    }

    /// Whether the samples reaching the encoder are full-range.
    ///
    /// Packed BGRA off the capture is full range; NV12 out of the scaler comes from the D3D11
    /// video processor, whose default output is BT.709 **limited**. Signalling full range over
    /// limited-range samples is a washed-out picture, so it is the format's answer rather than
    /// a constant.
    fn full_range_input(&self) -> bool {
        self.surface_format != AMF_SURFACE_NV12
    }

    fn probe_caps(&self) -> AmfCaps {
        let mut caps = AmfCaps::unknown();

        let mut raw: *mut AMFCaps = ptr::null_mut();
        // SAFETY: vtable slot 26 (AMFComponent::GetCaps). Documented and demonstrated to work
        // before Init(), this is exactly what OBS's preset-compatibility check does.
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

    fn build_ring(&mut self, format: DXGI_FORMAT) -> Result<()> {
        let mut ring = Vec::with_capacity(self.ring_len);
        for _ in 0..self.ring_len {
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
            // not fatal: AMF accepts packed RGB, through an internal GPU colour converter that
            // competes with the foreground application
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
    /// once per session, when the pipeline hands us something other than
    /// [`PREFERRED_INPUT_FORMAT`].
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

    fn set_raw(&self, name: &[u16], value: AMFVariantStruct) -> AMF_RESULT {
        // SAFETY: the component's first 13 vtable slots are the AMFPropertyStorage prefix, and
        // `name` is a NUL-terminated static.
        unsafe { set_property(as_property_storage(self.component), name, value) }
    }

    fn set_var(&self, name: &[u16], value: AMFVariantStruct) -> Result<()> {
        let r = self.set_raw(name, value);
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

    /// Whether this build of the runtime knows a property at all.
    fn knows(&self, name: &[u16]) -> bool {
        // SAFETY: as `set_raw`; `HasProperty` is vtable slot 5 of the same prefix.
        unsafe { has_property(as_property_storage(self.component), name) }
    }

    /// A knob that varies by driver and GPU generation, written through both halves of the gate:
    /// `HasProperty` first, then `SetProperty`'s own return code.
    ///
    /// `HasProperty` is *reported*, not obeyed: what AMF's implementation counts as "has" (a
    /// declared property with a default, or one that has been written) is unconfirmed, so a
    /// `false` still gets the write. A refused optional write is free on AMF (`AMF_NOT_FOUND` /
    /// `AMF_NOT_SUPPORTED` and nothing else happens), unlike NVENC where asking for an
    /// unsupported feature costs the whole vendor path.
    ///
    /// Every outcome lands in `report`. `want` is what was asked for, as an integer, so a driver
    /// that silently keeps its own value is visible rather than assumed.
    fn set_gated(
        &self,
        report: &mut Vec<String>,
        name: &[u16],
        value: AMFVariantStruct,
        want: i64,
    ) {
        let label = wide_to_string(name);
        let known = self.knows(name);
        let r = self.set_raw(name, value);
        if r != AMF_OK {
            report.push(format!(
                "{label}={}{}",
                if known { "rejected" } else { "unknown" },
                format_args!("({})", result_name(r))
            ));
            return;
        }
        let prefix = if known { "" } else { "unknown-but-set:" };
        match self.get_i64(name) {
            Some(read) if read == want => report.push(format!("{label}={prefix}{read}")),
            Some(read) => report.push(format!("{label}={prefix}{read}(asked {want})")),
            None => report.push(format!("{label}={prefix}set,unreadable")),
        }
    }

    fn set_gated_i64(&self, report: &mut Vec<String>, name: &[u16], v: i64) {
        self.set_gated(report, name, AMFVariantStruct::int64(v), v);
    }

    fn set_gated_bool(&self, report: &mut Vec<String>, name: &[u16], v: bool) {
        self.set_gated(report, name, AMFVariantStruct::bool(v), i64::from(v));
    }

    fn get_i64(&self, name: &[u16]) -> Option<i64> {
        // SAFETY: as `set_var`.
        unsafe { get_property(as_property_storage(self.component), name) }
            .ok()
            .and_then(|v| v.as_i64())
    }

    /// Formats an `AMF_RESULT` with AMD's own message when `AMFTrace::GetResultText` is bound.
    ///
    /// The class hint is load-bearing: `pipeline::is_transient_encode_error` looks for the
    /// substring "device busy" to decide whether to drop the frame and carry on or tear the
    /// session down, so a transient AMF result has to say it in those words.
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

#[cfg(test)]
impl Encoder {
    /// Whether the collect loop currently trusts `QueryOutput` to wait for it.
    pub(crate) fn query_timeout_blocking(&self) -> bool {
        self.query_timeout_supported
    }

    pub(crate) fn set_submit_deadline(&mut self, deadline: Duration) {
        self.submit_deadline = deadline;
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Terminate down the ownership chain, then release up: releasing the context before the
        // component is terminated, or unloading the library with an AMF object alive, frees the
        // code the vtables point into. `amf` is the last field, so the Library goes last.
        if !self.component.is_null() {
            // SAFETY: vtable slot 20 (Drain) then 23 (QueryOutput) then 19 (Terminate) then 1
            // (Release) on a component we own. Drain is best-effort: this path also runs after
            // a construction failure, where the component may never have been initialised.
            unsafe {
                let drain = ((*(*self.component).pVtbl).Drain)(self.component);
                if drain == AMF_OK {
                    let started = Instant::now();
                    let mut polls = 0u32;
                    loop {
                        let mut data: *mut AMFData = ptr::null_mut();
                        let r = ((*(*self.component).pVtbl).QueryOutput)(self.component, &mut data);
                        if !data.is_null() {
                            release(data);
                        }
                        if drain_should_stop(r, started.elapsed()) {
                            break;
                        }
                        if data.is_null() {
                            polls = polls.saturating_add(1);
                            PollWait::after(polls, true).wait();
                        }
                    }
                }
                let r = ((*(*self.component).pVtbl).Terminate)(self.component);
                // AMF_NOT_INITIALIZED is expected when construction failed before Init
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

        // the factory is a DLL singleton and AMFTrace has no Release slot: neither is ours to free
        self.factory = ptr::null_mut();
        self.trace = ptr::null_mut();
    }
}

/// BT.709 in `AMF_COLOR_PRIMARIES_ENUM` / `AMF_COLOR_TRANSFER_CHARACTERISTIC_ENUM`
/// (components/ColorSpace.h). Set for completeness; AMF is reported not to write any of the
/// `Out*` colour properties into an AVC bitstream, which is why `FullRangeColor` carries the
/// signalling that actually reaches the client.
pub(crate) const COLOR_PRIMARIES_BT709: i64 = 1;
pub(crate) const TRANSFER_CHARACTERISTIC_BT709: i64 = 1;
/// `AMF_COLOR_MATRIX_COEFF_BT_709`, the VUI `matrix_coefficients` value.
pub(crate) const MATRIX_COEFF_BT709: i64 = 1;
/// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_709`, BT.709 *studio* (limited) range.
pub(crate) const COLOR_PROFILE_709: i64 = 1;
/// `AMF_VIDEO_CONVERTER_COLOR_PROFILE_FULL_709`, BT.709 full range. This is 7, not the 4/5/6
/// the commented-out G10/G2084 entries above it occupy in `ColorSpace.h`.
pub(crate) const COLOR_PROFILE_FULL_709: i64 = 7;

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

/// Every AMD adapter, by **PCI vendor id**, not by description string: descriptions are
/// localized and OEM-rebranded, and WARP's contains none of the vendor names while reporting
/// Microsoft's `0x1414`. Software adapters are rejected outright. All of them, in DXGI's order,
/// because the first is not always one with an encoder (`first_that_works`).
fn amd_adapters() -> Result<Vec<(String, IDXGIAdapter1)>> {
    // SAFETY: DXGI enumeration; every out-parameter is a live local.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().context("CreateDXGIFactory1")?;

        let mut chosen = Vec::new();
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
                chosen.push((format!("adapter {i} ({name})"), adapter));
            }
            i += 1;
        }
        if chosen.is_empty() {
            bail!("no AMD adapter found; AMF requires an AMD Radeon/Ryzen GPU");
        }
        Ok(chosen)
    }
}

fn create_device_on(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    // SAFETY: D3D11 device creation on a live adapter; every out-parameter is a live local.
    unsafe {
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            adapter,
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

/// What [`probe_availability`] found, for the compatibility report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AmfAvailability {
    /// `amfrt64.dll` loaded, the AVC component was created on an AMD adapter, and `GetCaps`
    /// reported a hardware or GPU acceleration type.
    Hardware,
    /// One short sentence naming what stopped it, for the compatibility card and `doctor --json`.
    Unavailable(String),
}

/// Would AMF encode on this machine? Cheap, and safe on a box with no Radeon driver.
///
/// Answers the question the way the pipeline does: load the runtime, take a context on the AMD
/// adapter, create the AVC component and read `GetCaps`. It stops before the property table,
/// the input ring and `Init()`, so it costs a device creation and a component creation.
///
/// A missing `amfrt64.dll` is the normal outcome on any machine without a Radeon driver and is
/// reported as one line, never as a panic.
pub fn probe_availability() -> AmfAvailability {
    // nothing is encoded: the geometry only has to pass AMF's range check, and `open` never
    // reads the bitrate
    const PROBE: EncoderConfig = EncoderConfig {
        width: 1280,
        height: 720,
        fps: 60,
        bitrate_bps: 6_000_000,
        max_bitrate_bps: 6_000_000,
        profile: H264Profile::High,
        qp: None,
        min_qp: None,
        intra_refresh: true,
    };

    let adapters = match amd_adapters() {
        Ok(a) => a,
        Err(e) => return AmfAvailability::Unavailable(one_line(&e)),
    };
    let opened = first_that_works(adapters, |adapter| {
        let (device, context) = create_device_on(&adapter)?;
        Encoder::open(PROBE, device, context, 0)
    });
    match opened {
        Ok((name, encoder)) => {
            let accel = acceleration_name(encoder.caps.acceleration);
            tprintln!("AMF probe: an AVC encoder is available on {name} (accel={accel})");
            AmfAvailability::Hardware
        }
        Err(e) => AmfAvailability::Unavailable(one_line(&e)),
    }
}

/// An `anyhow` chain flattened to one short clause, so it can sit inside a UI string.
fn one_line(e: &anyhow::Error) -> String {
    const MAX: usize = 140;
    let mut s = format!("{e:#}").replace(['\r', '\n'], " ");
    while s.contains("  ") {
        s = s.replace("  ", " ");
    }
    if s.chars().count() > MAX {
        s = s.chars().take(MAX - 1).collect::<String>() + "…";
    }
    s
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idr_period_tries_the_documented_off_value_first() {
        assert_eq!(idr_period_candidates(true), [0, i32::MAX as i64]);
        assert_eq!(idr_period_candidates(false), [i32::MAX as i64, 0]);
        assert_eq!(other_idr_period(0), Some(i32::MAX as i64));
        assert_eq!(other_idr_period(i32::MAX as i64), Some(0));
        assert_eq!(
            other_idr_period(240),
            None,
            "the usage default: nothing to flip to"
        );
        for c in idr_period_candidates(true) {
            assert!(
                c == 0 || c > 1000,
                "both candidates mean no periodic IDR; {c} would be a period"
            );
        }
    }

    #[test]
    fn frame_rates_above_the_documented_ceiling_get_a_second_try_at_it() {
        assert_eq!(frame_rate_candidates(60, true), vec![60]);
        assert_eq!(frame_rate_candidates(120, true), vec![120]);
        assert_eq!(frame_rate_candidates(144, true), vec![144, 120]);
        assert_eq!(frame_rate_candidates(550, true), vec![550, 120]);
        assert_eq!(frame_rate_candidates(0, true), vec![1]);
        assert_eq!(frame_rate_candidates(240, false), vec![240]);
    }

    #[test]
    fn a_clamped_frame_rate_scales_the_target_so_bits_per_second_still_match() {
        assert_eq!(rc_target_bitrate(20_000_000, 60, 60), 20_000_000);
        assert_eq!(rc_target_bitrate(20_000_000, 120, 120), 20_000_000);
        assert_eq!(rc_target_bitrate(20_000_000, 240, 120), 10_000_000);
        assert_eq!(rc_target_bitrate(20_000_000, 144, 120), 16_666_666);
        // per-frame budget under AMF's model equals one real frame of the target
        let (bps, fps, held) = (30_000_000i64, 165u32, 120u32);
        let per_frame_rc = rc_target_bitrate(bps, fps, held) / held as i64;
        assert!((per_frame_rc - bps / fps as i64).abs() <= 1);
        // degenerate inputs never write a zero or a negative bitrate
        assert_eq!(rc_target_bitrate(0, 240, 120), 1);
        assert_eq!(rc_target_bitrate(1, 550, 1), 1);
        assert_eq!(rc_target_bitrate(5_000_000, 0, 0), 5_000_000);
    }

    #[test]
    fn an_idr_period_that_reads_back_clamped_is_a_refusal_while_a_candidate_is_left() {
        let max = i32::MAX as i64;
        assert!(idr_period_took(max, Some(max), false));
        assert!(!idr_period_took(max, Some(1000), false));
        assert!(idr_period_took(0, Some(1000), true));
        assert!(idr_period_took(0, None, false));
    }

    #[test]
    fn only_a_regular_run_of_unrequested_idrs_can_flip_the_period_and_only_behind_its_switch() {
        assert!(
            !is_periodic_idr(None, 300),
            "the first one is never a period"
        );
        assert!(is_periodic_idr(Some(300), 300));
        assert!(is_periodic_idr(Some(300), 301));
        assert!(
            !is_periodic_idr(Some(300), 150),
            "an IDR the runtime inserted on its own"
        );
        assert!(
            !is_periodic_idr(Some(1), 1),
            "back-to-back IDRs are not a period"
        );

        assert!(should_flip_idr_period(true, false, true));
        assert!(
            !should_flip_idr_period(false, false, true),
            "switch off: never written"
        );
        assert!(
            !should_flip_idr_period(true, true, true),
            "once per encoder"
        );
        assert!(
            !should_flip_idr_period(true, false, false),
            "no period, no write"
        );
    }

    #[test]
    fn a_stall_reinit_drops_the_forced_reference_but_keeps_the_idr_and_the_mark() {
        let taken = TakenRequests {
            idr: true,
            mark: Some(1),
            force_reference: Some(0b01),
        };
        assert_eq!(requests_after_failure(taken, false), taken);
        let kept = requests_after_failure(taken, true);
        assert!(kept.idr);
        assert_eq!(kept.mark, Some(1));
        assert_eq!(kept.force_reference, None);

        let (mut idr, mut mark, mut force) = (false, None, None);
        restore_requests(kept, &mut idr, &mut mark, &mut force);
        assert!(idr);
        assert_eq!(mark, Some(1));
        assert_eq!(force, None);
    }

    #[test]
    fn profile_level_matches_the_sdp_and_respects_the_adapter_cap() {
        assert_eq!(profile_level_for(1920, 1080, 60, 52), Some(42));
        assert_eq!(profile_level_for(1920, 1080, 120, 52), Some(51));
        assert_eq!(profile_level_for(2560, 1440, 60, 52), Some(51));
        assert_eq!(
            profile_level_for(3840, 2160, 120, 52),
            Some(52),
            "clamped to MaxLevel"
        );
        assert_eq!(
            profile_level_for(1920, 1080, 120, 0),
            None,
            "no cap: AMF's default"
        );
        // AMF_H264_LEVEL's values are level_idc, so the SDP's number goes straight through
        assert_eq!(AMF_H264_LEVEL__4_2 as i64, 42);
        assert_eq!(AMF_H264_LEVEL__5_1 as i64, 51);
    }

    #[test]
    fn amfinit_is_retried_lower_only_for_an_older_runtime() {
        let older = make_full_version(1, 4, 29, 0);
        let newer = make_full_version(1, 5, 2, 0);
        assert_eq!(init_version_retry(Some(older)), Some(older));
        assert_eq!(init_version_retry(Some(newer)), None);
        assert_eq!(init_version_retry(Some(AMFINIT_VERSION)), None);
        assert_eq!(init_version_retry(Some(0)), None);
        assert_eq!(init_version_retry(None), None);
    }

    #[test]
    fn the_poll_backs_off_through_spin_and_yield_before_sleeping() {
        assert_eq!(PollWait::after(1, true), PollWait::Spin);
        assert_eq!(PollWait::after(64, true), PollWait::Spin);
        assert_eq!(PollWait::after(65, true), PollWait::Yield);
        assert_eq!(PollWait::after(256, true), PollWait::Yield);
        assert_eq!(
            PollWait::after(257, true),
            PollWait::Sleep(Duration::from_micros(200))
        );
        for attempt in 1..1000 {
            assert_ne!(PollWait::after(attempt, true), PollWait::Sleep(POLL_SLEEP));
        }
        assert_eq!(PollWait::after(1, false), PollWait::Sleep(POLL_SLEEP));
    }

    #[test]
    fn query_timeout_is_judged_by_how_long_an_empty_return_took() {
        let mut ignored = QueryTimeoutProbe::default();
        for i in 0..15 {
            assert_eq!(
                ignored.record(Duration::from_micros(20)),
                None,
                "sample {i}"
            );
        }
        assert_eq!(ignored.record(Duration::from_micros(20)), Some(false));
        assert_eq!(ignored.fast(), 16);

        let mut honoured = QueryTimeoutProbe::default();
        for _ in 0..15 {
            honoured.record(Duration::from_micros(1100));
        }
        assert_eq!(honoured.record(Duration::from_micros(1050)), Some(true));

        // a few early returns (an output that became ready mid-wait) do not condemn it
        let mut mixed = QueryTimeoutProbe::default();
        let mut verdict = None;
        for i in 0..16 {
            let t = if i % 4 == 0 { 30 } else { 1000 };
            verdict = mixed.record(Duration::from_micros(t));
        }
        assert_eq!(verdict, Some(true));
    }

    #[test]
    fn the_teardown_drain_stops_on_eof_errors_and_its_deadline() {
        let soon = Duration::from_millis(1);
        assert!(drain_should_stop(AMF_EOF, soon));
        assert!(drain_should_stop(AMF_DIRECTX_FAILED, soon));
        assert!(drain_should_stop(AMF_NO_DEVICE, soon));
        assert!(drain_should_stop(AMF_FAIL, soon));
        assert!(!drain_should_stop(AMF_OK, soon));
        assert!(!drain_should_stop(AMF_REPEAT, soon));
        assert!(!drain_should_stop(AMF_NEED_MORE_INPUT, soon));
        assert!(drain_should_stop(AMF_OK, DRAIN_DEADLINE));
        const { assert!(DRAIN_DEADLINE.as_millis() < SUBMIT_DEADLINE.as_millis()) };
    }

    #[test]
    fn output_picture_types_are_judged_against_the_request() {
        assert_eq!(check_idr(false, Some(false), false), IdrCheck::Fine);
        assert_eq!(check_idr(true, Some(true), false), IdrCheck::Fine);
        assert_eq!(check_idr(false, Some(true), false), IdrCheck::Unrequested);
        assert_eq!(
            check_idr(false, Some(true), true),
            IdrCheck::Fine,
            "the first output after Init is an IDR whatever was asked"
        );
        assert_eq!(check_idr(true, Some(false), false), IdrCheck::NotHonoured);
        assert_eq!(check_idr(true, None, false), IdrCheck::Fine, "unreadable");
        assert!(log_now(1) && log_now(2) && log_now(64));
        assert!(!log_now(3) && !log_now(100));
    }

    #[test]
    fn only_a_correct_extradata_sps_stops_the_in_band_patch() {
        use super::super::sps::SpsInspection;
        assert!(!keep_patching_sps(&SpsInspection::AlreadyCorrect));
        assert!(keep_patching_sps(&SpsInspection::NoSps));
        assert!(keep_patching_sps(&SpsInspection::Unparseable));
        assert!(keep_patching_sps(&SpsInspection::Patched(vec![
            0, 0, 1, 0x67
        ])));
    }

    #[test]
    fn a_failed_submit_gives_its_requests_back() {
        let taken = TakenRequests {
            idr: true,
            mark: Some(1),
            force_reference: Some(0b01),
        };
        let (mut idr, mut mark, mut force) = (false, None, None);
        restore_requests(taken, &mut idr, &mut mark, &mut force);
        assert!(idr);
        assert_eq!(mark, Some(1));
        assert_eq!(force, Some(0b01));

        let (mut idr, mut mark, mut force) = (false, Some(0), Some(0b10));
        restore_requests(TakenRequests::default(), &mut idr, &mut mark, &mut force);
        assert!(!idr, "nothing was taken, nothing is invented");
        restore_requests(taken, &mut idr, &mut mark, &mut force);
        assert_eq!(mark, Some(0), "the newer mark wins");
        assert_eq!(force, Some(0b10), "the newer bitfield wins");
    }

    #[test]
    fn a_refused_constant_qp_is_detected_from_the_read_back() {
        let lcvbr = AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR;
        assert!(cqp_was_refused(true, lcvbr));
        assert!(cqp_was_refused(
            true,
            AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR
        ));
        assert!(!cqp_was_refused(
            true,
            AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP
        ));
        assert!(!cqp_was_refused(
            true,
            AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN
        ));
        assert!(!cqp_was_refused(false, lcvbr));
    }

    #[test]
    fn only_ultra_low_latency_init_failure_is_retried() {
        assert_eq!(
            retry_usage_after_init_failure(AMF_VIDEO_ENCODER_USAGE_ULTRA_LOW_LATENCY),
            Some(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY)
        );
        assert_eq!(
            retry_usage_after_init_failure(AMF_VIDEO_ENCODER_USAGE_LOW_LATENCY),
            None
        );
    }

    #[test]
    fn the_rate_tier_follows_the_request_and_the_read_back() {
        let cqp = AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CONSTANT_QP;
        let lcvbr = AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_LATENCY_CONSTRAINED_VBR;
        let unknown = AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_UNKNOWN;
        let cbr = AMF_VIDEO_ENCODER_RATE_CONTROL_METHOD_CBR;
        assert_eq!(select_rate_tier(false, cbr), RateTier::LinkCbr);
        assert_eq!(select_rate_tier(false, lcvbr), RateTier::LinkCbr);
        assert_eq!(select_rate_tier(true, cqp), RateTier::ConstantQp);
        assert_eq!(select_rate_tier(true, unknown), RateTier::ConstantQp);
        assert_eq!(select_rate_tier(true, lcvbr), RateTier::ConstantQpRefused);
    }

    /// An output is this picture's only when it carries this picture's identity and that has not
    /// been delivered; anything at or below the newest delivered identity is a duplicate, an
    /// older undelivered one is stale, and no identity at all is judged by position.
    #[test]
    fn outputs_are_judged_by_the_identity_they_carry() {
        assert_eq!(output_fate(Some(5), 5, Some(4)), OutputFate::Mine);
        assert_eq!(output_fate(Some(5), 5, None), OutputFate::Mine);
        assert_eq!(output_fate(Some(4), 5, Some(4)), OutputFate::Duplicate);
        assert_eq!(output_fate(Some(2), 5, Some(4)), OutputFate::Duplicate);
        assert_eq!(output_fate(Some(5), 5, Some(5)), OutputFate::Duplicate);
        assert_eq!(output_fate(Some(3), 5, Some(2)), OutputFate::Stale);
        assert_eq!(output_fate(None, 5, Some(4)), OutputFate::Unlabelled);
    }

    #[test]
    fn the_need_more_input_window_is_two_frame_periods() {
        let d = Duration::from_secs(2);
        assert_eq!(
            need_more_input_window(60, None, d),
            Duration::from_micros(33_332)
        );
        assert_eq!(
            need_more_input_window(1000, None, d),
            Duration::from_millis(4)
        );
        assert_eq!(need_more_input_window(0, None, d), d);
        assert_eq!(
            need_more_input_window(60, None, Duration::from_millis(10)),
            Duration::from_millis(10)
        );
    }

    #[test]
    fn the_need_more_input_window_follows_the_measured_encode_time() {
        let d = Duration::from_secs(2);
        assert_eq!(
            need_more_input_window(240, Some(Duration::from_millis(9)), d),
            Duration::from_millis(27)
        );
        assert_eq!(
            need_more_input_window(240, Some(Duration::from_millis(1)), d),
            Duration::from_micros(8_332),
            "never under two frame periods"
        );
        assert_eq!(
            need_more_input_window(60, Some(Duration::from_secs(5)), d),
            d,
            "never past the stall deadline"
        );
    }

    #[test]
    fn the_identity_source_is_learnt_from_the_first_output() {
        let step = 166_666;
        assert_eq!(
            learn_identity_source(1, Some(step), Some(1), step),
            IdentitySource::Property
        );
        assert_eq!(
            learn_identity_source(1, Some(step), None, step),
            IdentitySource::Pts
        );
        assert_eq!(
            learn_identity_source(1, Some(0), Some(1), step),
            IdentitySource::Property
        );
        assert_eq!(
            learn_identity_source(1, Some(0), None, step),
            IdentitySource::Unavailable
        );
        assert_eq!(
            resolve_identity(IdentitySource::Pts, Some(7 * step), Some(9), step),
            Some(9),
            "the property first, whatever the session learnt"
        );
        assert_eq!(
            resolve_identity(IdentitySource::Property, Some(7 * step), None, step),
            Some(7),
            "one output without the property falls back to its PTS"
        );
        assert_eq!(
            resolve_identity(IdentitySource::Property, Some(0), None, step),
            None
        );
        assert_eq!(
            resolve_identity(IdentitySource::Pts, Some(0), None, step),
            None
        );
        assert_eq!(
            resolve_identity(IdentitySource::Property, Some(0), Some(9), step),
            Some(9)
        );
        assert_eq!(
            resolve_identity(IdentitySource::Unavailable, Some(7 * step), Some(7), step),
            None
        );
    }

    #[test]
    fn the_need_more_input_meaning_is_revisable_within_a_bound() {
        use NeedMoreInputMeaning::*;
        assert_eq!(revise_need_more_input(Unknown, NotTaken, 0), (NotTaken, 0));
        assert_eq!(revise_need_more_input(NotTaken, Taken, 0), (Taken, 1));
        assert_eq!(revise_need_more_input(Taken, NotTaken, 1), (NotTaken, 2));
        assert_eq!(revise_need_more_input(NotTaken, Taken, 2), (Taken, 2));
        assert_eq!(revise_need_more_input(Taken, NotTaken, 2), (Taken, 2));
        assert_eq!(revise_need_more_input(Taken, Taken, 0), (Taken, 0));
        assert_eq!(revise_need_more_input(Taken, Unknown, 0), (Taken, 0));
        let mut state = (Unknown, 0);
        for i in 0..20 {
            let evidence = if i % 2 == 0 { NotTaken } else { Taken };
            state = revise_need_more_input(state.0, evidence, state.1);
        }
        assert_eq!(
            state,
            (Taken, NEED_MORE_INPUT_FLIPS),
            "settles, does not oscillate"
        );
    }

    #[test]
    fn need_more_input_is_probed_once_and_never_without_an_identity() {
        use IdentitySource as S;
        use NeedMoreInputAction as A;
        use NeedMoreInputMeaning as M;
        assert_eq!(
            need_more_input_action(S::Unavailable, M::Unknown, 0),
            A::Accept
        );
        assert_eq!(
            need_more_input_action(S::Unknown, M::NotTaken, 0),
            A::Accept
        );
        assert_eq!(need_more_input_action(S::Pts, M::Unknown, 0), A::Probe);
        assert_eq!(need_more_input_action(S::Pts, M::Unknown, 1), A::Accept);
        assert_eq!(need_more_input_action(S::Property, M::Taken, 0), A::Accept);
        assert_eq!(
            need_more_input_action(S::Pts, M::NotTaken, 0),
            A::ResubmitNow
        );
        assert_eq!(
            need_more_input_action(S::Pts, M::NotTaken, NEED_MORE_INPUT_RESUBMITS),
            A::Accept
        );
    }

    #[test]
    fn bitrate_pairs_are_written_so_target_never_passes_peak() {
        assert_eq!(
            rate_write_order(Some(5), 10),
            ["PeakBitrate", "TargetBitrate"]
        );
        assert_eq!(
            rate_write_order(Some(10), 5),
            ["TargetBitrate", "PeakBitrate"]
        );
        assert_eq!(rate_write_order(None, 5), ["TargetBitrate", "PeakBitrate"]);
    }

    #[test]
    fn bitrates_are_clamped_to_the_documented_range_and_the_adapter_cap() {
        assert_eq!(clamp_bitrate(400_000_000, 0), 100_000_000);
        assert_eq!(clamp_bitrate(400_000_000, 50_000_000), 50_000_000);
        assert_eq!(clamp_bitrate(400_000_000, 900_000_000), 100_000_000);
        assert_eq!(clamp_bitrate(1, 0), 10_000);
        assert_eq!(clamp_bitrate(6_000_000, 50_000_000), 6_000_000);
        assert_eq!(clamp_bitrate(6_000_000, 5_000), 10_000, "a nonsense cap");
    }

    #[test]
    fn long_term_references_need_room_in_the_sps() {
        assert!(ltr_fits_sps(1, 0));
        assert!(!ltr_fits_sps(1, 1));
        assert!(!ltr_fits_sps(2, 2));
        assert!(ltr_fits_sps(3, 2));
        assert!(ltr_fits_sps(4, 2));
        assert!(ltr_fits_sps(1, -1), "a negative request is no request");
    }

    /// With no AMD adapter at all the error is in the words the compatibility card shows.
    #[test]
    fn the_adapter_walk_takes_the_first_that_works_and_names_every_refusal() {
        let tried = std::cell::RefCell::new(Vec::new());
        let pick = first_that_works(
            vec![
                ("navi24".to_string(), 1),
                ("apu".to_string(), 2),
                ("x".to_string(), 3),
            ],
            |c| {
                tried.borrow_mut().push(c);
                if c == 1 {
                    bail!("CreateComponent: AMF_ENCODER_NOT_PRESENT")
                } else {
                    Ok(c * 10)
                }
            },
        )
        .expect("the second adapter works");
        assert_eq!(pick, ("apu".to_string(), 20));
        assert_eq!(
            *tried.borrow(),
            vec![1, 2],
            "it stops at the first that works"
        );

        let err = first_that_works(
            vec![("a".to_string(), ()), ("b".to_string(), ())],
            |()| -> Result<()> { bail!("no") },
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("a: no") && msg.contains("b: no"), "{msg}");

        let none = first_that_works(Vec::<(String, ())>::new(), |()| Ok(())).unwrap_err();
        assert!(format!("{none:#}").contains("no AMD adapter found"));
    }

    /// The values of a C enum in a vendored AMF header, with C's implicit numbering.
    fn header_enum(header: &str, name: &str) -> Vec<(String, i64)> {
        let start = header
            .find(&format!("enum {name}"))
            .unwrap_or_else(|| panic!("enum {name} not found in the header"));
        let body = &header[start..];
        let open = body.find('{').expect("enum body");
        let close = body.find('}').expect("enum end");
        let mut next = 0i64;
        let mut out = Vec::new();
        for item in body[open + 1..close].split(',') {
            let item = item
                .lines()
                .map(|l| l.split("//").next().unwrap_or("").trim())
                .collect::<Vec<_>>()
                .join(" ");
            let item = item.trim();
            if item.is_empty() {
                continue;
            }
            let (ident, value) = match item.split_once('=') {
                Some((i, v)) => (i.trim(), v.trim().parse::<i64>().expect("integer value")),
                None => (item, next),
            };
            out.push((ident.to_string(), value));
            next = value + 1;
        }
        out
    }

    /// The LTR mode values written under `amf_ltr_keep_unused`, read out of
    /// `AMF_VIDEO_ENCODER_LTR_MODE_ENUM` in the vendored `VideoEncoderVCE.h` itself, so a wrong
    /// constant in `amd_sys/types.rs` fails here.
    #[test]
    fn ltr_mode_values_match_the_header() {
        let header = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/resources/amf-headers/components/VideoEncoderVCE.h"
        ));
        let values = header_enum(header, "AMF_VIDEO_ENCODER_LTR_MODE_ENUM");
        let lookup = |name: &str| {
            values
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| *v)
                .unwrap_or_else(|| panic!("{name} not in the header's enum"))
        };
        assert_eq!(
            AMF_VIDEO_ENCODER_LTR_MODE_RESET_UNUSED as i64,
            lookup("AMF_VIDEO_ENCODER_LTR_MODE_RESET_UNUSED")
        );
        assert_eq!(
            AMF_VIDEO_ENCODER_LTR_MODE_KEEP_UNUSED as i64,
            lookup("AMF_VIDEO_ENCODER_LTR_MODE_KEEP_UNUSED")
        );
    }
}
