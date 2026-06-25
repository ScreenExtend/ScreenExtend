//! macOS desktop-capture + H.264 encode backend.
//!
// Much of the interface surface (probes, display setters, mach helpers) is
// consumed by the platform-agnostic `crate::streamer` layer rather than by
// tests, and some backends (SCK) are future stubs — so dead-code is expected.
#![allow(dead_code)]
//!
//! Structural twin of `windows_utils::streamer` (PRD §0.1): SCK /
//! CGDisplayStream replace WGC; VideoToolbox replaces NVENC/QSV. The
//! platform-agnostic `crate::streamer` layer consumes this module through the
//! identical interface re-exported by [`pipeline`], so the two platforms
//! diverge only inside the backend.
//!
//! ## Implementation status
//!
//! This is the **M0 scaffold** (PRD § Milestones). The module tree, the shared
//! interface, the lock-free frame hand-off ([`frame`]), and the config
//! derivation ([`config`]) are complete and compile. The objc2 FFI bodies —
//! the Metal core ([`gpu`]), the SCK ([`sck`]) and CGDisplayStream ([`cgds`])
//! capture backends, and the VideoToolbox [`encoder`] — carry the PRD design
//! and the `⚠ VERIFY` seams but return [`CaptureError::NotImplemented`] until
//! they are filled in and compile-verified on a Mac against `./samples`
//! (M1/M2). The PRD is explicit that objc2 signatures must be verified on-device
//! rather than assumed.

pub mod activity;
pub mod cgds;
pub mod config;
pub mod display;
pub mod encoder;
pub mod frame;
pub mod gpu;
pub mod mach;
pub mod pipeline;
pub mod qos;
pub mod sck;

use std::sync::Arc;

use frame::FrameSink;
use gpu::Gpu;

/// Errors from the capture/encode backend. Mirrors the shape of the Windows
/// backend's error handling (which leans on `anyhow`); kept as an explicit enum
/// here so backend selection and the FFI seams have precise variants.
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
    /// M1/M2 seam: the FFI path exists structurally but is not yet wired.
    #[error("{0} not yet implemented (M1/M2): see PRD {1}")]
    NotImplemented(&'static str, &'static str),
    /// Both capture backends failed to start: ScreenCaptureKit (preferred) and the
    /// CGDisplayStream fallback. Carries both messages so each cause stays visible
    /// (mirrors the Windows WGC -> DXGI fallback's combined error).
    #[error("ScreenCaptureKit failed ({sck}); CGDisplayStream fallback also failed: {cgds}")]
    FallbackFailed { sck: String, cgds: String },
}

/// Both backends produce the same thing — frames published into a shared
/// [`FrameSink`] — so the trait is minimal and never sits on the hot path
/// (PRD §3.2). The hot path goes through the sink, not through dynamic dispatch.
pub trait CaptureBackend: Send {
    /// Begin delivering frames into the sink. Returns once capture is running.
    fn start(&mut self) -> Result<(), CaptureError>;
    fn stop(&mut self);
}

/// A `CGDirectDisplayID`. Aliased so the orchestration reads the same on every
/// platform even before `objc2-core-graphics` is a dependency.
pub type DisplayId = u32;

/// Capture tuning that crosses into the backends (PRD §5.3 / §6.1).
#[derive(Debug, Clone, Copy)]
pub struct CaptureConfig {
    pub width: usize,
    pub height: usize,
    pub fps: i32,
    /// FourCC pixel format: `'420f'` (0x34323066) for streaming, `'BGRA'`
    /// (0x42475241) for the simple/preview path (PRD §4.1).
    pub pixel_format: u32,
}

/// `'420f'` — `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`. The streaming
/// default (PRD §4.1): VideoToolbox-native, ~⅓ the IOSurface bandwidth.
pub const PIXEL_FORMAT_420F: u32 = 0x3432_3066;
/// `'BGRA'` — `kCVPixelFormatType_32BGRA`. Simplest single-texture path.
pub const PIXEL_FORMAT_BGRA: u32 = 0x4247_5241;

/// True iff running on macOS `major.minor` or newer.
///
/// Detected once at startup, off the hot path (PRD §3.1). The PRD's version
/// uses `NSProcessInfo::operatingSystemVersion`; this dep-free M0 form shells
/// out to `sw_vers` and defaults to "SCK available" if detection fails (the
/// modern, supported path).
pub fn macos_at_least(major: u32, minor: u32) -> bool {
    fn product_version() -> Option<(u32, u32)> {
        let out = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        let mut it = s.trim().split('.');
        let maj = it.next()?.parse().ok()?;
        let min = it.next().unwrap_or("0").parse().unwrap_or(0);
        Some((maj, min))
    }
    match product_version() {
        Some((maj, min)) => (maj, min) >= (major, minor),
        None => true, // assume modern macOS → SCK path
    }
}

