use std::ffi::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context as _, Result};
use rayon::prelude::*;

use super::workers;
use super::x264_sys::*;
use crate::streamer::config::{Config, H264Profile};
use crate::streamer::platform::BitrateApplied;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::tune;

const PRESET_ULTRAFAST: &[u8] = b"ultrafast\0";
const TUNE_ZEROLATENCY: &[u8] = b"zerolatency\0";
const PROFILE_BASELINE: &[u8] = b"baseline\0";
const PROFILE_MAIN: &[u8] = b"main\0";
const PROFILE_HIGH: &[u8] = b"high\0";
const MAX_ENCODE_THREADS: usize = 8;
/// With `x264_slices_by_height`, the fewest macroblock rows a slice may be cut to.
const MIN_MB_ROWS_PER_SLICE: usize = 8;

/// The behaviour switches this encoder reads, asked once when it is built.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct X264Options {
    /// `x264_worker_tuning` (candidate, off): find libx264's slice workers after
    /// `x264_encoder_open` and give them HIGHEST priority, the EcoQoS opt-out and the creating
    /// thread's P-core CPU sets, and hold the slice count to those P-cores. See `workers.rs`.
    pub worker_tuning: bool,
    /// `x264_slices_by_height` (candidate, off): no fewer than `MIN_MB_ROWS_PER_SLICE`
    /// macroblock rows per slice, so a small frame is not cut into eight slivers.
    pub slices_by_height: bool,
    /// `x264_rate_by_delivered_fps` (candidate, off): hold the per-frame budget to the frame
    /// rate actually being delivered when it runs above the nominal one. See [`RatePacer`].
    pub rate_by_delivered_fps: bool,
    /// `x264_negotiated_profile` (on): turn on the coding tools of the negotiated profile that
    /// `ultrafast` turned off. See [`enable_profile_tools`].
    pub negotiated_profile: bool,
    /// A QP governor may move the constant QP: open CQP so that `set_qp` can reach 51.
    pub wide_qp_clamp: bool,
}

impl X264Options {
    pub(crate) fn from_switches() -> Self {
        Self {
            worker_tuning: tune::candidate("x264_worker_tuning"),
            slices_by_height: tune::candidate("x264_slices_by_height"),
            rate_by_delivered_fps: tune::candidate("x264_rate_by_delivered_fps"),
            negotiated_profile: tune::on("x264_negotiated_profile"),
            wide_qp_clamp: tune::candidate("link_governor")
                || tune::candidate("qp_ceiling_governor"),
        }
    }
}

/// `ultrafast` turns CABAC and the 8x8 transform off and `param_apply_profile` only ever takes
/// tools away, so without this a Main or High request comes out as constrained Baseline with
/// CAVLC. Main gets CABAC, High also the 8x8 transform.
pub(crate) fn enable_profile_tools(p: &mut x264_param_t, profile: H264Profile) {
    match profile {
        H264Profile::Baseline => {}
        H264Profile::Main => p.b_cabac = 1,
        H264Profile::High => {
            p.b_cabac = 1;
            p.analyse.b_transform_8x8 = 1;
        }
    }
}

/// The QP an encoder opened with `wide_qp_clamp` forces on every picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WideQp {
    /// The user's setting.
    pub base: u8,
    /// The keyframe QP libx264 gives that setting by default (`base - 6 log2 1.4`, rounded).
    pub key: u8,
    /// The highest QP reachable: 39 above `key`, at most 51.
    pub top: u8,
    /// The QP in force.
    pub now: u8,
}

impl WideQp {
    fn new(base: u8) -> Self {
        // libx264's own rounding of the default I-frame offset, `(int)(qp - 6 log2 ip + 0.5)`
        let key = (base as f32 - 6.0 * 1.4f32.log2() + 0.5).max(0.0) as u8;
        WideQp {
            base,
            key,
            top: (key + 39).min(51),
            now: base,
        }
    }
}

/// Which retained buffer `encode_repeat` re-encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Retained {
    /// Nothing usable: no frame yet, or the last frame came through
    /// [`X264Encoder::encode_nv12_unretained`] and was never copied.
    None,
    I420,
    Nv12,
}

/// One picture's planes as libx264 reads them. libx264 copies the picture inside
/// `x264_encoder_encode` (`x264_frame_copy_picture`) and never writes through these pointers,
/// so they may point into a caller's read-only buffer for the duration of that one call.
#[derive(Clone, Copy)]
struct Planes {
    csp: c_int,
    count: c_int,
    ptr: [*mut u8; 3],
    stride: [c_int; 3],
}

