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
pub mod perf_hold;
pub mod pipeline;
pub mod power;
pub mod qos;
pub mod raw_capture;
pub mod sck;
pub mod tune;
pub mod tuning;
pub mod vt_recipe;

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use objc2::runtime::AnyClass;

use frame::FrameSink;
use gpu::Gpu;

use crate::streamer::telemetry::SharedTelemetry;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
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
    #[error("ScreenCaptureKit failed ({sck}); CGDisplayStream fallback also failed: {cgds}")]
    FallbackFailed { sck: String, cgds: String },
}

pub trait CaptureBackend: Send {
    fn start(&mut self) -> Result<(), CaptureError>;
    fn stop(&mut self);
    /// Which tier started, for telemetry: `start_capture` falls back from ScreenCaptureKit to
    /// CGDisplayStream silently.
    fn name(&self) -> &'static str;
    /// Frames delivered so far: what separates a slow capture path from a dead one.
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
/// itself (`AnyClass::get` for a class, `dlsym` for a function, an attempted session create for an
/// encoder): a version number says nothing about a machine whose hardware, drivers or permissions
/// differ inside the same release.
///
/// Reads `NSProcessInfo.operatingSystemVersion` (10.10+, so below the floor) once and caches it.
pub fn macos_at_least(major: u32, minor: u32) -> bool {
    static VERSION: OnceLock<(u32, u32)> = OnceLock::new();
    let (maj, min) = *VERSION.get_or_init(|| {
        let v = objc2_foundation::NSProcessInfo::processInfo().operatingSystemVersion();
        (v.majorVersion.max(0) as u32, v.minorVersion.max(0) as u32)
    });
    (maj, min) >= (major, minor)
}

/// Whether ScreenCaptureKit exists on this machine. Probed by class lookup, not by version
/// number: `AnyClass::get` is also the SCK backend's own guard, so the selector and the guard are
/// the same test.
pub fn screencapturekit_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        AnyClass::get(c"SCStream").is_some() && AnyClass::get(c"SCShareableContent").is_some()
    })
}

/// How long a freshly started ScreenCaptureKit stream has to deliver its first frame. Generous on
/// purpose: it is there to catch a stream that starts cleanly and delivers *nothing* (the failure
/// Chromium reports for full-display capture on 12.3), not to race a busy machine into the
/// fallback onto the obsoleted API.
const SCK_FIRST_FRAME_TIMEOUT: Duration = Duration::from_millis(1500);

/// Called at every pipeline start (every device join and every stream refresh), so it is where
/// a consent that lapsed *mid-session* is noticed: macOS 15 re-asks for screen recording roughly
/// monthly.
///
/// Silent only: an automatic path may not prompt (`permissions::prompt_gate` is the only door).
/// Returns `false` only on `Silent::DefinitelyDenied`: never on the per-process cached preflight
/// negative, never on a read with nothing on screen to judge, and never once this process has
/// confirmed a grant. `Unknown` never blocks.
pub fn ensure_screen_recording_access() -> bool {
    use crate::macos_utils::permissions::{screen_recording_silent, Silent};
    let read = screen_recording_silent();
    match read.state {
        Silent::Granted => true,
        Silent::DefinitelyDenied => {
            teprintln!(
                "[tcc] Screen Recording is not authorized for this process ({}). Approve \
                 ScreenExtend under System Settings → Privacy & Security → Screen Recording; if \
                 the toggle is already on, quit and reopen ScreenExtend.",
                read.source
            );
            false
        }
        Silent::Unknown => {
            tprintln!(
                "[tcc] Screen Recording state could not be confirmed ({}); starting capture \
                 anyway and letting it report its own failure.",
                read.source
            );
            true
        }
    }
}

