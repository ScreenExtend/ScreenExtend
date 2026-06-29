use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use bytes::Bytes;
use tokio::sync::broadcast;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;
use windows::core::Interface;
use windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use windows_capture::frame::Frame;
use windows_capture::graphics_capture_api::InternalCaptureControl;
use windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};

use crate::streamer::config::{Config, H264Profile, ScalePercent};
use super::capture::{MonitorInfo, select_monitor, select_monitor_by_device_name};
use windows_capture::monitor::Monitor;
use super::dxgi::{Duplicator, PollStatus};
use super::intel::encoder::Encoder as IntelEncoder;
use super::nvidia::encoder::{Encoder, EncoderConfig, KEY_ENCODER, KEY_TIMEOUT_MS, KEY_WRITER};
use super::scaler::{Scaler, TextureReader};
use super::tuning;

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
}

const SP_WIDTH: u32 = 1280;
const SP_HEIGHT: u32 = 720;
const SP_FPS: u32 = 30;
const SP_BITRATE_BPS: u32 = 6_000_000;
const BROADCAST_CAPACITY: usize = 2;

#[derive(Clone)]
pub struct Pipeline {
    pub tx: broadcast::Sender<EncodedFrame>,
    pub frame_duration: Duration,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    wake: crossbeam_channel::Sender<()>,
    pub max_bitrate_bps: u32,
    pub h264_profile: H264Profile,
}

impl Pipeline {
    pub fn request_idr(&self) {
        self.idr_request.store(true, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }

    pub fn set_target_bitrate(&self, bps: u32) {
        self.target_bitrate.store(bps, Ordering::Relaxed);
        let _ = self.wake.try_send(());
    }
}

fn apply_pending_bitrate(
    backend: &mut Backend,
    target_bitrate: &AtomicU32,
    current: &mut u32,
) {
    let pending = target_bitrate.swap(0, Ordering::Relaxed);
    if pending == 0 || pending == *current {
        return;
    }
    match backend.set_bitrate(pending) {
        Ok(()) => {
            tprintln!("adapting bitrate: {} -> {pending} bps", *current);
            *current = pending;
        }
        Err(e) => teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; keeping current"),
    }
}

const MAX_TRANSIENT_ENCODE_DROPS: u32 = 60;

fn is_transient_encode_error(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("device busy")
}

pub(crate) fn live_encoder_config(
    native_w: u32,
    native_h: u32,
    refresh_hz: u32,
    cfg: &Config,
) -> EncoderConfig {
    let fps = if let Some(f) = cfg.fps {
        f.clamp(15, 500)
    } else {
        let refresh = if refresh_hz == 0 { 60 } else { refresh_hz };
        let max_fps = cfg.max_fps.clamp(15, 500);
        refresh.clamp(60.min(max_fps), max_fps)
    };

    let (width, height) = scaled_dims(native_w, native_h, cfg.scale);

    let computed =
        ((width as u64 * height as u64 * fps as u64) / 10).clamp(6_000_000, 30_000_000) as u32;
    let bitrate = cfg
        .max_bitrate_kbps
        .map(|kbps| kbps.saturating_mul(1000))
        .unwrap_or(computed);

    EncoderConfig {
        width,
        height,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: cfg.h264_profile,
        qp: cfg.qp,
        intra_refresh: cfg.intra_refresh,
    }
}

pub(crate) fn scaled_dims(native_w: u32, native_h: u32, scale: ScalePercent) -> (u32, u32) {
    if scale.is_native() || native_w == 0 || native_h == 0 {
        return (native_w & !1, native_h & !1);
    }
    let w = scale.apply(native_w).max(2) & !1;
    let h = scale.apply(native_h).max(2) & !1;
    (w, h)
}

pub fn start(cfg: &Config) -> Result<Pipeline> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr_request = Arc::new(AtomicBool::new(false));
    let target_bitrate = Arc::new(AtomicU32::new(0));

    if cfg.synthetic_pattern {
        let frame_duration = Duration::from_nanos(1_000_000_000 / SP_FPS as u64);
        let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
        let pipeline = Pipeline {
            tx: tx.clone(),
            frame_duration,
            idr_request: Arc::clone(&idr_request),
            target_bitrate: Arc::clone(&target_bitrate),
            wake,
            max_bitrate_bps: SP_BITRATE_BPS,
            h264_profile: cfg.h264_profile,
        };
        let enc = EncoderConfig {
            width: SP_WIDTH,
            height: SP_HEIGHT,
            fps: SP_FPS,
            bitrate_bps: SP_BITRATE_BPS,
            max_bitrate_bps: SP_BITRATE_BPS,
            profile: cfg.h264_profile,
            qp: cfg.qp,
            intra_refresh: cfg.intra_refresh,
        };
        std::thread::Builder::new()
            .name("nvenc-encode".to_string())
            .spawn(move || synthetic_pattern_loop(tx, idr_request, target_bitrate, enc, wake_rx))
            .expect("spawn encode thread");
        return Ok(pipeline);
    }

