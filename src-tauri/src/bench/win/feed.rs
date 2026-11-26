//! The feeder: frames at their deadlines, already written on the consumer's GPU.
//!
//! Producer threads read the clip (or render a synthetic pattern) and apply the transforms; an
//! uploader thread with its own device on the consumer's adapter writes each frame into a keyed
//! mutex slot texture created on the consumer's device, and waits for the GPU before handing it
//! over. Nothing on a consumer's timed path reads disk, allocates or converts.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};
use serde_json::{json, Value as Json};
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Multithread,
    ID3D11Query, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_RESOURCE_MISC_SHARED_NTHANDLE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIDevice, IDXGIKeyedMutex, IDXGIResource1};
use windows::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimer, WaitForSingleObject,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, INFINITE, TIMER_ALL_ACCESS,
};

use super::super::clip::{self, ClipInfo};
use super::super::plan::Workload as PlanWorkload;
use super::clipfmt::{decimate, decode_frame, BgraOp, FrameWord, PixelFormat, ReplayTransform};
use crate::streamer::synthetic::{self, Workload as Pattern};
use crate::windows_utils::streamer::scaler::{ReadbackFormat, Scaler};

/// Slots per session: frames uploaded ahead of the consumer.
pub const SLOTS: usize = 4;
/// Key the uploader holds a slot under; the consumer's is [`KEY_CONSUMER`].
const KEY_UPLOADER: u64 = 0;
const KEY_CONSUMER: u64 = 1;
/// `DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE`.
const SHARED_RW: u32 = 0x8000_0000 | 0x1;

/// What a consumer is handed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    /// BGRA8 on the consumer's device: what `on_frame_arrived` hands the encoder.
    Bgra,
    /// NV12 bytes in memory, produced by the production scaler ahead of the deadline.
    Nv12,
}

impl Input {
    pub fn parse(s: Option<&str>) -> Result<Input, String> {
        match s.unwrap_or("bgra") {
            "bgra" | "BGRA" => Ok(Input::Bgra),
            "nv12" | "NV12" => Ok(Input::Nv12),
            other => Err(format!("input '{other}' (bgra | nv12)")),
        }
    }
}

/// A workload's `transform`: `fps:N` plus any number of `clipfmt` picture operations, applied in
/// order (`tile2x2,crop:2560x1440` makes 1440p from a 1080p take).
#[derive(Clone, Debug, PartialEq)]
pub struct Ops {
    pub fps: Option<f64>,
    pub ops: Vec<BgraOp>,
}

impl Ops {
    pub fn parse(s: Option<&str>) -> Result<Ops, String> {
        let mut out = Ops {
            fps: None,
            ops: Vec::new(),
        };
        for part in s.unwrap_or("").split(',').map(str::trim) {
            if part.is_empty() || part == "none" {
                continue;
            }
            let t = ReplayTransform::parse(Some(part))?;
            if let Some(f) = t.fps {
                out.fps = Some(f);
            }
            if t.op != BgraOp::None {
                out.ops.push(t.op);
            }
        }
        Ok(out)
    }

    pub fn out_dims(&self, w: usize, h: usize) -> Result<(usize, usize), String> {
        let (mut w, mut h) = (w, h);
        for op in &self.ops {
            (w, h) = single(*op).out_dims(w, h)?;
        }
        Ok((w, h))
    }

    /// Apply every operation in order; the result lands in `out`, `scratch` is the spare buffer.
    fn apply(
        &self,
        src: &[u8],
        w: usize,
        h: usize,
        stride: usize,
        out: &mut Vec<u8>,
        scratch: &mut Vec<u8>,
    ) -> Result<(usize, usize), String> {
        if self.ops.is_empty() {
            return single(BgraOp::None).apply(src, w, h, stride, out);
        }
        let mut dims = (w, h);
        let mut rest = &self.ops[..];
        if let [BgraOp::Tile2x2, BgraOp::Crop(cw, ch), tail @ ..] = rest {
            dims = tile_then_crop(src, w, h, stride, (*cw, *ch), out)?;
            rest = tail;
        } else if let [op, tail @ ..] = rest {
            dims = single(*op).apply(src, w, h, stride, out)?;
            rest = tail;
        }
        for op in rest {
            std::mem::swap(out, scratch);
            dims = single(*op).apply(scratch, dims.0, dims.1, dims.0 * 4, out)?;
        }
        Ok(dims)
    }
}

/// `tile2x2` then `crop` in one pass, without the 2w x 2h picture in between: on a 1440p session
/// that intermediate is most of the transform's memory traffic.
fn tile_then_crop(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    crop: (usize, usize),
    out: &mut Vec<u8>,
) -> Result<(usize, usize), String> {
    let (tw, th) = single(BgraOp::Tile2x2).out_dims(w, h)?;
    let (ow, oh) = single(BgraOp::Crop(crop.0, crop.1)).out_dims(tw, th)?;
    if stride < w * 4 || src.len() < stride * (h - 1) + w * 4 {
        return Err(format!(
            "picture of {} bytes is short for {w}x{h} at stride {stride}",
            src.len()
        ));
    }
    let ox = ((tw - ow) / 2) & !1;
    let oy = ((th - oh) / 2) & !1;
    out.resize(ow * oh * 4, 0);
    for y in 0..oh {
        let sy = (y + oy) % h;
        let srow = &src[sy * stride..sy * stride + w * 4];
        let orow = &mut out[y * ow * 4..(y + 1) * ow * 4];
        let mut x = 0;
        while x < ow {
            let sx = (x + ox) % w;
            let n = (w - sx).min(ow - x);
            orow[x * 4..(x + n) * 4].copy_from_slice(&srow[sx * 4..(sx + n) * 4]);
            x += n;
        }
    }
    Ok((ow, oh))
}

fn single(op: BgraOp) -> ReplayTransform {
    ReplayTransform { fps: None, op }
}

/// Full-range BT.709 NV12 -> BGRA (what the macOS recorder's `420f` takes are).
pub fn nv12_to_bgra(nv12: &[u8], w: usize, h: usize, stride: usize, out: &mut [u8]) {
    let uv = &nv12[stride * h..];
    for y in 0..h {
        for x in 0..w {
            let yy = nv12[y * stride + x] as f32;
            let c = (y / 2) * stride + (x & !1);
            let u = uv[c] as f32 - 128.0;
            let v = uv[c + 1] as f32 - 128.0;
            let r = yy + 1.5748 * v;
            let g = yy - 0.1873 * u - 0.4681 * v;
            let b = yy + 1.8556 * u;
            let o = (y * w + x) * 4;
            out[o] = (b + 0.5).clamp(0.0, 255.0) as u8;
            out[o + 1] = (g + 0.5).clamp(0.0, 255.0) as u8;
            out[o + 2] = (r + 0.5).clamp(0.0, 255.0) as u8;
            out[o + 3] = 255;
        }
    }
}

/// Where the pictures come from.
#[derive(Clone)]
pub enum Source {
    Clip {
        path: PathBuf,
        info: Box<ClipInfo>,
    },
    Synthetic {
        pattern: Pattern,
        width: usize,
        height: usize,
    },
}

