use objc2_core_audio::AudioObjectID;

use super::branding;

pub fn driver_bundle_installed() -> bool {
    std::path::Path::new(branding::INSTALL_PATH).exists()
}

pub fn device_present() -> Option<AudioObjectID> {
    super::hal::device_by_uid(branding::DEVICE_UID)
}

pub fn driver_healthy() -> bool {
    device_present().is_some()
}

pub fn eligible_os() -> bool {
    use crate::macos_utils::streamer::macos_at_least;
    macos_at_least(10, 15) && !macos_at_least(13, 0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyState {
    Ready,
    NeedsInstall,
    InstalledButUnhealthy,
}

pub fn legacy_state() -> LegacyState {
    if driver_healthy() {
        LegacyState::Ready
    } else if driver_bundle_installed() {
        LegacyState::InstalledButUnhealthy
    } else {
        LegacyState::NeedsInstall
    }
}