/// Validate a padded NV12 buffer for a `w`x`h` picture and return its planes. `caller` names
/// the public entry point in the error.
fn nv12_planes(caller: &str, data: &[u8], row_pitch: usize, w: usize, h: usize) -> Result<Planes> {
    let rows = h + h / 2;
    if row_pitch < w || data.len() < row_pitch * rows {
        bail!(
            "{caller}: need row_pitch >= {w} and >= {} bytes for {w}x{h} NV12 \
             (got pitch {row_pitch}, {} bytes)",
            row_pitch * rows,
            data.len()
        );
    }
    let base = data.as_ptr() as *mut u8;
    Ok(Planes {
        csp: X264_CSP_NV12,
        count: 2,
        // in bounds: `row_pitch * h <= data.len()` was just checked
        ptr: [base, unsafe { base.add(row_pitch * h) }, ptr::null_mut()],
        stride: [row_pitch as c_int, row_pitch as c_int, 0],
    })
}

/// Holds the per-frame budget to the frame rate actually delivered (`x264_rate_by_delivered_fps`).
///
/// libx264's rate control charges every picture `1 / fps` of the bitrate, so a 60 fps session
/// delivering ~113 fps through a pointer sweep (cursor frames skip the capture cadence gate)
/// spends up to 1.9x its target. `b_vfr_input = 1` with real-time pts would charge each picture
/// the interval it covers, but it holds every picture until the next one arrives: one frame of
/// latency (`x264_vfr_input_holds_a_frame_back`). So the rate is corrected from outside: when
/// the delivered rate runs above nominal, the pacer asks libx264 for
/// `target * nominal / delivered` through `x264_encoder_reconfig`.
///
/// It never raises the per-frame budget above nominal: spending an idle stretch would turn the
/// first frame after it into a burst.
#[derive(Debug, Clone)]
pub(crate) struct RatePacer {
    nominal_fps: f64,
    /// Exponentially weighted mean frame interval, seconds.
    mean_interval: Option<f64>,
    last_at: Option<std::time::Instant>,
    /// The scale (0, 1] currently applied, and when it was last changed.
    applied: f64,
    applied_at: Option<std::time::Instant>,
}

impl RatePacer {
    /// Weight of the newest interval in the mean: about a 16-frame memory.
    const ALPHA: f64 = 1.0 / 16.0;
    /// Do not reconfigure for a change smaller than this fraction of the current scale.
    const HYSTERESIS: f64 = 0.10;
    /// Nor more often than this.
    const MIN_GAP: std::time::Duration = std::time::Duration::from_millis(250);
    /// Never ask for less than this share of the target.
    const FLOOR: f64 = 0.25;
    /// An interval longer than this is an idle gap, not a rate, and resets the mean.
    const IDLE_GAP: f64 = 0.25;

    pub(crate) fn new(nominal_fps: u32) -> Self {
        Self {
            nominal_fps: nominal_fps.max(1) as f64,
            mean_interval: None,
            last_at: None,
            applied: 1.0,
            applied_at: None,
        }
    }

    /// The bitrate scale the measured rate asks for: `nominal / delivered`, clamped to
    /// `[FLOOR, 1]`.
    pub(crate) fn wanted_scale(&self) -> f64 {
        match self.mean_interval {
            Some(m) if m > 0.0 => (m * self.nominal_fps).clamp(Self::FLOOR, 1.0),
            _ => 1.0,
        }
    }

    /// Note a frame submitted at `now`. Returns the new scale when the encoder should be
    /// reconfigured, `None` when the current one stands.
    pub(crate) fn note_frame(&mut self, now: std::time::Instant) -> Option<f64> {
        if let Some(last) = self.last_at {
            let dt = now.saturating_duration_since(last).as_secs_f64();
            if dt > Self::IDLE_GAP {
                self.mean_interval = None;
            } else {
                self.mean_interval = Some(match self.mean_interval {
                    Some(m) => m + Self::ALPHA * (dt - m),
                    None => dt,
                });
            }
        }
        self.last_at = Some(now);

        let wanted = self.wanted_scale();
        if (wanted - self.applied).abs() < Self::HYSTERESIS * self.applied {
            return None;
        }
        if let Some(at) = self.applied_at {
            if now.saturating_duration_since(at) < Self::MIN_GAP {
                return None;
            }
        }
        self.applied = wanted;
        self.applied_at = Some(now);
        Some(wanted)
    }

    pub(crate) fn applied(&self) -> f64 {
        self.applied
    }
}

/// Counted instead of asserted: the encode runs under a WGC callback where an unwind would
/// cross `extern "system"` frames and abort the host.
static REORDER_WARNED: AtomicBool = AtomicBool::new(false);

