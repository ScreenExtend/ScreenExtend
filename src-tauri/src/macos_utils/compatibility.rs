use crate::{CompatibilityReport, UnsupportedApi};

const MIN_MAJOR: u32 = 10;
const MIN_MINOR: u32 = 15;

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
        if n.is_empty() { "macOS".to_string() } else { n }
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
    let sck_available = super::streamer::screencapturekit_available(); // >= 12.3

    let os_supported = at_least_min;
    let mut unsupported: Vec<UnsupportedApi> = Vec::new();

    if !at_least_min {
        unsupported.push(UnsupportedApi {
            name: "CGDisplayStream (Screen Capture)".to_string(),
            description: "Captures the screen to stream to connected devices.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "blocking".to_string(),
        });
        unsupported.push(UnsupportedApi {
            name: "Core WLAN (Wi-Fi)".to_string(),
            description: "Detecting Wi-Fi state and hosting a hotspot.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "optional".to_string(),
        });
        unsupported.push(UnsupportedApi {
            name: "Video Toolbox (Hardware Encoding)".to_string(),
            description: "Hardware-accelerated video encoding for streaming.".to_string(),
            required_version: "macOS 10.15 Catalina".to_string(),
            severity: "optional".to_string(),
        });
    } else if !sck_available {
        unsupported.push(UnsupportedApi {
            name: "ScreenCaptureKit".to_string(),
            description: "Preferred screen capture backend; falling back to CGDisplayStream.".to_string(),
            required_version: "macOS 12.3".to_string(),
            severity: "optional".to_string(),
        });
    }

    CompatibilityReport {
        os_name: "macOS".to_string(),
        os_version: os_version_string(),
        min_os_version: "macOS 10.15 Catalina".to_string(),
        os_supported,
        unsupported_apis: unsupported,
    }
}
