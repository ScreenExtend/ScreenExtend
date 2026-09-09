/// Report a hardware-gated test as skipped instead of returning silently.
///
/// `teprintln!` only reaches stderr when the logbus is in verbose mode, which no test turns
/// on, so a skipped test used to print `ok` and nothing else — six of them covered nothing on
/// a stock box while showing green. The harness captures this line too, so it needs
/// `--nocapture` to be seen live; the point is that it exists at all. Set `SE_REQUIRE_HW=1` on
/// a machine that is supposed to have the hardware and a skip becomes a failure.
#[track_caller]
pub(crate) fn skip(what: &str, reason: impl std::fmt::Debug) {
    let msg = format!("SKIP {what}: {reason:?}");
    if std::env::var("SE_REQUIRE_HW").as_deref() == Ok("1") {
        panic!("{msg} — SE_REQUIRE_HW=1 demands this path run");
    }
    eprintln!("{msg}");
}

/// Route the log bus to stderr for the rest of the process.
///
/// The encode tests report their measurements with `tprintln!`, which only reaches stderr when
/// `logbus::VERBOSE` is set — and nothing under `cargo test` sets it, so the only per-frame
/// encode latency numbers this repo produces went into a ring buffer and were discarded at
/// exit. The harness still captures them; `--nocapture` is what shows them.
pub(crate) fn show_measurements() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| crate::logbus::set_verbose(true));
}

mod amd_layout;
mod h264;
mod intel_encoder;
mod intel_layout;
mod nvenc_encoder;
mod nvenc_layout;
mod pipeline;
mod scaler;
mod x264_encoder;

#[test]
#[ignore = "requires an interactive desktop; run with --ignored"]
fn dxgi_duplication_probe() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/target/dxgi_probe.bmp");
    super::dxgi::probe_to_bmp(0, path).expect("dxgi duplication probe");
}
