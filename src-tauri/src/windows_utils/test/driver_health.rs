use super::*;
use serde_json::json;

fn proc(pid: u32, image: &str, mb: u64, threads: u32) -> HostProcess {
    HostProcess {
        pid,
        image: image.to_string(),
        commit_bytes: mb * 1024 * 1024,
        threads,
        handles: threads * 7,
    }
}

/// The 2026-10-06 machine, as `Get-Process WUDFHost` printed it: one leaked host and four
/// small ones belonging to other UMDF drivers.
fn machine() -> Vec<HostProcess> {
    vec![
        proc(4, "System", 0, 300),
        proc(2164, "WUDFHost.exe", 2, 5),
        proc(1992, "WUDFHost.exe", 14_752, 1270),
        proc(2240, "WUDFHost.exe", 5, 5),
        proc(3832, "WUDFHost.exe", 9, 19),
        proc(54508, "ScreenExtend.exe", 400, 90),
    ]
}

#[test]
fn the_pipe_server_is_chosen_over_a_larger_host() {
    let mut procs = machine();
    // Make a stranger the largest: the pipe still decides.
    procs.push(proc(7777, "wudfhost.exe", 50_000, 10));
    let (host, how) = select_driver_host(&procs, Some(3832)).unwrap();
    assert_eq!(host.pid, 3832);
    assert_eq!(how, Identified::PipeServer);
}

#[test]
fn without_a_pipe_server_the_largest_wudfhost_is_a_labelled_guess() {
    let procs = machine();
    let (host, how) = select_driver_host(&procs, None).unwrap();
    assert_eq!(host.pid, 1992);
    assert_eq!(how, Identified::LargestWudfHost);
}

#[test]
fn a_pipe_served_by_something_other_than_wudfhost_is_not_trusted() {
    let procs = machine();
    // ScreenExtend.exe itself serving the name (a test mock) must not be measured as the driver.
    let (host, how) = select_driver_host(&procs, Some(54508)).unwrap();
    assert_eq!(host.pid, 1992);
    assert_eq!(how, Identified::LargestWudfHost);
    // And a pid that has already exited falls back the same way.
    assert_eq!(
        select_driver_host(&procs, Some(1)).unwrap().1,
        Identified::LargestWudfHost
    );
}

#[test]
fn no_wudfhost_at_all_is_no_answer() {
    let procs = vec![proc(4, "System", 0, 300)];
    assert!(select_driver_host(&procs, Some(4)).is_none());
}

#[test]
fn the_default_thresholds_trip_on_the_measured_leak_and_not_on_a_fresh_host() {
    let t = Thresholds::default();
    assert_eq!((t.commit_mb, t.threads), (8192, 800));
    // A fresh boot, and a long evening of joins (60 × 47 MB, 60 × 5 threads).
    assert!(!t.exceeded(10, 19));
    assert!(!t.exceeded(10 + 60 * 47, 19 + 60 * 5));
    // The machine today, and the failure point.
    assert!(t.exceeded(14_752, 1270));
    assert!(t.exceeded(36 * 1024, 3200));
    // Either axis alone.
    assert!(t.exceeded(8192, 0));
    assert!(t.exceeded(0, 800));
    assert!(!t.exceeded(8191, 799));
}

#[test]
fn the_env_override_replaces_the_commit_threshold_and_switches_threads_off() {
    let t = Thresholds::from_values(Some("999999"), None);
    assert_eq!((t.commit_mb, t.threads), (999_999, 0));
    // With threads off, a thread-heavy host no longer trips it: the override really suppresses.
    assert!(!t.exceeded(14_752, 1270));
    // A small value forces it.
    assert!(Thresholds::from_values(Some("1"), None).exceeded(9, 19));
    // Both given.
    let both = Thresholds::from_values(Some("100"), Some("50"));
    assert_eq!((both.commit_mb, both.threads), (100, 50));
    // Threads alone keeps the default commit threshold.
    let threads = Thresholds::from_values(None, Some("10"));
    assert_eq!((threads.commit_mb, threads.threads), (8192, 10));
    // Garbage and zero are ignored rather than turning the gate into "always".
    assert_eq!(
        Thresholds::from_values(Some("lots"), None),
        Thresholds::default()
    );
    assert_eq!(
        Thresholds::from_values(Some("0"), None),
        Thresholds::default()
    );
}

#[test]
fn assess_reports_the_leaked_host_and_asks_for_a_restart() {
    let h = assess(&machine(), PipeProbe::Server(1992), Thresholds::default());
    assert!(h.supported);
    assert!(h.needs_restart);
    assert_eq!(h.pid, Some(1992));
    assert_eq!(h.identified_by, "pipeServer");
    assert_eq!(h.commit_mb, 14_752);
    assert_eq!(h.threads, 1270);
    assert_eq!(h.threshold_mb, 8192);
    assert!(h.detail.contains("RESTART NEEDED"), "{}", h.detail);
}

#[test]
fn assess_never_asks_when_the_driver_is_not_running() {
    // The largest WUDFHost is huge, but no pipe means it is somebody else's driver.
    let h = assess(&machine(), PipeProbe::Absent, Thresholds::default());
    assert!(!h.needs_restart);
    assert_eq!(h.pid, None);
    assert_eq!(h.identified_by, "none");
}

#[test]
fn assess_with_a_busy_pipe_uses_the_labelled_fallback() {
    let h = assess(&machine(), PipeProbe::Present, Thresholds::default());
    assert!(h.needs_restart);
    assert_eq!(h.identified_by, "largestWudfHost");
    assert_eq!(h.pid, Some(1992));
}

