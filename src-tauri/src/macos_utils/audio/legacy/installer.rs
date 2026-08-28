use std::path::Path;
use std::process::Command as StdCommand;

use elevated_command::Command as ElevatedCommand;

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
    let quoted = format!("'{}'", shell_cmd.replace('\'', "'\\''"));
    let mut cmd = StdCommand::new("/bin/sh");
    cmd.arg("-c").arg(quoted);

    match ElevatedCommand::new(cmd).output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(InstallOutcome::Failed(stderr.trim().to_string()))
        }
        Err(e) => {
            crate::teprintln!("[audio] elevated command failed (treating as cancelled): {e}");
            Err(InstallOutcome::Cancelled)
        }
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
