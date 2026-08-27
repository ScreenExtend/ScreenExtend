mod format;

#[test]
fn probe_on_legacy_range_is_actionable_not_unsupported() {
    use crate::macos_utils::audio::legacy::probe::eligible_os;
    use crate::macos_utils::audio::{probe_audio_backend, AudioBackend};
    use crate::macos_utils::streamer::macos_at_least;

    if macos_at_least(13, 0) {
        return;
    }
    let backend = probe_audio_backend();
    if eligible_os() {
        assert!(
            matches!(
                backend,
                AudioBackend::NeedsDriverInstall | AudioBackend::VirtualDevice
            ),
            "expected an actionable legacy backend on 10.15–12.x, got {backend:?}"
        );
    } else {
        assert_eq!(backend, AudioBackend::Unsupported);
    }
}

#[test]
fn compatibility_report_marks_legacy_tier_actionable() {
    use crate::macos_utils::audio::legacy::probe::eligible_os;
    use crate::macos_utils::streamer::macos_at_least;

    if macos_at_least(13, 0) {
        return;
    }
    let report = crate::macos_utils::compatibility::check_system_requirements();
    if eligible_os() {
        assert!(
            report.audio_backend == "needs_driver_install"
                || report.audio_backend == "virtual_device",
            "expected an actionable legacy audio_backend on 10.15–12.x, got {:?}",
            report.audio_backend
        );
        assert!(
            !report
                .unsupported_apis
                .iter()
                .any(|a| a.name == "System Audio Capture"),
            "the legacy tier is actionable and must not be listed as unsupported, got: {:?}",
            report.unsupported_apis
        );
    } else {
        assert_eq!(report.audio_backend, "unsupported");
    }
}
