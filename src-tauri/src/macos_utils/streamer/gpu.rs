//! Shared zero-copy GPU core (PRD §4.2): the Metal device + `CVMetalTextureCache`,
//! created once and reused for the life of the stream.
//!
//! M1 seam. The full implementation (PRD §4.2) is:
//! ```ignore
//! pub struct Gpu {
//!     pub device:    Retained<ProtocolObject<dyn MTLDevice>>,
//!     pub tex_cache: CFRetained<CVMetalTextureCache>,
//! }
//! ```
//! built via `MTLCreateSystemDefaultDevice` + `CVMetalTextureCacheCreate`. The
//! cache recycles its `CVMetalTexture` wrappers against the IOSurface pool, so
//! there is zero per-frame allocation. Requires `objc2-metal` +
//! `objc2-core-video` (features `CVMetalTextureCache`, ...).

use std::sync::Arc;

use super::CaptureError;

/// The reused Metal device + texture cache.
///
/// Fields are added with the objc2 types at M1; the struct exists now so the
/// backends and orchestration can name `Arc<Gpu>` in their signatures.
pub struct Gpu {
    // M1: device: Retained<ProtocolObject<dyn MTLDevice>>,
    // M1: tex_cache: CFRetained<CVMetalTextureCache>,
    _private: (),
}

impl Gpu {
    /// Create the device + cache once (PRD §4.2).
    ///
    /// Intentionally an empty handle. The whole streaming path is Metal-free: the
    /// window-server composites and (when scaled) resizes into an `IOSurface`,
    /// capture wraps that zero-copy in a `CVPixelBuffer`, and VideoToolbox encodes
    /// it directly (PRD §14.3) — no texture ever round-trips through Metal. This is
    /// a *headless* streamer with no local preview, so a Metal device +
    /// `CVMetalTextureCache` (5.2) and the BGRA single-texture path (5.3) would
    /// have no consumer and would only add startup cost against the latency goal.
    /// The handle is kept so the backends/orchestration can name `Arc<Gpu>`; if a
    /// local preview / GPU pre-processing path is ever added, build the device and
    /// cache here.
    pub fn new() -> Result<Arc<Self>, CaptureError> {
        Ok(Arc::new(Gpu { _private: () }))
    }
}
