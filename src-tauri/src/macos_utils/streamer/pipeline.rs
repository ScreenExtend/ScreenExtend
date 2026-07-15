use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use bytes::Bytes;
use objc2_core_foundation::CFRetained;
use objc2_core_video::CVPixelBuffer;
use tokio::sync::broadcast;

use crate::streamer::config::{Config, H264Profile};

use super::config::{EncoderConfig, live_encoder_config};
use super::display;
use super::encoder::Encoder;
use super::frame::{FrameSource, frame_channel};
use super::gpu::Gpu;
use super::{CaptureBackend, CaptureConfig, DisplayId, PIXEL_FORMAT_420F, start_capture};

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub capture: Instant,
}

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

pub struct SessionCapture {
    pub pipeline: Pipeline,
    control: Option<Box<dyn CaptureBackend>>,
    stop: Arc<AtomicBool>,
    encode_thread: Option<std::thread::JoinHandle<()>>,
}

impl SessionCapture {
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(mut backend) = self.control.take() {
            backend.stop();
        }
        if let Some(handle) = self.encode_thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for SessionCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn spawn_pipeline(
    enc: EncoderConfig,
    source: FrameSource,
) -> Result<(Pipeline, Arc<AtomicBool>, std::thread::JoinHandle<()>)> {
    let (tx, _rx) = broadcast::channel::<EncodedFrame>(BROADCAST_CAPACITY);
    let idr_request = Arc::new(AtomicBool::new(false));
    let target_bitrate = Arc::new(AtomicU32::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let frame_duration = Duration::from_nanos(1_000_000_000 / enc.fps.max(1) as u64);

    let wake = source.wake_sender();
    let pipeline = Pipeline {
        tx: tx.clone(),
        frame_duration,
        idr_request: Arc::clone(&idr_request),
        target_bitrate: Arc::clone(&target_bitrate),
        wake,
        max_bitrate_bps: enc.max_bitrate_bps,
        h264_profile: enc.profile,
    };

    let encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;

    let stop_thread = Arc::clone(&stop);
    let encode_thread = std::thread::Builder::new()
        .name("videotoolbox-encode".to_string())
        .spawn(move || {
            run_encode_loop(
                encoder,
                source,
                tx,
                idr_request,
                target_bitrate,
                stop_thread,
                frame_duration,
            )
        })
        .context("spawn encode thread")?;

    Ok((pipeline, stop, encode_thread))
}

fn run_encode_loop(
    mut encoder: Encoder,
    source: FrameSource,
    tx: broadcast::Sender<EncodedFrame>,
    idr_request: Arc<AtomicBool>,
    target_bitrate: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    frame_duration: Duration,
) {
    super::qos::pin_current_thread_user_interactive();
    super::qos::pin_current_thread_time_constraint(frame_duration.as_nanos() as u64);
    super::qos::raise_current_thread_precedence();
    super::qos::pin_current_thread_encode_affinity();
    let _workgroup = super::qos::FrameWorkgroup::join(frame_duration.as_nanos() as u64);
    let _activity = super::activity::begin_latency_critical_activity();
    let _keep_awake = super::power::KeepAwake::begin();

    let pending_ts: Arc<Mutex<VecDeque<Instant>>> = Arc::new(Mutex::new(VecDeque::new()));

    let drain_ts = Arc::clone(&pending_ts);
    let output = encoder.output();
    let drain = std::thread::Builder::new()
        .name("videotoolbox-drain".to_string())
        .spawn(move || {
            super::qos::pin_current_thread_user_interactive();
            super::qos::raise_current_thread_precedence();
            super::qos::pin_current_thread_encode_affinity();
            while let Ok(sample) = output.recv() {
                let capture = drain_ts.lock().unwrap().pop_front().unwrap_or_else(Instant::now);
                let mut au: Vec<u8> = Vec::new();
                sample.to_annexb(&mut au);
                if au.is_empty() {
                    continue;
                }
                let _ = tx.send(EncodedFrame { data: Bytes::from(au), capture });
            }
        })
        .expect("spawn encode-drain thread");

    let idle = Duration::from_millis(4);
    let keepalive = Duration::from_millis(200);
    let mut last_frame: Option<Arc<super::frame::Frame>> = None;
    let mut last_emit = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        source.wait(idle);
        let force_idr = idr_request.swap(false, Ordering::Relaxed);
        let pending = target_bitrate.swap(0, Ordering::Relaxed);
        if pending != 0 {
            if let Err(e) = encoder.set_bitrate(pending) {
                teprintln!("set_bitrate failed (target_bps={pending}): {e:?}");
            }
        }

        if let Some(frame) = source.try_take_latest() {
            if let Some(pixbuf) = frame.pixel_buffer() {
                if !submit_one(&mut encoder, &pending_ts, frame.captured_at, pixbuf, force_idr) {
                    teprintln!("encode submit failed; stopping encode loop");
                    break;
                }
                if !encoder.low_latency() && !source.peek_has_frame() {
                    let _ = submit_one(&mut encoder, &pending_ts, frame.captured_at, pixbuf, false);
                }
                last_emit = Instant::now();
            }
            last_frame = Some(frame);
        } else if force_idr || last_emit.elapsed() >= keepalive {
            if let Some(frame) = last_frame.as_ref() {
                if let Some(pixbuf) = frame.pixel_buffer() {
                    let captured = frame.captured_at;
                    if !submit_one(&mut encoder, &pending_ts, captured, pixbuf, force_idr) {
                        break;
                    }
                    if !encoder.low_latency() {
                        let _ = submit_one(&mut encoder, &pending_ts, captured, pixbuf, false);
                    }
                    last_emit = Instant::now();
                }
            }
        }
    }

    let _ = encoder.flush();
    drop(encoder);
    let _ = drain.join();
}

fn submit_one(
    encoder: &mut Encoder,
    pending_ts: &Mutex<VecDeque<Instant>>,
    captured_at: Instant,
    pixbuf: &objc2_core_video::CVImageBuffer,
    force_idr: bool,
) -> bool {
    pending_ts.lock().unwrap().push_back(captured_at);
    if let Err(e) = encoder.submit(pixbuf, force_idr) {
        teprintln!("encode submit failed: {e:?}");
        pending_ts.lock().unwrap().pop_back();
        return false;
    }
    true
}

pub fn start(cfg: &Config) -> Result<Pipeline> {
    super::ensure_screen_recording_access();
    let display = select_display(cfg.monitor)?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let enc = live_encoder_config(native_w, native_h, refresh, cfg);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();

    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let backend = start_capture(display, gpu, sink, cap_cfg).context("starting capture backend")?;

    let (pipeline, _stop, _encode_thread) = spawn_pipeline(enc, source)?;
    std::mem::forget(backend);
    Ok(pipeline)
}

pub fn start_on_monitor(cfg: &Config, device_name: &str) -> Result<SessionCapture> {
    super::ensure_screen_recording_access();
    let display = display_by_name(device_name)
        .with_context(|| format!("resolving display {device_name}"))?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let enc = live_encoder_config(native_w, native_h, refresh, cfg);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();

    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let backend =
        start_capture(display, gpu, sink, cap_cfg).context("starting capture backend")?;

    let (pipeline, stop, encode_thread) = spawn_pipeline(enc, source)?;
    Ok(SessionCapture {
        pipeline,
        control: Some(backend),
        stop,
        encode_thread: Some(encode_thread),
    })
}

pub fn probe_bitrate(cfg: &Config) -> Result<()> {
    let (w, h) = (1280u32, 720u32);
    let low = 2_000_000u32;
    let high = 8_000_000u32;
    let enc = EncoderConfig {
        width: w,
        height: h,
        fps: 60,
        bitrate_bps: low,
        max_bitrate_bps: high,
        profile: cfg.h264_profile,
        qp: cfg.qp,
        intra_refresh: cfg.intra_refresh,
    };
    let mut encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;
    let out = encoder.output();

    let phase = |encoder: &mut Encoder, out: &crossbeam_channel::Receiver<super::encoder::CompressedSample>, tick0: usize| -> usize {
        let mut buf: Vec<u8> = Vec::new();
        let mut bytes = 0usize;
        for i in 0..120usize {
            if let Some(pb) = make_synthetic_bgra(w as usize, h as usize, tick0 + i) {
                let _ = encoder.submit(&pb, i == 0);
                if !encoder.low_latency() {
                    let _ = encoder.submit(&pb, false);
                }
            }
            while let Ok(cs) = out.try_recv() {
                cs.to_annexb(&mut buf);
                bytes += buf.len();
            }
        }
        let _ = encoder.flush();
        while let Ok(cs) = out.try_recv() {
            cs.to_annexb(&mut buf);
            bytes += buf.len();
        }
        bytes
    };

    let low_bytes = phase(&mut encoder, &out, 0);
    encoder.set_bitrate(high).context("set_bitrate")?;
    let high_bytes = phase(&mut encoder, &out, 1000);

    let kbps = |bytes: usize| (bytes as f64 * 8.0 / 2.0) / 1000.0;
    tprintln!(
        "[probe-bitrate] {w}x{h}@60: low target={} kbps achieved≈{:.0} kbps; \
         high target={} kbps achieved≈{:.0} kbps",
        low / 1000,
        kbps(low_bytes),
        high / 1000,
        kbps(high_bytes)
    );
    Ok(())
}

pub fn probe_live(cfg: &Config, path: &str) -> Result<()> {
    use std::io::Write;

    let display = select_display(cfg.monitor)?;
    let (native_w, native_h, refresh) = display_geometry(display);
    let enc = live_encoder_config(native_w, native_h, refresh, cfg);

    let gpu = Gpu::new().context("initializing Metal GPU core")?;
    let (sink, source) = frame_channel();
    let cap_fps = enc.fps.max(if refresh == 0 { 60 } else { refresh });
    let cap_cfg = CaptureConfig {
        width: enc.width as usize,
        height: enc.height as usize,
        fps: cap_fps as i32,
        pixel_format: PIXEL_FORMAT_420F,
    };
    let mut backend =
        start_capture(display, gpu, sink, cap_cfg).context("starting capture backend")?;
    let mut encoder = Encoder::new(enc).context("creating VideoToolbox encoder")?;
    let out = encoder.output();

    let mut file = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    let mut buf: Vec<u8> = Vec::new();
    let mut frames = 0usize;
    let mut bytes = 0usize;
    let mut first = true;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && frames < 150 {
        source.wait(Duration::from_millis(8));
        if let Some(frame) = source.try_take_latest() {
            if let Some(pixbuf) = frame.pixel_buffer() {
                encoder.submit(pixbuf, first).context("submit")?;
                first = false;
                if !encoder.low_latency() {
                    let _ = encoder.submit(pixbuf, false);
                }
            }
        }
        while let Ok(cs) = out.try_recv() {
            cs.to_annexb(&mut buf);
            if !buf.is_empty() {
                file.write_all(&buf)?;
                bytes += buf.len();
                frames += 1;
            }
        }
    }
    encoder.flush().ok();
    while let Ok(cs) = out.try_recv() {
        cs.to_annexb(&mut buf);
        if !buf.is_empty() {
            file.write_all(&buf)?;
            bytes += buf.len();
            frames += 1;
        }
    }
    backend.stop();

    tprintln!("[probe-live] wrote {frames} frames, {bytes} bytes to {path}");
    Ok(())
}

fn make_synthetic_bgra(w: usize, h: usize, tick: usize) -> Option<CFRetained<CVPixelBuffer>> {
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };
    const BGRA: u32 = 0x4247_5241; // kCVPixelFormatType_32BGRA
    let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
    let r = unsafe {
        CVPixelBufferCreate(
            objc2_core_foundation::kCFAllocatorDefault,
            w,
            h,
            BGRA,
            None,
            std::ptr::NonNull::from(&mut out),
        )
    };
    let out = std::ptr::NonNull::new(out)?;
    if r != 0 {
        return None;
    }
    let pb = unsafe { CFRetained::from_raw(out) };
    unsafe {
        CVPixelBufferLockBaseAddress(&pb, CVPixelBufferLockFlags(0));
        let base = CVPixelBufferGetBaseAddress(&pb) as *mut u8;
        if !base.is_null() {
            let bpr = CVPixelBufferGetBytesPerRow(&pb);
            for y in 0..h {
                let row = base.add(y * bpr);
                for x in 0..w {
                    let p = row.add(x * 4);
                    *p = (x + tick) as u8;
                    *p.add(1) = (y + tick) as u8;
                    *p.add(2) = tick as u8;
                    *p.add(3) = 255;
                }
            }
        }
        CVPixelBufferUnlockBaseAddress(&pb, CVPixelBufferLockFlags(0));
    }
    Some(pb)
}

