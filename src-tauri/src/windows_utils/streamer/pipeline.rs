use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use crate::windows_capture::frame::Frame;
use crate::windows_capture::graphics_capture_api::InternalCaptureControl;
use crate::windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use anyhow::{anyhow, bail, Context as _, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

use super::capture::{select_monitor, select_monitor_by_device_name, MonitorInfo};
use super::dxgi::{Duplicator, PollStatus};
use super::intel::encoder::Encoder as IntelEncoder;
use super::nvidia::encoder::{
    Encoder, EncoderConfig, KEY_ENCODER, KEY_TIMEOUT_MS, KEY_WRITER, SlotEncodeError,
};
use super::scaler::{Scaler, TextureReader};
use super::tuning;
use super::x264::encoder::X264Encoder;
use crate::streamer::config::{Config, H264Profile, ScalePercent};
use crate::windows_capture::monitor::Monitor;

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
}

const SP_WIDTH: u32 = 1280;
const SP_HEIGHT: u32 = 720;
const SP_FPS: u32 = 30;
const SP_BITRATE_BPS: u32 = 6_000_000;
const BROADCAST_CAPACITY: usize = 4;

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

fn apply_pending_bitrate(backend: &mut Backend, target_bitrate: &AtomicU32, current: &mut u32) {
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

fn effective_vendor(cfg: &Config) -> crate::streamer::config::EncoderVendor {
    if cfg.disable_gpu_encode {
        crate::streamer::config::EncoderVendor::Software
    } else {
        cfg.encoder_vendor
    }
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
                    return Err(
                        wgc_err.context(format!("monitor device name for DXGI fallback: {e}"))
                    );
                }
            };
            teprintln!(
                "WGC capture failed for display {}: {wgc_err:#}; \
                 falling back to DXGI Desktop Duplication on {device_name}",
                info.index
            );
            let (pipeline, control) =
                start_dxgi_capture(cfg, &device_name, &info, tx, idr_request, target_bitrate)
                    .map_err(|e| {
                        e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})"))
                    })?;
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
        Ok((pipeline, control)) => Ok(SessionCapture {
            pipeline,
            control: Some(SessionControl::Wgc(control)),
        }),
        Err(wgc_err) => {
            teprintln!(
                "WGC capture failed for {device_name}: {wgc_err:#}; \
                 falling back to DXGI Desktop Duplication"
            );
            let (pipeline, control) =
                start_dxgi_capture(cfg, device_name, &info, tx, idr_request, target_bitrate)
                    .map_err(|e| {
                        e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})"))
                    })?;
            Ok(SessionCapture {
                pipeline,
                control: Some(SessionControl::Dxgi(control)),
            })
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
        MinimumUpdateIntervalSettings::Custom(frame_duration * 2 / 3),
        DirtyRegionSettings::Default,
        ColorFormat::Bgra8,
        CaptureFlags {
            config,
            vendor: effective_vendor(cfg),
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

    let control = LiveCapture::start_free_threaded(settings)
        .map_err(|e| anyhow!("starting WGC capture: {e}"))?;

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
        device_name,
        info.width,
        info.height,
        config.width,
        config.height,
        downscale,
        config.fps,
        config.bitrate_bps,
    );

    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

    let thread_args = DxgiThreadArgs {
        device_name: device_name.to_string(),
        config,
        vendor: effective_vendor(cfg),
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
        Ok(Ok(())) => Ok((
            pipeline,
            DxgiControl {
                stop,
                join: Some(join),
            },
        )),
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
        let backend = build_backend(
            config,
            vendor,
            native_w,
            native_h,
            dup.device(),
            dup.context(),
        )?;

        let needs_scaler =
            backend.wants_prescale() && (config.width != native_w || config.height != native_h);
        let scaler = if needs_scaler {
            Some(
                Scaler::new(
                    dup.device(),
                    dup.context(),
                    native_w,
                    native_h,
                    config.width,
                    config.height,
                )
                .context("building GPU downscaler for --scale")?,
            )
        } else {
            None
        };
        let reader = if backend.is_cpu_bridge() && scaler.is_none() {
            Some(TextureReader::new(
                dup.device(),
                dup.context(),
                native_w,
                native_h,
            )?)
        } else {
            None
        };

        if let Some(dev) = backend.device() {
            tuning::raise_d3d11_gpu_priority(dev);
        }
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
    let mut was_idle = true;
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
            // Idle-branch AcquireNextFrame timeout. Kept small so the first frame after an idle
            // gap isn't delayed up to a full block interval; still parks the thread (no busy-spin).
            8
        };
        match dup.poll(timeout_ms) {
            Ok(PollStatus::Dirty) => {
                if !dirty {
                    // First dirty frame after an idle gap: reset the pacing gate so the burst's
                    // first frame is emitted immediately rather than waiting up to one
                    // frame_duration for the existing next_due to expire.
                    if was_idle {
                        next_due = Instant::now();
                    }
                    was_idle = false;
                }
                dirty = true;
            }
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
        let _ = tx.send(EncodedFrame {
            data: Bytes::from(au),
            capture,
        });
        dirty = false;
        was_idle = true;
        next_due = capture + frame_duration;

        frames_sent += 1;
        let dt = capture.elapsed().as_nanos();
        timing_sum_ns += dt;
        timing_count += 1;
        timing_max_ns = timing_max_ns.max(dt);
        if frames_sent.is_multiple_of(60) {
            let avg_ms = (timing_sum_ns / timing_count.max(1) as u128) as f64 / 1.0e6;
            let max_ms = timing_max_ns as f64 / 1.0e6;
            tprintln!(
                "encode-path latency: path=dxgi+{}, avg_ms={:.2}, max_ms={:.2}, frames={}",
                path_name,
                avg_ms,
                max_ms,
                frames_sent
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

#[allow(clippy::large_enum_variant)]
enum Backend {
    Nvenc { encoder: Encoder, path: EncodePath },
    Intel { encoder: IntelEncoder },
    IntelCpu { encoder: IntelEncoder },
    X264 { encoder: X264Encoder },
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Nvenc { path, .. } => path.name(),
            Backend::Intel { .. } => "intel-same-adapter",
            Backend::IntelCpu { .. } => "intel-cpu-bridge",
            Backend::X264 { .. } => "software-x264",
        }
    }

    fn device(&self) -> Option<&ID3D11Device> {
        match self {
            Backend::Nvenc { encoder, .. } => Some(encoder.device()),
            Backend::Intel { encoder } => Some(encoder.device()),
            Backend::IntelCpu { encoder } => Some(encoder.device()),
            Backend::X264 { .. } => None,
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<()> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.set_bitrate(bps),
            Backend::Intel { encoder } => encoder.set_bitrate(bps),
            Backend::IntelCpu { encoder } => encoder.set_bitrate(bps),
            Backend::X264 { encoder } => encoder.set_bitrate(bps),
        }
    }

    fn wants_prescale(&self) -> bool {
        !matches!(self, Backend::Intel { .. })
    }

    fn is_cpu_bridge(&self) -> bool {
        matches!(
            self,
            Backend::Nvenc {
                path: EncodePath::CpuBridge,
                ..
            } | Backend::IntelCpu { .. }
                | Backend::X264 { .. }
        )
    }

    fn encode_gpu(
        &mut self,
        raw: &ID3D11Texture2D,
        scaler: Option<&mut Scaler>,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        match self {
            Backend::Intel { encoder } => {
                debug_assert!(
                    scaler.is_none(),
                    "Intel fuses scaling in VPP; no prescaler expected"
                );
                encoder.encode_texture(raw, force_idr)
            }
            Backend::Nvenc {
                encoder,
                path:
                    EncodePath::ZeroCopy {
                        igpu_context,
                        shared_igpu,
                        igpu_mutex,
                    },
            } => {
                unsafe {
                    igpu_mutex
                        .AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS)
                        .context("iGPU keyed mutex AcquireSync(writer)")?;
                    let staged = match scaler {
                        Some(s) => s.scale_into(raw, &*shared_igpu),
                        None => {
                            igpu_context.CopyResource(&*shared_igpu, raw);
                            Ok(())
                        }
                    };
                    igpu_context.Flush();
                    let released = igpu_mutex.ReleaseSync(KEY_ENCODER);
                    staged.context("staging frame into shared NVENC input")?;
                    released.context("iGPU keyed mutex ReleaseSync(encoder)")?;
                }
                encoder.encode_input(force_idr)
            }
            _ => bail!("encode_gpu called on a CPU-bridge backend"),
        }
    }

    fn encode_cpu(&mut self, data: &[u8], row_pitch: u32, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Nvenc {
                encoder,
                path: EncodePath::CpuBridge,
            } => encoder.encode_bgra_padded(data, row_pitch, force_idr),
            Backend::IntelCpu { encoder } => encoder.encode_bgra_padded(data, row_pitch, force_idr),
            Backend::X264 { encoder } => encoder.encode_bgra_padded(data, row_pitch, force_idr),
            _ => bail!("encode_cpu called on a GPU-path backend"),
        }
    }

    fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.encode_repeat(force_idr),
            Backend::Intel { encoder } => encoder.encode_repeat(force_idr),
            Backend::IntelCpu { encoder } => encoder.encode_repeat(force_idr),
            Backend::X264 { encoder } => encoder.encode_repeat(force_idr),
        }
    }
}

