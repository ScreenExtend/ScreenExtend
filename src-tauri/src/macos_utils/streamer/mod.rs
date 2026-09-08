#![allow(dead_code)]
pub mod activity;
pub mod cgds;
pub mod clock;
pub mod config;
pub mod display;
pub mod encoder;
pub mod frame;
pub mod gpu;
pub mod mach;
pub mod pipeline;
pub mod power;
pub mod qos;
pub mod sck;
pub mod tuning;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use objc2::runtime::AnyClass;

use frame::FrameSink;
use gpu::Gpu;

use crate::streamer::telemetry::SharedTelemetry;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error("no Metal device available")]
    NoMetalDevice,
    #[error("failed to create CVMetalTextureCache (CVReturn {0})")]
    TextureCache(i32),
    #[error("could not fetch SCShareableContent")]
    ShareableContent,
    #[error("target display not found")]
    DisplayNotFound,
    #[error("capture stream creation failed")]
    StreamCreateFailed,
    #[error("capture start failed")]
    StartFailed,
    #[error("Screen Recording permission not granted (TCC)")]
    PermissionDenied,
    #[error("{0} not yet implemented (M1/M2): see PRD {1}")]
    NotImplemented(&'static str, &'static str),
    #[error("ScreenCaptureKit failed ({sck}); CGDisplayStream fallback also failed: {cgds}")]
    FallbackFailed { sck: String, cgds: String },
}

pub trait CaptureBackend: Send {
    fn start(&mut self) -> Result<(), CaptureError>;
    fn stop(&mut self);
    /// Which tier actually started, for telemetry. `start_capture` silently falls back from
    /// ScreenCaptureKit to CGDisplayStream, and a measurement that cannot say which one ran is
    /// not a measurement.
    fn name(&self) -> &'static str;
    /// Frames delivered so far. Used to prove a started stream is actually producing, which is
    /// what separates a slow capture path from a dead one.
    fn frames_captured(&self) -> u64;
}

pub type DisplayId = u32;

#[derive(Debug, Clone, Copy)]
pub struct CaptureConfig {
    pub width: usize,
    pub height: usize,
    pub fps: i32,
    pub pixel_format: u32,
}

pub const PIXEL_FORMAT_420F: u32 = 0x3432_3066;
pub const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

/// The OS version, for the human-readable half of `CompatibilityReport` only.
///
/// **Never gate a capability on this.** Every tier on this path is chosen by probing the thing
/// itself — `AnyClass::get` for a class, `dlsym` for a function, an attempted session create for an
/// encoder — because the version number is a proxy for the question and a worse one: it says
/// nothing about a machine whose hardware, drivers or permissions differ inside the same release.
///
/// It reads `NSProcessInfo.operatingSystemVersion` (10.10+, so below the floor) and caches the
/// answer. It used to fork `sw_vers` on every call, several times a second while the permission
/// gate was open, and parse its stdout.
pub fn macos_at_least(major: u32, minor: u32) -> bool {
    static VERSION: OnceLock<(u32, u32)> = OnceLock::new();
    let (maj, min) = *VERSION.get_or_init(|| {
        let v = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
        (v.majorVersion.max(0) as u32, v.minorVersion.max(0) as u32)
    });
    (maj, min) >= (major, minor)
}

/// Whether ScreenCaptureKit exists on this machine.
///
/// Probed by class lookup rather than by version number: `sw_vers` is a subprocess fork that
/// answers a proxy for the question, and it failed *open* — an unparseable version string returned
/// true. `AnyClass::get` is what the SCK backend already uses as its own belt-and-braces guard, so
/// this makes the selector and the guard the same test.
pub fn screencapturekit_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        AnyClass::get(c"SCStream").is_some() && AnyClass::get(c"SCShareableContent").is_some()
    })
}

/// How long a freshly started ScreenCaptureKit stream has to deliver its first frame.
///
/// Generous on purpose. The point is to catch a stream that starts cleanly and delivers *nothing*
/// — the failure Chromium reports for full-display capture on 12.3 — not to race a busy machine
/// into an unnecessary fallback onto the obsoleted API.
const SCK_FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(1500);

