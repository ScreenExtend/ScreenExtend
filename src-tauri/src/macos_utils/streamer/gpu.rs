use std::sync::Arc;

use super::CaptureError;

/// Deliberately empty: WindowServer delivers the frame already scaled and already in `420f`, and
/// the IOSurface is wrapped, not copied, into VideoToolbox, so there is no GPU scale or convert
/// stage. The type stays because it is threaded through `start_capture` and both backends.
pub struct Gpu {
    _private: (),
}

impl Gpu {
    pub fn new() -> Result<Arc<Self>, CaptureError> {
        Ok(Arc::new(Gpu { _private: () }))
    }
}
