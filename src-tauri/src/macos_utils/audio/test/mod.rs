mod clock;
mod format;
mod tiering;

/// Below 12.3 there is no native system-audio API at all, so the answer must be the driver — an
/// action the user can take — and never `Unsupported`, which the UI presents as a dead end.
#[test]
fn probe_without_a_native_tier_is_actionable_not_unsupported() {
    use crate::macos_utils::audio::{probe_audio_backend, AudioBackend};

    let backend = probe_audio_backend();
    assert_ne!(
        backend,
        AudioBackend::Unsupported,
        "system audio is reachable on every macOS this app runs on"
    );
}

#[test]
fn compatibility_report_marks_the_audio_tier_actionable() {
    let report = crate::macos_utils::compatibility::check_system_requirements();
    assert_ne!(report.audio_backend, "unsupported");
    assert!(
        !report
            .unsupported_apis
            .iter()
            .any(|a| a.name == "System Audio Capture"),
        "an installable tier is always available and must not be listed as unsupported, got: {:?}",
        report.unsupported_apis
    );
}

/// Tiering must not shell out: `macos_at_least` forks `sw_vers`, fails open when it cannot read
/// the version, and answers a question the class lookups answer exactly.
#[test]
fn the_audio_module_does_not_gate_on_an_os_version() {
    let src = include_str!("../mod.rs");
    assert!(
        !src.contains("macos_at_least("),
        "audio/mod.rs must probe by capability, not by OS version"
    );
    let probe = include_str!("../legacy/probe.rs");
    assert!(
        !probe.contains("macos_at_least("),
        "legacy/probe.rs must probe by capability, not by OS version"
    );
}
