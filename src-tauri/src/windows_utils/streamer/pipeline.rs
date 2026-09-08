use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::windows_capture::capture::{CaptureControl, Context, GraphicsCaptureApiHandler};
use crate::windows_capture::d3d11::StagingTexture;
use crate::windows_capture::frame::Frame;
use crate::windows_capture::graphics_capture_api::{wgc_features, InternalCaptureControl};
use crate::windows_capture::settings::{
    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
};
use anyhow::{anyhow, bail, Context as _, Result};
use bytes::Bytes;
use tokio::sync::broadcast;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D, ID3D11VideoContext,
    ID3D11VideoDevice, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

use super::amd::encoder::Encoder as AmfEncoder;
use super::capture::{select_monitor, select_monitor_by_device_name, MonitorInfo};
use super::dxgi::{Duplicator, PollStatus};
use super::intel::encoder::Encoder as IntelEncoder;
use super::nvidia::encoder::{
    Encoder, EncoderConfig, SlotEncodeError, KEY_ENCODER, KEY_TIMEOUT_MS, KEY_WRITER,
};
use super::scaler::{ReadbackFormat, Scaler, TextureReader};
use super::tuning;
use super::x264::encoder::X264Encoder;
use crate::streamer::config::{Config, H264Profile, ScalePercent};
use crate::streamer::platform::BitrateApplied;
use crate::streamer::telemetry::{FrameTiming, SessionTelemetry, SharedTelemetry};
use crate::windows_capture::monitor::Monitor;

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
    /// Per-stage stamps collected on the way here. The transport fills in the last two and folds
    /// the whole set into the session histograms; see `streamer::telemetry`.
    pub timing: FrameTiming,
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
    pub telemetry: SharedTelemetry,
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

/// Logged once per encode thread: repeating it would put a line on the logbus for every
/// congestion-control target, and every logbus line is an IPC event to the UI.
fn warn_constant_qp_once(warned: &mut bool) {
    if !*warned {
        *warned = true;
        teprintln!(
            "bitrate adaptation is inert: the encoder is in explicit constant-QP mode (--qp), \
             so congestion control cannot shrink the stream"
        );
    }
}

fn apply_pending_bitrate(
    backend: &mut Backend,
    target_bitrate: &AtomicU32,
    current: &mut u32,
    warned: &mut bool,
) {
    let pending = target_bitrate.swap(0, Ordering::Relaxed);
    if pending == 0 || pending == *current {
        return;
    }
    match backend.set_bitrate(pending) {
        Ok(BitrateApplied::Applied) => {
            tprintln!("adapting bitrate: {} -> {pending} bps", *current);
            *current = pending;
        }
        Ok(BitrateApplied::IgnoredConstantQp) => {
            warn_constant_qp_once(warned);
            *current = pending;
        }
        Err(e) => {
            teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; will retry");
            restore_pending_bitrate(target_bitrate, pending);
        }
    }
}

/// Puts a failed target back so the next tick retries it.
///
/// Both appliers consume the request with `swap(0)` *before* attempting the reconfigure, so a
/// transient `nvEncReconfigureEncoder` failure used to drop the congestion controller's response
/// on the floor until it happened to publish a different number -- possibly never, if it had
/// settled. `compare_exchange` so a newer target published in the meantime still wins.
fn restore_pending_bitrate(target_bitrate: &AtomicU32, pending: u32) {
    let _ = target_bitrate.compare_exchange(0, pending, Ordering::Relaxed, Ordering::Relaxed);
}

const MAX_TRANSIENT_ENCODE_DROPS: u32 = 60;

/// `MinUpdateInterval` is a floor on the gap between delivered frames, not a throttle to a rate:
/// WGC's default delivery is capped near 60 Hz whatever the panel does, and this property is what
/// raises that ceiling. The old `frame_duration * 2/3` scaled the floor with the target rate,
/// which is backwards — a floor near the target period caps delivery *below* it.
/// `windows-capture` issue #190 measured a 120 Hz panel with the floor at 1/60 delivering ~52 fps,
/// at 1/90 ~58 fps and at 1/120 ~59 fps. Sunshine sets a flat 4 ms (250 Hz); so do we, and let
/// the pipeline's own pacing decide what to keep.
const WGC_MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(4);

/// Frame-pool depth for a handler that encodes inline: one buffer the compositor writes, one we
/// read. OBS and Sunshine both use 2. The pool is a queue handed out oldest-first, so depth is
/// latency for anything the consumer cannot keep up with.
const FRAME_POOL_BUFFERS_INLINE: u32 = 2;

/// Frame-pool depth for the ring path, which only stages a copy in the callback and encodes
/// elsewhere: one more buffer so the copy and the encode overlap.
const FRAME_POOL_BUFFERS_RING: u32 = 3;

/// How many consecutive "nothing changed" reports to trust before encoding anyway. The DXGI path
/// carries the same valve for the same reason: a driver that under-reports dirty regions would
/// otherwise serve a stale picture forever.
const MAX_CONSECUTIVE_UNCHANGED: u32 = 5;

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
        // The display's own rate, capped by the configured ceiling. There used to be a 60 fps
        // floor here, which is not a floor on anything real: an indirect display has no scanout,
        // so its reported refresh *is* the rate DWM composes at, and asking the encoder for 60
        // when the compositor produces 30 only buys duplicate frames. Where the rate is unknown
        // (0), 60 is still the assumption.
        let refresh = if refresh_hz == 0 { 60 } else { refresh_hz };
        let max_fps = cfg.max_fps.clamp(15, 500);
        refresh.clamp(15, max_fps)
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
        min_qp: cfg.quality_qp,
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
    let telemetry = SessionTelemetry::new_shared();

    if cfg.synthetic_pattern {
        let frame_duration = Duration::from_nanos(1_000_000_000 / SP_FPS as u64);
        let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
        telemetry.capture_backend.set("synthetic".into());
        telemetry.encoder.set("nvenc".into());
        let pipeline = Pipeline {
            tx: tx.clone(),
            frame_duration,
            telemetry: Arc::clone(&telemetry),
            idr_request: Arc::clone(&idr_request),
            target_bitrate: Arc::clone(&target_bitrate),
            wake,
            max_bitrate_bps: SP_BITRATE_BPS,
            h264_profile: cfg.h264_profile,
        };
        let workload = cfg.workload;
        let counter = cfg.counter;
        let enc = EncoderConfig {
            width: SP_WIDTH,
            height: SP_HEIGHT,
            fps: SP_FPS,
            bitrate_bps: SP_BITRATE_BPS,
            max_bitrate_bps: SP_BITRATE_BPS,
            profile: cfg.h264_profile,
            qp: cfg.qp,
            min_qp: cfg.quality_qp,
            intra_refresh: cfg.intra_refresh,
        };
        std::thread::Builder::new()
            .name("nvenc-encode".to_string())
            .spawn(move || {
                synthetic_pattern_loop(
                    tx,
                    idr_request,
                    target_bitrate,
                    enc,
                    wake_rx,
                    telemetry,
                    workload,
                    counter,
                )
            })
            .expect("spawn encode thread");
        return Ok(pipeline);
    }

    super::capture::check_dwm_composition()?;

    let (monitor, info) = select_monitor(cfg.monitor)?;
    if cfg.force_dxgi {
        let device_name = monitor
            .device_name()
            .map_err(|e| anyhow!("monitor device name for forced DXGI capture: {e}"))?;
        let (pipeline, control) = start_dxgi_capture(
            cfg,
            &device_name,
            &info,
            tx,
            idr_request,
            target_bitrate,
            telemetry,
        )?;
        drop(control);
        return Ok(pipeline);
    }
    match start_live_capture(
        cfg,
        monitor,
        &info,
        tx.clone(),
        Arc::clone(&idr_request),
        Arc::clone(&target_bitrate),
        Arc::clone(&telemetry),
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
            telemetry
                .degradation
                .capture_fallback
                .fetch_add(1, Ordering::Relaxed);
            let (pipeline, control) = start_dxgi_capture(
                cfg,
                &device_name,
                &info,
                tx,
                idr_request,
                target_bitrate,
                telemetry,
            )
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
    let telemetry = SessionTelemetry::new_shared();

    super::capture::check_dwm_composition()?;

    let (monitor, info) = select_monitor_by_device_name(device_name)?;

    if cfg.force_dxgi {
        let (pipeline, control) = start_dxgi_capture(
            cfg,
            device_name,
            &info,
            tx,
            idr_request,
            target_bitrate,
            telemetry,
        )?;
        return Ok(SessionCapture {
            pipeline,
            control: Some(SessionControl::Dxgi(control)),
        });
    }

    match start_live_capture(
        cfg,
        monitor,
        &info,
        tx.clone(),
        Arc::clone(&idr_request),
        Arc::clone(&target_bitrate),
        Arc::clone(&telemetry),
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
            telemetry
                .degradation
                .capture_fallback
                .fetch_add(1, Ordering::Relaxed);
            let (pipeline, control) = start_dxgi_capture(
                cfg,
                device_name,
                &info,
                tx,
                idr_request,
                target_bitrate,
                telemetry,
            )
            .map_err(|e| e.context(format!("DXGI fallback (after WGC failed: {wgc_err:#})")))?;
            Ok(SessionCapture {
                pipeline,
                control: Some(SessionControl::Dxgi(control)),
            })
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_live_capture(
    cfg: &Config,
    monitor: Monitor,
    info: &MonitorInfo,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    telemetry: SharedTelemetry,
) -> Result<(Pipeline, CaptureControl<LiveCapture, anyhow::Error>)> {
    let config = live_encoder_config(info.width, info.height, info.refresh_hz, cfg);
    let downscale = config.width != info.width || config.height != info.height;
    let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps as u64);

    let features = wgc_features();
    telemetry.capture_backend.set("wgc".into());
    telemetry.capture_features.set(features.summary());
    let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        telemetry: Arc::clone(&telemetry),
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: config.max_bitrate_bps,
        h264_profile: cfg.h264_profile,
    };

    // Every one of these is applied only where the OS exposes the property; the ones it does not
    // are skipped, not fatal. `ReportOnly` gives us `Frame::dirty_regions()` without asking the
    // compositor to render the regions separately.
    let settings = Settings::new(
        monitor,
        CursorCaptureSettings::WithCursor,
        DrawBorderSettings::WithoutBorder,
        SecondaryWindowSettings::Default,
        MinimumUpdateIntervalSettings::Custom(WGC_MIN_UPDATE_INTERVAL),
        DirtyRegionSettings::ReportOnly,
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
            telemetry,
        },
    )
    .with_gpu_preference(super::capture_gpu_preference(effective_vendor(cfg)));

    tprintln!(
        "pipeline: starting live monitor capture (WGC): \
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
    tprintln!(
        "wgc: feature set {} ({}); optional properties this build lacks are skipped, not fatal",
        features.summary(),
        if features.has_26100_properties() {
            "build 26100+"
        } else {
            "pre-26100"
        },
    );

    let control = LiveCapture::start_free_threaded(settings)
        .map_err(|e| anyhow!("starting WGC capture: {e}"))?;

    Ok((pipeline, control))
}