pub struct X264Encoder {
    api: &'static X264Api,
    handle: *mut x264_t,
    params: x264_param_t,
    config: EncoderConfig,
    width: usize,
    height: usize,
    i420: Vec<u8>,
    /// The last NV12 frame, tightly packed at `width` pitch: `height` rows of luma followed by
    /// `height / 2` rows of interleaved chroma. Empty until an NV12 frame arrives, so a host
    /// whose scaler stays on BGRA never allocates it.
    nv12: Vec<u8>,
    retained: Retained,
    y_size: usize,
    c_size: usize,
    /// Pictures submitted so far (also the next pts); 0 means nothing to repeat yet.
    frames: u64,
    /// Access units whose dts and pts differed. Cannot happen with 0 B-frames; counted so a
    /// libx264 that ever did it is visible in a test rather than aborting a debug host.
    reordered: u64,
    pacer: Option<RatePacer>,
    /// The bitrate the caller asked for, before any `RatePacer` scale.
    target_kbps: c_int,
    /// `x264_picture_t::i_qpplus1` for every picture: 0 leaves the QP to rate control.
    qp_plus1: c_int,
    /// The CQP range was widened for `set_qp`, so every picture carries its QP explicitly.
    wide_qp: Option<WideQp>,
    #[cfg(test)]
    tuned_workers: Vec<u32>,
    /// A test-driven clock for the pacer, so a delivered rate can be simulated exactly.
    #[cfg(test)]
    test_now: Option<std::time::Instant>,
}

unsafe impl Send for X264Encoder {}