    super::capture::check_dwm_composition()?;

    let (monitor, info) = select_monitor(cfg.monitor)?;
    match start_live_capture(
        cfg,
        monitor,
        &info,
        tx.clone(),
        Arc::clone(&idr_request),
        Arc::clone(&target_bitrate),
    ) {
        Ok((pipeline, control)) => {
            std::mem::forget(control);
            Ok(pipeline)
        }
        Err(wgc_err) => {
            let device_name = match monitor.device_name() {
                Ok(name) => name,
                Err(e) => {
                    return Err(wgc_err
                        .context(format!("monitor device name for DXGI fallback: {e}")));
                }
            };
            teprintln!(
                "WGC capture failed for display {}: {wgc_err:#}; \
                 falling back to DXGI Desktop Duplication on {device_name}",
                info.index
            );
            let (pipeline, control) =
                start_dxgi_capture(cfg, &device_name, &info, tx, idr_request, target_bitrate)
                    .map_err(|e| e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})")))?;
            drop(control);
            Ok(pipeline)
        }
    }
}

pub struct DxgiControl {
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DxgiControl {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

enum SessionControl {
    Wgc(CaptureControl<LiveCapture, anyhow::Error>),
    Dxgi(DxgiControl),
}

pub struct SessionCapture {
    pub pipeline: Pipeline,
    control: Option<SessionControl>,
}

impl SessionCapture {
    pub fn stop(mut self) {
        match self.control.take() {
            Some(SessionControl::Wgc(control)) => {
                if let Err(e) = control.stop() {
                    teprintln!("pipeline: stopping session capture failed: {e:?}");
                }
            }
            Some(SessionControl::Dxgi(control)) => control.stop(),
            None => {}
        }
    }
}

pub fn start_on_monitor(cfg: &Config, device_name: &str) -> Result<SessionCapture> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr_request = Arc::new(AtomicBool::new(false));
    let target_bitrate = Arc::new(AtomicU32::new(0));

    super::capture::check_dwm_composition()?;

    let (monitor, info) = select_monitor_by_device_name(device_name)?;

    match start_live_capture(
        cfg,
        monitor,
        &info,
        tx.clone(),
        Arc::clone(&idr_request),
        Arc::clone(&target_bitrate),
    ) {
        Ok((pipeline, control)) => {
            Ok(SessionCapture { pipeline, control: Some(SessionControl::Wgc(control)) })
        }
        Err(wgc_err) => {
            teprintln!(
                "WGC capture failed for {device_name}: {wgc_err:#}; \
                 falling back to DXGI Desktop Duplication"
            );
            let (pipeline, control) =
                start_dxgi_capture(cfg, device_name, &info, tx, idr_request, target_bitrate)
                    .map_err(|e| e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})")))?;
            Ok(SessionCapture { pipeline, control: Some(SessionControl::Dxgi(control)) })
        }
    }
}

fn start_live_capture(
    cfg: &Config,
    monitor: Monitor,
    info: &MonitorInfo,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
) -> Result<(Pipeline, CaptureControl<LiveCapture, anyhow::Error>)> {
    let config = live_encoder_config(info.width, info.height, info.refresh_hz, cfg);
    let downscale = config.width != info.width || config.height != info.height;
    let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps as u64);

    let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: config.max_bitrate_bps,
        h264_profile: cfg.h264_profile,
    };

    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(frame_duration),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        CaptureFlags {
            config,
            vendor: cfg.encoder_vendor,
            native_w: info.width,
            native_h: info.height,
            tx,
            idr_request,
            target_bitrate,
            wake_rx,
        },
    );

    tprintln!(
        "pipeline: starting live monitor capture (WGC -> NVENC; zero-copy if available): \
         display={}, name={}, gpu={}, native={}x{}, encode={}x{}, downscale={}, fps={}, bitrate_bps={}",
        info.index,
        info.name,
        info.gpu,
        info.width,
        info.height,
        config.width,
        config.height,
        downscale,
        config.fps,
        config.bitrate_bps,
    );

    let control =
        LiveCapture::start_free_threaded(settings).map_err(|e| anyhow!("starting WGC capture: {e}"))?;

    Ok((pipeline, control))
}

