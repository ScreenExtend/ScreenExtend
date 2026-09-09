use std::ffi::{c_char, c_int};
use std::ptr;

use anyhow::{bail, Context as _, Result};
use rayon::prelude::*;

use super::x264_sys::*;
use crate::streamer::config::{Config, H264Profile};
use crate::streamer::platform::BitrateApplied;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;

const PRESET_ULTRAFAST: &[u8] = b"ultrafast\0";
const TUNE_ZEROLATENCY: &[u8] = b"zerolatency\0";
const PROFILE_BASELINE: &[u8] = b"baseline\0";
const PROFILE_MAIN: &[u8] = b"main\0";
const PROFILE_HIGH: &[u8] = b"high\0";
const MAX_ENCODE_THREADS: usize = 8;

pub struct X264Encoder {
    api: X264Api,
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
    /// Which of the two buffers holds the frame `encode_repeat` should re-encode.
    input_csp: c_int,
    y_size: usize,
    c_size: usize,
    have_frame: bool,
    pts: i64,
}

unsafe impl Send for X264Encoder {}

impl X264Encoder {
    pub fn new(config: EncoderConfig) -> Result<Self> {
        let api = X264Api::load().context("loading libx264 for the software H.264 fallback")?;

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
        p.b_vfr_input = 0;
        p.i_fps_num = fps;
        p.i_fps_den = 1;
        // frame-count timebase
        p.i_timebase_num = 1;
        p.i_timebase_den = fps;

        // latency-critical
        p.i_bframe = 0;
        p.b_sliced_threads = 1;
        p.i_threads = encode_thread_count() as c_int;
        p.rc.i_lookahead = 0;
        p.i_sync_lookahead = 0;

        // rate control
        let kbps = bitrate_kbps(config.bitrate_bps);
        if let Some(q) = config.qp {
            // Explicit constant QP (the --qp probe flag): bitrate is not a control input.
            p.rc.i_rc_method = X264_RC_CQP;
            p.rc.i_qp_constant = q as c_int;
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
        // macroblocks per frame, so keyint *is* the refresh period — leave it infinite and the
        // wave moves ~1e-9 MB/frame and never sweeps. One sweep per second matches the Intel
        // backend's `IntRefCycleSize = fps`.
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

        // Colour signalling. BT.709 limited range, which is what both input paths now carry:
        // the CPU BGRA converter uses the BT.709 matrix, and the scaler's NV12 comes out of the
        // D3D11 video processor in BT.709 limited. This used to say SMPTE 170M (BT.601, value 6)
        // and was correct only while the CPU converter — which was BT.601 — was the only door in.
        p.vui.b_fullrange = 0;
        p.vui.i_colorprim = 1; // BT.709
        p.vui.i_transfer = 1; // BT.709
        p.vui.i_colmatrix = 1; // BT.709

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

        // Input must be planar 4:2:0. libx264 does NOT convert RGB internally: any csp at or
        // above X264_CSP_I444 maps to I444 in frame_internal_csp(), and x264_sps_init then
        // forces PROFILE_HIGH444_PREDICTIVE regardless of what apply_profile asked for. A
        // packed-BGRA fast path therefore emits profile_idc 244 / chroma_format_idc 3, which
        // no browser WebRTC decoder accepts. `x264_profile_asserts_420` covers this.
        let handle = unsafe { (api.encoder_open)(&mut p) };
        if handle.is_null() {
            bail!("x264_encoder_open returned null (invalid params or libx264 build mismatch)");
        }

        let y_size = width * height;
        let c_size = (width / 2) * (height / 2);
        let i420 = vec![0u8; y_size + 2 * c_size];

        // Read back what libx264 settled on rather than reporting the request. `ultrafast` and
        // `zerolatency` are a preset and a tune, both of which move fields, and
        // `param_apply_profile` moves more — so printing `p` was printing what was asked for.
        // Every latency-critical setting in this file is in the list because each one is a way
        // for the encoder to be quietly buffering: a B frame, a lookahead, a frame-threaded
        // pipeline, or an intra-refresh period that never sweeps.
        let eff = read_back_params(&api, handle);
        tprintln!(
            "pipeline: software x264 encoder ready ({width}x{height}@{fps}, {kbps} kbps, \
             profile={:?}, lib={}, build={}). Effective: threads={}, sliced_threads={}, \
             bframes={}, rc_lookahead={}, sync_lookahead={}, keyint={}, intra_refresh={}, \
             scenecut={}, repeat_headers={}, annexb={}, nal_hrd={}, vbv_maxrate={}, \
             vbv_bufsize={}, colmatrix={}, fullrange={}",
            config.profile,
            api.library_path().display(),
            api.build,
            eff.i_threads,
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

        Ok(Self {
            api,
            handle,
            params: p,
            config,
            width,
            height,
            i420,
            nv12: Vec::new(),
            input_csp: X264_CSP_I420,
            y_size,
            c_size,
            have_frame: false,
            pts: 0,
        })
    }

    /// The parameters libx264 actually settled on, after the preset, the tune,
    /// `param_apply_profile` and its own validation have all had their say.
    ///
    /// `self.params` is the *request*. Everything this file claims about the encoder's
    /// configuration — no B frames, no lookahead, sliced rather than frame threads, an
    /// intra-refresh period that actually sweeps — was a claim about the request until this
    /// existed. `x264_zerolatency_settings_were_accepted` asserts against this, not against `p`.
    /// The libx264 build number the loader accepted, for the init log and the tests.
    pub fn build(&self) -> i32 {
        self.api.build
    }

    pub fn effective_params(&self) -> x264_param_t {
        read_back_params(&self.api, self.handle)
    }

    pub fn encode_bgra(&mut self, bgra: &[u8], force_idr: bool) -> Result<Vec<u8>> {
        self.encode_bgra_padded(bgra, (self.width * 4) as u32, force_idr)
    }

    pub fn encode_bgra_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        self.convert_bgra_to_i420(data, row_pitch as usize)?;
        self.input_csp = X264_CSP_I420;
        self.encode_converted(force_idr)
    }

    /// Encode a frame the GPU's video processor already converted to NV12.
    ///
    /// This is the whole point of `ReadbackFormat::Nv12`: the scale and the colour conversion are
    /// one `VideoProcessorBlt`, so asking for NV12 output costs the video engine nothing and
    /// deletes the CPU colour-matrix pass entirely. What is left here is a row copy into a
    /// tightly packed buffer — the mapped staging texture's pitch is not the picture width, and
    /// `encode_repeat` needs the frame to outlive the map — and then libx264 does the NV12→I420
    /// deinterleave itself, in SIMD, inside `x264_frame_copy_picture`.
    ///
    /// The picture csp differs from `param.i_csp` here, which is allowed: `frame_internal_csp`
    /// maps NV12 to I420, the same internal format the parameters opened with, so the SPS is
    /// unaffected. That is the difference from the BGRA fast path this replaces, which mapped to
    /// I444 and forced High 4:4:4 (`x264_profile_asserts_420`).
    pub fn encode_nv12_padded(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let row_pitch = row_pitch as usize;
        let (w, h) = (self.width, self.height);
        let rows = h + h / 2;
        if row_pitch < w || data.len() < row_pitch * rows {
            bail!(
                "encode_nv12_padded: need row_pitch >= {w} and >= {} bytes for {w}x{h} NV12 \
                 (got pitch {row_pitch}, {} bytes)",
                row_pitch * rows,
                data.len()
            );
        }

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

        self.input_csp = X264_CSP_NV12;
        self.encode_converted(force_idr)
    }

    /// Re-encode the already-converted I420 planes. No BGRA copy is retained, so a repeat can
    /// never disagree with the source frame's stride.
    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        if !self.have_frame {
            return Ok(Vec::new());
        }
        self.encode_converted(force_idr)
    }

