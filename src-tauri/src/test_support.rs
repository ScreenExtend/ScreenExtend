//! Shared helpers for the test suite. Compiled only under `cfg(test)`.
//!
//! The one rule this module exists to enforce: **a test that verified nothing must say so.**
//! An audit of the suite found six hardware-gated tests reporting `ok` after returning at the
//! first line, and three more skipping with no message at all — on the machine that most needs
//! the coverage, which is exactly where a green tick is most misleading. Every runtime-gated
//! early return now goes through [`skip`], and every test that cannot run unattended carries
//! `#[ignore = "<reason>"]` so the harness prints the reason instead of a bare `ignored`.

/// Report a runtime-gated test as skipped instead of returning silently.
///
/// Prints on plain stderr (not `teprintln!`, which only reaches the terminal when the logbus is
/// in verbose mode — nothing under `cargo test` turns that on, which is how six skips became
/// invisible). The harness captures stderr, so `--nocapture` is what shows the line live; the
/// point is that it exists at all and shows up in a failure report.
///
/// Set `SE_REQUIRE_HW=1` on a machine that is supposed to have the capability and a skip becomes
/// a failure, so a release box can be made strict without a second test suite.
#[track_caller]
pub(crate) fn skip(what: &str, reason: impl std::fmt::Debug) {
    let msg = format!("SKIP {what}: {reason:?}");
    if std::env::var("SE_REQUIRE_HW").as_deref() == Ok("1") {
        panic!("{msg} — SE_REQUIRE_HW=1 demands this path run");
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
/// The encode tests report their measurements with `tprintln!`, which only reaches stderr when
/// `logbus::VERBOSE` is set — and nothing under `cargo test` sets it, so the only per-frame
/// encode latency numbers this repo produces went into a ring buffer and were discarded at exit.
/// The harness still captures them; `--nocapture` is what shows them.
pub(crate) fn show_measurements() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| crate::logbus::set_verbose(true));
}

/// `p50 / p90 / p95 / p99 / max` of a sample set, in the sample's own unit.
///
/// Nearest-rank on a sorted copy. Latency harnesses print these; `scripts/perf-gate.mjs` parses
/// the line they end up on, so the shape of the numbers matters more than the exact estimator.
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
