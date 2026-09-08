use std::sync::OnceLock;

use objc2::runtime::AnyClass;
use objc2_foundation::NSProcessInfo;

use crate::macos_utils::hosted_network::{probe_hosted_network_backend, HostedNetworkBackend};
use crate::macos_utils::virtual_display::{probe_display_backend, DisplayBackend};
use crate::{CompatibilityReport, UnsupportedApi};

const MIN_MAJOR: isize = 10;
const MIN_MINOR: isize = 15;

/// `NSProcessInfo.operatingSystemVersion` is 10.10+ and link-time safe, so nothing here forks a
/// process. The previous implementation ran `sw_vers` three times per report, on the main thread,
/// from a command the bootstrap screen calls twice.
fn os_version() -> (isize, isize, isize) {
    static CACHED: OnceLock<(isize, isize, isize)> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let v = NSProcessInfo::processInfo().operatingSystemVersion();
        (v.majorVersion, v.minorVersion, v.patchVersion)
    })
}

fn os_version_string() -> String {
    let (major, minor, patch) = os_version();
    if patch == 0 {
        format!("macOS {major}.{minor}")
    } else {
        format!("macOS {major}.{minor}.{patch}")
    }
}

fn meets_minimum() -> bool {
    let (major, minor, _) = os_version();
    (major, minor) >= (MIN_MAJOR, MIN_MINOR)
}

/// Which capture path `start_capture` will actually take, decided by class lookup rather than by
/// an OS version. The tier with the largest latency and quality consequence of the three was the
/// one the report never named.
pub fn capture_backend() -> &'static str {
    static CACHED: OnceLock<&'static str> = OnceLock::new();
    CACHED.get_or_init(|| {
        if AnyClass::get(c"SCStream").is_some() && AnyClass::get(c"SCShareableContent").is_some() {
            "ScreenCaptureKit"
        } else {
            "CGDisplayStream"
        }
    })
}

/// The mechanism this Mac can use to create its own Wi-Fi network, if any.
pub fn hosted_network_backend() -> &'static str {
    probe_hosted_network_backend().as_str()
}

#[tauri::command]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    let at_least_min = meets_minimum();
    let sck_available = capture_backend() == "ScreenCaptureKit";
    let display_backend = probe_display_backend();
    let hostap_backend = probe_hosted_network_backend();
    let input_backend = crate::streamer::input::probe_backend();

    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    if !at_least_min {
        unsupported.push(UnsupportedApi {
            name: "CGVirtualDisplay (Virtual Displays)".to_string(),
            description: "Creating a display for each connected device.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "blocking".to_string(),
        });
        unsupported.push(UnsupportedApi {
            name: "System Audio Capture".to_string(),
            description: "Streams this Mac's audio output to connected devices.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "optional".to_string(),
        });
    } else if !sck_available {
        unsupported.push(UnsupportedApi {
            name: "ScreenCaptureKit".to_string(),
            description: "Preferred screen capture backend; falling back to CGDisplayStream."
                .to_string(),
            required_version: "macOS 12.3".to_string(),
            severity: "optional".to_string(),
        });
    }

    // The AirPlay tier is capped at one display by macOS itself. Reported as a backend name only,
    // that limit surfaced when the user connected a second device and it failed.
    if display_backend == DisplayBackend::AirPlayFallback {
        unsupported.push(UnsupportedApi {
            name: "CGVirtualDisplay".to_string(),
            description: "Displays are created over AirPlay instead, which macOS limits to one \
                          connected device at a time."
                .to_string(),
            required_version: "macOS 10.14.4".to_string(),
            severity: "optional".to_string(),
        });
    }

    if hostap_backend == HostedNetworkBackend::Unsupported {
        unsupported.push(UnsupportedApi {
            name: "Wi-Fi HostAP".to_string(),
            description: "Creating a Wi-Fi network from this Mac so a device can join without a \
                          router. Connect both devices to the same network instead."
                .to_string(),
            required_version: "not available on this Mac".to_string(),
            severity: "optional".to_string(),
        });
    }

    // Not granted means every `CGEventPost` is dropped by the OS with no error the process can
    // observe, so a client with remote control enabled sees a host that simply never moves.
    // Reported through the list that already exists rather than a new UI surface; the
    // `request_permission` / `open_permission_settings` commands are what act on it.
    if at_least_min && !input_backend.injects() {
        unsupported.push(UnsupportedApi {
            name: "Accessibility (Remote Control)".to_string(),
            description: "Letting a connected device drive this Mac's keyboard and pointer.                           Enable ScreenExtend in System Settings > Privacy & Security >                           Accessibility."
                .to_string(),
            required_version: "permission not granted".to_string(),
            severity: "optional".to_string(),
        });
    }

    let audio_backend = super::audio::probe_audio_backend();
    if at_least_min && audio_backend == super::audio::AudioBackend::Unsupported {
        unsupported.push(UnsupportedApi {
            name: "System Audio Capture".to_string(),
            description: "Streams this Mac's audio output to connected devices.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "optional".to_string(),
        });
    }

    tprintln!(
        "[compat] {} — capture {}, display {}, audio {}, hosted network {}, input {}",
        os_version_string(),
        capture_backend(),
        display_backend.as_str(),
        audio_backend.as_str(),
        hostap_backend.as_str(),
        input_backend.as_str()
    );

    CompatibilityReport {
        os_name: "macOS".to_string(),
        os_version: os_version_string(),
        min_os_version: "macOS 10.15 Catalina".to_string(),
        os_supported: at_least_min,
        unsupported_apis: unsupported,
        audio_backend: audio_backend.as_str().to_string(),
        display_backend: display_backend.as_str().to_string(),
        capture_backend: capture_backend().to_string(),
        encoder_backend: "VideoToolbox".to_string(),
        hosted_network_backend: hostap_backend.as_str().to_string(),
        input_backend: input_backend.as_str().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_os_version_is_read_without_a_subprocess() {
        let (major, minor, _) = os_version();
        assert!(major >= 10, "unexpected macOS major version {major}");
        assert!(minor >= 0);
        assert!(os_version_string().starts_with("macOS "));
    }

    #[test]
    fn the_capture_tier_is_a_class_lookup() {
        assert!(matches!(
            capture_backend(),
            "ScreenCaptureKit" | "CGDisplayStream"
        ));
    }
}
