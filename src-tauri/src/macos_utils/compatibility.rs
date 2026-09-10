use std::sync::OnceLock;

use objc2::runtime::AnyClass;
use objc2_foundation::NSProcessInfo;

use crate::macos_utils::hosted_network::{probe_hosted_network_backend, HostedNetworkBackend};
use crate::macos_utils::virtual_display::{probe_display_backend, DisplayBackend};
use crate::{api_key, BareMinimum, CompatibilityReport, UnsupportedApi};

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

/// What was probed, so [`capability_notes`] can be exercised for a fault this Mac does not have.
struct Probes {
    at_least_min: bool,
    sck_available: bool,
    display_backend: DisplayBackend,
    hostap: HostedNetworkBackend,
    injects_input: bool,
    audio_unsupported: bool,
}

/// Turn what was probed into the capability notes the report carries.
///
/// The severities encode one rule: `blocking` is only for a host that cannot create a display,
/// cannot capture, or cannot encode. On this platform that leaves exactly one blocking row — an
/// OS below the app's own floor, where `CGVirtualDisplay` is not reachable at all. Everything
/// else here is a degraded feature: the audio tier, the hosted network, Accessibility (which
/// only gates remote input), the AirPlay display cap, and ScreenCaptureKit-versus-CGDisplayStream
/// where both capture.
///
/// Screen Recording is deliberately absent. It is a *permission*, it is reported through
/// `check_permissions` as `required`, and the permission gate runs before this report is ever
/// asked for — reporting it twice would let one surface contradict the other.
fn capability_notes(probes: &Probes) -> Vec<UnsupportedApi> {
    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    if !probes.at_least_min {
        unsupported.push(UnsupportedApi::blocking(
            BareMinimum::Display,
            "CGVirtualDisplay (Virtual Displays)",
            "Creating a display for each connected device.",
            "macOS 10.15 Catalina",
        ));
        unsupported.push(UnsupportedApi::optional(
            api_key::AUDIO,
            "System Audio Capture",
            "Streams this Mac's audio output to connected devices.",
            "macOS 10.15 Catalina",
        ));
    } else if !probes.sck_available {
        unsupported.push(UnsupportedApi::optional(
            api_key::CAPTURE_PREFERRED,
            "ScreenCaptureKit",
            "Preferred screen capture backend; falling back to CGDisplayStream.",
            "macOS 12.3",
        ));
    }

    // No display backend at all. `probe_display_backend` does not return this today — it falls
    // back to AirPlay — but `new_shared_controller` answers `None` for it, which is a host that
    // cannot give a joining device anything to look at. That is bare minimum, and the arm exists
    // so a future tier that probes its way to `Unsupported` is classified rather than silently
    // reported as a working host.
    if probes.at_least_min && probes.display_backend == DisplayBackend::Unsupported {
        unsupported.push(UnsupportedApi::blocking(
            BareMinimum::Display,
            "Virtual Displays",
            "No way to create a display for a connected device was found on this Mac.",
            "macOS 10.14.4",
        ));
    }

    // The AirPlay tier is capped at one display by macOS itself. Reported as a backend name only,
    // that limit surfaced when the user connected a second device and it failed.
    if probes.display_backend == DisplayBackend::AirPlayFallback {
        unsupported.push(UnsupportedApi::optional(
            api_key::DISPLAY_CAPPED,
            "CGVirtualDisplay",
            "Displays are created over AirPlay instead, which macOS limits to one connected \
             device at a time.",
            "macOS 10.14.4",
        ));
    }

    if probes.hostap == HostedNetworkBackend::Unsupported {
        unsupported.push(UnsupportedApi::optional(
            api_key::HOSTED_NETWORK,
            "Wi-Fi HostAP",
            "Creating a Wi-Fi network from this Mac so a device can join without a router. \
             Connect both devices to the same network instead.",
            "not available on this Mac",
        ));
    }

    // Not granted means every `CGEventPost` is dropped by the OS with no error the process can
    // observe, so a client with remote control enabled sees a host that simply never moves.
    // Reported through the list that already exists rather than a new UI surface; the
    // `request_permission` / `open_permission_settings` commands are what act on it. Optional,
    // and it must stay optional: this Mac still extends a display without it.
    if probes.at_least_min && !probes.injects_input {
        unsupported.push(UnsupportedApi::optional(
            api_key::INPUT_ACCESSIBILITY,
            "Accessibility (Remote Control)",
            "Letting a connected device drive this Mac's keyboard and pointer. Enable \
             ScreenExtend in System Settings > Privacy & Security > Accessibility.",
            "permission not granted",
        ));
    }

    if probes.at_least_min && probes.audio_unsupported {
        unsupported.push(UnsupportedApi::optional(
            api_key::AUDIO,
            "System Audio Capture",
            "Streams this Mac's audio output to connected devices.",
            "macOS 10.15 Catalina",
        ));
    }

    unsupported
}

