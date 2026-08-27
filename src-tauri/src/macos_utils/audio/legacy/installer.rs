use std::path::Path;
use std::process::Command;

use super::{branding, probe};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    Uninstalled,
    NeedsReboot,
    Cancelled,
    Failed(String),
}

impl InstallOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            InstallOutcome::Installed => "installed",
            InstallOutcome::Uninstalled => "uninstalled",
            InstallOutcome::NeedsReboot => "needs_reboot",
            InstallOutcome::Cancelled => "cancelled",
            InstallOutcome::Failed(_) => "failed",
        }
    }
}

fn run_privileged(shell_cmd: &str) -> Result<(), InstallOutcome> {
    let escaped = shell_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("do shell script \"{escaped}\" with administrator privileges");
    let output = Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .map_err(|e| InstallOutcome::Failed(format!("failed to launch osascript: {e}")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("-128") || stderr.to_lowercase().contains("cancel") {
        Err(InstallOutcome::Cancelled)
    } else {
        Err(InstallOutcome::Failed(stderr.trim().to_string()))
    }
}

pub fn install_pkg(pkg_path: &Path) -> InstallOutcome {
    if !pkg_path.exists() {
        return InstallOutcome::Failed(format!(
            "installer package not found: {}",
            pkg_path.display()
        ));
    }
    let cmd = format!(
        "/usr/sbin/installer -pkg \"{}\" -target /",
        pkg_path.display()
    );
    match run_privileged(&cmd) {
        Ok(()) => {}
        Err(outcome) => return outcome,
    }

    for _ in 0..10 {
        if probe::driver_healthy() {
            return InstallOutcome::Installed;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    if probe::driver_bundle_installed() {
        InstallOutcome::NeedsReboot
    } else {
        InstallOutcome::Failed("install ran but the driver bundle is not present".into())
    }
}

pub fn uninstall() -> InstallOutcome {
    super::routing::recover_on_launch();

    let cmd = format!(
        "/bin/rm -rf \"{}\" ; /bin/launchctl kickstart -k system/com.apple.audio.coreaudiod \
         || /usr/bin/killall coreaudiod || true",
        branding::INSTALL_PATH
    );
    match run_privileged(&cmd) {
        Ok(()) => {}
        Err(outcome) => return outcome,
    }

    if let Ok(name) = std::ffi::CString::new(branding::SHM_NAME) {
        unsafe {
            libc::shm_unlink(name.as_ptr());
        }
    }

    if probe::driver_bundle_installed() {
        InstallOutcome::Failed("driver bundle still present after uninstall".into())
    } else if probe::driver_healthy() {
        InstallOutcome::NeedsReboot
    } else {
        InstallOutcome::Uninstalled
    }
}

pub fn health() -> probe::LegacyState {
    probe::legacy_state()
}