fn start_dxgi_capture(
    cfg: &Config,
    device_name: &str,
    info: &MonitorInfo,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
) -> Result<(Pipeline, DxgiControl)> {
    let config = live_encoder_config(info.width, info.height, info.refresh_hz, cfg);
    let downscale = config.width != info.width || config.height != info.height;
    let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);

    let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: config.max_bitrate_bps,
        h264_profile: cfg.h264_profile,
    };

    tprintln!(
        "pipeline: starting DXGI duplication capture: device={}, native={}x{}, encode={}x{}, \
         downscale={}, fps={}, bitrate_bps={}",
        device_name, info.width, info.height, config.width, config.height, downscale,
        config.fps, config.bitrate_bps,
    );

    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

    let thread_args = DxgiThreadArgs {
        device_name: device_name.to_string(),
        config,
        vendor: cfg.encoder_vendor,
        native_w: info.width,
        native_h: info.height,
        frame_duration,
        tx,
        idr_request,
        target_bitrate,
        wake_rx,
        stop: Arc::clone(&stop),
    };
    let join = std::thread::Builder::new()
        .name("dxgi-capture".to_string())
        .spawn(move || dxgi_capture_thread(thread_args, ready_tx))
        .context("spawning dxgi capture thread")?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok((pipeline, DxgiControl { stop, join: Some(join) })),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err(anyhow!("dxgi capture thread exited during setup"))
        }
    }
}

struct DxgiThreadArgs {
    device_name: String,
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    frame_duration: Duration,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    wake_rx: crossbeam_channel::Receiver<()>,
    stop: Arc<AtomicBool>,
}

fn dxgi_capture_thread(args: DxgiThreadArgs, ready_tx: std::sync::mpsc::Sender<Result<()>>) {
    let DxgiThreadArgs {
        device_name,
        config,
        vendor,
        native_w,
        native_h,
        frame_duration,
        tx,
        idr_request,
        target_bitrate,
        wake_rx,
        stop,
    } = args;

    let _thread_tuning = tuning::tune_current_thread();
    let _keep_awake = tuning::KeepAwake::begin();

    let setup = (|| -> Result<(Duplicator, Arc<Mutex<EncodeCore>>, &'static str)> {
        let dup = Duplicator::new(&device_name, native_w, native_h)?;
        let backend =
            build_backend(config, vendor, native_w, native_h, dup.device(), dup.context())?;

        let needs_scaler =
            backend.wants_prescale() && (config.width != native_w || config.height != native_h);
        let scaler = if needs_scaler {
            Some(
                Scaler::new(dup.device(), dup.context(), native_w, native_h, config.width, config.height)
                    .context("building GPU downscaler for --scale")?,
            )
        } else {
            None
        };
        let reader = if backend.is_cpu_bridge() && scaler.is_none() {
            Some(TextureReader::new(dup.device(), dup.context(), native_w, native_h)?)
        } else {
            None
        };

        tuning::raise_d3d11_gpu_priority(backend.device());
        tuning::raise_d3d11_gpu_priority(dup.device());
        let path_name = backend.name();

        let core = Arc::new(Mutex::new(EncodeCore {
            backend,
            scaler,
            reader,
            target_bitrate,
            idr_request: Arc::clone(&idr_request),
            current_bitrate: config.bitrate_bps,
            have_frame: false,
            frame_index: 0,
        }));
        Ok((dup, core, path_name))
    })();

    let (mut dup, core, path_name) = match setup {
        Ok(state) => state,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let epoch = Instant::now();
    let last_frame_at = Arc::new(AtomicU64::new(0));
    spawn_repeater(
        Arc::clone(&core),
        tx.clone(),
        idr_request,
        frame_duration,
        Arc::clone(&last_frame_at),
        epoch,
        Arc::clone(&stop),
        wake_rx,
    );

    tprintln!("pipeline: live capture ready -- DXGI duplication -> {path_name}");
    let _ = ready_tx.send(Ok(()));

    let mut dirty = false;
    let mut next_due = Instant::now();
    let mut busy_streak: u32 = 0;
    let mut frames_sent: u64 = 0;
    let mut timing_sum_ns: u128 = 0;
    let mut timing_count: u64 = 0;
    let mut timing_max_ns: u128 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let timeout_ms = if dirty {
            let rem = next_due.saturating_duration_since(Instant::now());
            (rem.as_micros() / 1000) as u32
        } else {
            50
        };
        match dup.poll(timeout_ms) {
            Ok(PollStatus::Dirty) => dirty = true,
            Ok(PollStatus::Timeout) => {}
            Err(e) => {
                teprintln!("dxgi capture: {e:?}; stopping capture");
                break;
            }
        }

        if !dirty {
            continue;
        }
        let now = Instant::now();
        if now < next_due {
            continue;
        }

        let capture = now;
        let encode_res = {
            let tex = match dup.frame() {
                Ok(t) => t,
                Err(e) => {
                    teprintln!("dxgi compose failed: {e:?}; stopping capture");
                    break;
                }
            };
            let mut core = match core.lock() {
                Ok(g) => g,
                Err(_) => break,
            };
            core.encode_texture(tex)
        };
        let au = match encode_res {
            Ok(au) => au,
            Err(e) if is_transient_encode_error(&e) && busy_streak < MAX_TRANSIENT_ENCODE_DROPS => {
                busy_streak += 1;
                teprintln!(
                    "dxgi encode transiently overloaded ({e:#}); dropping frame ({busy_streak} in a row)"
                );
                next_due = Instant::now() + frame_duration;
                continue;
            }
            Err(e) => {
                teprintln!("dxgi encode failed: {e:?}; stopping capture");
                break;
            }
        };
        busy_streak = 0;

        last_frame_at.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        let _ = tx.send(EncodedFrame { data: Bytes::from(au), capture });
        dirty = false;
        next_due = capture + frame_duration;

        frames_sent += 1;
        let dt = capture.elapsed().as_nanos();
        timing_sum_ns += dt;
        timing_count += 1;
        timing_max_ns = timing_max_ns.max(dt);
        if frames_sent % 60 == 0 {
            let avg_ms = (timing_sum_ns / timing_count.max(1) as u128) as f64 / 1.0e6;
            let max_ms = timing_max_ns as f64 / 1.0e6;
            tprintln!(
                "encode-path latency: path=dxgi+{}, avg_ms={:.2}, max_ms={:.2}, frames={}",
                path_name, avg_ms, max_ms, frames_sent
            );
            timing_sum_ns = 0;
            timing_count = 0;
            timing_max_ns = 0;
        }
    }

    stop.store(true, Ordering::Relaxed);
    tprintln!("dxgi capture stopped");
}