/// Serialises `x264_encoder_open` while worker tuning is on, so two first sessions opening at
/// once do not each see the other's workers appear.
static OPEN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl X264Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        Self::with_options(config, X264Options::from_switches())
    }

    pub(crate) fn with_options(config: EncoderConfig, options: X264Options) -> Result<Self> {
        let api = X264Api::shared().context("loading libx264 for the software H.264 fallback")?;

        let width = config.width as usize;
        let height = config.height as usize;
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            bail!("x264: invalid dimensions {width}x{height} (need even, non-zero)");
        }

        let fps = config.fps.max(1);
        let mut p: x264_param_t = unsafe { std::mem::zeroed() };
        let rv = unsafe {
            (api.param_default_preset)(
                &mut p,
                PRESET_ULTRAFAST.as_ptr() as *const c_char,
                TUNE_ZEROLATENCY.as_ptr() as *const c_char,
            )
        };
        if rv != 0 {
            bail!("x264_param_default_preset(ultrafast, zerolatency) failed ({rv})");
        }

        // geometry / input
        p.i_csp = X264_CSP_I420;
        p.i_width = width as c_int;
        p.i_height = height as c_int;
        // not `b_vfr_input`: that holds every picture back until the next one arrives (see
        // `RatePacer`)
        p.b_vfr_input = 0;
        p.i_fps_num = fps;
        p.i_fps_den = 1;
        p.i_timebase_num = 1;
        p.i_timebase_den = fps;

        // the building thread's own P-core masks (empty when it has none): the live paths build
        // the encoder on a thread `tune_current_thread` already tuned
        let worker_masks = if options.worker_tuning {
            workers::current_thread_cpu_set_masks()
        } else {
            Vec::new()
        };

        // latency-critical
        p.i_bframe = 0;
        p.b_sliced_threads = 1;
        p.i_threads = workers::slice_thread_count(
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4),
            MAX_ENCODE_THREADS,
            (!worker_masks.is_empty()).then(|| workers::logical_count(&worker_masks)),
            height,
            options.slices_by_height.then_some(MIN_MB_ROWS_PER_SLICE),
        ) as c_int;
        p.rc.i_lookahead = 0;
        p.i_sync_lookahead = 0;

        // rate control
        let kbps = bitrate_kbps(config.bitrate_bps);
        if let Some(q) = config.qp {
            // explicit constant QP (the --qp probe flag): bitrate is not a control input
            p.rc.i_rc_method = X264_RC_CQP;
            p.rc.i_qp_constant = q as c_int;
            // CQP clamps a forced per-picture QP to [I constant, B constant], and libx264 holds
            // each factor to 10 (19.9 QP) and rounds the top of the clamp up. Anchored at this
            // setting's default keyframe QP, the range runs 39 up: the setting and its keyframe
            // stay reachable. Each picture is forced (`WideQp`), so the centre is never coded.
            if let Some(w) = options.wide_qp_clamp.then(|| WideQp::new(q)) {
                let mid = (w.key + w.top) / 2;
                p.rc.i_qp_constant = mid as c_int;
                p.rc.f_ip_factor = 2f32.powf((mid - w.key) as f32 / 6.0);
                p.rc.f_pb_factor = 2f32.powf((w.top - mid) as f32 / 6.0);
            }
        } else {
            p.rc.i_rc_method = X264_RC_ABR;
            p.rc.i_bitrate = kbps;
            p.rc.i_vbv_max_bitrate = kbps;
            p.rc.i_vbv_buffer_size = vbv_buffer_kbit(kbps, fps);
            // Quality ceiling: never spend bits to go below this QP. `qp_min` only shrinks
            // the stream, so ABR stays in charge of the top end.
            if let Some(q) = config.min_qp.filter(|&q| q > 1) {
                p.rc.i_qp_min = q as c_int;
            }
        }

        // GOP / recovery. libx264 advances the intra-refresh column by `mb_width / i_keyint_max`
        // macroblocks per frame, so keyint is the refresh period: left infinite, the wave never
        // sweeps. One sweep per second matches the Intel backend's `IntRefCycleSize = fps`.
        p.i_keyint_max = if config.intra_refresh {
            fps.clamp(2, i32::MAX as u32) as c_int
        } else {
            X264_KEYINT_MAX_INFINITE
        };
        p.i_scenecut_threshold = 0;
        p.b_intra_refresh = if config.intra_refresh { 1 } else { 0 };

        // bitstream shape for WebRTC
        p.b_repeat_headers = 1;
        p.b_annexb = 1;
        p.i_log_level = X264_LOG_ERROR;

        // Colour signalling: BT.709 limited range, for both inputs. The CPU BGRA converter below
        // is BT.709, and the scaler's NV12 is BT.709 limited only while `scaler_bt709` is on (the
        // D3D11 video processor's documented default matrix is BT.601).
        p.vui.b_fullrange = 0;
        p.vui.i_colorprim = 1; // BT.709
        p.vui.i_transfer = 1; // BT.709
        p.vui.i_colmatrix = 1; // BT.709

        if options.negotiated_profile {
            enable_profile_tools(&mut p, config.profile);
        }
        let profile = match config.profile {
            H264Profile::Baseline => PROFILE_BASELINE,
            H264Profile::Main => PROFILE_MAIN,
            H264Profile::High => PROFILE_HIGH,
        };
        let rv = unsafe { (api.param_apply_profile)(&mut p, profile.as_ptr() as *const c_char) };
        if rv != 0 {
            bail!(
                "x264_param_apply_profile({:?}) failed ({rv})",
                config.profile
            );
        }

        // Input must be planar 4:2:0: libx264 does not convert RGB. Any csp at or above
        // X264_CSP_I444 maps to I444 in frame_internal_csp(), and x264_sps_init then forces
        // PROFILE_HIGH444_PREDICTIVE (profile_idc 244, chroma_format_idc 3) whatever
        // apply_profile asked for, which no browser WebRTC decoder accepts.
        // `x264_profile_asserts_420` covers this.
        #[cfg(feature = "bench")]
        crate::bench::win_hooks::x264(&mut p);
        let (handle, worker_report) = if options.worker_tuning {
            let _serial = OPEN_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let before = workers::process_thread_ids();
            let handle = unsafe { (api.encoder_open)(&mut p) };
            let report = match (before, workers::process_thread_ids()) {
                (Some(before), Some(after)) if !handle.is_null() => {
                    workers::module_of(api.encoder_open as *const std::ffi::c_void).map(|lib| {
                        let found = workers::select_new_workers(
                            &before,
                            &after,
                            lib,
                            workers::thread_start_module,
                        );
                        workers::tune_workers(&found, &worker_masks)
                    })
                }
                _ => None,
            };
            (handle, report)
        } else {
            (unsafe { (api.encoder_open)(&mut p) }, None)
        };
        if handle.is_null() {
            bail!("x264_encoder_open returned null (invalid params or libx264 build mismatch)");
        }

        let pacer =
            (options.rate_by_delivered_fps && config.qp.is_none()).then(|| RatePacer::new(fps));

        let y_size = width * height;
        let c_size = (width / 2) * (height / 2);
        let i420 = vec![0u8; y_size + 2 * c_size];

        // Log what libx264 settled on, not the request: the preset, the tune and
        // `param_apply_profile` all move fields. Each setting listed is a way for the encoder to
        // buffer quietly: a B frame, a lookahead, frame threads, a refresh period that never
        // sweeps.
        let eff = read_back_params(api, handle);
        let workers_note = match (&worker_report, options.worker_tuning) {
            (Some(r), _) => format!(
                "tuned {} slice workers (HIGHEST, EcoQoS off, {} P-core logical CPUs), {} refused",
                r.tuned.len(),
                r.masks,
                r.failed
            ),
            (None, true) => "worker tuning on but the workers could not be identified".into(),
            (None, false) => "workers untuned".into(),
        };
        tprintln!(
            "pipeline: software x264 encoder ready ({width}x{height}@{fps}, {kbps} kbps, \
             profile={:?}, lib={}, build={}). Effective: threads={}, lookahead_threads={}, \
             sliced_threads={}, bframes={}, rc_lookahead={}, sync_lookahead={}, keyint={}, \
             intra_refresh={}, scenecut={}, repeat_headers={}, annexb={}, nal_hrd={}, \
             vbv_maxrate={}, vbv_bufsize={}, colmatrix={}, fullrange={}; {workers_note}; \
             options={options:?}",
            config.profile,
            api.library_path().display(),
            api.build,
            eff.i_threads,
            eff.i_lookahead_threads,
            eff.b_sliced_threads,
            eff.i_bframe,
            eff.rc.i_lookahead,
            eff.i_sync_lookahead,
            eff.i_keyint_max,
            eff.b_intra_refresh,
            eff.i_scenecut_threshold,
            eff.b_repeat_headers,
            eff.b_annexb,
            eff.i_nal_hrd,
            eff.rc.i_vbv_max_bitrate,
            eff.rc.i_vbv_buffer_size,
            eff.vui.i_colmatrix,
            eff.vui.b_fullrange,
        );

        let wide_qp = config.qp.filter(|_| options.wide_qp_clamp).map(WideQp::new);
        Ok(Self {
            api,
            handle,
            params: p,
            config,
            width,
            height,
            i420,
            nv12: Vec::new(),
            retained: Retained::None,
            y_size,
            c_size,
            frames: 0,
            reordered: 0,
            pacer,
            target_kbps: kbps,
            qp_plus1: 0,
            wide_qp,
            #[cfg(test)]
            tuned_workers: worker_report.map(|r| r.tuned).unwrap_or_default(),
            #[cfg(test)]
            test_now: None,
        })
    }

    /// The libx264 build number the loader accepted, for the init log and the tests.
    pub fn build(&self) -> i32 {
        self.api.build
    }

    /// The parameters libx264 settled on, after the preset, the tune, `param_apply_profile` and
    /// its own validation. `self.params` is only the request;
    /// `x264_zerolatency_settings_were_accepted` asserts against this.
    pub fn effective_params(&self) -> x264_param_t {
        read_back_params(self.api, self.handle)
    }

    /// The slice workers `x264_worker_tuning` found and tuned.
    #[cfg(test)]
    pub(crate) fn tuned_workers(&self) -> &[u32] {
        &self.tuned_workers
    }

    /// Access units whose dts differed from their pts (none are expected).
    #[cfg(test)]
    pub(crate) fn reordered(&self) -> u64 {
        self.reordered
    }

    /// Drive the pacer's clock from the test: the next encode is taken to happen at `now`.
    #[cfg(test)]
    pub(crate) fn set_test_clock(&mut self, now: std::time::Instant) {
        self.test_now = Some(now);
    }

    /// The bitrate scale the delivered-rate pacer has applied (1.0 when it is off).
    #[cfg(test)]
    pub(crate) fn rate_scale(&self) -> f64 {
        self.pacer.as_ref().map_or(1.0, RatePacer::applied)
    }

    pub fn encode_bgra(&mut self, bgra: &[u8], force_idr: bool) -> Result<Vec<u8>> {
        self.encode_bgra_padded(bgra, (self.width * 4) as u32, force_idr)
    }

    /// Encode a BGRA frame: the CPU converts it to I420 (BT.709 limited) into a buffer the
    /// encoder keeps, which is also what `encode_repeat` re-encodes. Live whenever the scaler
    /// falls back to BGRA (NV12 refused, or an odd size) and on every no-scaler readback.
    pub fn encode_bgra_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        self.convert_bgra_to_i420(data, row_pitch as usize)?;
        let planes = self.i420_planes();
        let au = self.encode_planes(planes, force_idr)?;
        // only a frame libx264 accepted becomes the repeat source
        self.retained = Retained::I420;
        Ok(au)
    }

    /// Encode a frame the GPU's video processor already converted to NV12.
    ///
    /// The scale and the colour conversion are one `VideoProcessorBlt`, so NV12 output removes
    /// the CPU colour-matrix pass. What is left here is a row copy into a tightly packed buffer
    /// the encoder keeps for `encode_repeat` (the caller's buffer does not outlive this call);
    /// libx264 does the NV12 to I420 deinterleave itself inside `x264_frame_copy_picture`.
    /// [`Self::encode_nv12_unretained`] is the same encode without that copy, for a caller that
    /// keeps the frame itself.
    ///
    /// The picture csp differs from `param.i_csp` here, which is allowed: `frame_internal_csp`
    /// maps NV12 to I420, the internal format the parameters opened with, so the SPS is
    /// unaffected. A packed BGRA picture would map to I444 and force High 4:4:4
    /// (`x264_profile_asserts_420`).
    pub fn encode_nv12_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let row_pitch = row_pitch as usize;
        let (w, h) = (self.width, self.height);
        // validates the geometry before anything is copied
        nv12_planes("encode_nv12_padded", data, row_pitch, w, h)?;
        let rows = h + h / 2;

        if self.nv12.len() != w * rows {
            self.nv12 = vec![0u8; w * rows];
        }
        if row_pitch == w {
            self.nv12.copy_from_slice(&data[..w * rows]);
        } else {
            for (dst, src) in self
                .nv12
                .chunks_exact_mut(w)
                .zip(data.chunks_exact(row_pitch))
            {
                dst.copy_from_slice(&src[..w]);
            }
        }

        let planes = self.nv12_retained_planes();
        let au = self.encode_planes(planes, force_idr)?;
        self.retained = Retained::Nv12;
        Ok(au)
    }

    /// Encode an NV12 frame straight from the caller's buffer, at the caller's pitch, with no
    /// copy. libx264 copies the picture into its own frame inside the call either way.
    ///
    /// Nothing is retained, so a later plain [`Self::encode_repeat`] returns an error rather than
    /// repeat a stale picture. A caller that uses this must also supply its repeats through this
    /// method, from the buffer it kept (in the pipeline, the scaler's own `readback`, which
    /// outlives the map).
    pub fn encode_nv12_unretained(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let planes = nv12_planes(
            "encode_nv12_unretained",
            data,
            row_pitch as usize,
            self.width,
            self.height,
        )?;
        self.retained = Retained::None;
        self.encode_planes(planes, force_idr)
    }

    /// Re-encode the frame the encoder retained: the I420 planes of a BGRA frame, or the packed
    /// copy of an NV12 one. Empty (not an error) before the first frame. No source stride is
    /// retained, so a repeat can never disagree with the frame it repeats.
    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let planes = match self.retained {
            Retained::I420 => self.i420_planes(),
            Retained::Nv12 => self.nv12_retained_planes(),
            Retained::None if self.frames == 0 => return Ok(Vec::new()),
            Retained::None => bail!(
                "x264 repeat: the last frame was encoded unretained, so the caller must supply \
                 the repeat through encode_nv12_unretained"
            ),
        };
        self.encode_planes(planes, force_idr)
    }

    /// Change the constant QP from the next picture: a forced per-picture quantiser
    /// (`i_qpplus1`), which libx264 honours ahead of its CQP table and which neither resets the
    /// encoder nor forces a keyframe. On an encoder opened with `wide_qp_clamp` it reaches
    /// [`WideQp::top`] (51 for a setting of 15 or more); otherwise libx264 clamps it to its own
    /// CQP range. `Ok(false)` outside CQP.
    pub fn set_qp(&mut self, qp: u8) -> Result<bool> {
        let Some(base) = self.config.qp else {
            return Ok(false);
        };
        let q = qp.clamp(1, 51);
        if let Some(w) = self.wide_qp.as_mut() {
            w.now = q.min(w.top);
            return Ok(true);
        }
        self.qp_plus1 = if q == base { 0 } else { q as c_int + 1 };
        Ok(true)
    }

    /// The QP range `set_qp` can reach on this encoder: `(keyframe QP at the user's setting,
    /// highest)`, or `None` when it was not opened widened.
    pub fn qp_range(&self) -> Option<(u8, u8)> {
        self.wide_qp.map(|w| (w.key, w.top))
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;
        self.target_kbps = bitrate_kbps(bps);
        let scale = self.pacer.as_ref().map_or(1.0, RatePacer::applied);
        self.reconfigure_rate(scale)?;
        Ok(BitrateApplied::Applied)
    }

    /// Push `target_kbps * scale` into libx264's ABR and VBV.
    fn reconfigure_rate(&mut self, scale: f64) -> Result<()> {
        let kbps = scaled_kbps(self.target_kbps, scale);
        self.params.rc.i_bitrate = kbps;
        self.params.rc.i_vbv_max_bitrate = kbps;
        self.params.rc.i_vbv_buffer_size = vbv_buffer_kbit(kbps, self.config.fps.max(1));
        let rv = unsafe { (self.api.encoder_reconfig)(self.handle, &mut self.params) };
        if rv < 0 {
            bail!("x264_encoder_reconfig failed ({rv})");
        }
        Ok(())
    }

    fn now(&self) -> std::time::Instant {
        #[cfg(test)]
        if let Some(now) = self.test_now {
            return now;
        }

        std::time::Instant::now()
    }

    fn i420_planes(&mut self) -> Planes {
        let (y, rest) = self.i420.split_at_mut(self.y_size);
        let (u, v) = rest.split_at_mut(self.c_size);
        Planes {
            csp: X264_CSP_I420,
            count: 3,
            ptr: [y.as_mut_ptr(), u.as_mut_ptr(), v.as_mut_ptr()],
            stride: [
                self.width as c_int,
                (self.width / 2) as c_int,
                (self.width / 2) as c_int,
            ],
        }
    }

    fn nv12_retained_planes(&mut self) -> Planes {
        let (y, uv) = self.nv12.split_at_mut(self.y_size);
        Planes {
            csp: X264_CSP_NV12,
            count: 2,
            ptr: [y.as_mut_ptr(), uv.as_mut_ptr(), ptr::null_mut()],
            stride: [self.width as c_int, self.width as c_int, 0],
        }
    }

    fn encode_planes(&mut self, planes: Planes, force_idr: bool) -> Result<Vec<u8>> {
        if self.pacer.is_some() {
            let now = self.now();
            if let Some(scale) = self.pacer.as_mut().and_then(|p| p.note_frame(now)) {
                // a refused reconfigure leaves the previous rate in force; not a reason to drop
                // the frame
                if let Err(e) = self.reconfigure_rate(scale) {
                    teprintln!("x264: delivered-rate reconfigure to {scale:.2}x failed: {e:?}");
                }
            }
        }

        let mut pic_in: x264_picture_t = unsafe { std::mem::zeroed() };
        unsafe { (self.api.picture_init)(&mut pic_in) };
        pic_in.img.i_csp = planes.csp;
        pic_in.img.i_plane = planes.count;
        for i in 0..3 {
            pic_in.img.plane[i] = planes.ptr[i];
            pic_in.img.i_stride[i] = planes.stride[i];
        }
        pic_in.i_pts = self.frames as i64;
        pic_in.i_type = if force_idr {
            X264_TYPE_IDR
        } else {
            X264_TYPE_AUTO
        };
        pic_in.i_qpplus1 = match self.wide_qp {
            // at the user's QP a keyframe keeps the finer QP libx264 would have given it
            Some(w) => {
                let keyframe = force_idr || self.frames == 0;
                let q = if keyframe && w.now == w.base {
                    w.key
                } else {
                    w.now
                };
                q as c_int + 1
            }
            None => self.qp_plus1,
        };

        let mut nals: *mut x264_nal_t = ptr::null_mut();
        let mut n_nal: c_int = 0;
        let mut pic_out: x264_picture_t = unsafe { std::mem::zeroed() };
        let size = unsafe {
            (self.api.encoder_encode)(
                self.handle,
                &mut nals,
                &mut n_nal,
                &mut pic_in,
                &mut pic_out,
            )
        };
        if size < 0 {
            bail!("x264_encoder_encode failed ({size})");
        }
        self.frames += 1;

        if size == 0 || n_nal == 0 || nals.is_null() {
            return Ok(Vec::new());
        }
        if pic_out.i_dts != pic_out.i_pts {
            self.reordered += 1;
            if !REORDER_WARNED.swap(true, Ordering::Relaxed) {
                teprintln!(
                    "x264: an access unit came out with dts {} != pts {} although B-frames are \
                     off; counted, not asserted (this runs under a WGC callback)",
                    pic_out.i_dts,
                    pic_out.i_pts
                );
            }
        }

        let out = unsafe { std::slice::from_raw_parts((*nals).p_payload, size as usize).to_vec() };
        Ok(out)
    }

    fn convert_bgra_to_i420(&mut self, bgra: &[u8], row_pitch: usize) -> Result<()> {
        let w = self.width;
        let h = self.height;
        let min_stride = w * 4;
        if row_pitch < min_stride || bgra.len() < row_pitch * h {
            bail!(
                "x264 convert: need row_pitch >= {min_stride} and >= {} bytes (got pitch {row_pitch}, {} bytes)",
                row_pitch * h,
                bgra.len()
            );
        }
        let cw = w / 2;
        let (y_plane, rest) = self.i420.split_at_mut(self.y_size);
        let (u_plane, v_plane) = rest.split_at_mut(self.c_size);

        CONVERT_POOL.get_or_init(build_convert_pool).install(|| {
            y_plane
                .par_chunks_mut(2 * w)
                .zip(u_plane.par_chunks_mut(cw))
                .zip(v_plane.par_chunks_mut(cw))
                .enumerate()
                .for_each(|(j, ((y2, urow), vrow))| {
                    let top = j * 2;
                    let row0 = &bgra[top * row_pitch..top * row_pitch + min_stride];
                    let row1 = &bgra[(top + 1) * row_pitch..(top + 1) * row_pitch + min_stride];
                    convert_row_pair(row0, row1, y2, urow, vrow, w);
                });
        });
        Ok(())
    }
}