#[test]
fn a_healthy_driver_host_is_left_alone() {
    let h = assess(&machine(), PipeProbe::Server(3832), Thresholds::default());
    assert!(!h.needs_restart);
    assert_eq!(h.pid, Some(3832));
}

#[test]
fn hardware_ids_are_read_out_of_a_multi_sz() {
    let mut units: Vec<u16> = "Root\\VirtualDisplayDriver".encode_utf16().collect();
    units.push(0);
    units.extend("Other".encode_utf16());
    units.extend([0, 0]);
    assert_eq!(
        multi_sz(&units),
        vec![
            "Root\\VirtualDisplayDriver".to_string(),
            "Other".to_string()
        ]
    );
    assert!(multi_sz(&[0, 0]).is_empty());
}

// the refusal paths of `drivers restart`

#[test]
fn a_set_up_host_refuses_even_when_idle() {
    // `joins` exists only once AppState does, i.e. the host holds a driver client.
    let stats = json!({ "sessions": {}, "joins": [] , "live_transport_sessions": 0 });
    assert_eq!(
        restart_refusal(Some(&stats), Some(42), Ok(0), &[42]),
        Some(Refusal::HostConnected {
            pid: Some(42),
            joins: 0
        })
    );
}

#[test]
fn a_streaming_host_refuses_and_counts_its_joins() {
    let stats = json!({ "sessions": { "j1": {}, "j2": {} }, "joins": [{}, {}] });
    let r = restart_refusal(Some(&stats), Some(42), Ok(2), &[42]).unwrap();
    assert_eq!(
        r,
        Refusal::HostConnected {
            pid: Some(42),
            joins: 2
        }
    );
    assert!(r.sentence().contains("ScreenExtend stop"));
}

#[test]
fn the_desktop_apps_own_startup_gate_is_let_through() {
    // The app asking for the restart answers STATS before `setup`: no `joins`, no sessions. It
    // is the only other ScreenExtend process, and the driver lists nothing.
    let stats = json!({ "sessions": {}, "live_transport_sessions": 0 });
    assert_eq!(restart_refusal(Some(&stats), Some(42), Ok(0), &[42]), None);
    // Even when the driver's list could not be read: the only other process is the caller.
    assert_eq!(
        restart_refusal(Some(&stats), Some(42), Err("busy".into()), &[42]),
        None
    );
}

#[test]
fn displays_owned_by_another_host_refuse() {
    // An isolated `serve` (its own data dir, so no control socket here) holding a display.
    let r = restart_refusal(None, None, Ok(1), &[9001]).unwrap();
    assert_eq!(
        r,
        Refusal::DisplaysAttached {
            monitors: 1,
            others: vec![9001]
        }
    );
    assert!(r.sentence().contains("9001"));
}

#[test]
fn an_unreadable_display_list_with_another_host_alive_refuses() {
    assert!(matches!(
        restart_refusal(None, None, Err("timed out".into()), &[9001]),
        Some(Refusal::Unknown { .. })
    ));
}

#[test]
fn orphaned_displays_with_no_host_alive_do_not_block_the_restart_that_clears_them() {
    assert_eq!(restart_refusal(None, None, Ok(3), &[]), None);
}

#[test]
fn an_idle_other_process_with_no_displays_does_not_block() {
    assert_eq!(restart_refusal(None, None, Ok(0), &[9001]), None);
}

#[test]
fn every_exit_code_has_a_reason_the_ui_knows() {
    assert_eq!(reason_for_exit(EXIT_RESTARTED), "restarted");
    assert_eq!(reason_for_exit(EXIT_NOT_ELEVATED), "notElevated");
    assert_eq!(reason_for_exit(EXIT_REFUSED), "refused");
    assert_eq!(reason_for_exit(EXIT_TIMEOUT), "timeout");
    assert_eq!(reason_for_exit(EXIT_NEEDS_REBOOT), "needsReboot");
    assert_eq!(reason_for_exit(EXIT_NOT_INSTALLED), "notInstalled");
    assert_eq!(reason_for_exit(EXIT_FAILED), "failed");
    assert_eq!(reason_for_exit(2), "failed");
    // Distinct from usage (2) and from `doctor`'s permission code (3).
    let codes = [
        EXIT_FAILED,
        EXIT_NOT_ELEVATED,
        EXIT_REFUSED,
        EXIT_TIMEOUT,
        EXIT_NEEDS_REBOOT,
        EXIT_NOT_INSTALLED,
    ];
    for c in codes {
        assert!(c != 2 && c != 3, "{c}");
    }
}

/// The live read, on whatever machine runs the suite: unelevated, it must find the driver's
/// host by its pipe when the driver is installed.
#[test]
fn reads_the_live_driver_host_unelevated() {
    let procs = match process_list() {
        Ok(p) => p,
        Err(e) => return crate::test_support::skip_because("driver host read", &e),
    };
    assert!(
        procs.iter().any(|p| p.pid == std::process::id()),
        "this process is missing from the process list"
    );
    match probe_pipe_server() {
        PipeProbe::Absent => crate::test_support::skip_because(
            "driver host identification",
            "the display driver is not installed on this machine",
        ),
        PipeProbe::Present => crate::test_support::skip_because(
            "driver host identification",
            "the driver's pipe is busy and would not name its server",
        ),
        PipeProbe::Server(pid) => {
            let (host, how) = select_driver_host(&procs, Some(pid)).expect("a host");
            assert_eq!(how, Identified::PipeServer, "pid {pid} is not a WUDFHost");
            eprintln!(
                "driver host pid {} — {} MB commit, {} threads, {} handles",
                host.pid,
                host.commit_mb(),
                host.threads,
                host.handles
            );
        }
    }
}
