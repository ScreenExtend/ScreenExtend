use crate::CompatibilityReport;

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
    CompatibilityReport {
        os_name: "Linux".to_string(),
        os_version: os_version_string(),
        min_os_version: "Linux".to_string(),
        os_supported: true,
        unsupported_apis: Vec::new(),
    }
}
