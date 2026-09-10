use crate::{BareMinimum, CompatibilityReport, UnsupportedApi};

fn os_version_string() -> String {
    if let Some(pretty) = std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|content| {
            content.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })
    {
        return pretty;
    }
    if let Ok(out) = std::process::Command::new("uname").arg("-sr").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let t = s.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    "Linux".to_string()
}

#[tauri::command]
#[specta::specta]
pub fn check_system_requirements() -> CompatibilityReport {
    // The scaffold has no display, capture or encode backend, so it is missing all three parts
    // of the bare minimum. It used to report `os_supported: true` with an empty list, which the
    // startup gate reads as a working host.
    CompatibilityReport {
        os_name: "Linux".to_string(),
        os_version: os_version_string(),
        min_os_version: "Linux".to_string(),
        os_supported: false,
        unsupported_apis: vec![
            UnsupportedApi::blocking(
                BareMinimum::Display,
                "Virtual Displays",
                "Creating a display for each connected device.",
                "not implemented on Linux",
            ),
            UnsupportedApi::blocking(
                BareMinimum::Capture,
                "Screen Capture",
                "Captures the screen to stream to connected devices.",
                "not implemented on Linux",
            ),
            UnsupportedApi::blocking(
                BareMinimum::Encoder,
                "Video Encoder",
                "Encodes the captured screen as H.264.",
                "not implemented on Linux",
            ),
        ],
        audio_backend: "unsupported".to_string(),
        display_backend: "unsupported".to_string(),
        capture_backend: "unsupported".to_string(),
        encoder_backend: "unsupported".to_string(),
        hosted_network_backend: "unsupported".to_string(),
        input_backend: crate::streamer::input::probe_backend().as_str().to_string(),
    }
}