/// ScreenCaptureKit exists on macOS 12.3+. Below that, CGDisplayStream is the
/// genuine direct-to-WindowServer path (PRD §3.4).
#[inline]
pub fn screencapturekit_available() -> bool {
    macos_at_least(12, 3)
}

/// Ensure Screen-Recording (TCC) access, triggering the one-time system prompt on
/// first run if it is not yet granted (PRD pitfall #7 / 5.5). Returns whether
/// access is granted now.
///
/// `CGPreflightScreenCaptureAccess` / `CGRequestScreenCaptureAccess` are macOS
/// **11+** — they are absent from the 10.15 CoreGraphics, so referencing them as
/// link-time `extern` symbols breaks the dyld load on 10.15 (a lazy-bind failure
/// for `_CGPreflightScreenCaptureAccess`). They are therefore resolved via `dlsym`,
/// the same dyld-safe pattern the encoder uses for version-gated VideoToolbox
/// constants. On 10.15, where the API does not exist, there is nothing to query:
/// the Screen-Recording prompt is raised by the capture attempt itself
/// (CGDisplayStreamCreate), so we report "proceed".
pub fn ensure_screen_recording_access() -> bool {
    type AccessFn = unsafe extern "C" fn() -> bool;
    // SAFETY: RTLD_DEFAULT lookup of two parameterless, no-argument C functions; we
    // only transmute+call the resolved pointer when it is non-null (i.e. on 11+).
    unsafe {
        let sym = |name: &str| -> Option<AccessFn> {
            let cname = std::ffi::CString::new(name).ok()?;
            let p = libc::dlsym(libc::RTLD_DEFAULT, cname.as_ptr());
            if p.is_null() {
                None
            } else {
                Some(std::mem::transmute::<*mut std::ffi::c_void, AccessFn>(p))
            }
        };
        let Some(preflight) = sym("CGPreflightScreenCaptureAccess") else {
            return true; // 10.15: no preflight API — the capture call prompts.
        };
        if preflight() {
            return true;
        }
        let granted = sym("CGRequestScreenCaptureAccess").map(|req| req()).unwrap_or(false);
        if !granted {
            teprintln!(
                "[tcc] Screen Recording permission not granted. Enable ScreenExtend under \
                 System Settings → Privacy & Security → Screen Recording, then relaunch."
            );
        }
        granted
    }
}

/// Start desktop capture, preferring ScreenCaptureKit and falling back to
/// CGDisplayStream (PRD §3.3).
///
/// This mirrors the Windows `WGC -> DXGI Desktop Duplication` fallback
/// (`windows_utils::streamer::pipeline::start_on_monitor`): the preferred backend
/// is attempted first, and **any startup failure** drops to the backup rather
/// than failing the capture outright. ScreenCaptureKit is the modern
/// direct-to-WindowServer path (macOS 12.3+); CGDisplayStream is the backup.
///
/// Below 12.3 SCK does not exist, so we skip straight to the fallback — no wasted
/// attempt and no misleading failure log. When SCK is available but fails to
/// start, its error is logged and preserved: if the CGDisplayStream fallback then
/// also fails, both causes are reported via [`CaptureError::FallbackFailed`].
pub fn start_capture(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    // Try the preferred backend (SCK) first when the OS supports it.
    let sck_err = if screencapturekit_available() {
        match start_sck(display, gpu.clone(), sink.clone(), cfg) {
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

    // Backup path: CGDisplayStream. On its own failure, surface the SCK error too
    // (when we actually tried SCK) so both causes are visible.
    match start_cgds(display, gpu, sink, cfg) {
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

/// Build and start the ScreenCaptureKit backend (preferred path).
fn start_sck(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    let mut b = sck::SckBackend::new(display, gpu, sink, cfg)?;
    b.start()?;
    Ok(Box::new(b))
}

/// Build and start the CGDisplayStream backend (fallback path).
fn start_cgds(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
    let mut b = cgds::CgDisplayStreamBackend::new(display, gpu, sink, cfg)?;
    b.start()?;
    Ok(Box::new(b))
}
