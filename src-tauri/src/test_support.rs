//! Shared helpers for the test suite. Compiled only under `cfg(test)`.
//!
//! The one rule this module enforces: **a test that verified nothing must say so.** A
//! runtime-gated early return goes through [`skip`], and a test that cannot run unattended
//! carries `#[ignore = "<reason>"]` so the harness prints the reason, not a bare `ignored`.

/// Report a runtime-gated test as skipped instead of returning silently.
///
/// Prints on plain stderr, not `teprintln!`: nothing under `cargo test` puts the logbus in
/// verbose mode. The harness captures stderr, so `--nocapture` shows the line live.
///
/// `SE_REQUIRE_HW=1` turns a skip into a panic, for a machine that should have the capability.
#[track_caller]
pub(crate) fn skip(what: &str, reason: impl std::fmt::Debug) {
    let msg = format!("SKIP {what}: {reason:?}");
    if std::env::var("SE_REQUIRE_HW").as_deref() == Ok("1") {
        panic!("{msg} (SE_REQUIRE_HW=1 demands this path run)");
    }
    eprintln!("{msg}");
}

/// [`skip`] for a condition that is a plain sentence rather than an error value.
#[track_caller]
pub(crate) fn skip_because(what: &str, reason: &str) {
    skip(what, Reason(reason));
}

/// Renders as the bare sentence rather than a quoted string, so `SKIP x: no GPU` reads as prose.
struct Reason<'a>(&'a str);

impl std::fmt::Debug for Reason<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Route the log bus to stderr for the rest of the process.
///
/// The encode tests report measurements with `tprintln!`, which reaches stderr only when
/// `logbus::VERBOSE` is set, and nothing under `cargo test` sets it. The harness still captures
/// them, so pass `--nocapture`.
pub(crate) fn show_measurements() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| crate::logbus::set_verbose(true));
}

/// `p50 / p90 / p95 / p99 / max` of a sample set, in the sample's own unit.
///
/// Nearest-rank on a sorted copy. `scripts/perf-gate.mjs` parses the line the latency harnesses
/// print these on.
pub(crate) fn percentiles(samples: &[u128]) -> Percentiles {
    assert!(!samples.is_empty(), "percentiles of an empty sample set");
    let mut v = samples.to_vec();
    v.sort_unstable();
    let at = |p: usize| v[(v.len() * p / 100).min(v.len() - 1)];
    Percentiles {
        p50: at(50),
        p90: at(90),
        p95: at(95),
        p99: at(99),
        max: *v.last().unwrap(),
        mean: v.iter().sum::<u128>() / v.len() as u128,
        count: v.len(),
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Percentiles {
    pub p50: u128,
    pub p90: u128,
    pub p95: u128,
    pub p99: u128,
    pub max: u128,
    pub mean: u128,
    pub count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_are_nearest_rank_over_a_sorted_copy() {
        let samples: Vec<u128> = (1..=100).collect();
        let p = percentiles(&samples);
        assert_eq!(p.count, 100);
        assert_eq!(p.p50, 51);
        assert_eq!(p.p90, 91);
        assert_eq!(p.p95, 96);
        assert_eq!(p.p99, 100);
        assert_eq!(p.max, 100);
        assert_eq!(p.mean, 50);
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        let p = percentiles(&[7]);
        assert_eq!((p.p50, p.p99, p.max, p.mean), (7, 7, 7, 7));
    }

    #[test]
    fn the_input_order_does_not_matter() {
        let a = percentiles(&[9, 1, 5, 3, 7]);
        let b = percentiles(&[1, 3, 5, 7, 9]);
        assert_eq!((a.p50, a.p99, a.max), (b.p50, b.p99, b.max));
    }
}