/// Every frame of a run: when it is due, which picture it is, and whether the recording asked for
/// an IDR there.
#[derive(Clone, Debug)]
pub struct Schedule {
    /// Deadline of frame `g`, ns from the run's start.
    pub at: Vec<u64>,
    /// Record index (clip) or frame index (synthetic) of frame `g`.
    pub src: Vec<usize>,
    /// The recording marked frame `g` as a forced IDR.
    pub force_idr: Vec<bool>,
    /// Frames per loop.
    pub per_loop: usize,
    pub loops: usize,
    /// One loop's length, ns (after `speed`).
    pub period_ns: u64,
    pub timeline_ns: u64,
    /// Output picture size, after the transforms.
    pub width: usize,
    pub height: usize,
    /// The rate the schedule averages, frames per second.
    pub fps: f64,
}

impl Schedule {
    pub fn total(&self) -> usize {
        self.at.len()
    }
}

fn median_gap(t: &[u64]) -> u64 {
    let mut g: Vec<u64> = t.windows(2).map(|w| w[1].saturating_sub(w[0])).collect();
    if g.is_empty() {
        return 16_666_667;
    }
    g.sort_unstable();
    g[g.len() / 2].max(1)
}

/// `base` is `(source index, time ns)` for one loop, already decimated.
fn schedule_from(
    base: &[(usize, u64)],
    idr: impl Fn(usize) -> bool,
    loops: usize,
    speed: f64,
    dims: (usize, usize),
) -> Schedule {
    let loops = loops.max(1);
    let speed = if speed > 0.0 && speed.is_finite() {
        speed
    } else {
        1.0
    };
    let times: Vec<u64> = base.iter().map(|b| b.1).collect();
    let t0 = times.first().copied().unwrap_or(0);
    let last = times.last().copied().unwrap_or(0) - t0;
    let period_raw = last + median_gap(&times);
    let n = base.len();
    let mut at = Vec::with_capacity(n * loops);
    let mut src = Vec::with_capacity(n * loops);
    let mut force = Vec::with_capacity(n * loops);
    for l in 0..loops {
        for (i, &(s, t)) in base.iter().enumerate() {
            let raw = (t - t0) + l as u64 * period_raw;
            at.push((raw as f64 / speed) as u64);
            src.push(s);
            // the first frame of the run is an IDR anyway; at a loop seam force nothing
            force.push(i != 0 && idr(s));
        }
    }
    let period_ns = (period_raw as f64 / speed) as u64;
    Schedule {
        at,
        src,
        force_idr: force,
        per_loop: n,
        loops,
        period_ns,
        timeline_ns: period_ns * loops as u64,
        width: dims.0,
        height: dims.1,
        fps: if period_ns > 0 {
            n as f64 * 1e9 / period_ns as f64
        } else {
            0.0
        },
    }
}

/// Whether the feeder renders this workload itself (a `streamer::synthetic` pattern) rather than
/// reading a clip the runner rendered.
pub fn renders_live(w: &PlanWorkload) -> bool {
    w.kind == "synthetic" && w.pattern.as_deref().and_then(Pattern::parse).is_some()
}

/// The source and schedule for a run.
pub fn plan(
    w: &PlanWorkload,
    clip_path: Option<&str>,
    loops: u32,
) -> Result<(Source, Schedule, Ops), String> {
    plan_at(w, clip_path, loops, 0.0)
}

/// [`plan`] starting `offset_s` into the source, so several sessions on one take are out of phase.
pub fn plan_at(
    w: &PlanWorkload,
    clip_path: Option<&str>,
    loops: u32,
    offset_s: f64,
) -> Result<(Source, Schedule, Ops), String> {
    let ops = Ops::parse(w.transform.as_deref())?;
    if renders_live(w) {
        let pattern = w
            .pattern
            .as_deref()
            .and_then(Pattern::parse)
            .ok_or("pattern")?;
        if w.width < 16
            || w.height < 16
            || !w.width.is_multiple_of(2)
            || !w.height.is_multiple_of(2)
        {
            return Err(format!(
                "synthetic size {}x{} (even, >= 16)",
                w.width, w.height
            ));
        }
        let fps = w.fps.max(1);
        let mut n = (w.seconds * fps as f64).round().max(1.0) as usize;
        if let Some(m) = w.max_frames {
            n = n.min(m.max(1));
        }
        let k0 = (offset_s.max(0.0) * fps as f64).round() as usize;
        let times: Vec<u64> = (0..n)
            .map(|k| (k as u128 * 1_000_000_000 / fps as u128) as u64)
            .collect();
        let base: Vec<(usize, u64)> = match ops.fps {
            Some(f) => decimate(&times, f),
            None => times.iter().copied().enumerate().collect(),
        }
        .into_iter()
        .map(|(i, t)| (i + k0, t))
        .collect();
        let dims = ops.out_dims(w.width, w.height)?;
        let s = schedule_from(&base, |_| false, loops as usize, w.speed, dims);
        return Ok((
            Source::Synthetic {
                pattern,
                width: w.width,
                height: w.height,
            },
            s,
            ops,
        ));
    }
    let path = clip_path.ok_or("no clip path")?;
    let info = clip::scan(std::path::Path::new(path))?;
    let r0 = info
        .times
        .partition_point(|&t| (t as f64) < offset_s.max(0.0) * 1e9)
        .min(info.frames.saturating_sub(1));
    let n = w
        .max_frames
        .map_or(info.frames - r0, |m| m.min(info.frames - r0))
        .max(1);
    let times = &info.times[r0..r0 + n];
    let base: Vec<(usize, u64)> = match ops.fps {
        Some(f) => decimate(times, f),
        None => times.iter().copied().enumerate().collect(),
    }
    .into_iter()
    .map(|(i, t)| (i + r0, t))
    .collect();
    if base.is_empty() {
        return Err("the clip yields no frames".into());
    }
    let dims = ops.out_dims(info.width, info.height)?;
    if dims.0 % 2 != 0 || dims.1 % 2 != 0 {
        return Err(format!("output {}x{} is not even", dims.0, dims.1));
    }
    let idr = info.idr.clone();
    let s = schedule_from(
        &base,
        |i| idr.get(i).copied().unwrap_or(false),
        loops as usize,
        w.speed,
        dims,
    );
    Ok((
        Source::Clip {
            path: PathBuf::from(path),
            info: Box::new(info),
        },
        s,
        ops,
    ))
}

/// When each stage finished for one frame.
#[derive(Clone, Copy, Debug)]
pub struct Times {
    /// Disk time for this frame, including records read and dropped to reach it (decimation).
    pub read_ns: u64,
    /// Decode, conversion and transforms (or rendering, for a synthetic frame).
    pub xform_ns: u64,
    /// Of `xform_ns`, decoding a compressed take's frame.
    pub decode_ns: u64,
    /// Records read only to be skipped on the way to this one.
    pub skipped: u32,
    /// How long the uploader waited for this session to give a slot back.
    pub slot_wait_ns: u64,
    pub cpu_ready: Instant,
    /// The uploader started on this frame for this session (it had a free slot).
    pub upload_start: Instant,
    /// The GPU finished writing the slot (or the NV12 bytes were in memory).
    pub upload_done: Instant,
}