pub fn probe_bitrate(cfg: &Config) -> Result<()> {
    let mut cfg = cfg.clone();
    cfg.synthetic_pattern = true;
    let pipeline = start(&cfg)?;
    let mut rx = pipeline.tx.subscribe();

    let _ = rx.blocking_recv();

    let targets = [4_000_000u32, 2_000_000, 1_000_000, 3_000_000, 6_000_000];
    for (i, &t) in targets.iter().enumerate() {
        tprintln!("probe-bitrate: injecting synthetic target (step={i}, target_bps={t})");
        pipeline.set_target_bitrate(t);
        for _ in 0..10 {
            let _ = rx.blocking_recv();
        }
    }

    tprintln!("probe-bitrate complete: cross-thread bitrate update exercised");
    Ok(())
}

pub fn probe_live(cfg: &Config, path: &str) -> Result<()> {
    use std::io::Write;

    const FRAMES: u64 = 150;

    let mut cfg = cfg.clone();
    cfg.synthetic_pattern = false;
    let pipeline = start(&cfg)?;
    let mut rx = pipeline.tx.subscribe();
    pipeline.request_idr();

    let mut file = std::fs::File::create(path)?;
    let mut written = 0u64;
    let mut total = 0usize;
    let mut started = false;

    while written < FRAMES {
        match rx.blocking_recv() {
            Ok(frame) => {
                let au = frame.data;
                if !started {
                    if is_keyframe(&au) {
                        started = true;
                    } else {
                        continue;
                    }
                }
                file.write_all(&au)?;
                total += au.len();
                written += 1;
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => {
                bail!("capture stopped after {written} frames (encoder error?)")
            }
        }
    }

    tprintln!("probe-live complete: path={path}, frames={written}, total_bytes={total}");
    Ok(())
}

fn is_keyframe(au: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 < au.len() {
        let nal_type = if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            i += 3;
            au[i] & 0x1f
        } else if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1 {
            i += 4;
            au[i] & 0x1f
        } else {
            i += 1;
            continue;
        };
        if nal_type == 5 || nal_type == 7 {
            return true;
        }
        i += 1;
    }
    false
}

struct CaptureFlags {
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    wake_rx: crossbeam_channel::Receiver<()>,
}

enum EncodePath {
    ZeroCopy {
        igpu_context: ID3D11DeviceContext,
        shared_igpu: ID3D11Texture2D,
        igpu_mutex: IDXGIKeyedMutex,
    },
    CpuBridge,
}

impl EncodePath {
    fn name(&self) -> &'static str {
        match self {
            EncodePath::ZeroCopy { .. } => "zero-copy",
            EncodePath::CpuBridge => "cpu-bridge",
        }
    }
}

