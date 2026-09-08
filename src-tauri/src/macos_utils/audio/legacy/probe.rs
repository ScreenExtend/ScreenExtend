use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_audio::AudioObjectID;

use super::branding;

/// How long a probe result is reused before the HAL is asked again.
///
/// One probe costs a `stat`, an enumeration of every CoreAudio device, and one UID property read
/// per device — each a round trip to `coreaudiod`. The compatibility report polls this from the
/// frontend and `start_capture` calls it on the join path, so uncached it is tens of milliseconds
/// in front of the first audio packet. Install state does change at runtime, but only through
/// `installer::install_pkg`/`uninstall`, which invalidate explicitly, or a user running the
/// package by hand, which the TTL catches.
const CACHE_TTL: Duration = Duration::from_secs(2);

static CACHE: Mutex<Option<(Instant, LegacyState)>> = Mutex::new(None);

pub fn driver_bundle_installed() -> bool {
    std::path::Path::new(branding::INSTALL_PATH).exists()
}

pub fn device_present() -> Option<AudioObjectID> {
    super::hal::device_by_uid(branding::DEVICE_UID)
}

pub fn driver_healthy() -> bool {
    legacy_state() == LegacyState::Ready
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyState {
    Ready,
    NeedsInstall,
    InstalledButUnhealthy,
}

/// Drop the cached probe result; call after anything that can change the install state.
pub fn invalidate() {
    if let Ok(mut cache) = CACHE.lock() {
        *cache = None;
    }
}

pub fn legacy_state() -> LegacyState {
    if let Ok(cache) = CACHE.lock() {
        if let Some((at, state)) = *cache {
            if at.elapsed() < CACHE_TTL {
                return state;
            }
        }
    }
    let state = probe_state();
    if let Ok(mut cache) = CACHE.lock() {
        *cache = Some((Instant::now(), state));
    }
    state
}

fn probe_state() -> LegacyState {
    if device_present().is_some() {
        LegacyState::Ready
    } else if driver_bundle_installed() {
        LegacyState::InstalledButUnhealthy
    } else {
        LegacyState::NeedsInstall
    }
}
