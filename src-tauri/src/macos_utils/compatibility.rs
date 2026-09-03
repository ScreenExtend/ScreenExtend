use crate::macos_utils::virtual_display::{probe_display_backend, DisplayBackend};
use crate::{CompatibilityReport, UnsupportedApi};

const MIN_MAJOR: u32 = 10;
const MIN_MINOR: u32 = 15;

const FALLBACK_MIN_MINOR: u32 = 13;

fn sw_vers(field: &str) -> String {
    std::process::Command::new("sw_vers")
        .arg(field)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn os_version_string() -> String {
    let name = {
        let n = sw_vers("-productName");
        if n.is_empty() {
            "macOS".to_string()
        } else {
            n
        }
    };
    let ver = sw_vers("-productVersion");
    if ver.is_empty() {
        name
    } else {
        format!("{name} {ver}")
    }
}

#[tauri::command]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    let at_least_min = super::streamer::macos_at_least(MIN_MAJOR, MIN_MINOR);
    let fallback_eligible = super::streamer::macos_at_least(MIN_MAJOR, FALLBACK_MIN_MINOR);
    let sck_available = super::streamer::screencapturekit_available(); // >= 12.3
    let display_backend = probe_display_backend();

    let os_supported =
        at_least_min || (fallback_eligible && display_backend == DisplayBackend::AirPlayFallback);
    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    if !at_least_min {
        unsupported.push(UnsupportedApi {
            name: "CGVirtualDisplay (Virtual Displays)".to_string(),
            description: if fallback_eligible {
                "Creating a display for each connected device. Falling back to AirPlay, which \
                 gives one display at a fixed resolution and needs Accessibility permission."
                    .to_string()
            } else {
                "Creating a display for each connected device.".to_string()
            },
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: if fallback_eligible {
                "optional".to_string()
            } else {
                "blocking".to_string()
            },
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

    let audio_backend = super::audio::probe_audio_backend();
    if at_least_min && audio_backend == super::audio::AudioBackend::Unsupported {
        unsupported.push(UnsupportedApi {
            name: "System Audio Capture".to_string(),
            description: "Streams this Mac's audio output to connected devices.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "optional".to_string(),
        });
    }

    CompatibilityReport {
        os_name: "macOS".to_string(),
        os_version: os_version_string(),
        min_os_version: if fallback_eligible && !at_least_min {
            "macOS 10.13 High Sierra (reduced)".to_string()
        } else {
            "macOS 10.15 Catalina".to_string()
        },
        os_supported,
        unsupported_apis: unsupported,
        audio_backend: audio_backend.as_str().to_string(),
        display_backend: display_backend.as_str().to_string(),
    }
}