enum Backend {
    Nvenc { encoder: Encoder, path: EncodePath },
    Intel { encoder: IntelEncoder },
    /// Intel Quick Sync on its own device, fed via CPU readback — last-resort bridge for when
    /// the capture device is neither Intel (no same-adapter path) nor NVENC-reachable.
    IntelCpu { encoder: IntelEncoder },
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Nvenc { path, .. } => path.name(),
            Backend::Intel { .. } => "intel-same-adapter",
            Backend::IntelCpu { .. } => "intel-cpu-bridge",
        }
    }

    fn device(&self) -> &ID3D11Device {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.device(),
            Backend::Intel { encoder } => encoder.device(),
            Backend::IntelCpu { encoder } => encoder.device(),
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.set_bitrate(bps),
            Backend::Intel { encoder } => encoder.set_bitrate(bps),
            Backend::IntelCpu { encoder } => encoder.set_bitrate(bps),
        }
    }

    /// Whether captured frames must be downscaled to encode size before being handed over.
    /// The Intel same-adapter path fuses the downscale into its VPP pass and wants the
    /// native-size texture; every other path expects encode-size input.
    fn wants_prescale(&self) -> bool {
        !matches!(self, Backend::Intel { .. })
    }

    /// Whether frames reach the encoder as CPU bytes (`encode_cpu`) instead of textures.
    fn is_cpu_bridge(&self) -> bool {
        matches!(self, Backend::Nvenc { path: EncodePath::CpuBridge, .. } | Backend::IntelCpu { .. })
    }

    fn encode_gpu(&mut self, src_texture: &ID3D11Texture2D, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Intel { encoder } => encoder.encode_texture(src_texture, force_idr),
            Backend::Nvenc {
                encoder,
                path: EncodePath::ZeroCopy { igpu_context, shared_igpu, igpu_mutex },
            } => {
                unsafe {
                    igpu_mutex
                        .AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS)
                        .context("iGPU keyed mutex AcquireSync(writer)")?;
                    igpu_context.CopyResource(&*shared_igpu, src_texture);
                    igpu_context.Flush();
                    igpu_mutex
                        .ReleaseSync(KEY_ENCODER)
                        .context("iGPU keyed mutex ReleaseSync(encoder)")?;
                }
                encoder.encode_input(force_idr)
            }
            _ => bail!("encode_gpu called on a CPU-bridge backend"),
        }
    }

    fn encode_cpu(&mut self, data: &[u8], row_pitch: u32, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Nvenc { encoder, path: EncodePath::CpuBridge } => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            Backend::IntelCpu { encoder } => encoder.encode_bgra_padded(data, row_pitch, force_idr),
            _ => bail!("encode_cpu called on a GPU-path backend"),
        }
    }

    fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.encode_repeat(force_idr),
            Backend::Intel { encoder } => encoder.encode_repeat(force_idr),
            Backend::IntelCpu { encoder } => encoder.encode_repeat(force_idr),
        }
    }

}

struct EncodeCore {
    backend: Backend,
    scaler: Option<Scaler>,
    /// CPU readback for texture-fed (DXGI duplication) captures on a CPU-bridge backend
    /// without a scaler; `None` on the WGC path (it reads back via `Frame::buffer`).
    reader: Option<TextureReader>,
    target_bitrate: Arc<AtomicU32>,
    idr_request: Arc<AtomicBool>,
    current_bitrate: u32,
    have_frame: bool,
    frame_index: u64,
}

impl EncodeCore {
    fn take_force_idr(&mut self) -> bool {
        self.frame_index == 0 || self.idr_request.swap(false, Ordering::Relaxed)
    }

    fn encode_captured(&mut self, frame: &mut Frame) -> Result<Vec<u8>> {
        let force_idr = self.take_force_idr();
        apply_pending_bitrate(&mut self.backend, &self.target_bitrate, &mut self.current_bitrate);
        let Self { backend, scaler, .. } = self;

        let au = if backend.is_cpu_bridge() {
            if let Some(scaler) = scaler {
                scaler.scale(frame.as_raw_texture())?;
                let (data, row_pitch) = scaler.read_back()?;
                backend.encode_cpu(data, row_pitch, force_idr)?
            } else {
                let mut fb = frame.buffer()?;
                let row_pitch = fb.row_pitch();
                backend.encode_cpu(fb.as_raw_buffer(), row_pitch, force_idr)?
            }
        } else {
            let src_texture: ID3D11Texture2D = match scaler {
                Some(s) => s.scale(frame.as_raw_texture())?.clone(),
                None => frame.as_raw_texture().clone(),
            };
            backend.encode_gpu(&src_texture, force_idr)?
        };

        self.have_frame = true;
        self.frame_index += 1;
        Ok(au)
    }

    /// Encode a raw capture-device texture (DXGI duplication path; no `windows_capture::Frame`).
    fn encode_texture(&mut self, tex: &ID3D11Texture2D) -> Result<Vec<u8>> {
        let force_idr = self.take_force_idr();
        apply_pending_bitrate(&mut self.backend, &self.target_bitrate, &mut self.current_bitrate);
        let Self { backend, scaler, reader, .. } = self;

        let au = if backend.is_cpu_bridge() {
            let (data, row_pitch) = if let Some(scaler) = scaler {
                scaler.scale(tex)?;
                scaler.read_back()?
            } else {
                let reader = reader
                    .as_mut()
                    .ok_or_else(|| anyhow!("cpu-bridge texture encode without a TextureReader"))?;
                reader.read_back(tex)?
            };
            backend.encode_cpu(data, row_pitch, force_idr)?
        } else {
            let src_texture: ID3D11Texture2D = match scaler {
                Some(s) => s.scale(tex)?.clone(),
                None => tex.clone(),
            };
            backend.encode_gpu(&src_texture, force_idr)?
        };

        self.have_frame = true;
        self.frame_index += 1;
        Ok(au)
    }