/// NV12 bytes for one frame, `height * 3 / 2` rows at `pitch`.
pub struct Nv12 {
    pub data: Vec<u8>,
    pub pitch: u32,
}

/// One frame for one session.
pub struct Ready {
    pub g: usize,
    pub slot: usize,
    pub nv12: Option<Arc<Nv12>>,
    pub times: Times,
}

/// A consumer's end. Owns the slot textures as opened on its own device.
pub struct FeedSession {
    pub index: usize,
    ready_rx: Receiver<Ready>,
    free_tx: Sender<usize>,
    slots: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
}

// SAFETY: the textures belong to the consumer's device, which only the consumer's thread uses;
// the keyed mutexes order every access with the uploader's device.
unsafe impl Send for FeedSession {}

impl FeedSession {
    /// The next frame, blocking. `None` when the feeder ended (all frames delivered, or a fault).
    pub fn recv(&self) -> Option<Ready> {
        self.ready_rx.recv().ok()
    }

    /// The next frame if it is already ready.
    pub fn try_recv(&self) -> Option<Ready> {
        self.ready_rx.try_recv().ok()
    }

    pub fn ready_len(&self) -> usize {
        self.ready_rx.len()
    }

    /// Take the slot for the consumer's device. The upload has already finished on the GPU, so
    /// this does not wait for it.
    pub fn acquire(&self, r: &Ready) -> Result<&ID3D11Texture2D, String> {
        let (tex, m) = self
            .slots
            .get(r.slot)
            .ok_or_else(|| format!("slot {} out of range", r.slot))?;
        unsafe { m.AcquireSync(KEY_CONSUMER, 2000) }.map_err(|e| format!("AcquireSync: {e}"))?;
        Ok(tex)
    }

    /// Give the slot back (after `acquire`).
    pub fn release(&self, r: Ready) {
        if let Some((_, m)) = self.slots.get(r.slot) {
            let _ = unsafe { m.ReleaseSync(KEY_UPLOADER) };
        }
        let _ = self.free_tx.send(r.slot);
    }

    /// Give back an NV12 frame (nothing to release on the GPU).
    pub fn done(&self, r: Ready) {
        drop(r);
    }
}

/// What the feeder threads report when they finish.
#[derive(Default, Debug, Clone)]
pub struct FeedReport {
    pub frames_produced: usize,
    pub frames_uploaded: usize,
    pub bytes_read: u64,
    pub records_skipped: u64,
    pub producer_error: Option<String>,
    pub uploader_error: Option<String>,
    pub producers: usize,
    pub read_ahead: usize,
    pub adapter: Option<String>,
}

/// Frames produced, bytes read and records skipped, per producer.
type ProducerHandle = std::thread::JoinHandle<Result<(usize, u64, u64), String>>;

pub struct Feeder {
    producers: Vec<ProducerHandle>,
    uploader: Option<std::thread::JoinHandle<Result<usize, String>>>,
    stop: Arc<AtomicBool>,
    pub read_ahead: usize,
    pub adapter: Option<String>,
    pub input: Input,
    /// Transform or render workers (a clip also has one reader thread).
    pub workers: usize,
}

impl Feeder {
    /// Wait until every session has `want` frames ready (or the feeder ended), up to `limit`.
    pub fn wait_prefill(&self, sessions: &[FeedSession], want: usize, limit: Duration) -> bool {
        let t = Instant::now();
        loop {
            if sessions.iter().all(|s| s.ready_len() >= want) {
                return true;
            }
            if self.uploader.as_ref().is_some_and(|h| h.is_finished()) {
                return false;
            }
            if t.elapsed() > limit {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Stop and join every thread. Call after every session has been dropped or drained.
    pub fn finish(mut self) -> FeedReport {
        self.stop.store(true, Ordering::Relaxed);
        let mut r = FeedReport {
            producers: self.workers,
            read_ahead: self.read_ahead,
            adapter: self.adapter.clone(),
            ..Default::default()
        };
        if let Some(h) = self.uploader.take() {
            match h.join() {
                Ok(Ok(n)) => r.frames_uploaded = n,
                Ok(Err(e)) => r.uploader_error = Some(e),
                Err(_) => r.uploader_error = Some("uploader panicked".into()),
            }
        }
        for h in self.producers.drain(..) {
            match h.join() {
                Ok(Ok((n, bytes, skipped))) => {
                    r.frames_produced += n;
                    r.bytes_read += bytes;
                    r.records_skipped += skipped;
                }
                Ok(Err(e)) => r.producer_error = Some(e),
                Err(_) => r.producer_error = Some("producer panicked".into()),
            }
        }
        r
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A CPU picture on its way to the uploader.
struct Cpu {
    g: usize,
    buf: Vec<u8>,
    read_ns: u64,
    xform_ns: u64,
    decode_ns: u64,
    skipped: u32,
    ready: Instant,
}

/// A device on the same adapter as `of`, with BGRA and video support (the scaler needs the
/// latter), multithread-protected.
pub fn device_on_adapter_of(
    of: &ID3D11Device,
) -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let dxgi: IDXGIDevice = of.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
    let adapter: IDXGIAdapter =
        unsafe { dxgi.GetAdapter() }.map_err(|e| format!("GetAdapter: {e}"))?;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let mut dev = None;
    let mut ctx = None;
    let flags = [
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    ];
    let mut last = String::new();
    for f in flags {
        let r = unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                windows::Win32::Foundation::HMODULE::default(),
                f,
                Some(&levels[..]),
                D3D11_SDK_VERSION,
                Some(&mut dev),
                None,
                Some(&mut ctx),
            )
        };
        match r {
            Ok(()) => break,
            Err(e) => last = e.to_string(),
        }
    }
    let (dev, ctx) = match (dev, ctx) {
        (Some(d), Some(c)) => (d, c),
        _ => {
            return Err(format!(
                "D3D11CreateDevice on the consumer's adapter: {last}"
            ))
        }
    };
    if let Ok(mt) = dev.cast::<ID3D11Multithread>() {
        let _ = unsafe { mt.SetMultithreadProtected(true) };
    }
    Ok((dev, ctx))
}

/// `SLOTS` shared BGRA8 textures on `consumer`, opened on `uploader`.
#[allow(clippy::type_complexity)]
fn make_slots(
    consumer: &ID3D11Device,
    uploader: &ID3D11Device,
    w: u32,
    h: u32,
) -> Result<
    (
        Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
        Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
    ),
    String,
> {
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
        // what a capture texture carries, so the scaler and a CopyResource both take it
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0)
            as u32,
    };
    let up1: ID3D11Device1 = uploader.cast().map_err(|e| format!("ID3D11Device1: {e}"))?;
    let mut cons = Vec::with_capacity(SLOTS);
    let mut ups = Vec::with_capacity(SLOTS);
    for i in 0..SLOTS {
        let mut t: Option<ID3D11Texture2D> = None;
        unsafe { consumer.CreateTexture2D(&desc, None, Some(&mut t)) }
            .map_err(|e| format!("CreateTexture2D (feed slot {i}): {e}"))?;
        let t = t.ok_or("feed slot texture was null")?;
        let res: IDXGIResource1 = t.cast().map_err(|e| format!("IDXGIResource1: {e}"))?;
        let handle: HANDLE = unsafe { res.CreateSharedHandle(None, SHARED_RW, PCWSTR::null()) }
            .map_err(|e| format!("CreateSharedHandle: {e}"))?;
        let opened: Result<ID3D11Texture2D, String> = unsafe { up1.OpenSharedResource1(handle) }
            .map_err(|e| format!("OpenSharedResource1: {e}"));
        let _ = unsafe { CloseHandle(handle) };
        let opened = opened?;
        let mc: IDXGIKeyedMutex = t.cast().map_err(|e| format!("keyed mutex: {e}"))?;
        let mu: IDXGIKeyedMutex = opened.cast().map_err(|e| format!("keyed mutex: {e}"))?;
        cons.push((t, mc));
        ups.push((opened, mu));
    }
    Ok((cons, ups))
}

