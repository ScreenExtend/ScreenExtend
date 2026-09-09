use std::sync::Arc;

use super::CaptureError;

/// A placeholder, deliberately empty.
///
/// It was meant to hold a `MTLDevice` and a `CVMetalTextureCache` for a GPU scale/convert stage,
/// the way the Windows backend has `scaler.rs`. Nothing on the macOS path needs one: WindowServer
/// delivers the frame already scaled to the requested size and already in the pixel format the
/// encoder wants (`420f`), and the delivered IOSurface is wrapped rather than copied all the way
/// into VideoToolbox. There is no conversion or scale on the CPU to move to the GPU.
///
/// The type stays because it is threaded through `start_capture` and both backends, and because a
/// Metal texture cache is what a source-rect crop or a host-side downscale would need if one is
/// ever wanted. Whether WindowServer's own downscale costs anything is
/// `findings/speculative-cgds-windowserver-does-the-downscale-with-no-source-rect.md`.
pub struct Gpu {
    _private: (),
}

impl Gpu {
    pub fn new() -> Result<Arc<Self>, CaptureError> {
        Ok(Arc::new(Gpu { _private: () }))
    }
}
