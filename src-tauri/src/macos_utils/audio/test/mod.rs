//! macOS audio unit tests. Only the format-conversion / downmix math is exercisable without a
//! Process Tap grant or SCK hardware (mirrors `windows_utils/audio/test/format.rs`); the two native
//! capture backends need macOS 13.0+/14.2+ hardware (see `AUDIO_NOTES_MACOS.md`), and the legacy
//! virtual-device tier needs a signed/installed driver (see `AUDIO_NOTES_MACOS_LEGACY.md`).

mod format;

/// On a host below the SCK/Process-Tap floor (macOS < 13.0 — e.g. the 10.15 dev box), the tiered
/// probe must NOT touch a 14.2/13.0 symbol. Since the legacy virtual-device tier (10.15–12.x) now
/// exists, the outcome there is the **actionable** `NeedsDriverInstall` (or `VirtualDevice` if the
/// driver happens to be installed) — never `Unsupported`, which is reserved for below-10.15. The
/// mere fact that this binary loads and runs on 10.15 also proves the dyld-safety gating (no
/// undefined Process-Tap/SCK link symbols).
///
/// On 13.0+ the probe may construct a real tap / SCK stream (side effects, a TCC prompt), so it is
/// skipped there — this assertion is only meaningful, and only side-effect-free, below the floor.
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
        // 10.15–12.x: the virtual-device tier applies — a one-time install, not a dead end.
        assert!(
            matches!(
                backend,
                AudioBackend::NeedsDriverInstall | AudioBackend::VirtualDevice
            ),
            "expected an actionable legacy backend on 10.15–12.x, got {backend:?}"
        );
    } else {
        // Below 10.15 there is genuinely nothing we can do.
        assert_eq!(backend, AudioBackend::Unsupported);
    }
}

/// End-to-end check of the compatibility integration on a below-13.0 host. On the legacy range the
/// report exposes an actionable `audio_backend` (`needs_driver_install` / `virtual_device`) and must
/// NOT carry a "System Audio Capture" *unsupported* entry (that would misrepresent an installable
/// feature as a dead end — PRD-macos-legacy-audio §4, §9.2). Guarded to below-13.0 so it stays
/// meaningful and side-effect-free.
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