impl Drop for X264Encoder {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        unsafe {
            let mut nals: *mut x264_nal_t = ptr::null_mut();
            let mut n_nal: c_int = 0;
            let mut pic_out: x264_picture_t = std::mem::zeroed();
            let mut guard = 0;
            while (self.api.encoder_delayed_frames)(self.handle) > 0 && guard < 64 {
                let s = (self.api.encoder_encode)(
                    self.handle,
                    &mut nals,
                    &mut n_nal,
                    ptr::null_mut(),
                    &mut pic_out,
                );
                if s <= 0 {
                    break;
                }
                guard += 1;
            }
            (self.api.encoder_close)(self.handle);
        }
        self.handle = ptr::null_mut();
    }
}

/// A dedicated bounded pool, not rayon's global one: the x264 encode thread is TIME_CRITICAL +
/// MMCSS and x264 runs its own slice threads, so letting the convert fan out across every core
/// oversubscribes those hot threads and adds latency under CPU contention.
static CONVERT_POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();

/// Workers run at ABOVE_NORMAL so the TIME_CRITICAL encode thread, blocked inside `install()`
/// while they run, is not stalled behind lower-priority system work.
fn build_convert_pool() -> rayon::ThreadPool {
    let n = std::thread::available_parallelism()
        .map(|c| (c.get() / 2).clamp(1, 4))
        .unwrap_or(2);
    rayon::ThreadPoolBuilder::new()
        .num_threads(n)
        .thread_name(|i| format!("x264-csc-{i}"))
        .start_handler(|_| unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
            };
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
        })
        .build()
        .unwrap_or_else(|_| {
            rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("1-thread fallback rayon pool")
        })
}