#[tauri::command]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    let at_least_min = meets_minimum();
    let sck_available = capture_backend() == "ScreenCaptureKit";
    let display_backend = probe_display_backend();
    let hostap_backend = probe_hosted_network_backend();
    let input_backend = crate::streamer::input::probe_backend();
    let audio_backend = super::audio::probe_audio_backend();

    let unsupported = capability_notes(&Probes {
        at_least_min,
        sck_available,
        display_backend,
        hostap: hostap_backend,
        injects_input: input_backend.injects(),
        audio_unsupported: audio_backend == super::audio::AudioBackend::Unsupported,
    });

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
        // Which VideoToolbox encoder, not just "VideoToolbox": `VTCopyVideoEncoderList` says
        // whether this Mac has a hardware H.264 encoder at all, and the report is the one place
        // a user can see that answer without reading the log.
        encoder_backend: super::streamer::encoder::probe_encoder_backend().to_string(),
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

    fn healthy() -> Probes {
        Probes {
            at_least_min: true,
            sck_available: true,
            display_backend: DisplayBackend::CgVirtualDisplay,
            hostap: HostedNetworkBackend::HostApSpi,
            injects_input: true,
            audio_unsupported: false,
        }
    }

    fn blocking_keys(notes: &[UnsupportedApi]) -> Vec<String> {
        notes
            .iter()
            .filter(|a| a.is_blocking())
            .map(|a| a.key.clone())
            .collect()
    }

    #[test]
    fn a_healthy_mac_has_nothing_to_report() {
        assert!(capability_notes(&healthy()).is_empty());
    }

    /// The rule the owner set: a Mac that can create a display and capture it starts, whatever
    /// else is missing. Accessibility in particular gates only remote input, and no combination
    /// of these faults may take "Continue anyway" away.
    #[test]
    fn no_optional_fault_can_stop_startup() {
        let cases: Vec<(&str, Probes)> = vec![
            (
                "no ScreenCaptureKit",
                Probes {
                    sck_available: false,
                    ..healthy()
                },
            ),
            (
                "AirPlay display tier",
                Probes {
                    display_backend: DisplayBackend::AirPlayFallback,
                    ..healthy()
                },
            ),
            (
                "no hosted network",
                Probes {
                    hostap: HostedNetworkBackend::Unsupported,
                    ..healthy()
                },
            ),
            (
                "Accessibility not granted",
                Probes {
                    injects_input: false,
                    ..healthy()
                },
            ),
            (
                "no audio tier",
                Probes {
                    audio_unsupported: true,
                    ..healthy()
                },
            ),
            (
                "every optional fault at once",
                Probes {
                    at_least_min: true,
                    sck_available: false,
                    display_backend: DisplayBackend::AirPlayFallback,
                    hostap: HostedNetworkBackend::Unsupported,
                    injects_input: false,
                    audio_unsupported: true,
                },
            ),
        ];
        for (what, probes) in cases {
            let notes = capability_notes(&probes);
            assert!(!notes.is_empty(), "{what}: nothing was reported");
            assert!(
                blocking_keys(&notes).is_empty(),
                "{what}: produced {:?}; only the bare minimum may block",
                blocking_keys(&notes)
            );
        }
    }

    /// Below the app's own floor there is no display API to reach for, which is the one thing
    /// on this platform that cannot be continued past.
    #[test]
    fn an_os_below_the_floor_is_the_bare_minimum() {
        let notes = capability_notes(&Probes {
            at_least_min: false,
            ..healthy()
        });
        assert_eq!(blocking_keys(&notes), vec![api_key::DISPLAY.to_string()]);
    }

    #[test]
    fn no_display_backend_at_all_is_the_bare_minimum() {
        let notes = capability_notes(&Probes {
            display_backend: DisplayBackend::Unsupported,
            ..healthy()
        });
        assert_eq!(blocking_keys(&notes), vec![api_key::DISPLAY.to_string()]);
    }
}
