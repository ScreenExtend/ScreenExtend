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

    let mut elevated = ElevatedCommand::new(cmd);
    elevated.name("ScreenExtend".to_string());
    if let Some(icon) = bundle_icon_bytes() {
        elevated.icon(icon);
    }

    match elevated.output() {
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

fn bundle_icon_bytes() -> Option<Vec<u8>> {
    let exe = std::env::current_exe().ok()?;
    let resources = exe.parent()?.parent()?.join("Resources");
    std::fs::read(resources.join("icon.icns")).ok()
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

    if wait_for_device() {
        return InstallOutcome::Installed;
    }

    probe::invalidate();
    if !probe::driver_bundle_installed() {
        return InstallOutcome::Failed("install ran but the driver bundle is not present".into());
    }

    // The bundle is there but no device appeared. macOS loads `AudioServerPlugIn`s only when
    // `coreaudiod` starts, and the package's postinstall already tries to bounce it — so reaching
    // here means that attempt failed. One more try is much better than telling the user to reboot
    // for what is a daemon restart, and it rides the elevation they have already granted.
    crate::teprintln!(
        "[audio] driver installed but no device appeared; restarting coreaudiod and re-checking"
    );
    let restart = "/bin/launchctl kickstart -k system/com.apple.audio.coreaudiod \
                   || /usr/bin/killall coreaudiod";
    if run_privileged(restart).is_ok() && wait_for_device() {
        return InstallOutcome::Installed;
    }
    InstallOutcome::NeedsReboot
}

/// Poll for the virtual device to show up, for as long as a `coreaudiod` restart takes.
fn wait_for_device() -> bool {
    for _ in 0..10 {
        probe::invalidate(); // the whole point of this loop is to see the state change
        if probe::driver_healthy() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    false
}

pub fn uninstall() -> InstallOutcome {
    super::routing::recover_on_launch();
    // Without the driver there is no virtual device to be left as the default output, so the
    // recovery agent has no job — and leaving it loaded is how a LaunchAgent outlives the feature
    // that created it and keeps launching a path the user may since have deleted.
    super::routing::uninstall_watchdog_agent();

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

    probe::invalidate();
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