struct EncodeCore {
    backend: Backend,
    scaler: Option<Scaler>,
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
        apply_pending_bitrate(
            &mut self.backend,
            &self.target_bitrate,
            &mut self.current_bitrate,
        );
        let Self {
            backend, scaler, ..
        } = self;

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
            backend.encode_gpu(frame.as_raw_texture(), scaler.as_mut(), force_idr)?
        };

        self.have_frame = true;
        self.frame_index += 1;
        Ok(au)
    }

    fn encode_texture(&mut self, tex: &ID3D11Texture2D) -> Result<Vec<u8>> {
        let force_idr = self.take_force_idr();
        apply_pending_bitrate(
            &mut self.backend,
            &self.target_bitrate,
            &mut self.current_bitrate,
        );
        let Self {
            backend,
            scaler,
            reader,
            ..
        } = self;

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
            backend.encode_gpu(tex, scaler.as_mut(), force_idr)?
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
        apply_pending_bitrate(
            &mut self.backend,
            &self.target_bitrate,
            &mut self.current_bitrate,
        );
        let au = self.backend.encode_repeat(force_idr)?;
        self.frame_index += 1;
        Ok(Some(au))
    }
}

struct LiveCapture {
    core: Option<Arc<Mutex<EncodeCore>>>,
    // Present ONLY for the NVENC zero-copy path; when `Some`, `core` is `None` and
    // no repeater thread is spawned (the ring's encode thread handles both encode and
    // keepalive). For every other backend `ring` is `None` and the classic
    // `core`/`EncodeCore`/`spawn_repeater` path is used unchanged.
    ring: Option<AsyncNvencRing>,
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
        // Tear down the ring encode thread (sets its stop flag, joins). `AsyncNvencRing`'s
        // own Drop also does this, but taking it here makes the ordering explicit.
        if let Some(mut ring) = self.ring.take() {
            ring.stop();
        }
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
        EncodePath::ZeroCopy {
            igpu_context: igpu_context.clone(),
            shared_igpu,
            igpu_mutex,
        },
    ))
}