/// `x264_encoder_parameters` into a zeroed struct. Separate from the method because the init
/// log needs it before there is a `Self` to call it on.
fn read_back_params(api: &X264Api, handle: *mut x264_t) -> x264_param_t {
    let mut out: x264_param_t = unsafe { std::mem::zeroed() };
    unsafe { (api.encoder_parameters)(handle, &mut out) };
    out
}

fn bitrate_kbps(bps: u32) -> c_int {
    (bps / 1000).max(64) as c_int
}

/// `target` scaled by the delivered-rate pacer, never below the 64 kbps floor `bitrate_kbps`
/// already applies.
fn scaled_kbps(target: c_int, scale: f64) -> c_int {
    ((target as f64 * scale.clamp(0.0, 1.0)).round() as c_int).max(64)
}

fn vbv_buffer_kbit(kbps: c_int, fps: u32) -> c_int {
    let frame_kbit = kbps as f32 / fps.max(1) as f32;
    (frame_kbit * 2.0).max(1.0) as c_int
}

#[inline]
fn convert_row_pair(row0: &[u8], row1: &[u8], y2: &mut [u8], u: &mut [u8], v: &mut [u8], w: usize) {
    let (y0, y1) = y2.split_at_mut(w);
    for x in 0..w {
        let i = x * 4;
        let b0 = row0[i] as i32;
        let g0 = row0[i + 1] as i32;
        let r0 = row0[i + 2] as i32;
        y0[x] = bt709_y(r0, g0, b0);
        let b1 = row1[i] as i32;
        let g1 = row1[i + 1] as i32;
        let r1 = row1[i + 2] as i32;
        y1[x] = bt709_y(r1, g1, b1);
    }
    for cx in 0..w / 2 {
        let i = (cx * 2) * 4;
        let b =
            (row0[i] as i32 + row0[i + 4] as i32 + row1[i] as i32 + row1[i + 4] as i32 + 2) >> 2;
        let g =
            (row0[i + 1] as i32 + row0[i + 5] as i32 + row1[i + 1] as i32 + row1[i + 5] as i32 + 2)
                >> 2;
        let r =
            (row0[i + 2] as i32 + row0[i + 6] as i32 + row1[i + 2] as i32 + row1[i + 6] as i32 + 2)
                >> 2;
        u[cx] = bt709_u(r, g, b);
        v[cx] = bt709_v(r, g, b);
    }
}