    fn encode_repeat(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.have_frame {
            return Ok(None);
        }
        let force_idr = self.take_force_idr();
        apply_pending_bitrate(&mut self.backend, &self.target_bitrate, &mut self.current_bitrate);
        let au = self.backend.encode_repeat(force_idr)?;
        self.frame_index += 1;
        Ok(Some(au))
    }
}

struct LiveCapture {
    core: Arc<Mutex<EncodeCore>>,
    tx: broadcast::Sender<EncodedFrame>,
    epoch: Instant,
    last_frame_at: Arc<AtomicU64>,
    path_name: &'static str,
    frames_sent: u64,
    busy_streak: u32,
    stop: Arc<AtomicBool>,
    _thread_tuning: tuning::ThreadTuning,
    _keep_awake: tuning::KeepAwake,
    timing_sum_ns: u128,
    timing_count: u64,
    timing_max_ns: u128,
}

impl Drop for LiveCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn build_zero_copy(
    config: EncoderConfig,
    igpu_device: &ID3D11Device,
    igpu_context: &ID3D11DeviceContext,
) -> Result<(Encoder, EncodePath)> {
    let encoder = Encoder::new_shared(config)?;
    let handle = encoder
        .shared_handle()
        .ok_or_else(|| anyhow!("shared encoder produced no handle"))?;
    let device1: ID3D11Device1 = igpu_device.cast().context("iGPU device as ID3D11Device1")?;
    let shared_igpu: ID3D11Texture2D =
        unsafe { device1.OpenSharedResource1(handle) }.context("OpenSharedResource1 on iGPU")?;
    let igpu_mutex: IDXGIKeyedMutex = shared_igpu
        .cast()
        .context("opened shared texture as IDXGIKeyedMutex")?;
    Ok((
        encoder,
        EncodePath::ZeroCopy { igpu_context: igpu_context.clone(), shared_igpu, igpu_mutex },
    ))
}

fn build_backend(
    config: EncoderConfig,
    vendor: crate::streamer::config::EncoderVendor,
    native_w: u32,
    native_h: u32,
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
) -> Result<Backend> {
    use crate::streamer::config::EncoderVendor;

    let try_nvenc = || -> Result<Backend> {
        match build_zero_copy(config, device, context) {
            Ok((encoder, path)) => {
                tprintln!(
                    "pipeline: live capture ready -- NVENC ZERO-COPY cross-adapter GPU path ({}x{}@{})",
                    config.width, config.height, config.fps
                );
                Ok(Backend::Nvenc { encoder, path })
            }
            Err(e) => {
                teprintln!(
                    "NVENC zero-copy path unavailable ({e:?}); falling back to CPU bridge (higher latency)"
                );
                match Encoder::new(config) {
                    Ok(encoder) => {
                        tprintln!(
                            "pipeline: live capture ready -- NVENC CPU-bridge fallback ({}x{}@{})",
                            config.width, config.height, config.fps
                        );
                        Ok(Backend::Nvenc { encoder, path: EncodePath::CpuBridge })
                    }
                    Err(nv_err) => Err(nv_err),
                }
            }
        }
    };

    let try_intel = || -> Result<Backend> {
        match IntelEncoder::new_on_device(config, native_w, native_h, device, context) {
            Ok(encoder) => {
                tprintln!(
                    "pipeline: live capture ready -- INTEL Quick Sync same-adapter path ({}x{}@{})",
                    config.width, config.height, config.fps
                );
                Ok(Backend::Intel { encoder })
            }
            Err(e) => {
                teprintln!(
                    "Intel Quick Sync same-adapter path unavailable ({e:?}); trying own-device CPU bridge"
                );
                let encoder = IntelEncoder::new(config)
                    .context("Intel Quick Sync unavailable (same-adapter and CPU-bridge both failed)")?;
                tprintln!(
                    "pipeline: live capture ready -- INTEL Quick Sync CPU-bridge path ({}x{}@{})",
                    config.width, config.height, config.fps
                );
                Ok(Backend::IntelCpu { encoder })
            }
        }
    };

    match vendor {
        EncoderVendor::Nvidia => try_nvenc(),
        EncoderVendor::Intel => try_intel(),
        EncoderVendor::Auto => match try_nvenc() {
            Ok(backend) => Ok(backend),
            Err(nv_err) => {
                teprintln!("NVENC unavailable ({nv_err:?}); trying Intel Quick Sync");
                try_intel().map_err(|ie| {
                    nv_err.context(format!("no working encoder (Intel fallback also failed: {ie:?})"))
                })
            }
        },
    }
}

