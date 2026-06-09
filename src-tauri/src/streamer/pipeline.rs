#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use crate::windows_utils::streamer::pipeline::{
    EncodedFrame, Pipeline, SessionCapture, probe_bitrate, probe_live, start, start_on_monitor,
};

#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use crate::windows_utils::streamer::capture::{
    monitor_device_names, set_display_orientation, set_display_resolution, set_display_scale,
};