// BT.709 limited range, 8-bit, in the usual >> 8 integer form. It has to match the VUI, which
// also covers the scaler's NV12 output (BT.709 limited under `scaler_bt709`).

#[inline(always)]
fn bt709_y(r: i32, g: i32, b: i32) -> u8 {
    clamp8(((47 * r + 157 * g + 16 * b + 128) >> 8) + 16)
}

#[inline(always)]
fn bt709_u(r: i32, g: i32, b: i32) -> u8 {
    clamp8(((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128)
}

#[inline(always)]
fn bt709_v(r: i32, g: i32, b: i32) -> u8 {
    clamp8(((112 * r - 102 * g - 10 * b + 128) >> 8) + 128)
}

#[inline(always)]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

pub fn probe_encode(config: &Config, path: &str) -> Result<()> {
    use std::io::Write as _;

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FPS: u32 = 60;
    const FRAMES: u32 = 300;
    const BITRATE: u32 = 10_000_000;

    tprintln!(
        "x264: encoding synthetic pattern to Annex-B: path={path}, {WIDTH}x{HEIGHT}@{FPS}, {FRAMES} frames"
    );

    // a file encode runs faster than real time, so the delivered-rate pacer would read it as a
    // very fast stream and starve it
    let options = X264Options {
        rate_by_delivered_fps: false,
        ..X264Options::from_switches()
    };
    let mut encoder = X264Encoder::with_options(
        EncoderConfig {
            width: WIDTH,
            height: HEIGHT,
            fps: FPS,
            bitrate_bps: BITRATE,
            max_bitrate_bps: BITRATE,
            profile: config.h264_profile,
            qp: config.qp,
            min_qp: config.quality_qp,
            intra_refresh: config.intra_refresh,
        },
        options,
    )?;

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
                "x264: encoded frame={i} (au_bytes={}, total_bytes={total_bytes})",
                au.len()
            );
        }
    }
    file.flush().context("flushing output file")?;

    tprintln!("x264: wrote Annex-B H.264: path={path}, frames={FRAMES}, total_bytes={total_bytes}");
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