fn event_query(dev: &ID3D11Device) -> Result<ID3D11Query, String> {
    let desc = D3D11_QUERY_DESC {
        Query: D3D11_QUERY_EVENT,
        MiscFlags: 0,
    };
    let mut q: Option<ID3D11Query> = None;
    unsafe { dev.CreateQuery(&desc, Some(&mut q)) }.map_err(|e| format!("CreateQuery: {e}"))?;
    q.ok_or_else(|| "event query was null".into())
}

/// Wait for everything submitted before `End(q)` to finish on the GPU.
fn gpu_wait(ctx: &ID3D11DeviceContext, q: &ID3D11Query) {
    unsafe {
        ctx.End(q);
        ctx.Flush();
        let mut spins = 0u32;
        loop {
            let mut done: i32 = 0;
            let r = ctx.GetData(
                q,
                Some(&mut done as *mut i32 as *mut core::ffi::c_void),
                std::mem::size_of::<i32>() as u32,
                0,
            );
            // S_FALSE (not done yet) is also `Ok`; only a written TRUE means done
            if r.is_ok() && done != 0 {
                return;
            }
            spins += 1;
            if spins < 200 {
                std::hint::spin_loop();
            } else {
                std::thread::yield_now();
            }
        }
    }
}

/// Start the feeder for `sessions` consumer devices (one per session; all on one adapter).
pub fn start(
    source: Source,
    sched: Arc<Schedule>,
    ops: Ops,
    consumers: &[ID3D11Device],
    input: Input,
) -> Result<(Feeder, Vec<FeedSession>), String> {
    let first = consumers.first().ok_or("no consumer device")?;
    let (up_dev, up_ctx) = device_on_adapter_of(first)?;
    let adapter = crate::windows_utils::windows_capture::d3d11::device_adapter_info(&up_dev)
        .map(|a| a.label());
    let (ow, oh) = (sched.width, sched.height);
    let frame_bytes = ow * oh * 4;
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    // a clip has one reader; its decode, conversion and transforms run on these workers
    let producers_n = match &source {
        Source::Clip { info, .. } if info.table.is_some() => (cores / 4).clamp(2, 6),
        Source::Clip { .. } => (cores / 4).clamp(1, 4),
        Source::Synthetic { .. } => (cores / 3).clamp(1, 6),
    };
    let read_ahead = (256_000_000 / frame_bytes.max(1))
        .clamp(4, 64)
        .max(2 * producers_n);
    let per_producer = (read_ahead / producers_n).max(1);
    let pool_n = producers_n * (per_producer + 2) + 1;

    // CPU buffer pool: its only sender moves into the uploader, so a dead uploader frees every
    // producer blocked on it
    let (pool_tx, pool_rx) = bounded::<Vec<u8>>(pool_n);
    for _ in 0..pool_n {
        let _ = pool_tx.send(vec![0u8; frame_bytes]);
    }
    let stop = Arc::new(AtomicBool::new(false));

    let mut sessions = Vec::with_capacity(consumers.len());
    let mut up_side = Vec::with_capacity(consumers.len());
    for (k, dev) in consumers.iter().enumerate() {
        let (ready_tx, ready_rx) = bounded::<Ready>(SLOTS.max(8));
        let (free_tx, free_rx) = bounded::<usize>(SLOTS);
        let (cons, ups) = if input == Input::Bgra {
            make_slots(dev, &up_dev, ow as u32, oh as u32)?
        } else {
            (Vec::new(), Vec::new())
        };
        if input == Input::Bgra {
            for i in 0..SLOTS {
                let _ = free_tx.send(i);
            }
        }
        sessions.push(FeedSession {
            index: k,
            ready_rx,
            free_tx,
            slots: cons,
        });
        up_side.push((ready_tx, free_rx, ups));
    }

    let mut prod_rx = Vec::with_capacity(producers_n);
    let mut producers = Vec::with_capacity(producers_n);
    let mut raw_ins: Vec<Option<RawIn>> = (0..producers_n).map(|_| None).collect();
    if let Source::Clip { path, info } = &source {
        let raw_n = producers_n * (per_producer + 2) + 1;
        let (raw_back, raw_pool) = bounded::<Vec<u8>>(raw_n);
        for _ in 0..raw_n {
            let _ = raw_back.send(Vec::with_capacity(info.layout.payload));
        }
        let mut to_workers = Vec::with_capacity(producers_n);
        for slot in raw_ins.iter_mut() {
            let (wtx, wrx) = bounded::<Raw>(per_producer);
            to_workers.push(wtx);
            *slot = Some((wrx, raw_back.clone()));
        }
        drop(raw_back);
        let (path, info, sched, stop) = (
            path.clone(),
            info.clone(),
            Arc::clone(&sched),
            Arc::clone(&stop),
        );
        producers.push(
            std::thread::Builder::new()
                .name("feed-read".into())
                .spawn(move || read_clip(path, *info, sched, raw_pool, to_workers, stop))
                .map_err(|e| e.to_string())?,
        );
    }
    for (p, raw_slot) in raw_ins.iter_mut().enumerate() {
        let (tx, rx) = bounded::<Cpu>(per_producer);
        prod_rx.push(rx);
        let pool = pool_rx.clone();
        let sched = Arc::clone(&sched);
        let source = source.clone();
        let ops = ops.clone();
        let stop = Arc::clone(&stop);
        let raw_in = raw_slot.take();
        producers.push(
            std::thread::Builder::new()
                .name(format!("feed-produce-{p}"))
                .spawn(move || produce(p, producers_n, source, sched, ops, pool, tx, stop, raw_in))
                .map_err(|e| e.to_string())?,
        );
    }
    drop(pool_rx);

    struct SendDev(ID3D11Device, ID3D11DeviceContext, Vec<UpSide>);
    unsafe impl Send for SendDev {}
    let bundle = SendDev(up_dev, up_ctx, up_side);
    let sched_u = Arc::clone(&sched);
    let stop_u = Arc::clone(&stop);
    let uploader = std::thread::Builder::new()
        .name("feed-upload".into())
        .spawn(move || {
            // bind the whole bundle first: a closure that only destructured it would capture
            // its (non-`Send`) fields one by one
            let bundle = bundle;
            let SendDev(dev, ctx, ups) = bundle;
            upload(dev, ctx, ups, prod_rx, pool_tx, sched_u, input, stop_u)
        })
        .map_err(|e| e.to_string())?;
    Ok((
        Feeder {
            producers,
            uploader: Some(uploader),
            stop,
            read_ahead,
            workers: producers_n,
            adapter,
            input,
        },
        sessions,
    ))
}