/// Like `build_zero_copy`, but builds a K-slot ring encoder and opens EACH of the
/// encoder's K shared handles on the iGPU/capture device, returning one
/// `(shared_texture, keyed_mutex)` pair per slot (in slot order).
fn build_zero_copy_ring(
    config: EncoderConfig,
    igpu_device: &ID3D11Device,
    igpu_context: &ID3D11DeviceContext,
    ring: usize,
) -> Result<(Encoder, Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>)> {
    let _ = igpu_context; // context is passed for symmetry with build_zero_copy / used by caller
    let encoder = Encoder::new_shared_ring(config, ring)?;
    let handles = encoder.shared_handles();
    if handles.len() != ring {
        bail!(
            "ring encoder produced {} handles, expected {}",
            handles.len(),
            ring
        );
    }
    let device1: ID3D11Device1 = igpu_device.cast().context("iGPU device as ID3D11Device1")?;
    let mut pairs: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)> = Vec::with_capacity(ring);
    for handle in handles {
        let shared_igpu: ID3D11Texture2D = unsafe { device1.OpenSharedResource1(handle) }
            .context("OpenSharedResource1 on iGPU (ring)")?;
        let igpu_mutex: IDXGIKeyedMutex = shared_igpu
            .cast()
            .context("opened shared ring texture as IDXGIKeyedMutex")?;
        pairs.push((shared_igpu, igpu_mutex));
    }
    Ok((encoder, pairs))
}

/// Message from the capture side to the ring encode thread: encode this staged slot.
struct ReadyMsg {
    slot: usize,
    force_idr: bool,
    capture: Instant,
}