/// Liveness, not authorization. An *unauthorized* process still gets a stream and
/// still gets `FrameComplete` frames (they just contain wallpaper), so this must
/// never gate a permission decision. Use `permissions::screen_recording_live()`.
///
/// Both probes are prompting calls while unauthorized (`SCShareableContent` on macOS 15+,
/// and starting a `CGDisplayStream`), hence the permit.
pub fn probe_screen_recording(permit: crate::macos_utils::permissions::PromptPermit) -> bool {
    if screencapturekit_available() {
        if let Some(granted) = sck::probe_screen_recording(permit) {
            return granted;
        }
        // SCK is present but its probe could not answer. Logged because touching CGDisplayStream
        // is what raises Sequoia's recurring screen-recording alert
        teprintln!(
            "[capture] the ScreenCaptureKit probe was inconclusive; falling back to the \
             deprecated CGDisplayStream probe"
        );
    }
    cgds::probe_screen_recording()
}

/// One rung of the capture fallback ladder.
///
/// A rung is a choice, not a started stream, so `capture_ladder` can be asserted on a machine
/// where neither API exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureRung {
    ScreenCaptureKit,
    CgDisplayStream,
}

/// The backends [`start_capture`] tries, in order, given what the class lookup answered.
///
/// CGDisplayStream is always last and always present: it is obsoleted in the macOS 15 SDK, but
/// it is the only capture API on the 10.15 floor, so this rung must never be removed.
pub fn capture_ladder(sck_available: bool) -> &'static [CaptureRung] {
    if sck_available {
        &[CaptureRung::ScreenCaptureKit, CaptureRung::CgDisplayStream]
    } else {
        &[CaptureRung::CgDisplayStream]
    }
}

pub fn start_capture(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
    telemetry: SharedTelemetry,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    // TEST HOOK, bench builds only (compile time) and `SE_TEST_HOOKS=1` + `SE_TEST_SOURCE`
    // (run time): a synthetic frame source in place of screen capture (`bench/synth_source.rs`).
    #[cfg(feature = "bench")]
    if let Some(b) = crate::bench::synth_source::from_env(cfg, sink.clone()) {
        return Ok(b);
    }
    let sck_err =
        if capture_ladder(screencapturekit_available()).contains(&CaptureRung::ScreenCaptureKit) {
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
    // prove the capability, not just the class: a stream that starts and delivers nothing is the
    // documented failure mode for full-display capture on 12.3
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

#[cfg(test)]
mod tests {
    use super::*;

    // `start_capture` needs a display, a Metal device and a Screen Recording grant; the ladder it
    // chooses from is pure, so these run on any Mac

    #[test]
    fn cgdisplaystream_is_always_the_last_rung() {
        // obsoleted in the macOS 15 SDK, but the only capture API that exists on the 10.15 floor
        for sck in [false, true] {
            let ladder = capture_ladder(sck);
            assert_eq!(
                ladder.last(),
                Some(&CaptureRung::CgDisplayStream),
                "sck={sck}: something must be behind ScreenCaptureKit"
            );
            assert!(!ladder.is_empty());
        }
    }

    #[test]
    fn screencapturekit_leads_only_where_its_classes_exist() {
        assert_eq!(
            capture_ladder(true),
            &[CaptureRung::ScreenCaptureKit, CaptureRung::CgDisplayStream]
        );
        // no SCK on the 10.15 floor: a rung for it would be a failed class lookup, not a fallback
        assert_eq!(capture_ladder(false), &[CaptureRung::CgDisplayStream]);
    }

    #[test]
    fn no_ladder_tries_the_same_backend_twice() {
        for sck in [false, true] {
            let ladder = capture_ladder(sck);
            for (i, rung) in ladder.iter().enumerate() {
                assert!(
                    !ladder[i + 1..].contains(rung),
                    "sck={sck} tries {rung:?} twice: {ladder:?}"
                );
            }
        }
    }

    #[test]
    fn this_machine_agrees_with_its_own_probe() {
        // ties the ladder to the live class lookup. Which rung leads is printed, not asserted:
        // that is a property of the machine, not of the code
        let available = screencapturekit_available();
        let ladder = capture_ladder(available);
        assert_eq!(ladder.contains(&CaptureRung::ScreenCaptureKit), available);
        println!("ScreenCaptureKit available: {available}; ladder {ladder:?}");
    }
}