type UpSide = (
    Sender<Ready>,
    Receiver<usize>,
    Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
);

/// One record on its way from the clip reader to a worker: the picture, or for a compressed
/// take its stored bytes and word, decoded on the worker.
struct Raw {
    g: usize,
    frame: usize,
    raw: Vec<u8>,
    word: Option<FrameWord>,
    read_ns: u64,
    skipped: u32,
}

/// A worker's input and the way back to the reader's buffer pool.
type RawIn = (Receiver<Raw>, Sender<Vec<u8>>);

/// The clip reader: sequential reads in schedule order, frame `g` to worker `g % workers`.
fn read_clip(
    path: PathBuf,
    info: ClipInfo,
    sched: Arc<Schedule>,
    pool: Receiver<Vec<u8>>,
    workers: Vec<Sender<Raw>>,
    stop: Arc<AtomicBool>,
) -> Result<(usize, u64, u64), String> {
    let mut bytes = 0u64;
    let mut skipped_total = 0u64;
    let mut g = 0usize;
    'loops: for _ in 0..sched.loops {
        let mut r = clip::ClipReader::open(&path, &info)
            .map_err(|e| format!("open {}: {e}", path.display()))?;
        let mut cursor = 0usize;
        for _ in 0..sched.per_loop {
            if stop.load(Ordering::Relaxed) {
                break 'loops;
            }
            let want = sched.src[g];
            let Ok(mut raw) = pool.recv() else {
                break 'loops;
            };
            let t = Instant::now();
            let skipped = want.saturating_sub(cursor) as u32;
            if cursor != want {
                r.seek_frame(want)
                    .map_err(|e| format!("seek to record {want}: {e}"))?;
            }
            let word = r
                .next_stored(&mut raw)
                .map_err(|e| format!("read record {want}: {e}"))?;
            cursor = want + 1;
            bytes += raw.len() as u64;
            skipped_total += skipped as u64;
            let msg = Raw {
                g,
                frame: want,
                raw,
                word,
                read_ns: t.elapsed().as_nanos() as u64,
                skipped,
            };
            if workers[g % workers.len()].send(msg).is_err() {
                break 'loops;
            }
            g += 1;
        }
    }
    Ok((0, bytes, skipped_total))
}