/// The async NVENC zero-copy ring. Owns the K capture-side `(shared_igpu, mutex)`
/// pairs plus a dedicated encode thread (which owns the `Encoder`). Capture stages a
/// frame into a free slot and enqueues it on `ready`; the encode thread encodes and
/// returns the slot to `free_slots`. This decouples capture(N+1) from encode(N).
///
/// Keyed-mutex invariant (per slot): capture takes a slot ONLY after pulling it from
/// `free_slots`, WRITER->ENCODER acquires+releases it, and enqueues it; the encode
/// thread ENCODER->WRITER acquires+releases it and ONLY THEN returns it to
/// `free_slots`. So each slot's mutex ping-pongs WRITER->ENCODER->WRITER in balance,
/// and a dropped frame (no free slot, or a staging error) never leaves a slot
/// half-acquired.
struct AsyncNvencRing {
    /// Per-slot capture-side shared texture + keyed mutex (indexed by slot).
    shared_igpu: Vec<ID3D11Texture2D>,
    igpu_mutex: Vec<IDXGIKeyedMutex>,
    igpu_context: ID3D11DeviceContext,
    scaler: Option<Scaler>,
    /// Slots available for capture to stage into (pre-filled with 0..K).
    free_slots: crossbeam_channel::Sender<usize>,
    free_recv: crossbeam_channel::Receiver<usize>,
    /// Slots staged by capture and awaiting encode.
    ready: crossbeam_channel::Sender<ReadyMsg>,
    encode_join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    last_frame_at: Arc<AtomicU64>,
    idr_request: Arc<AtomicBool>,
    epoch: Instant,
    frame_index: u64,
}

impl AsyncNvencRing {
    #[allow(clippy::too_many_arguments)]
    fn new(
        encoder: Encoder,
        pairs: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>,
        igpu_context: ID3D11DeviceContext,
        scaler: Option<Scaler>,
        tx: broadcast::Sender<EncodedFrame>,
        target_bitrate: Arc<AtomicU32>,
        initial_bitrate: u32,
        last_frame_at: Arc<AtomicU64>,
        idr_request: Arc<AtomicBool>,
        epoch: Instant,
    ) -> Self {
        let k = pairs.len();
        let (shared_igpu, igpu_mutex): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();

        let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(k);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<ReadyMsg>(k);
        for slot in 0..k {
            free_tx.send(slot).expect("prefill free_slots");
        }

        let stop = Arc::new(AtomicBool::new(false));

        let encode_join = {
            let stop = Arc::clone(&stop);
            let free_tx = free_tx.clone();
            let last_frame_at = Arc::clone(&last_frame_at);
            std::thread::Builder::new()
                .name("nvenc-ring-encode".to_string())
                .spawn(move || {
                    ring_encode_thread(
                        encoder,
                        ready_rx,
                        free_tx,
                        tx,
                        target_bitrate,
                        initial_bitrate,
                        last_frame_at,
                        stop,
                    )
                })
                .expect("spawn nvenc ring encode thread")
        };

        Self {
            shared_igpu,
            igpu_mutex,
            igpu_context,
            scaler,
            free_slots: free_tx,
            free_recv: free_rx,
            ready: ready_tx,
            encode_join: Some(encode_join),
            stop,
            last_frame_at,
            idr_request,
            epoch,
            frame_index: 0,
        }
    }

