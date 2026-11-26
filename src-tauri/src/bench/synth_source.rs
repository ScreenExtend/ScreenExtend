//! TEST HOOK: a synthetic frame source in place of screen capture, for REAL sessions.
//!
//! Compiled only with `--features bench`, and active only with `SE_TEST_HOOKS=1` and
//! `SE_TEST_SOURCE=<workload>[:<fps>]` (`stress` or a `streamer::synthetic` workload). Frames go
//! through the `FrameSink` the real backends use, stamped when offered, so everything after
//! capture is the production path and a locked-screen machine can be measured under motion.
//! `static` delivers one frame and then nothing, as a real backend does for an unchanged screen.
//! Only capture is replaced: the virtual display is still created, the permission flow untouched.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant};

use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetIOSurface, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress,
};

use crate::macos_utils::streamer::frame::{Backing, Frame, FrameSink, HeldSurface};
use crate::macos_utils::streamer::{CaptureBackend, CaptureConfig, CaptureError};

use super::cf::Pool;
use super::clip;

/// What `SE_TEST_SOURCE` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    pub workload: String,
    pub fps: Option<u32>,
}

/// `<workload>[:<fps>]`, the workload one this module can render.
pub fn parse(v: &str) -> Result<Spec, String> {
    let (w, fps) = match v.trim().split_once(':') {
        Some((w, f)) => (
            w,
            Some(
                f.trim()
                    .parse::<u32>()
                    .ok()
                    .filter(|&f| (1..=240).contains(&f))
                    .ok_or_else(|| format!("bad frame rate '{f}' (1..=240)"))?,
            ),
        ),
        None => (v.trim(), None),
    };
    let w = w.to_ascii_lowercase();
    if w != "stress" && crate::streamer::synthetic::Workload::parse(&w).is_none() {
        return Err(format!(
            "unknown workload '{w}' (stress, static, scroll, motion, drag, mixed)"
        ));
    }
    Ok(Spec { workload: w, fps })
}

/// The hook: `Some(backend)` only with `SE_TEST_HOOKS=1` and a valid `SE_TEST_SOURCE`.
pub fn from_env(cfg: CaptureConfig, sink: Arc<FrameSink>) -> Option<Box<dyn CaptureBackend>> {
    let v = crate::macos_utils::streamer::vt_recipe::hook_var("SE_TEST_SOURCE")?;
    let spec = match parse(&v) {
        Ok(s) => s,
        Err(e) => {
            teprintln!("[test-hook] SE_TEST_SOURCE={v} ignored: {e}; using real screen capture");
            return None;
        }
    };
    let fps = spec.fps.unwrap_or(cfg.fps.max(1) as u32);
    teprintln!(
        "[test-hook] synthetic source '{}' replaces screen capture ({}x{} @ {fps} fps)",
        spec.workload,
        cfg.width,
        cfg.height
    );
    let mut src = SynthSource::new(spec, cfg, fps, sink);
    match src.start() {
        Ok(()) => Some(Box::new(src)),
        Err(_) => None,
    }
}

/// Frames of one loop, NV12 (`w*h*3/2` each), rendered once.
enum Clip {
    /// The stress clip: a desktop, a window and the window's path (composited per frame, so a
    /// loop of any length costs two pictures of memory).
    Stress {
        desk: Vec<u8>,
        win: Vec<u8>,
        ww: usize,
        wh: usize,
        path: Vec<(usize, usize)>,
    },
    Frames(Vec<Vec<u8>>),
}