#[allow(clippy::too_many_arguments)]
fn produce(
    p: usize,
    np: usize,
    source: Source,
    sched: Arc<Schedule>,
    ops: Ops,
    pool: Receiver<Vec<u8>>,
    tx: Sender<Cpu>,
    stop: Arc<AtomicBool>,
    raw_in: Option<RawIn>,
) -> Result<(usize, u64, u64), String> {
    let total = sched.total();
    let mut scratch = Vec::new();
    let mut produced = 0usize;
    match source {
        Source::Synthetic {
            pattern,
            width,
            height,
        } => {
            let direct = ops.ops.is_empty();
            let mut raw = if direct {
                Vec::new()
            } else {
                vec![0u8; width * height * 4]
            };
            let mut g = p;
            while g < total {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut buf) = pool.recv() else {
                    break;
                };
                let t = Instant::now();
                let frame = sched.src[g] as u32;
                if direct {
                    synthetic::fill(&mut buf, width as u32, height as u32, frame, pattern);
                } else {
                    synthetic::fill(&mut raw, width as u32, height as u32, frame, pattern);
                    ops.apply(&raw, width, height, width * 4, &mut buf, &mut scratch)?;
                }
                let xform_ns = t.elapsed().as_nanos() as u64;
                if tx
                    .send(Cpu {
                        g,
                        buf,
                        read_ns: 0,
                        xform_ns,
                        decode_ns: 0,
                        skipped: 0,
                        ready: Instant::now(),
                    })
                    .is_err()
                {
                    break;
                }
                produced += 1;
                g += np;
            }
            Ok((produced, 0, 0))
        }
        Source::Clip { info, .. } => {
            let lay = info.layout;
            let (rx, back) = raw_in.ok_or("clip worker without a reader")?;
            let mut conv = if lay.format == PixelFormat::Nv12 {
                vec![0u8; info.width * info.height * 4]
            } else {
                Vec::new()
            };
            let mut decoded: Vec<u8> = Vec::new();
            while let Ok(Raw {
                g,
                frame,
                raw,
                word,
                read_ns,
                skipped,
            }) = rx.recv()
            {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(mut buf) = pool.recv() else {
                    break;
                };
                let t = Instant::now();
                let pic: &[u8] = match word {
                    Some(w) => {
                        decode_frame(frame, &w, &raw, lay.payload, &mut decoded)?;
                        &decoded
                    }
                    None => &raw,
                };
                let decode_ns = t.elapsed().as_nanos() as u64;
                if lay.format == PixelFormat::Nv12 {
                    nv12_to_bgra(pic, info.width, info.height, lay.stride, &mut conv);
                    ops.apply(
                        &conv,
                        info.width,
                        info.height,
                        info.width * 4,
                        &mut buf,
                        &mut scratch,
                    )?;
                } else {
                    ops.apply(
                        pic,
                        info.width,
                        info.height,
                        lay.stride,
                        &mut buf,
                        &mut scratch,
                    )?;
                }
                let xform_ns = t.elapsed().as_nanos() as u64;
                let _ = back.send(raw);
                if tx
                    .send(Cpu {
                        g,
                        buf,
                        read_ns,
                        xform_ns,
                        decode_ns,
                        skipped,
                        ready: Instant::now(),
                    })
                    .is_err()
                {
                    break;
                }
                produced += 1;
            }
            Ok((produced, 0, 0))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn upload(
    dev: ID3D11Device,
    ctx: ID3D11DeviceContext,
    sessions: Vec<UpSide>,
    prod: Vec<Receiver<Cpu>>,
    pool: Sender<Vec<u8>>,
    sched: Arc<Schedule>,
    input: Input,
    stop: Arc<AtomicBool>,
) -> Result<usize, String> {
    let _tuning = crate::windows_utils::streamer::tuning::tune_timer_thread();
    let (w, h) = (sched.width as u32, sched.height as u32);
    let query = event_query(&dev)?;
    // NV12: the production scaler on this device, fed from a local BGRA texture
    let mut nv = if input == Input::Nv12 {
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
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut t: Option<ID3D11Texture2D> = None;
        unsafe { dev.CreateTexture2D(&desc, None, Some(&mut t)) }
            .map_err(|e| format!("CreateTexture2D (nv12 source): {e}"))?;
        let scaler = Scaler::with_format(&dev, &ctx, w, h, w, h, ReadbackFormat::Nv12)
            .map_err(|e| format!("production scaler (BGRA -> NV12): {e:#}"))?;
        if scaler.format() != ReadbackFormat::Nv12 {
            return Err("the video processor refused NV12 output on this adapter".into());
        }
        Some((t.ok_or("nv12 source texture was null")?, scaler))
    } else {
        None
    };
    let total = sched.total();
    let np = prod.len();
    let mut uploaded = 0usize;
    for g in 0..total {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(cpu) = prod[g % np].recv() else {
            break; // the producer ended (error or stop)
        };
        if cpu.g != g {
            return Err(format!("producer order: expected frame {g}, got {}", cpu.g));
        }
        match &mut nv {
            None => {
                for (ready_tx, free_rx, slots) in &sessions {
                    let waited = Instant::now();
                    let Ok(slot) = free_rx.recv() else {
                        return Ok(uploaded);
                    };
                    let start = Instant::now();
                    let slot_wait_ns = (start - waited).as_nanos() as u64;
                    let (tex, m) = &slots[slot];
                    unsafe {
                        m.AcquireSync(KEY_UPLOADER, INFINITE)
                            .map_err(|e| format!("uploader AcquireSync: {e}"))?;
                        ctx.UpdateSubresource(tex, 0, None, cpu.buf.as_ptr() as *const _, w * 4, 0);
                    }
                    gpu_wait(&ctx, &query);
                    unsafe { m.ReleaseSync(KEY_CONSUMER) }
                        .map_err(|e| format!("uploader ReleaseSync: {e}"))?;
                    let done = Instant::now();
                    let r = Ready {
                        g,
                        slot,
                        nv12: None,
                        times: Times {
                            read_ns: cpu.read_ns,
                            xform_ns: cpu.xform_ns,
                            decode_ns: cpu.decode_ns,
                            skipped: cpu.skipped,
                            slot_wait_ns,
                            cpu_ready: cpu.ready,
                            upload_start: start,
                            upload_done: done,
                        },
                    };
                    if ready_tx.send(r).is_err() {
                        return Ok(uploaded);
                    }
                }
            }
            Some((src, scaler)) => {
                let start = Instant::now();
                unsafe {
                    ctx.UpdateSubresource(&*src, 0, None, cpu.buf.as_ptr() as *const _, w * 4, 0);
                }
                scaler.scale(src).map_err(|e| format!("scaler: {e:#}"))?;
                let (data, pitch) = scaler
                    .read_back()
                    .map_err(|e| format!("read_back: {e:#}"))?;
                let frame = Arc::new(Nv12 {
                    data: data.to_vec(),
                    pitch,
                });
                let done = Instant::now();
                for (ready_tx, _, _) in &sessions {
                    let r = Ready {
                        g,
                        slot: usize::MAX,
                        nv12: Some(Arc::clone(&frame)),
                        times: Times {
                            read_ns: cpu.read_ns,
                            xform_ns: cpu.xform_ns,
                            decode_ns: cpu.decode_ns,
                            skipped: cpu.skipped,
                            slot_wait_ns: 0,
                            cpu_ready: cpu.ready,
                            upload_start: start,
                            upload_done: done,
                        },
                    };
                    if ready_tx.send(r).is_err() {
                        return Ok(uploaded);
                    }
                }
            }
        }
        uploaded += 1;
        let _ = pool.send(cpu.buf);
    }
    Ok(uploaded)
}

/// Sleeps to a deadline: a high-resolution waitable timer (`CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`,
/// probed by creating one) to within `margin` of it, then a spin.
pub struct Pacer {
    timer: Option<HANDLE>,
    margin: Duration,
}

impl Pacer {
    pub fn new() -> Pacer {
        let timer = unsafe {
            CreateWaitableTimerExW(
                None,
                PCWSTR::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS.0,
            )
        }
        .ok();
        Pacer {
            margin: if timer.is_some() {
                Duration::from_micros(700)
            } else {
                Duration::from_micros(2000)
            },
            timer,
        }
    }

    pub fn kind(&self) -> &'static str {
        if self.timer.is_some() {
            "high_resolution_waitable_timer+spin"
        } else {
            "sleep+spin (no high-resolution timer)"
        }
    }

    /// Wait until `deadline`; returns how late the release was, ns.
    pub fn wait_until(&self, deadline: Instant) -> u64 {
        loop {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let left = deadline - now;
            if left > self.margin {
                let sleep = left - self.margin;
                match self.timer {
                    Some(t) => unsafe {
                        // relative due time: negative, in 100 ns units
                        let due = -((sleep.as_nanos() / 100) as i64).max(1);
                        if SetWaitableTimer(t, &due, 0, None, None, false).is_ok() {
                            let _ = WaitForSingleObject(t, INFINITE);
                        } else {
                            std::thread::sleep(sleep);
                        }
                    },
                    None => std::thread::sleep(sleep),
                }
            } else {
                std::hint::spin_loop();
            }
        }
        Instant::now()
            .saturating_duration_since(deadline)
            .as_nanos() as u64
    }
}

impl Default for Pacer {
    fn default() -> Self {
        Pacer::new()
    }
}

impl Drop for Pacer {
    fn drop(&mut self) {
        if let Some(t) = self.timer.take() {
            let _ = unsafe { CloseHandle(t) };
        }
    }
}

/// Why a frame was not on the GPU by its deadline: `disk`, `decode` or `transform` (the CPU
/// picture was late), `upload`, or `consumer` (every slot was still held by a consumer that was
/// behind, which is not a feeder fault). `None` when it was ready.
pub fn classify_miss(deadline: Instant, t: &Times) -> Option<&'static str> {
    if t.upload_done <= deadline {
        return None;
    }
    if t.cpu_ready > deadline {
        return Some(if t.read_ns >= t.xform_ns {
            "disk"
        } else if t.decode_ns * 2 > t.xform_ns {
            "decode"
        } else {
            "transform"
        });
    }
    if t.upload_start > deadline && t.slot_wait_ns > 200_000 {
        return Some("consumer");
    }
    Some("upload")
}

/// Per-frame feeder stats, in preallocated arrays, written to JSON after the run.
pub struct FeedStats {
    pub read_ms: Vec<f64>,
    pub xform_ms: Vec<f64>,
    pub decode_ms: Vec<f64>,
    pub upload_ms: Vec<f64>,
    /// Deadline minus upload-done (negative = late).
    pub lead_ms: Vec<f64>,
    pub misses: Vec<(usize, &'static str)>,
    pub skipped: u64,
}

impl FeedStats {
    pub fn with_capacity(n: usize) -> FeedStats {
        FeedStats {
            read_ms: Vec::with_capacity(n),
            xform_ms: Vec::with_capacity(n),
            decode_ms: Vec::with_capacity(n),
            upload_ms: Vec::with_capacity(n),
            lead_ms: Vec::with_capacity(n),
            misses: Vec::new(),
            skipped: 0,
        }
    }

    /// Record one frame against its deadline; returns the miss cause, if any.
    pub fn note(&mut self, g: usize, deadline: Instant, t: &Times) -> Option<&'static str> {
        self.read_ms.push(t.read_ns as f64 / 1e6);
        self.xform_ms.push(t.xform_ns as f64 / 1e6);
        self.decode_ms.push(t.decode_ns as f64 / 1e6);
        self.upload_ms.push(
            t.upload_done
                .saturating_duration_since(t.upload_start)
                .as_secs_f64()
                * 1e3,
        );
        let lead = if t.upload_done <= deadline {
            (deadline - t.upload_done).as_secs_f64() * 1e3
        } else {
            -((t.upload_done - deadline).as_secs_f64() * 1e3)
        };
        self.lead_ms.push(lead);
        self.skipped += t.skipped as u64;
        let c = classify_miss(deadline, t);
        if let Some(c) = c {
            if self.misses.len() < 100_000 {
                self.misses.push((g, c));
            }
        }
        c
    }

