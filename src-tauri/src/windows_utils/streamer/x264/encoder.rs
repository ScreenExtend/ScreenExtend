use std::ffi::{c_char, c_int};
use std::ptr;

use anyhow::{bail, Context as _, Result};
use rayon::prelude::*;

use super::x264_sys::*;
use crate::streamer::config::{Config, H264Profile};
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
    y_size: usize,
    c_size: usize,
    have_frame: bool,
    pts: i64,
    use_bgra_csp: bool,
    repeat_bgra: Vec<u8>,
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
        p.rc.i_rc_method = X264_RC_ABR;
        p.rc.i_bitrate = kbps;
        p.rc.i_vbv_max_bitrate = kbps;
        p.rc.i_vbv_buffer_size = vbv_buffer_kbit(kbps, fps);

        // GOP / recovery
        p.i_keyint_max = X264_KEYINT_MAX_INFINITE;
        p.i_scenecut_threshold = 0;
        p.b_intra_refresh = if config.intra_refresh { 1 } else { 0 };

        // bitstream shape for WebRTC
        p.b_repeat_headers = 1;
        p.b_annexb = 1;
        p.i_log_level = X264_LOG_ERROR;

        // colour signalling
        p.vui.b_fullrange = 0;
        p.vui.i_colorprim = 6; // SMPTE 170M (BT.601)
        p.vui.i_transfer = 6; // SMPTE 170M
        p.vui.i_colmatrix = 6; // SMPTE 170M

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

        let mut p_bgra = p; // x264_param_t is Copy
        p_bgra.i_csp = X264_CSP_BGRA;
        let bgra_handle = unsafe { (api.encoder_open)(&mut p_bgra) };

        let (handle, use_bgra_csp) = if !bgra_handle.is_null() {
            (bgra_handle, true)
        } else {
            let h = unsafe { (api.encoder_open)(&mut p) };
            if h.is_null() {
                bail!("x264_encoder_open returned null (invalid params or libx264 build mismatch)");
            }
            (h, false)
        };

        let y_size = width * height;
        let c_size = (width / 2) * (height / 2);
        let i420 = vec![0u8; y_size + 2 * c_size];

        let csp_label = if use_bgra_csp { "BGRA" } else { "I420" };
        tprintln!(
            "pipeline: software x264 encoder ready ({width}x{height}@{fps}, {kbps} kbps, \
             threads={}, profile={:?}, csp={csp_label})",
            p.i_threads,
            config.profile,
        );

        Ok(Self {
            api,
            handle,
            params: p,
            config,
            width,
            height,
            i420,
            y_size,
            c_size,
            have_frame: false,
            pts: 0,
            use_bgra_csp,
            repeat_bgra: Vec::new(),
        })
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
        if self.use_bgra_csp {
            self.repeat_bgra.clear();
            self.repeat_bgra.extend_from_slice(data);
            let out = self.encode_with_bgra_direct(data, row_pitch, force_idr)?;
            self.have_frame = true;
            Ok(out)
        } else {
            self.convert_bgra_to_i420(data, row_pitch as usize)?;
            self.encode_converted(force_idr)
        }
    }

    pub fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        if !self.have_frame {
            return Ok(Vec::new());
        }
        if self.use_bgra_csp && !self.repeat_bgra.is_empty() {
            let data = self.repeat_bgra.clone();
            let row_pitch = (self.width * 4) as u32;
            return self.encode_with_bgra_direct(&data, row_pitch, force_idr);
        }
        self.encode_converted(force_idr)
    }

    pub fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        if self.config.qp.is_some() {
            return Ok(());
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
        Ok(())
    }

    fn encode_with_bgra_direct(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        let mut pic_in: x264_picture_t = unsafe { std::mem::zeroed() };
        unsafe { (self.api.picture_init)(&mut pic_in) };
        pic_in.img.i_csp = X264_CSP_BGRA;
        pic_in.img.i_plane = 1;
        pic_in.img.plane[0] = data.as_ptr() as *mut u8;
        pic_in.img.i_stride[0] = row_pitch as c_int;
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
            bail!("x264_encoder_encode (BGRA direct) failed ({size})");
        }
        self.pts += 1;

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

    fn encode_converted(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        let (y_ptr, u_ptr, v_ptr) = {
            let (y, rest) = self.i420.split_at_mut(self.y_size);
            let (u, v) = rest.split_at_mut(self.c_size);
            (y.as_mut_ptr(), u.as_mut_ptr(), v.as_mut_ptr())
        };

        let mut pic_in: x264_picture_t = unsafe { std::mem::zeroed() };
        unsafe { (self.api.picture_init)(&mut pic_in) };
        pic_in.img.i_csp = X264_CSP_I420;
        pic_in.img.i_plane = 3;
        pic_in.img.plane[0] = y_ptr;
        pic_in.img.plane[1] = u_ptr;
        pic_in.img.plane[2] = v_ptr;
        pic_in.img.i_stride[0] = self.width as c_int;
        pic_in.img.i_stride[1] = (self.width / 2) as c_int;
        pic_in.img.i_stride[2] = (self.width / 2) as c_int;
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

static CONVERT_POOL: std::sync::OnceLock<rayon::ThreadPool> = std::sync::OnceLock::new();

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
        y0[x] = bt601_y(r0, g0, b0);
        let b1 = row1[i] as i32;
        let g1 = row1[i + 1] as i32;
        let r1 = row1[i + 2] as i32;
        y1[x] = bt601_y(r1, g1, b1);
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
        u[cx] = bt601_u(r, g, b);
        v[cx] = bt601_v(r, g, b);
    }
}

#[inline(always)]
fn bt601_y(r: i32, g: i32, b: i32) -> u8 {
    clamp8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16)
}

#[inline(always)]
fn bt601_u(r: i32, g: i32, b: i32) -> u8 {
    clamp8(((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128)
}

#[inline(always)]
fn bt601_v(r: i32, g: i32, b: i32) -> u8 {
    clamp8(((112 * r - 94 * g - 18 * b + 128) >> 8) + 128)
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