fn render(spec: &Spec, w: usize, h: usize, fps: u32) -> Clip {
    if spec.workload == "stress" {
        let ww = (w * 55 / 100) & !1;
        let wh = (h * 60 / 100) & !1;
        let mut desk = vec![0u8; w * h * 3 / 2];
        clip::bgra_to_nv12(&clip::stress_desktop(w, h, 1), w, h, &mut desk);
        let mut win = vec![0u8; ww * wh * 3 / 2];
        clip::bgra_to_nv12(&clip::stress_window(ww, wh, 1), ww, wh, &mut win);
        let path = clip::drag_path(fps as usize * 10, fps as f64, w - ww, h - wh, 1);
        return Clip::Stress {
            desk,
            win,
            ww,
            wh,
            path,
        };
    }
    let wl = crate::streamer::synthetic::Workload::parse(&spec.workload)
        .unwrap_or(crate::streamer::synthetic::Workload::Static);
    // up to two seconds, within ~400 MB
    let pic = w * h * 3 / 2;
    let n = if wl == crate::streamer::synthetic::Workload::Static {
        1
    } else {
        ((fps as usize) * 2).min((400 << 20) / pic.max(1)).max(1)
    };
    let mut bgra = vec![0u8; w * h * 4];
    let frames = (0..n)
        .map(|i| {
            crate::streamer::synthetic::fill(&mut bgra, w as u32, h as u32, i as u32, wl);
            let mut nv12 = vec![0u8; pic];
            clip::bgra_to_nv12(&bgra, w, h, &mut nv12);
            nv12
        })
        .collect();
    Clip::Frames(frames)
}

/// Copy a tightly packed NV12 picture into a pool buffer, honouring its row strides.
fn copy_nv12(pb: &CVPixelBuffer, src: &[u8], w: usize, h: usize) {
    unsafe {
        CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0));
        for (plane, rows, off) in [(0usize, h, 0usize), (1, h / 2, w * h)] {
            let base = CVPixelBufferGetBaseAddressOfPlane(pb, plane) as *mut u8;
            let bpr = CVPixelBufferGetBytesPerRowOfPlane(pb, plane);
            if base.is_null() {
                continue;
            }
            for y in 0..rows {
                std::ptr::copy_nonoverlapping(src.as_ptr().add(off + y * w), base.add(y * bpr), w);
            }
        }
        CVPixelBufferUnlockBaseAddress(pb, CVPixelBufferLockFlags(0));
    }
}

pub struct SynthSource {
    spec: Spec,
    cfg: CaptureConfig,
    fps: u32,
    sink: Arc<FrameSink>,
    stop: Arc<AtomicBool>,
    delivered: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
    name: &'static str,
}

impl SynthSource {
    fn new(spec: Spec, cfg: CaptureConfig, fps: u32, sink: Arc<FrameSink>) -> SynthSource {
        let name: &'static str = Box::leak(format!("synthetic:{}", spec.workload).into_boxed_str());
        SynthSource {
            spec,
            cfg,
            fps,
            sink,
            stop: Arc::new(AtomicBool::new(false)),
            delivered: Arc::new(AtomicU64::new(0)),
            thread: None,
            name,
        }
    }
}