    /// The `feeder` object of a result: per-stage times, headroom and misses.
    pub fn to_json(
        &self,
        sched: &Schedule,
        frame_bytes: usize,
        report: &FeedReport,
        pacer: &str,
    ) -> Json {
        use super::super::metrics::summary;
        let mut causes: std::collections::BTreeMap<&str, usize> = Default::default();
        for (_, c) in &self.misses {
            *causes.entry(c).or_default() += 1;
        }
        let harness = self.misses.iter().filter(|(_, c)| *c != "consumer").count();
        let med = |v: &[f64]| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            let mut s = v.to_vec();
            s.sort_by(f64::total_cmp);
            s[s.len() / 2]
        };
        let p95 = |v: &[f64]| -> f64 {
            if v.is_empty() {
                return 0.0;
            }
            let mut s = v.to_vec();
            s.sort_by(f64::total_cmp);
            s[((s.len() as f64 * 0.95) as usize).min(s.len() - 1)]
        };
        // capacity: each stage runs on its own thread(s), so the slowest one bounds the rate
        // (one reader, `producers` transform workers)
        let w = report.producers.max(1) as f64;
        let prod = med(&self.read_ms).max(med(&self.xform_ms) / w);
        let prod95 = p95(&self.read_ms).max(p95(&self.xform_ms) / w);
        let up = med(&self.upload_ms);
        let up95 = p95(&self.upload_ms);
        let cap = |ms: f64| if ms > 0.0 { 1000.0 / ms } else { f64::INFINITY };
        let read_total_s: f64 = self.read_ms.iter().sum::<f64>() / 1e3;
        json!({
            "pacer": pacer,
            "adapter": report.adapter,
            "producers": report.producers,
            "read_ahead_frames": report.read_ahead,
            "slots": SLOTS,
            "frame_bytes": frame_bytes,
            "read_ms": summary(&self.read_ms),
            "transform_ms": summary(&self.xform_ms),
            "decode_ms": summary(&self.decode_ms),
            "upload_ms": summary(&self.upload_ms),
            "lead_ms": summary(&self.lead_ms),
            "records_skipped": self.skipped,
            "bytes_read": report.bytes_read,
            "read_mbps": if read_total_s > 0.0 { report.bytes_read as f64 / read_total_s / 1e6 } else { 0.0 },
            "capacity_fps_p50": (cap(prod.max(up)) * 10.0).round() / 10.0,
            "capacity_fps_p95": (cap(prod95.max(up95)) * 10.0).round() / 10.0,
            "limiting_stage": if prod >= up { "producer (disk + transform)" } else { "upload" },
            "schedule_fps": (sched.fps * 100.0).round() / 100.0,
            "misses": self.misses.len(),
            "harness_misses": harness,
            "miss_causes": causes,
            "first_misses": self.misses.iter().take(10).map(|(g, c)| json!({ "frame": g, "cause": c })).collect::<Vec<_>>(),
            "producer_error": report.producer_error,
            "uploader_error": report.uploader_error,
            "frames_uploaded": report.frames_uploaded,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wl(pattern: &str, w: usize, h: usize, fps: u32, seconds: f64) -> PlanWorkload {
        serde_json::from_value(json!({
            "id": "t", "kind": "synthetic", "pattern": pattern,
            "width": w, "height": h, "fps": fps, "seconds": seconds,
        }))
        .unwrap()
    }

    #[test]
    fn synthetic_schedules_loop_with_a_period_and_scale_with_speed() {
        let w = wl("scroll", 64, 32, 50, 0.1);
        let (_, s, _) = plan(&w, None, 3).unwrap();
        assert_eq!(s.per_loop, 5);
        assert_eq!(s.total(), 15);
        assert_eq!(
            &s.at[..5],
            &[0, 20_000_000, 40_000_000, 60_000_000, 80_000_000]
        );
        // the period is the last time plus the median gap, so the seam is one frame apart
        assert_eq!(s.period_ns, 100_000_000);
        assert_eq!(s.at[5], 100_000_000);
        assert_eq!(s.src[7], 2);
        assert!(s.force_idr.iter().all(|f| !f));
        assert_eq!(s.timeline_ns, 300_000_000);
        assert!((s.fps - 50.0).abs() < 1e-9);
        let mut w2 = w.clone();
        w2.speed = 2.0;
        let (_, s2, _) = plan(&w2, None, 1).unwrap();
        assert_eq!(s2.at[1], 10_000_000);
    }

    #[test]
    fn decimation_and_chained_transforms_size_the_output() {
        let mut w = wl("motion", 64, 32, 100, 0.1);
        w.transform = Some("fps:50,tile2x2,crop:96x48".into());
        let (_, s, ops) = plan(&w, None, 1).unwrap();
        assert_eq!(ops.ops.len(), 2);
        assert_eq!((s.width, s.height), (96, 48));
        // `decimate` also takes the final frame at the first deadline past the end
        assert_eq!(s.src, vec![0, 2, 4, 6, 8, 9]);
        assert!(Ops::parse(Some("fps:0")).is_err());
        assert!(Ops::parse(Some("tile3x3")).is_err());
    }

    #[test]
    fn chained_ops_equal_the_steps_one_by_one() {
        let (w, h) = (32usize, 16usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7) as u8).collect();
        let ops = Ops::parse(Some("tile2x2,crop:48x24,down2x")).unwrap();
        let mut out = Vec::new();
        let mut scratch = Vec::new();
        let dims = ops
            .apply(&src, w, h, w * 4, &mut out, &mut scratch)
            .unwrap();
        let mut a = Vec::new();
        let mut b = Vec::new();
        let d1 = single(BgraOp::Tile2x2)
            .apply(&src, w, h, w * 4, &mut a)
            .unwrap();
        let d2 = single(BgraOp::Crop(48, 24))
            .apply(&a, d1.0, d1.1, d1.0 * 4, &mut b)
            .unwrap();
        let d3 = single(BgraOp::Down2x)
            .apply(&b, d2.0, d2.1, d2.0 * 4, &mut a)
            .unwrap();
        assert_eq!(dims, d3);
        assert_eq!(out, a);
    }

    /// Covers an origin that is not a multiple of the source width.
    #[test]
    fn tile_then_crop_equals_the_two_steps() {
        let (w, h) = (32usize, 16usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 13 + 5) as u8).collect();
        for (cw, ch) in [(50usize, 26usize), (64, 32), (16, 16), (40, 18)] {
            let mut fused = Vec::new();
            let d = tile_then_crop(&src, w, h, w * 4, (cw, ch), &mut fused).unwrap();
            let (mut a, mut b) = (Vec::new(), Vec::new());
            let d1 = single(BgraOp::Tile2x2)
                .apply(&src, w, h, w * 4, &mut a)
                .unwrap();
            let d2 = single(BgraOp::Crop(cw, ch))
                .apply(&a, d1.0, d1.1, d1.0 * 4, &mut b)
                .unwrap();
            assert_eq!(d, d2, "{cw}x{ch}");
            assert_eq!(fused, b, "{cw}x{ch}");
        }
    }