use crate::macos_utils::virtual_display::reconfigure_display;

const HIDPI_THRESHOLD_PERCENT: u32 = 150;

fn select_display(monitor: u32) -> Result<DisplayId> {
    display::select_display(monitor).context("no active display to capture")
}

fn display_by_name(device_name: &str) -> Result<DisplayId> {
    display::display_by_name(device_name)
        .with_context(|| format!("display {device_name} is not active"))
}

fn display_geometry(display: DisplayId) -> (u32, u32, u32) {
    display::display_geometry(display)
}

pub fn monitor_device_names() -> Vec<String> {
    display::device_names()
}

pub fn monitor_dimensions(device_name: &str) -> Option<(u32, u32)> {
    display::dimensions_by_name(device_name)
}

pub fn monitor_rect(_device_name: &str) -> Option<(i32, i32, u32, u32)> {
    None
}

fn pixel_dims(device_name: &str) -> Result<(u32, u32, u32)> {
    let id = display::display_by_name(device_name)
        .with_context(|| format!("display {device_name} is not active"))?;
    let (w, h, refresh) = display::display_geometry(id);
    if w == 0 || h == 0 {
        anyhow::bail!("display {device_name} reports zero geometry");
    }
    Ok((w, h, refresh))
}

pub fn set_display_resolution(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
) -> Result<()> {
    reconfigure_display(device_name, width, height, refresh, 0).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_mode(
    device_name: &str,
    width: u32,
    height: u32,
    refresh: u32,
    portrait: bool,
) -> Result<()> {
    let (w, h) = if portrait { (height, width) } else { (width, height) };
    reconfigure_display(device_name, w, h, refresh, 0).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_orientation(device_name: &str, portrait: bool) -> Result<()> {
    let (w, h, refresh) = pixel_dims(device_name)?;
    let is_portrait_now = h > w;
    if is_portrait_now == portrait {
        return Ok(());
    }
    reconfigure_display(device_name, h, w, refresh, 0).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_scale(device_name: &str, percent: u32) -> Result<()> {
    let (w, h, refresh) = pixel_dims(device_name)?;
    let hidpi = if percent >= HIDPI_THRESHOLD_PERCENT { 1 } else { 0 };
    reconfigure_display(device_name, w, h, refresh, hidpi).map_err(|e| anyhow::anyhow!(e))
}

pub fn set_display_topology_extend() {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::config::H264Profile;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::time::Duration;

    use super::super::cgds::CgDisplayStreamBackend;
    use super::super::config::EncoderConfig;
    use super::super::encoder::Encoder;
    use super::super::mach::{mach_age_ms, mach_now};
    use super::super::{CaptureBackend, CaptureConfig, PIXEL_FORMAT_420F};

    #[test]
    #[ignore]
    fn end_to_end_latency() {
        let display = display::select_display(0).expect("main display");
        let (w, h, refresh) = display::display_geometry(display);
        let fps = if refresh == 0 { 60 } else { refresh };
        println!("display {display}: {w}x{h} @ {fps}fps");

        let gpu = Gpu::new().unwrap();
        let (sink, source) = frame_channel();
        let cap_cfg = CaptureConfig {
            width: w as usize,
            height: h as usize,
            fps: fps as i32,
            pixel_format: PIXEL_FORMAT_420F,
        };
        let mut backend = CgDisplayStreamBackend::new(display, gpu, sink, cap_cfg).unwrap();
        backend.start().expect("capture starts");

        let enc_cfg = EncoderConfig {
            width: (w & !1),
            height: (h & !1),
            fps,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 12_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            intra_refresh: false,
        };
        let mut encoder = Encoder::new(enc_cfg).expect("encoder");
        let out = encoder.output();

        let ts: Arc<Mutex<VecDeque<(u64, bool)>>> = Arc::new(Mutex::new(VecDeque::new()));
        let drain_ts = Arc::clone(&ts);
        let drain = std::thread::spawn(move || {
            let mut lat: Vec<f64> = Vec::new();
            let mut bytes = 0usize;
            let mut keyframes = 0usize;
            let mut new_frames = 0usize;
            let mut buf: Vec<u8> = Vec::new();
            while let Ok(cs) = out.recv() {
                let tag = drain_ts.lock().unwrap().pop_front();
                let is_keyframe = cs.to_annexb(&mut buf);
                if buf.is_empty() {
                    continue;
                }
                if let Some((a, is_new)) = tag {
                    if is_new {
                        lat.push(mach_age_ms(a));
                        new_frames += 1;
                    }
                }
                bytes += buf.len();
                if is_keyframe {
                    keyframes += 1;
                }
            }
            (lat, bytes, keyframes, new_frames)
        });

        super::super::qos::pin_current_thread_user_interactive();
        let idle = Duration::from_millis(1);
        let keepalive_budget = 4u32;
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        let mut first = true;
        let mut submitted = 0usize;
        let mut last_frame: Option<std::sync::Arc<super::super::frame::Frame>> = None;
        let mut budget = 0u32;
        while std::time::Instant::now() < deadline {
            source.wait(idle);
            if let Some(frame) = source.try_take_latest() {
                if let Some(pixbuf) = frame.pixel_buffer() {
                    ts.lock().unwrap().push_back((frame.arrived_mach, true));
                    encoder.submit(pixbuf, first).expect("submit");
                    first = false;
                    submitted += 1;
                    last_frame = Some(frame);
                    budget = keepalive_budget; // refill the flush burst
                }
            } else if budget > 0 {
                if let Some(lf) = last_frame.as_ref() {
                    if let Some(pixbuf) = lf.pixel_buffer() {
                        ts.lock().unwrap().push_back((lf.arrived_mach, false));
                        encoder.submit(pixbuf, false).ok();
                    }
                    budget -= 1;
                }
            }
        }
        encoder.flush().ok();
        backend.stop();
        drop(encoder);
        let (mut lat, bytes, keyframes, new_frames) = drain.join().unwrap();

        println!(
            "submitted={submitted} new_frames={new_frames} keyframes={keyframes} bytes={bytes}"
        );
        assert!(submitted > 0, "no frames captured (idle screen? add activity)");
        assert!(!lat.is_empty(), "no encoded frames");
        lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let mean = lat.iter().sum::<f64>() / lat.len() as f64;
        let p50 = lat[lat.len() / 2];
        let p99 = lat[((lat.len() as f64 * 0.99) as usize).min(lat.len() - 1)];
        println!(
            "capture→encoded latency ms (new frames): mean={mean:.3} p50={p50:.3} p99={p99:.3} max={:.3}",
            lat.last().unwrap()
        );
    }

    #[test]
    #[ignore]
    fn production_pipeline_smoke() {
        let display = display::select_display(0).expect("main display");
        let (w, h, refresh) = display::display_geometry(display);
        let fps = if refresh == 0 { 60 } else { refresh };

        let gpu = Gpu::new().unwrap();
        let (sink, source) = frame_channel();
        let cap_cfg = CaptureConfig {
            width: w as usize,
            height: h as usize,
            fps: fps as i32,
            pixel_format: PIXEL_FORMAT_420F,
        };
        let mut backend = CgDisplayStreamBackend::new(display, gpu, sink, cap_cfg).unwrap();
        backend.start().expect("capture starts");

        let enc = EncoderConfig {
            width: w & !1,
            height: h & !1,
            fps,
            bitrate_bps: 12_000_000,
            max_bitrate_bps: 12_000_000,
            profile: H264Profile::Baseline,
            qp: None,
            intra_refresh: false,
        };
        let (pipeline, stop, _encode_thread) = spawn_pipeline(enc, source).expect("pipeline");
        let mut rx = pipeline.tx.subscribe();
        pipeline.request_idr();

        let mut count = 0usize;
        let mut bytes = 0usize;
        let mut lat: Vec<f64> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match rx.try_recv() {
                Ok(ef) => {
                    count += 1;
                    bytes += ef.data.len();
                    lat.push(ef.capture.elapsed().as_secs_f64() * 1000.0);
                }
                Err(_) => std::thread::sleep(Duration::from_micros(500)),
            }
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        backend.stop();

        println!("production pipeline: broadcast {count} EncodedFrames, {bytes} bytes");
        if !lat.is_empty() {
            lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let mean = lat.iter().sum::<f64>() / lat.len() as f64;
            println!(
                "submit→broadcast latency ms: mean={mean:.3} p50={:.3} max={:.3}",
                lat[lat.len() / 2],
                lat.last().unwrap()
            );
        }
        assert!(count > 0, "production pipeline produced no encoded frames");
    }
}