/// Called at every pipeline start, which is also every device join and every
/// stream refresh — so it is where a consent that lapsed *mid-session* is noticed.
/// macOS 15 re-asks for screen recording roughly monthly, and a host left running
/// as a permanent second monitor will hit that with a session live.
pub fn ensure_screen_recording_access() -> bool {
    match crate::macos_utils::permissions::screen_recording_live() {
        Some(true) => return true,
        Some(false) => teprintln!(
            "[tcc] Screen Recording is not authorized for this process. macOS 15 and later \
             re-ask about every 30 days, so a session that has been working can stop here. \
             Re-approve ScreenExtend under System Settings → Privacy & Security → Screen \
             Recording; if the toggle is already on, quit and reopen ScreenExtend."
        ),
        None => tprintln!(
            "[tcc] Screen Recording state could not be read; starting capture anyway and \
             letting it report its own failure."
        ),
    }
    type AccessFn = unsafe extern "C" fn() -> bool;
    unsafe {
        // A `c"..."` literal rather than `CString::new(...).unwrap()`: the name is a compile-time
        // constant, so there is nothing to allocate and nothing to fail.
        let p = libc::dlsym(libc::RTLD_DEFAULT, c"CGRequestScreenCaptureAccess".as_ptr());
        if p.is_null() {
            true
        } else {
            std::mem::transmute::<*mut std::ffi::c_void, AccessFn>(p)()
        }
    }
}

/// Liveness, not authorization. An *unauthorized* process still gets a stream and
/// still gets `FrameComplete` frames — they just contain wallpaper — so this must
/// never gate a permission decision. Use `permissions::screen_recording_live()`.
pub fn probe_screen_recording() -> bool {
    if screencapturekit_available() {
        if let Some(granted) = sck::probe_screen_recording() {
            return granted;
        }
        // Only reached when SCK is present and its probe could not answer at all. CGDisplayStream
        // is obsoleted in the macOS 15 SDK and touching it is what raises Sequoia's recurring
        // screen-recording alert, so the reason for reaching for it is worth a line.
        teprintln!(
            "[capture] the ScreenCaptureKit probe was inconclusive; falling back to the \
             deprecated CGDisplayStream probe"
        );
    }
    cgds::probe_screen_recording()
}

pub fn start_capture(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
    telemetry: SharedTelemetry,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    let sck_err = if screencapturekit_available() {
        match start_sck(display, gpu.clone(), sink.clone(), cfg, telemetry.clone()) {
            Ok(backend) => return Ok(backend),
            Err(e) => {
                teprintln!(
                    "[capture] ScreenCaptureKit failed to start for display {display}: {e}; \
                     falling back to CGDisplayStream"
                );
                Some(e)
            }
        }
    } else {
        None
    };

    match start_cgds(display, gpu, sink, cfg, telemetry) {
        Ok(backend) => Ok(backend),
        Err(cgds_err) => match sck_err {
            Some(sck_err) => Err(CaptureError::FallbackFailed {
                sck: sck_err.to_string(),
                cgds: cgds_err.to_string(),
            }),
            None => Err(cgds_err),
        },
    }
}

fn start_sck(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
    telemetry: SharedTelemetry,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    let mut b = sck::SckBackend::new(display, gpu, sink, cfg, telemetry)?;
    b.start()?;
    // Prove the capability, not just the class. A stream that starts and delivers nothing is
    // indistinguishable from a slow one from the outside, and it is the documented failure mode
    // for full-display capture on 12.3.
    let deadline = Instant::now() + SCK_FIRST_FRAME_TIMEOUT;
    while Instant::now() < deadline {
        if b.frames_captured() > 0 {
            return Ok(Box::new(b));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    b.stop();
    teprintln!(
        "[sck] no frame within {} ms of startCapture; treating ScreenCaptureKit as unusable on \
         display {display}",
        SCK_FIRST_FRAME_TIMEOUT.as_millis()
    );
    Err(CaptureError::StartFailed)
}

fn start_cgds(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
    telemetry: SharedTelemetry,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    let mut b = cgds::CgDisplayStreamBackend::new(display, gpu, sink, cfg, telemetry)?;
    b.start()?;
    Ok(Box::new(b))
}
