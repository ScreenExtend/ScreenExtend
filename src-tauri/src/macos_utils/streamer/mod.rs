#![allow(dead_code)]
pub mod activity;
pub mod cgds;
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

use std::sync::Arc;

use frame::FrameSink;
use gpu::Gpu;

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
        None => true,
    }
}

#[inline]
pub fn screencapturekit_available() -> bool {
    macos_at_least(12, 3)
}

pub fn ensure_screen_recording_access() -> bool {
    type AccessFn = unsafe extern "C" fn() -> bool;
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
            return true;
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

pub fn start_capture(
    display: DisplayId,
    gpu: Arc<Gpu>,
    sink: Arc<FrameSink>,
    cfg: CaptureConfig,
) -> Result<Box<dyn CaptureBackend>, CaptureError> {
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