    #[test]
    fn nv12_grey_converts_to_grey() {
        let (w, h) = (4usize, 2usize);
        let mut nv = vec![128u8; w * h * 3 / 2];
        nv[..w * h].fill(200);
        let mut out = vec![0u8; w * h * 4];
        nv12_to_bgra(&nv, w, h, w, &mut out);
        assert!(out.chunks(4).all(|p| p == [200, 200, 200, 255]));
    }

    fn times(base: Instant, cpu: u64, start: u64, done: u64, read: u64, xf: u64) -> Times {
        Times {
            read_ns: read,
            xform_ns: xf,
            decode_ns: 0,
            skipped: 0,
            slot_wait_ns: start.saturating_sub(cpu) * 1000,
            cpu_ready: base + Duration::from_micros(cpu),
            upload_start: base + Duration::from_micros(start),
            upload_done: base + Duration::from_micros(done),
        }
    }

    #[test]
    fn misses_are_named_by_the_stage_that_was_late() {
        let b = Instant::now();
        let dl = b + Duration::from_micros(1000);
        assert_eq!(classify_miss(dl, &times(b, 100, 100, 900, 5, 5)), None);
        assert_eq!(
            classify_miss(dl, &times(b, 1500, 1500, 1700, 9, 5)),
            Some("disk")
        );
        assert_eq!(
            classify_miss(dl, &times(b, 1500, 1500, 1700, 1, 5)),
            Some("transform")
        );
        assert_eq!(
            classify_miss(dl, &times(b, 100, 200, 1300, 1, 5)),
            Some("upload")
        );
        assert_eq!(
            classify_miss(dl, &times(b, 100, 1200, 1300, 1, 5)),
            Some("consumer")
        );
        let mut st = FeedStats::with_capacity(4);
        st.note(0, dl, &times(b, 100, 200, 1300, 1, 5));
        st.note(1, dl, &times(b, 100, 1200, 1300, 1, 5));
        assert_eq!(st.misses.len(), 2);
        assert!(st.lead_ms[0] < 0.0);
    }

    #[test]
    fn the_pacer_reaches_its_deadline_and_reports_lateness() {
        let p = Pacer::new();
        let dl = Instant::now() + Duration::from_millis(5);
        let late = p.wait_until(dl);
        assert!(Instant::now() >= dl);
        // a functional check, not a timing claim: the machine may be loaded
        assert!(late < 50_000_000, "late {late} ns");
        assert!(p.wait_until(Instant::now() - Duration::from_millis(1)) > 0);
    }

    fn write_clip(path: &std::path::Path, w: u32, h: u32, n: usize) -> Vec<Vec<u8>> {
        use super::super::clipfmt::{record_header, FileHeader, FILE_CLOSED};
        use std::io::Write;
        let mut hdr = FileHeader::new(PixelFormat::Bgra, w, h, 60);
        hdr.frame_count = n as u64;
        hdr.flags = FILE_CLOSED;
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&hdr.to_bytes()).unwrap();
        let mut pics = Vec::new();
        for i in 0..n {
            let pic: Vec<u8> = (0..(w * h * 4) as usize)
                .map(|b| (b as u8).wrapping_mul(3) ^ (i as u8).wrapping_mul(29))
                .collect();
            f.write_all(&record_header(
                1_000 + i as u64 * 16_666_667,
                0,
                w,
                h,
                false,
                0,
            ))
            .unwrap();
            f.write_all(&pic).unwrap();
            pics.push(pic);
        }
        pics
    }

    fn read_back(
        dev: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        w: u32,
        h: u32,
    ) -> Vec<u8> {
        let mut r =
            crate::windows_utils::streamer::scaler::TextureReader::new(dev, ctx, w, h).unwrap();
        let (data, pitch) = r.read_back(tex).unwrap();
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h as usize {
            out.extend_from_slice(&data[y * pitch as usize..y * pitch as usize + w as usize * 4]);
        }
        out
    }

    // WARP first; the hardware default device if WARP cannot share keyed-mutex textures
    #[test]
    fn a_clip_frame_arrives_on_the_consumer_device_transformed() {
        use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
        let path = std::env::temp_dir().join(format!(
            "se-bench-feed-{}-{}.seraw",
            std::process::id(),
            crate::streamer::audio::host_now_ns()
        ));
        let (w, h) = (32u32, 16u32);
        let pics = write_clip(&path, w, h, 6);
        let mut wl: PlanWorkload = serde_json::from_value(json!({
            "id": "c", "kind": "recording", "transform": "tile2x2",
        }))
        .unwrap();
        wl.max_frames = Some(6);
        let (source, sched, ops) = plan(&wl, Some(path.to_str().unwrap()), 2).unwrap();
        assert_eq!(sched.total(), 12);
        assert_eq!((sched.width, sched.height), (64, 32));
        let sched = Arc::new(sched);

        let mut ran = None;
        for driver in [D3D_DRIVER_TYPE_WARP, D3D_DRIVER_TYPE_HARDWARE] {
            let mut dev = None;
            let mut ctx = None;
            let ok = unsafe {
                D3D11CreateDevice(
                    None::<&IDXGIAdapter>,
                    driver,
                    windows::Win32::Foundation::HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut dev),
                    None,
                    Some(&mut ctx),
                )
            };
            let (Ok(()), Some(dev), Some(ctx)) = (ok, dev, ctx) else {
                continue;
            };
            let started = start(
                source.clone(),
                Arc::clone(&sched),
                ops.clone(),
                std::slice::from_ref(&dev),
                Input::Bgra,
            );
            let Ok((feeder, sessions)) = started else {
                continue;
            };
            let s = &sessions[0];
            let mut got = Vec::new();
            for _ in 0..sched.total() {
                let r = s.recv().expect("frame");
                let tex = s.acquire(&r).unwrap().clone();
                got.push((r.g, read_back(&dev, &ctx, &tex, 64, 32)));
                s.release(r);
            }
            assert!(s.recv().is_none(), "nothing after the last frame");
            drop(sessions);
            let rep = feeder.finish();
            assert_eq!(rep.frames_uploaded, 12);
            assert!(rep.producer_error.is_none() && rep.uploader_error.is_none());
            ran = Some((driver, got));
            break;
        }
        let _ = std::fs::remove_file(&path);
        let Some((_driver, got)) = ran else {
            crate::test_support::skip_because(
                "feed::a_clip_frame_arrives_on_the_consumer_device_transformed",
                "neither WARP nor a hardware device could share a keyed-mutex texture",
            );
            return;
        };
        for (g, bytes) in got {
            let src = &pics[g % 6];
            let mut want = Vec::new();
            single(BgraOp::Tile2x2)
                .apply(src, w as usize, h as usize, w as usize * 4, &mut want)
                .unwrap();
            assert_eq!(bytes, want, "frame {g}");
        }
    }
}