fn spawn_repeater(
    core: Arc<Mutex<EncodeCore>>,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    frame_duration: Duration,
    last_frame_at: Arc<AtomicU64>,
    epoch: Instant,
    stop: Arc<AtomicBool>,
    wake_rx: crossbeam_channel::Receiver<()>,
) {
    let tick = frame_duration.max(Duration::from_millis(8));
    let idle_after_ms = (frame_duration.as_millis() as u64 * 2).max(34);
    let keepalive_ms = 200u64;
    std::thread::Builder::new()
        .name("nvenc-repeat".to_string())
        .spawn(move || {
            let _thread_tuning = tuning::tune_current_thread();
            let mut last_emit = Instant::now();
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                match wake_rx.recv_timeout(tick) {
                    Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        std::thread::sleep(tick);
                    }
                }
                while wake_rx.try_recv().is_ok() {}
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let now_ms = epoch.elapsed().as_millis() as u64;
                let last = last_frame_at.load(Ordering::Relaxed);
                if last != 0 && now_ms.saturating_sub(last) < idle_after_ms {
                    continue;
                }
                let idr_pending = idr_request.load(Ordering::Relaxed);
                let keepalive_due = last_emit.elapsed().as_millis() as u64 >= keepalive_ms;
                if !idr_pending && !keepalive_due {
                    continue;
                }
                let capture = Instant::now();
                let au = {
                    let mut core = match core.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    match core.encode_repeat() {
                        Ok(Some(au)) => au,
                        Ok(None) => continue,
                        Err(e) => {
                            teprintln!("idle repeat encode failed: {e:?}");
                            continue;
                        }
                    }
                };
                let _ = tx.send(EncodedFrame { data: Bytes::from(au), capture });
                last_emit = Instant::now();
            }
            tprintln!("idle repeater stopped");
        })
        .expect("spawn repeater thread");
}

impl GraphicsCaptureApiHandler for LiveCapture {
    type Flags = CaptureFlags;
    type Error = anyhow::Error;

    fn new(ctx: Context<Self::Flags>) -> Result<Self, Self::Error> {
        let CaptureFlags {
            config,
            vendor,
            native_w,
            native_h,
            tx,
            idr_request,
            target_bitrate,
            wake_rx,
        } = ctx.flags;

        let thread_tuning = tuning::tune_current_thread();
        let keep_awake = tuning::KeepAwake::begin();

        let backend = build_backend(
            config,
            vendor,
            native_w,
            native_h,
            &ctx.device,
            &ctx.device_context,
        )?;

        let needs_scaler =
            backend.wants_prescale() && (config.width != native_w || config.height != native_h);
        let scaler = if needs_scaler {
            match Scaler::new(
                &ctx.device,
                &ctx.device_context,
                native_w,
                native_h,
                config.width,
                config.height,
            ) {
                Ok(s) => Some(s),
                Err(e) => {
                    return Err(e.context("building GPU downscaler for --scale"));
                }
            }
        } else {
            None
        };

        tuning::raise_d3d11_gpu_priority(backend.device());

        let path_name = backend.name();
        let core = Arc::new(Mutex::new(EncodeCore {
            backend,
            scaler,
            reader: None,
            target_bitrate: Arc::clone(&target_bitrate),
            idr_request: Arc::clone(&idr_request),
            current_bitrate: config.bitrate_bps,
            have_frame: false,
            frame_index: 0,
        }));

        let epoch = Instant::now();
        let last_frame_at = Arc::new(AtomicU64::new(0));
        let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);
        let stop = Arc::new(AtomicBool::new(false));

        spawn_repeater(
            Arc::clone(&core),
            tx.clone(),
            idr_request,
            frame_duration,
            Arc::clone(&last_frame_at),
            epoch,
            Arc::clone(&stop),
            wake_rx,
        );

        Ok(Self {
            core,
            tx,
            epoch,
            last_frame_at,
            path_name,
            frames_sent: 0,
            busy_streak: 0,
            stop,
            _thread_tuning: thread_tuning,
            _keep_awake: keep_awake,
            timing_sum_ns: 0,
            timing_count: 0,
            timing_max_ns: 0,
        })
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let capture = Instant::now();
        let t0 = capture;