#[allow(clippy::too_many_arguments)]
fn start_dxgi_capture(
    cfg: &Config,
    device_name: &str,
    info: &MonitorInfo,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    telemetry: SharedTelemetry,
) -> Result<(Pipeline, DxgiControl)> {
    let config = live_encoder_config(info.width, info.height, info.refresh_hz, cfg);
    let downscale = config.width != info.width || config.height != info.height;
    let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);

    telemetry.capture_backend.set("dxgi".into());
    let (wake, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        telemetry: Arc::clone(&telemetry),
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
        telemetry,
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

/// Staging slots between the DXGI capture thread and its encode thread.
///
/// Two is enough to overlap one encode with one capture; three buys slack for a jittery encoder
/// without adding a frame of latency, because the consumer always takes the oldest queued slot
/// and the producer drops rather than queueing when none is free.
const DXGI_STAGE_SLOTS: usize = 3;

/// The ring plus the three handles its encode thread needs: the ready queue, the slot-return
/// channel, and its own view of the slot textures.
type DxgiRingParts = (
    DxgiRing,
    crossbeam_channel::Receiver<DxgiStaged>,
    crossbeam_channel::Sender<usize>,
    Vec<ID3D11Texture2D>,
);

/// A staged frame on its way to the DXGI encode thread.
struct DxgiStaged {
    slot: usize,
    capture: Instant,
    timing: FrameTiming,
}

/// Decouples DXGI capture from encode.
///
/// Before this, `dxgi_capture_thread` composited and encoded in one loop body, so capture of
/// frame N+1 could not begin until the encode of frame N returned — the achievable rate was
/// `1 / (capture + composite + encode)` rather than `1 / capture`, and the encode is the large
/// term on every CPU-bridge backend. The capture thread now only copies the composed frame into
/// a free slot (a GPU-to-GPU copy on the duplication device, which is multithread-protected) and
/// hands the index over.
struct DxgiRing {
    slots: Vec<ID3D11Texture2D>,
    context: ID3D11DeviceContext,
    free_rx: crossbeam_channel::Receiver<usize>,
    ready_tx: crossbeam_channel::Sender<DxgiStaged>,
    dropped: u64,
    dropped_reported: u64,
    telemetry: SharedTelemetry,
}

impl DxgiRing {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        width: u32,
        height: u32,
        telemetry: SharedTelemetry,
    ) -> Result<DxgiRingParts> {
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
            // The encode side may hand this to a VideoProcessor input view (the scaler) or read
            // it back, so it needs both of the bind flags a duplication target has.
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut slots = Vec::with_capacity(DXGI_STAGE_SLOTS);
        for i in 0..DXGI_STAGE_SLOTS {
            let mut tex: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }
                .with_context(|| format!("CreateTexture2D (dxgi stage slot {i})"))?;
            slots.push(tex.context("dxgi stage slot texture was null")?);
        }

        let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(DXGI_STAGE_SLOTS);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<DxgiStaged>(DXGI_STAGE_SLOTS);
        for i in 0..DXGI_STAGE_SLOTS {
            free_tx.send(i).expect("prefill dxgi free slots");
        }
        let encode_slots = slots.clone();
        Ok((
            Self {
                slots,
                context: context.clone(),
                free_rx,
                ready_tx,
                dropped: 0,
                dropped_reported: 0,
                telemetry,
            },
            ready_rx,
            free_tx,
            encode_slots,
        ))
    }

    fn stage(
        &mut self,
        src: &ID3D11Texture2D,
        capture: Instant,
        timing: FrameTiming,
    ) -> Result<()> {
        let Ok(slot) = self.free_rx.try_recv() else {
            // Every slot is with the encoder. Dropping here is the same trade the WGC ring makes:
            // a fresher frame will be along before the encoder is free again.
            self.dropped += 1;
            self.telemetry
                .degradation
                .frames_dropped_ring
                .fetch_add(1, Ordering::Relaxed);
            if self.dropped - self.dropped_reported >= 120 {
                self.dropped_reported = self.dropped;
                teprintln!(
                    "dxgi ring: {} frames dropped for want of a free slot; the encoder is not \
                     keeping up",
                    self.dropped
                );
            }
            return Ok(());
        };
        // SAFETY: both textures are live, on the same multithread-protected device, and have the
        // same descriptor.
        unsafe {
            self.context.CopyResource(&self.slots[slot], src);
            self.context.Flush();
        }
        if self
            .ready_tx
            .send(DxgiStaged {
                slot,
                capture,
                timing,
            })
            .is_err()
        {
            bail!("dxgi encode thread disconnected");
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_dxgi_encoder(
    core: Arc<Mutex<EncodeCore>>,
    slots: Vec<ID3D11Texture2D>,
    ready_rx: crossbeam_channel::Receiver<DxgiStaged>,
    free_tx: crossbeam_channel::Sender<usize>,
    tx: broadcast::Sender<EncodedFrame>,
    last_frame_at: Arc<AtomicU64>,
    epoch: Instant,
    frame_duration: Duration,
    stop: Arc<AtomicBool>,
    path_name: &'static str,
    telemetry: SharedTelemetry,
) -> std::thread::JoinHandle<()> {
    // `ID3D11Texture2D` is not `Send`, but the device is multithread-protected and the slots are
    // only ever touched by the capture thread's `CopyResource` and this thread's encode, which the
    // free/ready channels already order.
    struct SendSlots(Vec<ID3D11Texture2D>);
    unsafe impl Send for SendSlots {}
    let slots = SendSlots(slots);

    std::thread::Builder::new()
        .name("dxgi-encode".to_string())
        .spawn(move || {
            let slots = slots;
            let _thread_tuning = tuning::tune_current_thread();
            let mut busy_streak: u32 = 0;
            let mut frames_sent: u64 = 0;
            let mut timing_sum_ns: u128 = 0;
            let mut timing_count: u64 = 0;
            let mut timing_max_ns: u128 = 0;

            while !stop.load(Ordering::Relaxed) {
                let staged = match ready_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(s) => s,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
                };
                let DxgiStaged {
                    slot,
                    capture,
                    mut timing,
                } = staged;
                let Some(tex) = slots.0.get(slot) else {
                    break;
                };

                let encode_res = {
                    let mut core = match core.lock() {
                        Ok(g) => g,
                        Err(_) => break,
                    };
                    timing.mark_encode_submit();
                    core.encode_texture(tex)
                };
                timing.mark_encode_done();
                let returned = free_tx.send(slot).is_ok();

                let au = match encode_res {
                    Ok(au) => au,
                    Err(e)
                        if is_transient_encode_error(&e)
                            && busy_streak < MAX_TRANSIENT_ENCODE_DROPS =>
                    {
                        busy_streak += 1;
                        telemetry
                            .degradation
                            .encoder_transient_errors
                            .fetch_add(1, Ordering::Relaxed);
                        teprintln!(
                            "dxgi encode transiently overloaded ({e:#}); dropping frame \
                             ({busy_streak} in a row)"
                        );
                        continue;
                    }
                    Err(e) => {
                        teprintln!("dxgi encode failed: {e:?}; stopping capture");
                        break;
                    }
                };
                busy_streak = 0;
                if !returned {
                    break;
                }

                last_frame_at.store(epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture,
                    timing,
                });

                frames_sent += 1;
                let dt = capture.elapsed().as_nanos();
                timing_sum_ns += dt;
                timing_count += 1;
                timing_max_ns = timing_max_ns.max(dt);
                if frames_sent.is_multiple_of(60) {
                    let avg_ms = (timing_sum_ns / timing_count.max(1) as u128) as f64 / 1.0e6;
                    let max_ms = timing_max_ns as f64 / 1.0e6;
                    tprintln!(
                        "encode-path latency: path=dxgi+{}, avg_ms={:.2}, max_ms={:.2}, \
                         frames={}, budget_ms={:.2}",
                        path_name,
                        avg_ms,
                        max_ms,
                        frames_sent,
                        frame_duration.as_secs_f64() * 1000.0,
                    );
                    timing_sum_ns = 0;
                    timing_count = 0;
                    timing_max_ns = 0;
                }
            }
            stop.store(true, Ordering::Relaxed);
            tprintln!("dxgi encode thread stopped");
        })
        .expect("spawn dxgi encode thread")
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
    telemetry: SharedTelemetry,
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
        telemetry,
    } = args;

    let _thread_tuning = tuning::tune_current_thread();
    let _keep_awake = tuning::KeepAwake::begin();

    let setup = (|| -> Result<(Duplicator, Arc<Mutex<EncodeCore>>, &'static str)> {
        let dup = Duplicator::new(&device_name, native_w, native_h)?;

        // Duplication is adapter-local, so the scaler has to live on whichever adapter composes
        // this display — including an indirect-display adapter, which is not guaranteed to expose
        // a video processor. Ask before configuring the encoder for a size only the scaler can
        // produce: this used to fail the whole session with "building GPU downscaler for --scale"
        // after WGC had already been tried, so a host with no video device and a video-scale
        // below 100% could not stream at all. Encoding at native and letting the client scale is
        // a worse picture, not a dead session.
        let mut config = config;
        let wants_downscale = config.width != native_w || config.height != native_h;
        if wants_downscale && !video_processor_available(dup.device(), dup.context()) {
            teprintln!(
                "dxgi: the adapter composing {device_name} exposes no D3D11 video processor, \
                 so the requested {}x{} downscale cannot be done on the GPU; encoding at the \
                 native {native_w}x{native_h} and leaving the scale to the client",
                config.width,
                config.height
            );
            config.width = native_w & !1;
            config.height = native_h & !1;
        }

        let backend = build_backend(
            config,
            vendor,
            native_w,
            native_h,
            dup.device(),
            dup.context(),
        )?;

        // No scaler at all when the display is already the size the client asked for: the blt
        // would be an identity pass and the extra surface a wasted allocation.
        let needs_scaler =
            backend.wants_prescale() && (config.width != native_w || config.height != native_h);
        let scaler = if needs_scaler {
            Some(
                Scaler::with_format(
                    dup.device(),
                    dup.context(),
                    native_w,
                    native_h,
                    config.width,
                    config.height,
                    scaler_output_format(&backend),
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
            staging: None,
            target_bitrate,
            idr_request: Arc::clone(&idr_request),
            current_bitrate: config.bitrate_bps,
            bitrate_warned: false,
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

    let ring = DxgiRing::new(
        dup.device(),
        dup.context(),
        native_w,
        native_h,
        Arc::clone(&telemetry),
    );
    let (mut ring, ready_rx, free_tx, encode_slots) = match ring {
        Ok(parts) => parts,
        Err(e) => {
            let _ = ready_tx.send(Err(
                e.context("building the DXGI capture/encode staging ring")
            ));
            return;
        }
    };
    let encode_join = spawn_dxgi_encoder(
        Arc::clone(&core),
        encode_slots,
        ready_rx,
        free_tx,
        tx.clone(),
        Arc::clone(&last_frame_at),
        epoch,
        frame_duration,
        Arc::clone(&stop),
        path_name,
        Arc::clone(&telemetry),
    );

    spawn_repeater(
        Arc::clone(&core),
        tx.clone(),
        idr_request,
        frame_duration,
        Arc::clone(&last_frame_at),
        epoch,
        Arc::clone(&stop),
        wake_rx,
        Arc::clone(&telemetry),
    );

    telemetry.encoder.set(path_name.into());
    telemetry.capture_features.set(dup.api().name().into());
    tprintln!(
        "pipeline: live capture ready -- DXGI duplication ({}) -> {path_name}",
        dup.api().name()
    );
    let _ = ready_tx.send(Ok(()));

    let mut dirty = false;
    let mut next_due = Instant::now();
    let mut was_idle = true;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let timeout_ms = if dirty {
            let rem = next_due.saturating_duration_since(Instant::now());
            (rem.as_micros() / 1000) as u32
        } else {
            8
        };
        match dup.poll(timeout_ms) {
            Ok(PollStatus::Dirty) => {
                if !dirty {
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

        // Frames the OS coalesced away while we held the previous one. Drained every poll so
        // the counter tracks the capture, not the report cadence.
        let coalesced = dup.take_accumulated_drops();
        if coalesced != 0 {
            telemetry
                .degradation
                .frames_dropped_capture
                .fetch_add(coalesced, Ordering::Relaxed);
        }

        if !dirty {
            continue;
        }
        let now = Instant::now();
        if now < next_due {
            continue;
        }

        let capture = now;
        let mut timing = FrameTiming::entering_capture();
        timing.capture_available_ns = dup.last_present_host_ns();
        let tex = match dup.frame() {
            Ok(t) => t,
            Err(e) => {
                teprintln!("dxgi compose failed: {e:?}; stopping capture");
                break;
            }
        };
        match ring.stage(tex, capture, timing) {
            Ok(()) => {}
            Err(e) => {
                teprintln!("dxgi stage failed: {e:?}; stopping capture");
                break;
            }
        }
        dirty = false;
        was_idle = true;
        next_due = capture + frame_duration;
    }

    stop.store(true, Ordering::Relaxed);
    drop(ring);
    let _ = encode_join.join();
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

    // `--workload` normally implies `--synthetic-pattern`, which bypasses capture entirely. Here
    // the same generated frames go onto the real display instead, so the run exercises the
    // capture backend, the scaler and the encoder with content that is still repeatable. The
    // window is dropped at the end of the probe, which closes it.
    let _workload_window = match select_monitor(cfg.monitor) {
        Ok((monitor, _)) => monitor
            .device_name()
            .ok()
            .and_then(|name| super::capture::monitor_rect(&name))
            .and_then(|(left, top, width, height)| {
                match super::workload_window::WorkloadWindow::spawn(
                    left,
                    top,
                    width,
                    height,
                    cfg.max_fps.clamp(1, 240),
                    cfg.workload,
                    cfg.counter,
                ) {
                    Ok(window) => {
                        tprintln!(
                            "probe: painting the '{}' workload onto {width}x{height} at ({left},{top})",
                            cfg.workload.name()
                        );
                        Some(window)
                    }
                    Err(e) => {
                        teprintln!(
                            "probe: could not put the workload on the display ({e:#}); \
                             capturing whatever is on it instead"
                        );
                        None
                    }
                }
            }),
        Err(e) => {
            teprintln!("probe: no monitor to paint the workload onto ({e:#})");
            None
        }
    };

    let pipeline = start(&cfg)?;
    // Without this the probe's own telemetry is unreachable: the registry is keyed by session
    // and only the HTTP server publishes into it, so a probe run collected numbers nobody
    // could read.
    crate::streamer::telemetry::publish("probe-live", Arc::clone(&pipeline.telemetry));
    let mut rx = pipeline.tx.subscribe();
    pipeline.request_idr();

    let mut file = std::fs::File::create(path)?;
    let mut written = 0u64;
    let mut total = 0usize;
    let mut started = false;
    // A fixed 150 frames is 2.5 s at 60 fps, which is startup transients and little else.
    let deadline =
        (cfg.probe_secs > 0).then(|| Instant::now() + Duration::from_secs(cfg.probe_secs));

    while deadline.map_or(written < FRAMES, |d| Instant::now() < d) {
        match rx.blocking_recv() {
            Ok(frame) => {
                let timing = frame.timing;
                let au = frame.data;
                if !started {
                    if is_keyframe(&au) {
                        started = true;
                    } else {
                        continue;
                    }
                }
                // The transport is what normally folds a frame's stamps into the histograms,
                // and this probe has no transport. Without this the live probe reports frame
                // counts and nothing else.
                pipeline.telemetry.record_frame(&timing);
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
    telemetry: SharedTelemetry,
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
    Amf { encoder: AmfEncoder },
    AmfCpu { encoder: AmfEncoder },
    X264 { encoder: X264Encoder },
}

impl Backend {
    fn name(&self) -> &'static str {
        match self {
            Backend::Nvenc { path, .. } => path.name(),
            Backend::Intel { .. } => "intel-same-adapter",
            Backend::IntelCpu { .. } => "intel-cpu-bridge",
            Backend::Amf { .. } => "amd-amf-same-adapter",
            Backend::AmfCpu { .. } => "amd-amf-cpu-bridge",
            Backend::X264 { .. } => "software-x264",
        }
    }

    fn device(&self) -> Option<&ID3D11Device> {
        match self {
            Backend::Nvenc { encoder, .. } => Some(encoder.device()),
            Backend::Intel { encoder } => Some(encoder.device()),
            Backend::IntelCpu { encoder } => Some(encoder.device()),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => Some(encoder.device()),
            Backend::X264 { .. } => None,
        }
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<BitrateApplied> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.set_bitrate(bps),
            Backend::Intel { encoder } => encoder.set_bitrate(bps),
            Backend::IntelCpu { encoder } => encoder.set_bitrate(bps),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => encoder.set_bitrate(bps),
            Backend::X264 { encoder } => encoder.set_bitrate(bps),
        }
    }

    fn wants_prescale(&self) -> bool {
        !matches!(self, Backend::Intel { .. })
    }

    /// What this backend wants a CPU-bridge readback to be in.
    ///
    /// The scale and the colour conversion are one `VideoProcessorBlt`, so NV12 costs the video
    /// engine nothing extra and takes 62% off the readback, the memcpy and the upload. Every
    /// CPU-bridge encoder here converts BGRA to 4:2:0 on the CPU anyway — x264 on a rayon pool,
    /// oneVPL through a second VPP pass, AMF through an internal converter — so the conversion is
    /// not saved so much as moved onto silicon that was already running.
    ///
    /// x264 takes NV12: libx264 deinterleaves it to its internal I420 in SIMD inside
    /// `x264_frame_copy_picture`, so the scalar colour matrix that ran on a rayon pool is gone
    /// entirely. Measured at 640x360, 30 frames, debug libraries: 1655 us median per frame on the
    /// BGRA path against 450 us on the NV12 one, coding the same picture to within 1% of the same
    /// bytes (`x264_nv12_input_matches_the_bgra_path`).
    ///
    /// The Intel CPU bridge takes NV12 for a bigger reason: it can then skip its VPP pass
    /// altogether. On BGRA it re-uploaded 4 bytes a pixel and ran a `VideoProcessorBlt` to reach
    /// NV12 — a conversion the scaler has already done, on the same kind of fixed-function block,
    /// as part of a blt it was running anyway.
    ///
    /// The AMF bridge takes NV12 because VCN wants it: AMD state that packed-RGB input silently
    /// invokes an internal GPU colour converter competing with the foreground application, and
    /// OBS refuses BGRA on its texture path for the same reason. `UNVERIFIED` — no machine on
    /// this project has an AMD GPU (`docs/manual-tests/amd-amf.md`).
    ///
    /// NVENC is deliberately not a candidate: `NV_ENC_BUFFER_FORMAT_ARGB` is byte-identical to
    /// the captured texture and its internal conversion beats a pre-pass. `Backend::Amf` — the
    /// GPU path — is not a bridge and takes the blt straight into its own ring texture, whose
    /// format is the encoder's business.
    fn cpu_bridge_format(&self) -> ReadbackFormat {
        match self {
            Backend::X264 { .. } | Backend::IntelCpu { .. } | Backend::AmfCpu { .. } => {
                ReadbackFormat::Nv12
            }
            Backend::Nvenc { .. } | Backend::Intel { .. } | Backend::Amf { .. } => {
                ReadbackFormat::Bgra8
            }
        }
    }

    fn is_cpu_bridge(&self) -> bool {
        matches!(
            self,
            Backend::Nvenc {
                path: EncodePath::CpuBridge,
                ..
            } | Backend::IntelCpu { .. }
                | Backend::AmfCpu { .. }
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
            // AMF stages into its own texture ring, so the prescaler (when there is one) blits
            // straight into the slot the encoder is about to wrap.
            Backend::Amf { encoder } => encoder.encode_texture(raw, scaler, force_idr),
            _ => bail!("encode_gpu called on a CPU-bridge backend"),
        }
    }

    /// Hand the readback to the encoder, in the format the reader actually produced.
    ///
    /// `format` is not `self.cpu_bridge_format()`: that is what the backend *asks* the scaler
    /// for, and only the scaler path can honour it. The two no-scaler routes — the staging
    /// readback of a captured frame and `TextureReader` — read the source texture's own format,
    /// which is BGRA. Guessing here would hand an encoder 1.5 bytes a pixel to read as 4.
    fn encode_cpu(
        &mut self,
        data: &[u8],
        row_pitch: u32,
        format: ReadbackFormat,
        force_idr: bool,
    ) -> Result<Vec<u8>> {
        match (self, format) {
            (
                Backend::Nvenc {
                    encoder,
                    path: EncodePath::CpuBridge,
                },
                ReadbackFormat::Bgra8,
            ) => encoder.encode_bgra_padded(data, row_pitch, force_idr),
            (Backend::IntelCpu { encoder }, ReadbackFormat::Bgra8) => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            (Backend::IntelCpu { encoder }, ReadbackFormat::Nv12) => {
                encoder.encode_nv12_padded(data, row_pitch, force_idr)
            }
            (Backend::AmfCpu { encoder }, ReadbackFormat::Bgra8) => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            (Backend::AmfCpu { encoder }, ReadbackFormat::Nv12) => {
                encoder.encode_nv12_padded(data, row_pitch, force_idr)
            }
            (Backend::X264 { encoder }, ReadbackFormat::Bgra8) => {
                encoder.encode_bgra_padded(data, row_pitch, force_idr)
            }
            (Backend::X264 { encoder }, ReadbackFormat::Nv12) => {
                encoder.encode_nv12_padded(data, row_pitch, force_idr)
            }
            (backend, format) => bail!(
                "encode_cpu: {} cannot take a {} readback",
                backend.name(),
                format.name()
            ),
        }
    }

    fn encode_repeat(&mut self, force_idr: bool) -> Result<Vec<u8>> {
        match self {
            Backend::Nvenc { encoder, .. } => encoder.encode_repeat(force_idr),
            Backend::Intel { encoder } => encoder.encode_repeat(force_idr),
            Backend::IntelCpu { encoder } => encoder.encode_repeat(force_idr),
            Backend::Amf { encoder } | Backend::AmfCpu { encoder } => {
                encoder.encode_repeat(force_idr)
            }
            Backend::X264 { encoder } => encoder.encode_repeat(force_idr),
        }
    }
}

struct EncodeCore {
    backend: Backend,
    scaler: Option<Scaler>,
    reader: Option<TextureReader>,
    /// Reused readback texture for the WGC classic CPU-bridge path. Rebuilt only when the
    /// captured content size or format changes, which is a virtual-display mode change.
    staging: Option<StagingTexture>,
    target_bitrate: Arc<AtomicU32>,
    idr_request: Arc<AtomicBool>,
    current_bitrate: u32,
    bitrate_warned: bool,
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
            &mut self.bitrate_warned,
        );
        let Self {
            backend,
            scaler,
            staging,
            ..
        } = self;

        let au = if backend.is_cpu_bridge() {
            if let Some(scaler) = scaler {
                let format = scaler.format();
                scaler.scale(frame.as_raw_texture())?;
                let (data, row_pitch) = scaler.read_back()?;
                backend.encode_cpu(data, row_pitch, format, force_idr)?
            } else {
                let desc = *frame.desc();
                let stale = staging.as_ref().is_none_or(|s| {
                    let d = s.desc();
                    d.Width != desc.Width || d.Height != desc.Height || d.Format != desc.Format
                });
                if stale {
                    *staging = Some(
                        StagingTexture::new(frame.device(), desc.Width, desc.Height, desc.Format)
                            .context("creating the reusable CPU-bridge readback texture")?,
                    );
                }
                let staging = staging
                    .as_mut()
                    .ok_or_else(|| anyhow!("cpu-bridge readback texture missing"))?;
                let mut fb = frame.buffer_with(staging)?;
                let row_pitch = fb.row_pitch();
                // No scaler, so no video processor: this is the captured texture's own format.
                backend.encode_cpu(
                    fb.as_raw_buffer(),
                    row_pitch,
                    ReadbackFormat::Bgra8,
                    force_idr,
                )?
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
            &mut self.bitrate_warned,
        );
        let Self {
            backend,
            scaler,
            reader,
            ..
        } = self;

        let au = if backend.is_cpu_bridge() {
            let (format, (data, row_pitch)) = if let Some(scaler) = scaler {
                let format = scaler.format();
                scaler.scale(tex)?;
                (format, scaler.read_back()?)
            } else {
                let reader = reader
                    .as_mut()
                    .ok_or_else(|| anyhow!("cpu-bridge texture encode without a TextureReader"))?;
                (ReadbackFormat::Bgra8, reader.read_back(tex)?)
            };
            backend.encode_cpu(data, row_pitch, format, force_idr)?
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
            &mut self.bitrate_warned,
        );
        let au = self.backend.encode_repeat(force_idr)?;
        self.frame_index += 1;
        Ok(Some(au))
    }
}

struct LiveCapture {
    core: Option<Arc<Mutex<EncodeCore>>>,
    ring: Option<AsyncNvencRing>,
    tx: broadcast::Sender<EncodedFrame>,
    epoch: Instant,
    last_frame_at: Arc<AtomicU64>,
    path_name: &'static str,
    frames_sent: u64,
    busy_streak: u32,
    stop: Arc<AtomicBool>,
    frame_pool_buffers: u32,
    /// Incremented by the frame pool for every frame it discards while draining to the newest.
    /// Folded into telemetry from the callback, which is the only place that can see it move.
    stale_frames: Arc<AtomicU32>,
    stale_frames_reported: u32,
    /// True when this build exposes `DirtyRegionMode`, so `Frame::dirty_regions()` is meaningful.
    dirty_region_reporting: bool,
    unchanged_streak: u32,
    _thread_tuning: tuning::ThreadTuning,
    _keep_awake: tuning::KeepAwake,
    telemetry: SharedTelemetry,
}

impl Drop for LiveCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

fn build_zero_copy_ring(
    config: EncoderConfig,
    igpu_device: &ID3D11Device,
    ring: usize,
) -> Result<(Encoder, Vec<(ID3D11Texture2D, IDXGIKeyedMutex)>)> {
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

struct ReadyMsg {
    slot: usize,
    force_idr: bool,
    capture: Instant,
    timing: FrameTiming,
}

/// A slot the encode thread could not acquire (GPU hang, TDR, driver fault) is wedged at
/// `KEY_ENCODER` and nobody will ever release it. The producer must know, or it pulls the slot
/// out of the free pool and blocks the capture callback on a mutex with no owner. `K` is at most
/// a handful, so a bitmask is enough and can be read from the capture thread with one load.
#[derive(Default)]
struct PoisonedSlots(AtomicU32);

impl PoisonedSlots {
    fn mark(&self, slot: usize) {
        if slot < 32 {
            self.0.fetch_or(1 << slot, Ordering::Relaxed);
        }
    }

    fn contains(&self, slot: usize) -> bool {
        slot < 32 && self.0.load(Ordering::Relaxed) & (1 << slot) != 0
    }

    fn count(&self) -> u32 {
        self.0.load(Ordering::Relaxed).count_ones()
    }
}

struct AsyncNvencRing {
    shared_igpu: Vec<ID3D11Texture2D>,
    igpu_mutex: Vec<IDXGIKeyedMutex>,
    igpu_context: ID3D11DeviceContext,
    scaler: Option<Scaler>,
    free_slots: crossbeam_channel::Sender<usize>,
    free_recv: crossbeam_channel::Receiver<usize>,
    ready: crossbeam_channel::Sender<ReadyMsg>,
    encode_join: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    last_frame_at: Arc<AtomicU64>,
    idr_request: Arc<AtomicBool>,
    poisoned: Arc<PoisonedSlots>,
    slots: usize,
    epoch: Instant,
    frame_index: u64,
    dropped: u64,
    dropped_reported: u64,
    stage_timeout_ms: u32,
    telemetry: SharedTelemetry,
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
        wake_rx: crossbeam_channel::Receiver<()>,
        frame_duration: Duration,
        epoch: Instant,
        telemetry: SharedTelemetry,
    ) -> Self {
        let k = pairs.len();
        // The keepalive path re-encodes the held slot without going through `stage()`, which is
        // where `idr_request` is otherwise consumed. Hand the encoder the same atomic so a PLI
        // on a static desktop produces an IDR rather than another P frame.
        let mut encoder = encoder;
        encoder.attach_idr_flag(Arc::clone(&idr_request));
        let (shared_igpu, igpu_mutex): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();

        let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(k);
        let (ready_tx, ready_rx) = crossbeam_channel::bounded::<ReadyMsg>(k);
        for slot in 0..k {
            free_tx.send(slot).expect("prefill free_slots");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let poisoned = Arc::new(PoisonedSlots::default());

        let encode_join = {
            let args = RingEncodeArgs {
                encoder,
                ready_rx,
                free_tx: free_tx.clone(),
                tx,
                target_bitrate,
                initial_bitrate,
                idr_request: Arc::clone(&idr_request),
                wake_rx,
                frame_duration,
                stop: Arc::clone(&stop),
                poisoned: Arc::clone(&poisoned),
                telemetry: Arc::clone(&telemetry),
            };
            std::thread::Builder::new()
                .name("nvenc-ring-encode".to_string())
                .spawn(move || ring_encode_thread(args))
                .expect("spawn nvenc ring encode thread")
        };

        // A capture-side acquire only ever waits for a slot the encoder has already released, so
        // it should be instantaneous. `KEY_TIMEOUT_MS` (1 s) here would turn a GPU hiccup into a
        // full second of a blocked WGC callback; two frame periods is the useful ceiling, since
        // anything longer has already cost the frame.
        let stage_timeout_ms = (frame_duration.as_millis() as u32)
            .saturating_mul(2)
            .clamp(5, 50);

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
            poisoned,
            slots: k,
            epoch,
            frame_index: 0,
            dropped: 0,
            dropped_reported: 0,
            stage_timeout_ms,
            telemetry,
        }
    }

    /// Pulls a usable free slot, discarding any the encode thread has quarantined. A poisoned
    /// slot is never returned to the pool, so this drains at most once per poisoning.
    fn take_free_slot(&mut self) -> Option<usize> {
        while let Ok(slot) = self.free_recv.try_recv() {
            if !self.poisoned.contains(slot) {
                return Some(slot);
            }
        }
        None
    }

    fn note_drop(&mut self) {
        self.dropped += 1;
        self.telemetry
            .degradation
            .frames_dropped_ring
            .fetch_add(1, Ordering::Relaxed);
        // Rate-limited so a sustained overload does not put a logbus line (and therefore a UI
        // IPC event) on every dropped frame.
        if self.dropped - self.dropped_reported >= 120 {
            self.dropped_reported = self.dropped;
            let live = self.slots as u32 - self.poisoned.count();
            teprintln!(
                "ring: {} frames dropped for want of a free slot (live slots {live}/{}); \
                 the encoder is not keeping up",
                self.dropped,
                self.slots
            );
        }
    }

    fn stage(&mut self, raw: &ID3D11Texture2D, mut timing: FrameTiming) -> Result<bool> {
        let Some(slot) = self.take_free_slot() else {
            // Every slot is still in the encoder: this frame is dropped outright, which is the
            // one place the ring discards captured pixels.
            self.note_drop();
            return Ok(false);
        };

        let mutex = &self.igpu_mutex[slot];
        let dst = &self.shared_igpu[slot];

        if let Err(e) = unsafe { mutex.AcquireSync(KEY_WRITER, self.stage_timeout_ms) }
            .context("ring iGPU keyed mutex AcquireSync(writer)")
        {
            // Nobody released it within two frame periods; treat the slot as wedged rather than
            // handing it back for the next frame to block on all over again.
            self.poisoned.mark(slot);
            self.note_drop();
            return Err(e);
        }

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
            teprintln!(
                "ring stage copy failed (slot={slot}): {e:?}; encoding stale slot to recycle"
            );
        }
        if let Err(e) = released {
            // The slot is still held at `KEY_WRITER` by this device and the encoder will never
            // get it. Returning it to the free pool would wedge the next capture callback.
            self.poisoned.mark(slot);
            return Err(e).context("ring iGPU keyed mutex ReleaseSync(encoder)");
        }

        timing.mark_scale_done();
        let force_idr = self.frame_index == 0 || self.idr_request.swap(false, Ordering::Relaxed);
        if force_idr {
            self.telemetry
                .degradation
                .idrs_sent
                .fetch_add(1, Ordering::Relaxed);
        }
        self.frame_index += 1;
        let msg = ReadyMsg {
            slot,
            force_idr,
            capture: Instant::now(),
            timing,
        };
        if self.ready.send(msg).is_err() {
            let _ = self.free_slots.send(slot);
            bail!("ring encode thread disconnected");
        }
        self.last_frame_at
            .store(self.epoch.elapsed().as_millis() as u64, Ordering::Relaxed);
        Ok(true)
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.encode_join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AsyncNvencRing {
    fn drop(&mut self) {
        self.stop();
    }
}

struct RingEncodeArgs {
    encoder: Encoder,
    ready_rx: crossbeam_channel::Receiver<ReadyMsg>,
    free_tx: crossbeam_channel::Sender<usize>,
    tx: broadcast::Sender<EncodedFrame>,
    target_bitrate: Arc<AtomicU32>,
    initial_bitrate: u32,
    idr_request: Arc<AtomicBool>,
    wake_rx: crossbeam_channel::Receiver<()>,
    frame_duration: Duration,
    stop: Arc<AtomicBool>,
    poisoned: Arc<PoisonedSlots>,
    telemetry: SharedTelemetry,
}

/// What woke the ring encode thread.
enum RingEvent {
    Frame(ReadyMsg),
    /// `Pipeline::request_idr` or `set_target_bitrate` poked the wake channel.
    Wake,
    /// Nothing arrived within the tick.
    Idle,
    ReadyClosed,
    WakeClosed,
}

/// A repeat of the held slot, emitted when the desktop is static.
///
/// Two things make this the only place a keyframe request can land on an idle desktop: NVENC is
/// configured with an infinite GOP and IDR period, so there is no periodic keyframe at all, and
/// `AsyncNvencRing::stage` — the only other reader of `idr_request` — runs only when WGC delivers
/// a new frame. Without this, a client that joined or lost its reference chain while nothing was
/// moving waited for the user to move something.
fn ring_keepalive(
    encoder: &mut Encoder,
    slot: usize,
    force_idr: bool,
    tx: &broadcast::Sender<EncodedFrame>,
    telemetry: &SharedTelemetry,
) {
    if force_idr {
        encoder.request_idr();
    }
    match encoder.encode_repeat_slot(slot) {
        Ok(au) => {
            let mut timing = FrameTiming::repeating();
            timing.mark_encode_submit();
            timing.mark_encode_done();
            telemetry
                .degradation
                .frames_repeated
                .fetch_add(1, Ordering::Relaxed);
            if force_idr {
                telemetry
                    .degradation
                    .idrs_sent
                    .fetch_add(1, Ordering::Relaxed);
            }
            telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(EncodedFrame {
                data: Bytes::from(au),
                capture: Instant::now(),
                timing,
            });
        }
        Err(e) => teprintln!("ring keepalive encode failed (slot={slot}): {e:?}"),
    }
}

fn ring_encode_thread(args: RingEncodeArgs) {
    let RingEncodeArgs {
        mut encoder,
        ready_rx,
        free_tx,
        tx,
        target_bitrate,
        initial_bitrate,
        idr_request,
        wake_rx,
        frame_duration,
        stop,
        poisoned,
        telemetry,
    } = args;

    let _thread_tuning = tuning::tune_current_thread();
    let mut current_bitrate = initial_bitrate;
    let mut bitrate_warned = false;
    const KEEPALIVE: Duration = Duration::from_millis(200);
    // Tick at the frame period rather than the keepalive period so a pending IDR is honoured
    // within one frame even when nothing is moving; a repeat is still only *emitted* every 200 ms.
    let tick = frame_duration.clamp(Duration::from_millis(4), KEEPALIVE);
    let mut last_emit = Instant::now();
    let mut held_slot: Option<usize> = None;
    let mut wake_open = true;

    let release_and_hold = |free_tx: &crossbeam_channel::Sender<usize>,
                            held: &mut Option<usize>,
                            slot: usize|
     -> bool {
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

        let event = if wake_open {
            crossbeam_channel::select! {
                recv(ready_rx) -> msg => match msg {
                    Ok(m) => RingEvent::Frame(m),
                    Err(_) => RingEvent::ReadyClosed,
                },
                recv(wake_rx) -> r => if r.is_ok() { RingEvent::Wake } else { RingEvent::WakeClosed },
                default(tick) => RingEvent::Idle,
            }
        } else {
            match ready_rx.recv_timeout(tick) {
                Ok(m) => RingEvent::Frame(m),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => RingEvent::Idle,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => RingEvent::ReadyClosed,
            }
        };

        let msg = match event {
            RingEvent::ReadyClosed => break,
            RingEvent::WakeClosed => {
                wake_open = false;
                continue;
            }
            RingEvent::Wake | RingEvent::Idle => {
                while wake_rx.try_recv().is_ok() {}
                let Some(slot) = held_slot else { continue };
                if poisoned.contains(slot) {
                    held_slot = None;
                    continue;
                }
                let idr_pending = idr_request.swap(false, Ordering::Relaxed);
                if !idr_pending && last_emit.elapsed() < KEEPALIVE {
                    continue;
                }
                apply_pending_bitrate_atomic(
                    &mut encoder,
                    &target_bitrate,
                    &mut current_bitrate,
                    &mut bitrate_warned,
                );
                ring_keepalive(&mut encoder, slot, idr_pending, &tx, &telemetry);
                last_emit = Instant::now();
                continue;
            }
            RingEvent::Frame(msg) => msg,
        };

        if poisoned.contains(msg.slot) {
            // The producer no longer hands these out, so this can only be a message that was
            // already in flight when the slot was quarantined.
            telemetry
                .degradation
                .frames_dropped_ring
                .fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let mut timing = msg.timing;
        timing.mark_encode_submit();

        apply_pending_bitrate_atomic(
            &mut encoder,
            &target_bitrate,
            &mut current_bitrate,
            &mut bitrate_warned,
        );
        match encoder.encode_input_slot(msg.slot, msg.force_idr) {
            Ok(au) => {
                timing.mark_encode_done();
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture: msg.capture,
                    timing,
                });
                last_emit = Instant::now();
                if !release_and_hold(&free_tx, &mut held_slot, msg.slot) {
                    break;
                }
            }
            Err(SlotEncodeError::Poisoned(e)) => {
                telemetry
                    .degradation
                    .encoder_resets
                    .fetch_add(1, Ordering::Relaxed);
                teprintln!(
                    "ring encode: slot {} poisoned (AcquireSync failed - GPU hang or driver fault): {e:?}",
                    msg.slot
                );
                poisoned.mark(msg.slot);

                let held_poisoned = held_slot.is_some_and(|s| poisoned.contains(s));
                if held_poisoned || held_slot.is_none() {
                    teprintln!(
                        "ring encode: CRITICAL - held slot also poisoned or absent; \
                         ring is unrecoverable, stopping encode thread"
                    );
                    break;
                }
            }
            Err(SlotEncodeError::Encode(e)) => {
                telemetry
                    .degradation
                    .encoder_transient_errors
                    .fetch_add(1, Ordering::Relaxed);
                teprintln!("ring encode failed (slot={}): {e:?}", msg.slot);
                if !release_and_hold(&free_tx, &mut held_slot, msg.slot) {
                    break;
                }
            }
        }
    }
    tprintln!("nvenc ring encode thread stopped");
}

fn apply_pending_bitrate_atomic(
    encoder: &mut Encoder,
    target_bitrate: &AtomicU32,
    current: &mut u32,
    warned: &mut bool,
) {
    let pending = target_bitrate.swap(0, Ordering::Relaxed);
    if pending == 0 || pending == *current {
        return;
    }
    match encoder.set_bitrate(pending) {
        Ok(BitrateApplied::Applied) => {
            tprintln!("adapting bitrate: {} -> {pending} bps", *current);
            *current = pending;
        }
        Ok(BitrateApplied::IgnoredConstantQp) => {
            warn_constant_qp_once(warned);
            *current = pending;
        }
        Err(e) => {
            teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; will retry");
            restore_pending_bitrate(target_bitrate, pending);
        }
    }
}

/// Whether this device can run a `VideoProcessorBlt` at all.
///
/// `Scaler::new` casts the device to `ID3D11VideoDevice` and the context to `ID3D11VideoContext`;
/// both casts fail on a device created without `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` or on an
/// adapter with no video engine. Asking first turns a failed session into a degraded one.
fn video_processor_available(device: &ID3D11Device, context: &ID3D11DeviceContext) -> bool {
    device.cast::<ID3D11VideoDevice>().is_ok() && context.cast::<ID3D11VideoContext>().is_ok()
}

/// The scaler's destination format for a given backend.
///
/// Only a CPU-bridge backend reads the scaled surface back, so only it has an opinion. A GPU-path
/// backend receives the blt straight into its own input texture (`scale_into`), and that
/// texture's format is the encoder's business.
fn scaler_output_format(backend: &Backend) -> ReadbackFormat {
    if backend.is_cpu_bridge() {
        backend.cpu_bridge_format()
    } else {
        ReadbackFormat::Bgra8
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

    let try_amf = || -> Result<Backend> {
        match AmfEncoder::new_on_device(config, device, context) {
            Ok(encoder) => {
                tprintln!(
                    "pipeline: live capture ready -- AMD AMF same-adapter GPU path ({}x{}@{})",
                    config.width,
                    config.height,
                    config.fps
                );
                Ok(Backend::Amf { encoder })
            }
            Err(e) => {
                teprintln!(
                    "AMD AMF same-adapter path unavailable ({e:?}); trying own-device CPU bridge"
                );
                let encoder = AmfEncoder::new(config)
                    .context("AMD AMF unavailable (same-adapter and CPU-bridge both failed)")?;
                tprintln!(
                    "pipeline: live capture ready -- AMD AMF CPU-bridge path ({}x{}@{})",
                    config.width,
                    config.height,
                    config.fps
                );
                Ok(Backend::AmfCpu { encoder })
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
        EncoderVendor::Amd => try_amf().or_else(|ae| {
            teprintln!("AMD AMF unavailable ({ae:?}); falling back to software x264");
            try_x264().map_err(|xe| ae.context(format!("software fallback also failed: {xe:?}")))
        }),
        // AMF sits after Intel deliberately: an Intel-iGPU + Radeon-dGPU laptop should still
        // prefer the same-adapter Quick Sync path, and an all-AMD box never reaches the Intel
        // branch because its capture device is not Intel.
        EncoderVendor::Auto => try_nvenc()
            .or_else(|nv_err| {
                teprintln!("NVENC unavailable ({nv_err:?}); trying Intel Quick Sync");
                try_intel()
                    .map_err(|ie| nv_err.context(format!("Intel fallback also failed: {ie:?}")))
            })
            .or_else(|hw_err| {
                teprintln!("Intel Quick Sync unavailable ({hw_err:?}); trying AMD AMF");
                try_amf().map_err(|ae| hw_err.context(format!("AMD fallback also failed: {ae:?}")))
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
    telemetry: SharedTelemetry,
) {
    let tick = frame_duration.max(Duration::from_millis(8));
    let idle_after_ms = (frame_duration.as_millis() as u64 * 2).max(34);
    let keepalive_ms = 200u64;
    std::thread::Builder::new()
        .name("nvenc-repeat".to_string())
        .spawn(move || {
            let _thread_tuning = tuning::tune_timer_thread();
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
                let mut timing = FrameTiming::repeating();
                timing.mark_encode_submit();
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
                timing.mark_encode_done();
                telemetry
                    .degradation
                    .frames_repeated
                    .fetch_add(1, Ordering::Relaxed);
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture,
                    timing,
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
            telemetry,
        } = ctx.flags;

        let thread_tuning = tuning::tune_current_thread();
        let keep_awake = tuning::KeepAwake::begin();
        let features = wgc_features();

        let epoch = Instant::now();
        let last_frame_at = Arc::new(AtomicU64::new(0));
        let frame_duration = Duration::from_nanos(1_000_000_000 / config.fps.max(1) as u64);
        let stop = Arc::new(AtomicBool::new(false));

        use crate::streamer::config::EncoderVendor;
        const RING: usize = 3;
        let try_ring = matches!(vendor, EncoderVendor::Nvidia | EncoderVendor::Auto);
        if try_ring {
            match build_zero_copy_ring(config, &ctx.device, RING) {
                Ok((encoder, pairs)) => {
                    tuning::raise_d3d11_gpu_priority(encoder.device());

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

                    super::report_adapter_pairing(&ctx.adapter, Some(encoder.device()));
                    telemetry.encoder.set("nvenc-zero-copy-ring".into());
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
                        wake_rx,
                        frame_duration,
                        epoch,
                        Arc::clone(&telemetry),
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
                        frame_pool_buffers: FRAME_POOL_BUFFERS_RING,
                        stale_frames: Arc::new(AtomicU32::new(0)),
                        stale_frames_reported: 0,
                        dirty_region_reporting: features.dirty_region,
                        unchanged_streak: 0,
                        _thread_tuning: thread_tuning,
                        _keep_awake: keep_awake,
                        telemetry,
                    });
                }
                Err(e) => {
                    telemetry
                        .degradation
                        .capture_fallback
                        .fetch_add(1, Ordering::Relaxed);
                    teprintln!(
                        "NVENC zero-copy ring unavailable ({e:?}); falling back to classic backend path"
                    );
                }
            }
        }

        let backend = build_backend(
            config,
            vendor,
            native_w,
            native_h,
            &ctx.device,
            &ctx.device_context,
        )?;

        // Same as the DXGI path: an equal-size display needs no blt.
        let needs_scaler =
            backend.wants_prescale() && (config.width != native_w || config.height != native_h);
        let scaler = if needs_scaler {
            match Scaler::with_format(
                &ctx.device,
                &ctx.device_context,
                native_w,
                native_h,
                config.width,
                config.height,
                scaler_output_format(&backend),
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

        super::report_adapter_pairing(&ctx.adapter, backend.device());
        let path_name = backend.name();
        telemetry.encoder.set(path_name.into());
        let core = Arc::new(Mutex::new(EncodeCore {
            backend,
            scaler,
            reader: None,
            staging: None,
            target_bitrate: Arc::clone(&target_bitrate),
            idr_request: Arc::clone(&idr_request),
            current_bitrate: config.bitrate_bps,
            bitrate_warned: false,
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
            Arc::clone(&telemetry),
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
            frame_pool_buffers: FRAME_POOL_BUFFERS_INLINE,
            stale_frames: Arc::new(AtomicU32::new(0)),
            stale_frames_reported: 0,
            dirty_region_reporting: features.dirty_region,
            unchanged_streak: 0,
            _thread_tuning: thread_tuning,
            _keep_awake: keep_awake,
            telemetry,
        })
    }

    fn frame_pool_buffers(&self) -> u32 {
        self.frame_pool_buffers
    }

    fn stale_frame_counter(&self) -> Option<Arc<AtomicU32>> {
        Some(Arc::clone(&self.stale_frames))
    }

    fn on_frame_arrived(
        &mut self,
        frame: &mut Frame,
        _control: InternalCaptureControl,
    ) -> Result<(), Self::Error> {
        let capture = Instant::now();
        let mut timing = FrameTiming::entering_capture();
        // WGC's own frame time. Discarding it meant every "capture latency" number started at
        // the moment the message pump reached us, hiding whatever the frame spent in the pool.
        timing.capture_available_ns = frame
            .timestamp()
            .map(|ts| super::clock::wgc_timespan_to_host_ns(ts.Duration))
            .unwrap_or(0);

        // The DXGI backend has skipped unchanged frames since `present_region_is_empty` landed;
        // WGC had no equivalent and re-encoded every delivered frame. `DirtyRegionMode::ReportOnly`
        // (build 26100+, requested only where the property exists) gives the same information, and
        // the repeater/keepalive already emits a frame every 200 ms so a skipped frame is not a
        // stalled client. The consecutive cap is the same safety valve the DXGI path uses, for a
        // driver that under-reports.
        if self.dirty_region_reporting {
            if let Ok(regions) = frame.dirty_regions() {
                if regions.is_empty() && self.unchanged_streak < MAX_CONSECUTIVE_UNCHANGED {
                    self.unchanged_streak += 1;
                    return Ok(());
                }
                self.unchanged_streak = 0;
            }
        }

        let stale = self.stale_frames.load(Ordering::Relaxed);
        if stale != self.stale_frames_reported {
            self.telemetry.degradation.frames_dropped_stale.fetch_add(
                (stale - self.stale_frames_reported) as u64,
                Ordering::Relaxed,
            );
            self.stale_frames_reported = stale;
        }

        if let Some(ring) = self.ring.as_mut() {
            match ring.stage(frame.as_raw_texture(), timing) {
                Ok(_staged) => {}
                Err(e) => {
                    teprintln!("ring capture stage failed: {e:?}");
                }
            }
            return Ok(());
        }

        // No panics past this point: we are inside a WinRT `TypedEventHandler`, so an unwind
        // would cross `extern "system"` frames. Returning `Err` halts the capture cleanly.
        let core = self
            .core
            .as_ref()
            .ok_or_else(|| anyhow!("classic capture path has no EncodeCore"))?;
        let encode_res = {
            let mut core = core
                .lock()
                .map_err(|_| anyhow!("encode core mutex poisoned; stopping capture"))?;
            timing.mark_encode_submit();
            core.encode_captured(frame)
        };
        timing.mark_encode_done();
        let au = match encode_res {
            Ok(au) => au,
            Err(e)
                if is_transient_encode_error(&e)
                    && self.busy_streak < MAX_TRANSIENT_ENCODE_DROPS =>
            {
                self.busy_streak += 1;
                self.telemetry
                    .degradation
                    .encoder_transient_errors
                    .fetch_add(1, Ordering::Relaxed);
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
        self.telemetry
            .frames_encoded
            .fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(EncodedFrame {
            data: Bytes::from(au),
            capture,
            timing,
        });
        self.frames_sent += 1;
        Ok(())
    }

    fn on_closed(&mut self) -> Result<(), Self::Error> {
        teprintln!("pipeline: capture item closed (display disconnected?)");
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn synthetic_pattern_loop(
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    config: EncoderConfig,
    wake_rx: crossbeam_channel::Receiver<()>,
    telemetry: SharedTelemetry,
    workload: crate::streamer::synthetic::Workload,
    counter: bool,
) {
    let _thread_tuning = tuning::tune_current_thread();
    let counter_epoch = Instant::now();

    let mut encoder = match Encoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            teprintln!("encode thread: failed to create NVENC encoder ({e:?}); pipeline stopped");
            return;
        }
    };
    tuning::raise_d3d11_gpu_priority(encoder.device());
    let mut current_bitrate = config.bitrate_bps;
    let mut bitrate_warned = false;

    let frame_interval = Duration::from_nanos(1_000_000_000 / SP_FPS as u64);
    let mut frame_buf = vec![0u8; (SP_WIDTH * SP_HEIGHT * 4) as usize];
    let mut frame_index: u32 = 0;
    let mut next_deadline = Instant::now();

    tprintln!(
        "pipeline: synthetic pattern encode loop started ({SP_WIDTH}x{SP_HEIGHT}@{SP_FPS}, \
         workload={}, counter={counter})",
        workload.name()
    );

    loop {
        let pending = target_bitrate.swap(0, Ordering::Relaxed);
        if pending != 0 && pending != current_bitrate {
            match encoder.set_bitrate(pending) {
                Ok(BitrateApplied::Applied) => {
                    tprintln!("adapting bitrate: {current_bitrate} -> {pending} bps");
                    current_bitrate = pending;
                }
                Ok(BitrateApplied::IgnoredConstantQp) => {
                    warn_constant_qp_once(&mut bitrate_warned);
                    current_bitrate = pending;
                }
                Err(e) => {
                    teprintln!("set_bitrate failed (target_bps={pending}): {e:?}; keeping current")
                }
            }
        }

        let force_idr = frame_index == 0 || idr_request.swap(false, Ordering::Relaxed);
        crate::streamer::synthetic::fill(
            &mut frame_buf,
            SP_WIDTH,
            SP_HEIGHT,
            frame_index,
            workload,
        );
        if counter {
            crate::streamer::synthetic::draw_counter(
                &mut frame_buf,
                SP_WIDTH,
                SP_HEIGHT,
                frame_index,
                counter_epoch.elapsed().as_millis() as u64,
            );
        }

        let mut timing = FrameTiming::entering_capture();
        timing.mark_encode_submit();
        match encoder.encode_bgra(&frame_buf, force_idr) {
            Ok(au) => {
                timing.mark_encode_done();
                telemetry.frames_encoded.fetch_add(1, Ordering::Relaxed);
                let _ = tx.send(EncodedFrame {
                    data: Bytes::from(au),
                    capture: Instant::now(),
                    timing,
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