    /// Advance the periodic intra-refresh wave immediately (loss repair without an IDR).
    /// A no-op unless the encoder was opened with `intra_refresh`.
    pub fn intra_refresh(&mut self) {
        if self.config.intra_refresh {
            unsafe { (self.api.encoder_intra_refresh)(self.handle) };
        }
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        if self.config.qp.is_some() {
            return Ok(BitrateApplied::IgnoredConstantQp);
        }
        let kbps = bitrate_kbps(bps);
        self.config.bitrate_bps = bps;
        self.config.max_bitrate_bps = bps;
        self.params.rc.i_bitrate = kbps;
        self.params.rc.i_vbv_max_bitrate = kbps;
        self.params.rc.i_vbv_buffer_size = vbv_buffer_kbit(kbps, self.config.fps.max(1));
        let rv = unsafe { (self.api.encoder_reconfig)(self.handle, &mut self.params) };
        if rv < 0 {
            bail!("x264_encoder_reconfig failed ({rv})");
        }
        Ok(BitrateApplied::Applied)
    }

    fn encode_converted(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let mut pic_in: x264_picture_t = unsafe { std::mem::zeroed() };
        unsafe { (self.api.picture_init)(&mut pic_in) };
        if self.input_csp == X264_CSP_NV12 {
            let (y, uv) = self.nv12.split_at_mut(self.y_size);
            pic_in.img.i_csp = X264_CSP_NV12;
            pic_in.img.i_plane = 2;
            pic_in.img.plane[0] = y.as_mut_ptr();
            pic_in.img.plane[1] = uv.as_mut_ptr();
            pic_in.img.i_stride[0] = self.width as c_int;
            pic_in.img.i_stride[1] = self.width as c_int;
        } else {
            let (y, rest) = self.i420.split_at_mut(self.y_size);
            let (u, v) = rest.split_at_mut(self.c_size);
            pic_in.img.i_csp = X264_CSP_I420;
            pic_in.img.i_plane = 3;
            pic_in.img.plane[0] = y.as_mut_ptr();
            pic_in.img.plane[1] = u.as_mut_ptr();
            pic_in.img.plane[2] = v.as_mut_ptr();
            pic_in.img.i_stride[0] = self.width as c_int;
            pic_in.img.i_stride[1] = (self.width / 2) as c_int;
            pic_in.img.i_stride[2] = (self.width / 2) as c_int;
        }
        pic_in.i_pts = self.pts;
        pic_in.i_type = if force_idr {
            X264_TYPE_IDR
        } else {
            X264_TYPE_AUTO
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
        self.pts += 1;
        self.have_frame = true;

        if size == 0 || n_nal == 0 || nals.is_null() {
            return Ok(Vec::new());
        }
        debug_assert_eq!(
            pic_out.i_dts, pic_out.i_pts,
            "0 B-frames must keep dts == pts (no reordering)"
        );

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

/// Workers are nonetheless raised to ABOVE_NORMAL so the TIME_CRITICAL encode thread, which is
/// blocked inside `install()` while they run, is not stalled by lower-priority system work.
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

fn encode_thread_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, MAX_ENCODE_THREADS)
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

// BT.709 limited range, 8-bit, in the usual >> 8 integer form.
//
// These were BT.601 (`66/129/25`, `-38/-74/112`, `112/-94/-18`), matching a BT.601 VUI, which was
// self-consistent as long as this CPU conversion was the only way in. It is not any more: the
// scaler's NV12 output comes from the D3D11 video processor, whose default output space is
// BT.709 limited on every driver observed, and there is one VUI for both. So the CPU path moves
// to BT.709 rather than the signalling moving to a lie. It also matches what the NVENC path has
// always written into its own VUI.

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

    let mut encoder = X264Encoder::new(EncoderConfig {
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