        let encode_res = {
            let mut core = self.core.lock().expect("encode core mutex poisoned");
            core.encode_captured(frame)
        };
        let au = match encode_res {
            Ok(au) => au,
            Err(e)
                if is_transient_encode_error(&e)
                    && self.busy_streak < MAX_TRANSIENT_ENCODE_DROPS =>
            {
                self.busy_streak += 1;
                teprintln!(
                    "encode transiently overloaded ({e:#}); dropping frame ({} in a row)",
                    self.busy_streak
                );
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        self.busy_streak = 0;

        self.last_frame_at
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        let _ = self.tx.send(EncodedFrame { data: Bytes::from(au), capture });
        self.frames_sent += 1;

        let dt = t0.elapsed().as_nanos();
        self.timing_sum_ns += dt;
        self.timing_count += 1;
        self.timing_max_ns = self.timing_max_ns.max(dt);
        if self.frames_sent % 60 == 0 {
            let avg_ms = (self.timing_sum_ns / self.timing_count.max(1) as u128) as f64 / 1.0e6;
            let max_ms = self.timing_max_ns as f64 / 1.0e6;
            tprintln!(
                "encode-path latency: path={}, avg_ms={:.2}, max_ms={:.2}, frames={}",
                self.path_name, avg_ms, max_ms, self.frames_sent
            );
            self.timing_sum_ns = 0;
            self.timing_count = 0;
            self.timing_max_ns = 0;
        }
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        teprintln!("pipeline: capture item closed (display disconnected?)");
        Ok(())
    }
}

fn synthetic_pattern_loop(
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    config: EncoderConfig,
    wake_rx: crossbeam_channel::Receiver<()>,
) {
    let _thread_tuning = tuning::tune_current_thread();

    let mut encoder = match Encoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            teprintln!("encode thread: failed to create NVENC encoder ({e:?}); pipeline stopped");
            return;
        }
    };
    tuning::raise_d3d11_gpu_priority(encoder.device());
    let mut current_bitrate = config.bitrate_bps;

    let frame_interval = Duration::from_nanos(1_000_000_000 / SP_FPS as u64);
    let mut frame_buf = vec![0u8; (SP_WIDTH * SP_HEIGHT * 4) as usize];
    let mut frame_index: u32 = 0;
    let mut next_deadline = Instant::now();

    tprintln!(
        "pipeline: synthetic pattern encode loop started ({SP_WIDTH}x{SP_HEIGHT}@{SP_FPS})"
    );

    loop {
        let pending = target_bitrate.swap(0, Ordering::Relaxed);
        if pending != 0 && pending != current_bitrate {
            match encoder.set_bitrate(pending) {
                Ok(()) => {
                    tprintln!("adapting bitrate: {current_bitrate} -> {pending} bps");
                    current_bitrate = pending;
                }
                Err(e) => teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; keeping current"),
            }
        }

        let force_idr = frame_index == 0 || idr_request.swap(false, Ordering::Relaxed);
        fill_synthetic_pattern(&mut frame_buf, SP_WIDTH, SP_HEIGHT, frame_index);

        match encoder.encode_bgra(&frame_buf, force_idr) {
            Ok(au) => {
                let _ = tx.send(EncodedFrame { data: Bytes::from(au), capture: Instant::now() });
            }
            Err(e) => {
                teprintln!("encode failed (frame={frame_index}): {e:?}; pipeline stopped");
                return;
            }
        }

        frame_index = frame_index.wrapping_add(1);

        next_deadline += frame_interval;
        let now = Instant::now();
        if next_deadline > now {
            match wake_rx.recv_timeout(next_deadline - now) {
                Ok(()) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(next_deadline - now);
                }
            }
            while wake_rx.try_recv().is_ok() {}
        } else {
            next_deadline = now;
        }
    }
}

fn fill_synthetic_pattern(buf: &mut [u8], width: u32, height: u32, frame: u32) {
    let w = width as usize;
    let h = height as usize;
    let f = frame as usize;

    let box_w = w / 6;
    let box_h = h / 6;
    let span_x = w.saturating_sub(box_w).max(1);
    let span_y = h.saturating_sub(box_h).max(1);
    let box_x = (f * 11) % span_x;
    let box_y = (f * 7) % span_y;

    let bar_w = (w / 60).max(2);
    let bar_x = (f * (w / 90).max(1)) % w;
    let bar_h = h / 12;

    for y in 0..h {
        let row = y * w * 4;
        for x in 0..w {
            let o = row + x * 4;
            let b = ((x + f * 3) & 0xff) as u8;
            let g = ((y + f * 5) & 0xff) as u8;
            let r = ((x + y + f * 2) & 0xff) as u8;

            let in_box = x >= box_x && x < box_x + box_w && y >= box_y && y < box_y + box_h;
            let in_bar = y < bar_h && x >= bar_x && x < bar_x + bar_w;

            if in_bar {
                buf[o] = 255;
                buf[o + 1] = 255;
                buf[o + 2] = 255;
            } else if in_box {
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