    /// CAPTURE side. Take a free slot (latest-wins: if none is free, DROP this frame
    /// and return without touching any mutex), stage `raw` into that slot's shared
    /// texture under `KEY_WRITER`, release it to `KEY_ENCODER`, and enqueue it for the
    /// encode thread. Returns `Ok(true)` if a frame was staged, `Ok(false)` if dropped.
    ///
    /// Slot lifecycle guarantee: the slot is only WRITER-acquired AFTER it came from
    /// `free_slots`. If staging fails after we AcquireSync, we STILL ReleaseSync and
    /// return the slot to `free_slots`, so a failed frame never leaks a slot or leaves
    /// a mutex half-acquired.
    fn stage(&mut self, raw: &ID3D11Texture2D) -> Result<bool> {
        let slot = match self.free_recv.try_recv() {
            Ok(s) => s,
            // No free slot -> latest-wins drop (encode is still catching up). No mutex
            // was touched, nothing to clean up.
            Err(_) => return Ok(false),
        };

        // From here on we own `slot`. On ANY early return we must put it back in
        // `free_slots` so it isn't leaked.
        let mutex = &self.igpu_mutex[slot];
        let dst = &self.shared_igpu[slot];

        if let Err(e) = unsafe { mutex.AcquireSync(KEY_WRITER, KEY_TIMEOUT_MS) }
            .context("ring iGPU keyed mutex AcquireSync(writer)")
        {
            // We never got the mutex; just return the slot.
            let _ = self.free_slots.send(slot);
            return Err(e);
        }

        // Stage the WGC frame into the shared texture, then hand ownership to the
        // encoder (KEY_ENCODER) regardless of copy success — mirroring the single-path
        // `encode_gpu` which flushes and releases on all paths.
        let staged = unsafe {
            let r = match self.scaler.as_mut() {
                Some(s) => s.scale_into(raw, dst),
                None => {
                    self.igpu_context.CopyResource(dst, raw);
                    Ok(())
                }
            };
            self.igpu_context.Flush();
            r
        };
        let released = unsafe { mutex.ReleaseSync(KEY_ENCODER) };

        if let Err(e) = staged {
            // The copy failed, but ReleaseSync(ENCODER) already ran: the mutex now sits
            // at KEY_ENCODER, so the slot MUST pass through an ENCODER->WRITER release
            // before capture may reuse it. The only thing that does that release is the
            // encode thread, so we fall through and enqueue the slot anyway; the encode
            // thread ENCODER-acquires, encodes the (stale) texture, WRITER-releases, and
            // recycles it. Just dropping the frame here would strand the mutex at
            // KEY_ENCODER and eventually deadlock that slot.
            teprintln!("ring stage copy failed (slot={slot}): {e:?}; encoding stale slot to recycle");
        }
        if let Err(e) = released {
            // If ReleaseSync itself failed the mutex state is unknown; safest is to
            // return the slot to free and hope the next AcquireSync(writer) recovers.
            let _ = self.free_slots.send(slot);
            return Err(e).context("ring iGPU keyed mutex ReleaseSync(encoder)");
        }

        let force_idr = self.frame_index == 0 || self.idr_request.swap(false, Ordering::Relaxed);
        self.frame_index += 1;
        let msg = ReadyMsg {
            slot,
            force_idr,
            capture: Instant::now(),
        };
        if self.ready.send(msg).is_err() {
            // Encode thread is gone; we can't recycle through it. Return the slot.
            let _ = self.free_slots.send(slot);
            bail!("ring encode thread disconnected");
        }
        self.last_frame_at
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        Ok(true)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Dropping the ready sender disconnects the channel so the encode thread's
        // `ready.recv()` returns Disconnected and it breaks out of its loop.
        // We can't move `ready` out of &mut self, so rely on `stop` + the join:
        if let Some(join) = self.encode_join.take() {
            // Wake the thread if it's blocked on recv by nothing to send; the stop
            // flag is checked after each recv timeout. Give it a bounded chance.
            let _ = join.join();
        }
    }
}

