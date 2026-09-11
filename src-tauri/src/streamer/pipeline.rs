#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use crate::windows_utils::streamer::pipeline::{
    probe_bitrate, probe_live, start, start_on_monitor, EncodedFrame, Pipeline, SessionCapture,
};

#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use crate::windows_utils::streamer::capture::{
    monitor_device_names, monitor_dimensions, monitor_dimensions_owned, monitor_rect,
    set_display_mode, set_display_mode_owned, set_display_orientation, set_display_resolution,
    set_display_scale, set_display_scale_owned,
};

#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use crate::windows_utils::virtual_display::{
    attached_display_paths, claim_display_identity, forget_display_identity,
};

#[cfg(target_os = "windows")]
#[allow(unused_imports)]
pub use crate::windows_utils::set_display_topology_extend;

#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use crate::macos_utils::streamer::pipeline::{
    probe_bitrate, probe_live, start, start_on_monitor, EncodedFrame, Pipeline, SessionCapture,
};

#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub use crate::macos_utils::streamer::pipeline::{
    attached_display_paths, claim_display_identity, forget_display_identity, monitor_device_names,
    monitor_dimensions, monitor_dimensions_owned, monitor_rect, set_display_mode,
    set_display_mode_owned, set_display_orientation, set_display_resolution, set_display_scale,
    set_display_scale_owned, set_display_topology_extend,
};
