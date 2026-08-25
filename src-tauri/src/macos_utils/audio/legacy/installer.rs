//! Install / uninstall / health-check for the ScreenExtend Audio driver (PRD-macos-legacy-audio.md
//! §8.4).
//!
//! The bundle goes to `/Library/Audio/Plug-Ins/HAL/`, which is root-owned — **admin rights are
//! required and there is no fully silent install.** We ship a signed + notarized `.pkg` and run it
//! with a single authorization prompt (`installer(8)` via an admin `osascript` escalation), then
//! everything else is scripted by the pkg's postinstall (chown + coreaudiod restart). A persistent
//! SMJobBless helper (the SwiftPrivilegedHelper reference) is the harder alternative if we ever need
//! repeated privileged actions without re-prompting; one prompt per install/uninstall is enough for
//! now.
//!
//! On recent macOS the coreaudiod restart the pkg attempts is often refused, so a successful
//! `installer` run does not guarantee the device appears immediately — [`install_pkg`] reports
//! `NeedsReboot` in that case rather than claiming success (PRD §7.5).

use std::path::Path;
use std::process::Command;

use super::{branding, probe};

/// Outcome of an install/uninstall attempt, surfaced to the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallOutcome {
    /// Installed and the device is live now.
    Installed,
    /// Uninstalled cleanly.
    Uninstalled,
    /// Operation ran on disk, but coreaudiod didn't pick it up — a reboot / re-login is needed.
    NeedsReboot,
    /// The user dismissed the admin prompt.
    Cancelled,
    /// Anything else, with a message.
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

/// Run an admin shell command via osascript's "with administrator privileges" (one GUI prompt).
/// Returns Ok(()) on success, Err(Cancelled) if the user dismissed the prompt, Err(Failed) else.
fn run_privileged(shell_cmd: &str) -> Result<(), InstallOutcome> {
    // AppleScript string: escape embedded double quotes for the `do shell script` literal.
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
    // osascript returns "User canceled. (-128)" when the auth prompt is dismissed.
    if stderr.contains("-128") || stderr.to_lowercase().contains("cancel") {
        Err(InstallOutcome::Cancelled)
    } else {
        Err(InstallOutcome::Failed(stderr.trim().to_string()))
    }
}

/// Install the driver from a signed/notarized `.pkg` (one admin prompt), then verify the device
/// actually appeared. `pkg_path` is resolved by the caller from the app's resources.
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

    // The pkg is laid down; did coreaudiod publish the device? Give it a moment, then check.
    for _ in 0..10 {
        if probe::driver_healthy() {
            return InstallOutcome::Installed;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    // Bundle on disk but no device → coreaudiod restart was refused; a reboot is needed (§7.5).
    if probe::driver_bundle_installed() {
        InstallOutcome::NeedsReboot
    } else {
        InstallOutcome::Failed("install ran but the driver bundle is not present".into())
    }
}

/// Fully remove the driver (one admin prompt) and nudge coreaudiod, then restore the default output
/// device (PRD §8.4: uninstall with equal care). Best-effort on the coreaudiod restart.
pub fn uninstall() -> InstallOutcome {
    // Restore the user's output device first, in case we currently hold it.
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

    // POSIX shm outlives the driver (measured: it persists after the bundle is removed), so unlink
    // the transport segment to fully clean up.
    if let Ok(name) = std::ffi::CString::new(branding::SHM_NAME) {
        // SAFETY: shm_unlink on our fixed segment name; harmless if it's already gone.
        unsafe {
            libc::shm_unlink(name.as_ptr());
        }
    }

    // Confirm the bundle is gone. The device may linger until coreaudiod cycles; the bundle being
    // gone is the authoritative "uninstalled" signal.
    if probe::driver_bundle_installed() {
        InstallOutcome::Failed("driver bundle still present after uninstall".into())
    } else if probe::driver_healthy() {
        // Bundle gone but device still cached → needs a coreaudiod cycle / reboot to disappear.
        InstallOutcome::NeedsReboot
    } else {
        InstallOutcome::Uninstalled
    }
}

/// Current health, for the UI to decide install vs. repair vs. ready.
pub fn health() -> probe::LegacyState {
    probe::legacy_state()
}
