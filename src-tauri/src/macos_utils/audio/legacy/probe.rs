//! Detection for the legacy virtual-device tier (PRD-macos-legacy-audio.md §4).
//!
//! Probe by **attempting**, not by version alone: a driver can be installed on disk but not loaded
//! (coreaudiod didn't pick it up, or the signature was rejected), so "healthy" means the device is
//! actually enumerable and responding — the bundle existing is necessary but not sufficient.

use objc2_core_audio::AudioObjectID;

use super::branding;

/// The `.driver` bundle is present at the HAL install path (necessary, not sufficient).
pub fn driver_bundle_installed() -> bool {
    std::path::Path::new(branding::INSTALL_PATH).exists()
}

/// Our device's `AudioObjectID` if the HAL currently knows it (i.e. the driver loaded and coreaudiod
/// published the device).
pub fn device_present() -> Option<AudioObjectID> {
    super::hal::device_by_uid(branding::DEVICE_UID)
}

/// Healthy = the device is enumerable and its UID resolves. This is the gate for selecting the
/// VirtualDevice backend (PRD §4). Re-checked when the device list changes rather than cached
/// forever, since install/uninstall flips it at runtime.
pub fn driver_healthy() -> bool {
    device_present().is_some()
}

/// This OS is in the legacy range: 10.15 ≤ version < 13.0. On 13.0+ the native backends win
/// unconditionally and the virtual device must never be selected (PRD §4).
pub fn eligible_os() -> bool {
    use crate::macos_utils::streamer::macos_at_least;
    macos_at_least(10, 15) && !macos_at_least(13, 0)
}

/// The distinct install/health states the UI surfaces (PRD §4, §9.2). Only computed when no native
/// backend is available and the OS is in range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyState {
    /// Driver installed + device healthy → VirtualDevice backend is usable.
    Ready,
    /// In range, but the driver isn't installed or isn't loaded → actionable one-time install.
    NeedsInstall,
    /// Installed on disk but the device never appeared → likely unsigned/rejected or needs a
    /// coreaudiod restart / reboot. Still surfaced as NeedsInstall to the user (re-install/repair),
    /// but logged distinctly for diagnostics.
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