impl Drop for AsyncNvencRing {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The dedicated ring encode thread: pull a staged slot, apply any pending bitrate
/// change, encode it (this is where the blocking `nvEncLockBitstream` runs, now OFF
/// the WGC capture callback), publish the AU, and return the slot to `free_slots`.
/// Keepalive is handled INSIDE this thread (see below) so we never need the old
/// `spawn_repeater`, which would require the encoder that now lives here.
#[allow(clippy::too_many_arguments)]
fn ring_encode_thread(
    mut encoder: Encoder,
    ready_rx: crossbeam_channel::Receiver<ReadyMsg>,
    free_tx: crossbeam_channel::Sender<usize>,
    tx: broadcast::Sender<EncodedFrame>,
    target_bitrate: Arc<AtomicU32>,
    initial_bitrate: u32,
    last_frame_at: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
) {
    let _thread_tuning = tuning::tune_current_thread();
    let mut current_bitrate = initial_bitrate;
    let _ = last_frame_at; // capture side owns the idle clock; kept for construction symmetry.

    // Keepalive lives INSIDE this thread (the encoder is owned here, so the old
    // `spawn_repeater` — which needs the encoder — cannot be used for the ring).
    //
    // To make keepalive race-free, the encode thread HOLDS BACK the most-recently
    // encoded slot instead of immediately returning it to `free_slots`. The held slot
    // is owned exclusively by the encode thread, so capture can never be staging into
    // it while we re-encode it for keepalive. When the next real frame arrives on a
    // DIFFERENT slot, we return the previously-held slot to `free_slots` and hold the
    // new one. With K=3, holding 1 back still leaves 2 slots for capture to overlap
    // capture(N+1) with encode(N).
    //
    // Ping-pong balance: capture WRITER->ENCODER-acquires a slot it pulled from
    // `free_slots`; the encode thread ENCODER->WRITER-releases it inside
    // `encode_input_slot`; the slot then returns to `free_slots` only via `release`
    // below. Keepalive re-encodes the held slot with `encode_repeat_slot`, which does a
    // self-contained WRITER acquire+release, leaving the slot at KEY_WRITER exactly as
    // it was — no imbalance, and the slot is never in `free_slots` during keepalive.
    let keepalive = Duration::from_millis(200);
    let mut held_slot: Option<usize> = None;

    // Slots whose keyed mutex is permanently wedged at KEY_ENCODER due to an
    // AcquireSync timeout (GPU hang / driver fault). Poisoned slots must never be
    // passed to encode_input_slot or encode_repeat_slot again; their mutex cannot be
    // recovered without a full encoder rebuild.
    let mut poisoned_slots: std::collections::HashSet<usize> = std::collections::HashSet::new();

    // Return the previously held slot (if any) to capture, then remember `slot` as held.
    let release_and_hold =
        |free_tx: &crossbeam_channel::Sender<usize>, held: &mut Option<usize>, slot: usize| -> bool {
            if let Some(prev) = held.take() {
                if free_tx.send(prev).is_err() {
                    return false;
                }
            }
            *held = Some(slot);
            true
        };

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match ready_rx.recv_timeout(keepalive) {
            Ok(msg) => {
                // A poisoned slot's mutex is stuck at KEY_ENCODER; calling
                // encode_input_slot on it would attempt AcquireSync(KEY_ENCODER) on an
                // already-acquired mutex and time out again. Return it directly to
                // free_slots so capture can avoid reusing it (capture will just drop
                // frames until the slot comes back — but since it's poisoned it never
                // should be reused; we send it anyway to keep the free-slot count
                // correct and let the "all poisoned" check below catch the terminal
                // case).
                if poisoned_slots.contains(&msg.slot) {
                    teprintln!(
                        "ring encode: skipping poisoned slot {} (keyed mutex wedged at KEY_ENCODER)",
                        msg.slot
                    );
                    let _ = free_tx.send(msg.slot);
                    continue;
                }

                apply_pending_bitrate_atomic(&mut encoder, &target_bitrate, &mut current_bitrate);
                match encoder.encode_input_slot(msg.slot, msg.force_idr) {
                    Ok(au) => {
                        let _ = tx.send(EncodedFrame {
                            data: Bytes::from(au),
                            capture: msg.capture,
                        });
                        // Free the previously-held slot and hold this one back for
                        // keepalive. encode_input_slot has already ENCODER->WRITER-
                        // released msg.slot, so it is safe to hold (mutex at
                        // KEY_WRITER, owned only by us).
                        if !release_and_hold(&free_tx, &mut held_slot, msg.slot) {
                            break;
                        }
                    }
                    Err(SlotEncodeError::Poisoned(e)) => {
                        // AcquireSync failed: the mutex was NOT acquired, so
                        // ReleaseSync(KEY_WRITER) has NOT run. The slot's mutex is
                        // stuck at KEY_ENCODER. We must NOT call release_and_hold or
                        // attempt encode_repeat_slot on this slot.
                        teprintln!(
                            "ring encode: slot {} poisoned (AcquireSync failed — GPU hang or driver fault): {e:?}",
                            msg.slot
                        );
                        poisoned_slots.insert(msg.slot);

                        // Check for total ring failure (all slots gone).
                        // The ring size equals the channel capacity; we can infer it
                        // from the sum of free + ready + held + poisoned. Simpler: the
                        // ring was built with K slots; if every slot is poisoned the
                        // ring is unrecoverable. We detect this when the held slot (if
                        // any) is also poisoned and there are no more non-poisoned
                        // slots that could arrive via ready_rx — conservatively check
                        // whether the held slot is still healthy.
                        let held_poisoned = held_slot.map_or(false, |s| poisoned_slots.contains(&s));
                        if held_poisoned || held_slot.is_none() {
                            // No held slot can serve keepalive; if all slots arriving
                            // from the capture side are also poisoned the ring is dead.
                            // We break here rather than spinning forever. The pipeline's
                            // reconnect logic will rebuild the encoder.
                            teprintln!(
                                "ring encode: CRITICAL — held slot also poisoned or absent; \
                                 ring is unrecoverable, stopping encode thread"
                            );
                            break;
                        }
                        // The slot is poisoned but there is still a healthy held slot;
                        // continue serving keepalive frames while capture drains the
                        // remaining slots.
                    }
                    Err(SlotEncodeError::Encode(e)) => {
                        // Acquire succeeded; ReleaseSync(KEY_WRITER) has already run
                        // inside encode_input_slot. The slot is clean at KEY_WRITER and
                        // safe to hold/recycle normally.
                        teprintln!("ring encode failed (slot={}): {e:?}", msg.slot);
                        if !release_and_hold(&free_tx, &mut held_slot, msg.slot) {
                            break;
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if let Some(slot) = held_slot {
                    // Skip keepalive on a poisoned held slot (shouldn't happen if the
                    // critical-error branch above ran, but guard defensively).
                    if poisoned_slots.contains(&slot) {
                        held_slot = None;
                        continue;
                    }
                    apply_pending_bitrate_atomic(
                        &mut encoder,
                        &target_bitrate,
                        &mut current_bitrate,
                    );
                    match encoder.encode_repeat_slot(slot) {
                        Ok(au) => {
                            let _ = tx.send(EncodedFrame {
                                data: Bytes::from(au),
                                capture: Instant::now(),
                            });
                        }
                        Err(e) => {
                            teprintln!("ring keepalive encode failed (slot={slot}): {e:?}");
                        }
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    tprintln!("nvenc ring encode thread stopped");
}

/// Bitrate application for the ring encode thread (mirrors `apply_pending_bitrate`
/// but talks directly to an `Encoder` instead of a `Backend`).
fn apply_pending_bitrate_atomic(
    encoder: &mut Encoder,
    target_bitrate: &AtomicU32,
    current: &mut u32,
) {
    let pending = target_bitrate.swap(0, Ordering::Relaxed);
    if pending == 0 || pending == *current {
        return;
    }
    match encoder.set_bitrate(pending) {
        Ok(()) => {
            tprintln!("adapting bitrate: {} -> {pending} bps", *current);
            *current = pending;
        }
        Err(e) => teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; keeping current"),
    }
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
                            config.width,
                            config.height,
                            config.fps
                        );
                        Ok(Backend::Nvenc {
                            encoder,
                            path: EncodePath::CpuBridge,
                        })
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
                    config.width,
                    config.height,
                    config.fps
                );
                Ok(Backend::Intel { encoder })
            }
            Err(e) => {
                teprintln!(
                    "Intel Quick Sync same-adapter path unavailable ({e:?}); trying own-device CPU bridge"
                );
                let encoder = IntelEncoder::new(config).context(
                    "Intel Quick Sync unavailable (same-adapter and CPU-bridge both failed)",
                )?;
                tprintln!(
                    "pipeline: live capture ready -- INTEL Quick Sync CPU-bridge path ({}x{}@{})",
                    config.width,
                    config.height,
                    config.fps
                );
                Ok(Backend::IntelCpu { encoder })
            }
        }
    };

    let try_x264 = || -> Result<Backend> {
        match X264Encoder::new(config) {
            Ok(encoder) => {
                tprintln!(
                    "pipeline: live capture ready -- SOFTWARE x264 CPU path ({}x{}@{})",
                    config.width,
                    config.height,
                    config.fps
                );
                Ok(Backend::X264 { encoder })
            }
            Err(e) => Err(e.context("software x264 fallback unavailable")),
        }
    };

    match vendor {
        EncoderVendor::Software => try_x264(),
        EncoderVendor::Nvidia => try_nvenc().or_else(|nv_err| {
            teprintln!("NVENC unavailable ({nv_err:?}); falling back to software x264");
            try_x264()
                .map_err(|xe| nv_err.context(format!("software fallback also failed: {xe:?}")))
        }),
        EncoderVendor::Intel => try_intel().or_else(|ie| {
            teprintln!("Intel Quick Sync unavailable ({ie:?}); falling back to software x264");
            try_x264().map_err(|xe| ie.context(format!("software fallback also failed: {xe:?}")))
        }),
        EncoderVendor::Auto => try_nvenc()
            .or_else(|nv_err| {
                teprintln!("NVENC unavailable ({nv_err:?}); trying Intel Quick Sync");
                try_intel()
                    .map_err(|ie| nv_err.context(format!("Intel fallback also failed: {ie:?}")))
            })
            .or_else(|hw_err| {
                teprintln!(
                    "no hardware encoder available ({hw_err:?}); falling back to software x264"
                );
                try_x264()
                    .map_err(|xe| hw_err.context(format!("software fallback also failed: {xe:?}")))
            }),
    }
}

#[allow(clippy::too_many_arguments)]
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
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture,
                });
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

