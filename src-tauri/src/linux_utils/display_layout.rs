//! No display arrangement on Linux yet: the dialog shows why and offers nothing to apply.

use crate::display_layout::{DisplayArranger, DisplayLayout, DisplayPlacement};

const UNSUPPORTED: &str = "rearranging displays is not supported on this system";

pub struct Arranger;

impl DisplayArranger for Arranger {
    fn read() -> Result<DisplayLayout, String> {
        Ok(DisplayLayout {
            displays: Vec::new(),
            units: "pixels".to_string(),
            can_apply: false,
            reason: Some(UNSUPPORTED.to_string()),
        })
    }

    fn apply(_placements: &[DisplayPlacement]) -> Result<DisplayLayout, String> {
        Err(UNSUPPORTED.to_string())
    }
}

pub fn watch(_on_change: Box<dyn Fn(DisplayLayout) + Send + Sync>) -> Result<(), String> {
    Ok(())
}
