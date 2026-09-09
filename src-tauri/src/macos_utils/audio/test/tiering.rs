//! The audio tier ranking, exhaustively, on any Mac.
//!
//! macOS audio coverage was inverted: the tier that is never selected on 13.0+ carried 20 of the
//! 34 tests, while the two that run on every supported Mac — Core Audio Process Tap and
//! ScreenCaptureKit audio — had none. The reason was structural rather than an oversight: the
//! only dev box is 10.15, where neither tier's classes exist, so nothing about them could be
//! executed there at all.
//!
//! Splitting the probing from the ranking fixes that. `AudioProbes` is what the machine
//! answered; `rank_backend` and `rank_capable_backends` are what we do about it, with no
//! `dlsym`, no class lookup and no filesystem in them. The table below is every combination —
//! including the ones this machine can never reach — so the 14.2 and 13.0 branches are asserted
//! on the 10.15 floor and on all three CI runners.
//!
//! (It is still under `macos_utils`, so it does not run on Windows. Moving the ranking out of
//! the platform module to gain that would put macOS policy in cross-platform code, which is a
//! worse trade than losing one runner.)

use crate::macos_utils::audio::{rank_backend, rank_capable_backends, AudioBackend, AudioProbes};

fn probes(process_tap: bool, sck: bool, driver_healthy: bool) -> AudioProbes {
    AudioProbes {
        process_tap,
        sck,
        driver_healthy,
    }
}

#[test]
fn the_ranking_is_process_tap_then_sck_then_the_driver() {
    // Every combination of the three probes. The order is the whole contract: Process Tap is
    // audio-only and shows no screen-recording indicator, SCK needs the Screen Recording grant
    // and rides a dummy video stream, and the driver takes over the system output device.
    let cases = [
        // (process_tap, sck, driver, expected)
        (true, true, true, AudioBackend::ProcessTap),
        (true, true, false, AudioBackend::ProcessTap),
        (true, false, true, AudioBackend::ProcessTap),
        (true, false, false, AudioBackend::ProcessTap),
        (false, true, true, AudioBackend::ScreenCaptureKitAudio),
        (false, true, false, AudioBackend::ScreenCaptureKitAudio),
        (false, false, true, AudioBackend::VirtualDevice),
        (false, false, false, AudioBackend::NeedsDriverInstall),
    ];
    for (tap, sck, driver, want) in cases {
        assert_eq!(
            rank_backend(probes(tap, sck, driver)),
            want,
            "probes(process_tap={tap}, sck={sck}, driver={driver})"
        );
    }
}

#[test]
fn no_combination_of_probes_reports_unsupported() {
    // `Unsupported` is presented in the UI as a dead end. There is always something the user can
    // do — the driver covers the whole 10.15 floor — so the ranking must never produce it.
    for tap in [false, true] {
        for sck in [false, true] {
            for driver in [false, true] {
                assert_ne!(
                    rank_backend(probes(tap, sck, driver)),
                    AudioBackend::Unsupported,
                    "probes({tap}, {sck}, {driver}) reported a dead end"
                );
            }
        }
    }
}

#[test]
fn the_reported_tier_is_the_first_one_start_capture_would_try() {
    // Two functions, one decision. If `probe_audio_backend` reported a tier the candidate loop
    // does not lead with, the compatibility card would describe a session the host never opens.
    for tap in [false, true] {
        for sck in [false, true] {
            for driver in [false, true] {
                let p = probes(tap, sck, driver);
                let candidates = rank_capable_backends(p);
                match candidates.first() {
                    Some(first) => assert_eq!(
                        rank_backend(p),
                        *first,
                        "probes({tap}, {sck}, {driver}) reports one tier and tries another"
                    ),
                    None => assert_eq!(
                        rank_backend(p),
                        AudioBackend::NeedsDriverInstall,
                        "no startable tier must read as an install prompt"
                    ),
                }
            }
        }
    }
}

#[test]
fn the_driver_stays_in_the_ladder_behind_a_native_tier() {
    // The failure this exists for: on 13.x with Screen Recording denied, SCK is selected, fails
    // to start, and there is nothing after it — even with the driver installed and working.
    let ladder = rank_capable_backends(probes(false, true, true));
    assert_eq!(
        ladder,
        vec![
            AudioBackend::ScreenCaptureKitAudio,
            AudioBackend::VirtualDevice
        ]
    );

    let ladder = rank_capable_backends(probes(true, true, true));
    assert_eq!(
        ladder,
        vec![
            AudioBackend::ProcessTap,
            AudioBackend::ScreenCaptureKitAudio,
            AudioBackend::VirtualDevice
        ],
        "a 14.2 Mac with the driver installed has three rungs, not one"
    );
}

#[test]
fn a_machine_with_nothing_offers_no_candidate_to_start() {
    // `start_capture` bails with the install prompt rather than constructing a backend that
    // cannot run; `make_backend` treats those two variants as unreachable and would panic.
    assert!(rank_capable_backends(probes(false, false, false)).is_empty());
}

#[test]
fn the_ladder_never_repeats_a_tier() {
    for tap in [false, true] {
        for sck in [false, true] {
            for driver in [false, true] {
                let ladder = rank_capable_backends(probes(tap, sck, driver));
                for (i, kind) in ladder.iter().enumerate() {
                    assert!(
                        !ladder[i + 1..].contains(kind),
                        "probes({tap}, {sck}, {driver}) tries {kind:?} twice: {ladder:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn the_tier_names_the_ui_switches_on_are_stable() {
    // These strings reach the desktop UI through `CompatibilityReport.audio_backend` and CI
    // compares them literally (`.github/workflows/ci.yml`, the macos-smoke job).
    assert_eq!(AudioBackend::ProcessTap.as_str(), "process_tap");
    assert_eq!(
        AudioBackend::ScreenCaptureKitAudio.as_str(),
        "screencapturekit"
    );
    assert_eq!(AudioBackend::VirtualDevice.as_str(), "virtual_device");
    assert_eq!(
        AudioBackend::NeedsDriverInstall.as_str(),
        "needs_driver_install"
    );
    assert_eq!(AudioBackend::Unsupported.as_str(), "unsupported");
}

#[test]
fn this_machine_ranks_the_same_way_the_table_says_it_should() {
    // The one assertion that touches the real probes: whatever they answered, the live ranking
    // has to agree with the pure one. That is what keeps the table above from describing a
    // function nothing calls.
    use crate::macos_utils::audio::probe_audio_backend;

    let live = probe_audio_backend();
    let expected = [
        AudioBackend::ProcessTap,
        AudioBackend::ScreenCaptureKitAudio,
        AudioBackend::VirtualDevice,
        AudioBackend::NeedsDriverInstall,
    ];
    assert!(
        expected.contains(&live),
        "this Mac reported {live:?}, which is not a tier the ranking can produce"
    );
    println!("this Mac ranks to {live:?} ({})", live.as_str());
}
