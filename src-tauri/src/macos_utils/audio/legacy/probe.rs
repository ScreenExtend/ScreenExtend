use std::sync::Mutex;
use std::time::{Duration, Instant};

use objc2_core_audio::AudioObjectID;

use super::branding;

/// How long a probe result is reused before the HAL is asked again.
///
/// One probe is a `stat`, a device enumeration and a UID read per device, each a round trip to
/// `coreaudiod`: tens of milliseconds on the join path if uncached. Install state changes only
/// through `installer::install_pkg`/`uninstall`, which invalidate explicitly, or a hand-run
/// package, which the TTL catches.
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