        let epoch = Instant::now();
        let last_frame_at = Arc::new(AtomicU64::new(0));
        let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);
        let stop = Arc::new(AtomicBool::new(false));

        // --- NVENC zero-copy RING path (decoupled capture/encode) -----------------
        // For Nvidia/Auto vendors, try the K=3 shared-texture ring first. On success
        // we run the async ring (dedicated encode thread + slot pool) and DO NOT build
        // the single-texture EncodeCore or spawn the classic repeater. On failure we
        // fall through to `build_backend`, which handles the CPU-bridge, Intel, and
        // x264 paths exactly as before.
        use crate::streamer::config::EncoderVendor;
        const RING: usize = 3;
        let try_ring = matches!(vendor, EncoderVendor::Nvidia | EncoderVendor::Auto);
        if try_ring {
            match build_zero_copy_ring(config, &ctx.device, &ctx.device_context, RING) {
                Ok((encoder, pairs)) => {
                    tuning::raise_d3d11_gpu_priority(encoder.device());

                    // The ring shares NVENC input textures with the encoder's own D3D11
                    // device; capture stages into them via the WGC capture device. A
                    // downscaler is needed iff the encode size differs from native.
                    let scaler = if config.width != native_w || config.height != native_h {
                        match Scaler::new(
                            &ctx.device,
                            &ctx.device_context,
                            native_w,
                            native_h,
                            config.width,
                            config.height,
                        ) {
                            Ok(s) => Some(s),
                            Err(e) => return Err(e.context("building GPU downscaler for --scale")),
                        }
                    } else {
                        None
                    };

                    let ring = AsyncNvencRing::new(
                        encoder,
                        pairs,
                        ctx.device_context.clone(),
                        scaler,
                        tx.clone(),
                        Arc::clone(&target_bitrate),
                        config.bitrate_bps,
                        Arc::clone(&last_frame_at),
                        Arc::clone(&idr_request),
                        epoch,
                    );

                    tprintln!(
                        "pipeline: live capture ready -- NVENC ZERO-COPY RING (K={}) decoupled path ({}x{}@{})",
                        RING, config.width, config.height, config.fps
                    );

                    return Ok(Self {
                        core: None,
                        ring: Some(ring),
                        tx,
                        epoch,
                        last_frame_at,
                        path_name: "zero-copy-ring",
                        frames_sent: 0,
                        busy_streak: 0,
                        stop,
                        _thread_tuning: thread_tuning,
                        _keep_awake: keep_awake,
                        timing_sum_ns: 0,
                        timing_count: 0,
                        timing_max_ns: 0,
                    });
                }
                Err(e) => {
                    teprintln!(
                        "NVENC zero-copy ring unavailable ({e:?}); falling back to classic backend path"
                    );
                }
            }
        }

        // --- Classic (non-ring) path: unchanged from before ------------------------
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

        if let Some(dev) = backend.device() {
            tuning::raise_d3d11_gpu_priority(dev);
        }

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
            core: Some(core),
            ring: None,
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

        // --- NVENC zero-copy RING path: CAPTURE side only -------------------------
        // Stage the WGC frame into a free ring slot and hand it to the encode thread;
        // the actual encode (incl. the blocking nvEncLockBitstream) runs OFF this WGC
        // callback so capture(N+1) can overlap encode(N). No free slot => drop frame
        // (latest-wins). The encode thread publishes the AU and updates timing.
        if let Some(ring) = self.ring.as_mut() {
            match ring.stage(frame.as_raw_texture()) {
                Ok(_staged) => {}
                Err(e) => {
                    teprintln!("ring capture stage failed: {e:?}");
                }
            }
            return Ok(());
        }

        // --- Classic (non-ring) path: unchanged -----------------------------------
        let core = self
            .core
            .as_ref()
            .expect("classic path must have an EncodeCore");
        let encode_res = {
            let mut core = core.lock().expect("encode core mutex poisoned");
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
        let _ = self.tx.send(EncodedFrame {
            data: Bytes::from(au),
            capture,
        });
        self.frames_sent += 1;

        let dt = t0.elapsed().as_nanos();
        self.timing_sum_ns += dt;
        self.timing_count += 1;
        self.timing_max_ns = self.timing_max_ns.max(dt);
        if self.frames_sent.is_multiple_of(60) {
            let avg_ms = (self.timing_sum_ns / self.timing_count.max(1) as u128) as f64 / 1.0e6;
            let max_ms = self.timing_max_ns as f64 / 1.0e6;
            tprintln!(
                "encode-path latency: path={}, avg_ms={:.2}, max_ms={:.2}, frames={}",
                self.path_name,
                avg_ms,
                max_ms,
                self.frames_sent
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

    tprintln!("pipeline: synthetic pattern encode loop started ({SP_WIDTH}x{SP_HEIGHT}@{SP_FPS})");

    loop {
        let pending = target_bitrate.swap(0, Ordering::Relaxed);
        if pending != 0 && pending != current_bitrate {
            match encoder.set_bitrate(pending) {
                Ok(()) => {
                    tprintln!("adapting bitrate: {current_bitrate} -> {pending} bps");
                    current_bitrate = pending;
                }
                Err(e) => {
                    teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; keeping current")
                }
            }
        }

        let force_idr = frame_index == 0 || idr_request.swap(false, Ordering::Relaxed);
        fill_synthetic_pattern(&mut frame_buf, SP_WIDTH, SP_HEIGHT, frame_index);

        match encoder.encode_bgra(&frame_buf, force_idr) {
            Ok(au) => {
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture: Instant::now(),
                });
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
