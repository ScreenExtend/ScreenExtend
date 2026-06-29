use std::sync::Arc;

use super::CaptureError;

pub struct Gpu {
    // M1: device: Retained<ProtocolObject<dyn MTLDevice>>,
    // M1: tex_cache: CFRetained<CVMetalTextureCache>,
    _private: (),
}

impl Gpu {
    pub fn new() -> Result<Arc<Self>, CaptureError> {
        Ok(Arc::new(Gpu { _private: () }))
    }
}
