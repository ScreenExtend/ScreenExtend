use std::sync::Arc;

use crate::streamer::session::{SharedVirtualDisplay, VirtualDisplayController};

#[derive(Debug)]
pub struct MacosVirtualDisplay;

impl MacosVirtualDisplay {
    pub fn new_shared() -> Option<SharedVirtualDisplay> {
        Some(Arc::new(Self))
    }
}

impl VirtualDisplayController for MacosVirtualDisplay {
    fn create_display(
        &self,
        _name: String,
        _width: u32,
        _height: u32,
        _refresh_rate: u32,
    ) -> Result<u32, String> {
        Err("virtual displays are not yet implemented on macOS".to_string())
    }

    fn remove_display(&self, _id: u32) {}

    fn remove_all_displays(&self) {}
}