impl CaptureBackend for SynthSource {
    fn start(&mut self) -> Result<(), CaptureError> {
        let (w, h) = (self.cfg.width & !1, self.cfg.height & !1);
        if w < 64 || h < 64 {
            teprintln!("[test-hook] synthetic source needs at least 64x64, got {w}x{h}");
            return Err(CaptureError::StartFailed);
        }
        let pool = Pool::new(w, h, 8).map_err(|st| {
            teprintln!("[test-hook] synthetic source: CVPixelBufferPoolCreate failed: {st}");
            CaptureError::StartFailed
        })?;
        let (spec, fps, sink) = (self.spec.clone(), self.fps, Arc::clone(&self.sink));
        let (stop, delivered) = (Arc::clone(&self.stop), Arc::clone(&self.delivered));
        let handle = std::thread::Builder::new()
            .name("test-synthetic-source".into())
            .spawn(move || {
                crate::macos_utils::streamer::qos::pin_current_thread_user_interactive();
                let clip = render(&spec, w, h, fps);
                let period = Duration::from_nanos(1_000_000_000 / fps.max(1) as u64);
                let mut frame_buf = vec![0u8; w * h * 3 / 2];
                let t0 = Instant::now();
                let mut k: u64 = 0;
                while !stop.load(Relaxed) {
                    let deadline = t0 + period * k as u32;
                    loop {
                        let now = Instant::now();
                        if now >= deadline || stop.load(Relaxed) {
                            break;
                        }
                        let left = deadline - now;
                        if left > Duration::from_micros(2000) {
                            std::thread::sleep(
                                (left - Duration::from_micros(1500)).min(Duration::from_millis(10)),
                            );
                        } else {
                            std::hint::spin_loop();
                        }
                    }
                    let src: &[u8] = match &clip {
                        Clip::Stress {
                            desk,
                            win,
                            ww,
                            wh,
                            path,
                        } => {
                            let (px, py) = path[(k as usize) % path.len()];
                            frame_buf.copy_from_slice(desk);
                            for y in 0..*wh {
                                let d = (py + y) * w + px;
                                frame_buf[d..d + ww].copy_from_slice(&win[y * ww..(y + 1) * ww]);
                            }
                            let (duv, suv) = (w * h, ww * wh);
                            for y in 0..wh / 2 {
                                let d = duv + (py / 2 + y) * w + px;
                                let s = suv + y * ww;
                                frame_buf[d..d + ww].copy_from_slice(&win[s..s + ww]);
                            }
                            &frame_buf
                        }
                        Clip::Frames(f) if f.len() == 1 && k > 0 => {
                            // a still picture: nothing new to deliver
                            k += 1;
                            continue;
                        }
                        Clip::Frames(f) => &f[(k as usize) % f.len()],
                    };
                    k += 1;
                    let Ok(pb) = pool.get() else { continue };
                    copy_nv12(&pb, src, w, h);
                    let Some(surface) = (unsafe { CVPixelBufferGetIOSurface(Some(&pb)) }) else {
                        continue;
                    };
                    let ns = crate::streamer::audio::host_now_ns();
                    sink.publish(Frame {
                        width: w,
                        height: h,
                        arrived_mach: k,
                        captured_at: Instant::now(),
                        os_time_ns: ns,
                        os_time_raw_ns: ns,
                        backing: Backing::PixelBuffer {
                            pixbuf: pb,
                            _surface: HeldSurface::hold(surface),
                        },
                    });
                    delivered.fetch_add(1, Relaxed);
                }
            })
            .map_err(|e| {
                teprintln!("[test-hook] synthetic source: spawn failed: {e}");
                CaptureError::StartFailed
            })?;
        self.thread = Some(handle);
        Ok(())
    }

    fn stop(&mut self) {
        self.stop.store(true, Relaxed);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn frames_captured(&self) -> u64 {
        self.delivered.load(Relaxed)
    }
}

impl Drop for SynthSource {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_value_parses_to_a_workload_and_an_optional_rate() {
        assert_eq!(
            parse("stress"),
            Ok(Spec {
                workload: "stress".into(),
                fps: None
            })
        );
        assert_eq!(
            parse(" Scroll:30 "),
            Ok(Spec {
                workload: "scroll".into(),
                fps: Some(30)
            })
        );
        assert!(parse("static:0").is_err());
        assert!(parse("static:999").is_err());
        assert!(parse("fireworks").is_err());
    }

    #[test]
    #[ignore = "creates IOSurface-backed pixel buffers and runs for ~1 s; run with --ignored --nocapture"]
    fn the_source_delivers_frames_at_its_cadence_and_size() {
        use crate::macos_utils::streamer::frame::frame_channel;
        for (spec, fps, expect) in [("stress", 60u32, 55..=62u64), ("static", 60, 1..=1)] {
            let (sink, source) = frame_channel();
            let cfg = CaptureConfig {
                width: 1280,
                height: 720,
                fps: fps as i32,
                pixel_format: crate::macos_utils::streamer::PIXEL_FORMAT_420F,
            };
            let mut s = SynthSource::new(parse(spec).unwrap(), cfg, fps, sink);
            s.start().expect("starts");
            // let the first frame render (the stress clip renders its desktop first)
            std::thread::sleep(Duration::from_millis(300));
            let n0 = s.frames_captured();
            std::thread::sleep(Duration::from_millis(1000));
            let n = s.frames_captured() - n0;
            let f = source.try_take_latest().expect("a frame was published");
            assert_eq!((f.width, f.height), (1280, 720));
            s.stop();
            println!("{spec}: {n} frames in 1 s ({})", s.name());
            assert!(
                expect.contains(&(n + u64::from(spec == "static"))),
                "{spec}: {n}"
            );
        }
    }
}
