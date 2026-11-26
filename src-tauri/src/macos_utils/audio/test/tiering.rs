//! The audio tier ranking, exhaustively, on any Mac.
//!
//! `AudioProbes` is what the machine answered; `rank_backend` and `rank_capable_backends` are the
//! policy, with no `dlsym`, class lookup or filesystem access in them, so the 14.2 and 13.0
//! branches are asserted on a 10.15 machine too.

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
    // the order is the contract: Process Tap is audio-only and shows no recording indicator, SCK
    // needs the Screen Recording grant and a dummy video stream, the driver takes over the
    // system output device
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
    // the UI presents `Unsupported` as a dead end, and the driver covers the whole 10.15 floor,
    // so the ranking must never produce it
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
    // if `probe_audio_backend` reported a tier the candidate loop does not lead with, the
    // compatibility card would describe a session the host never opens
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
    // on 13.x with Screen Recording denied, SCK is selected and fails to start: the installed
    // driver must still be there behind it
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
    // `start_capture` bails with the install prompt here: `make_backend` treats the two
    // non-startable variants as unreachable and would panic
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
    // these strings reach the desktop UI through `CompatibilityReport.audio_backend`, and the
    // macos-smoke CI job compares them literally
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
    // the one assertion on the real probes: whatever they answer, the live ranking must be a
    // tier the pure one can produce
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
