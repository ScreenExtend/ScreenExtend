//! When a client was last heard from. A phone whose tab is killed or whose screen goes off sends
//! no `/leave`, and ICE alone takes its disconnected timeout plus a recovery wait to give up, so
//! the session's own traffic decides sooner.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Silence this long ends the transport. The page sends a PING and a STATS frame every second
/// over the reliable channel and its RTCP flows natively even from a hidden tab, so this is six
/// missed heartbeats, not a lossy second.
pub const SILENCE_LIMIT: Duration = Duration::from_secs(6);

/// How often the watchdog looks.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub struct Liveness {
    /// Host clock nanoseconds of the last thing received; 0 until the transport is up.
    last_ns: AtomicU64,
}

impl Liveness {
    /// Something arrived from the client.
    pub fn heard(&self, now_ns: u64) {
        self.last_ns.fetch_max(now_ns, Ordering::Relaxed);
    }

    /// Whether the client has said nothing for `limit`. Never before the first [`Self::heard`]:
    /// a transport still connecting is ICE's to judge.
    pub fn silent_for(&self, now_ns: u64, limit: Duration) -> bool {
        let last = self.last_ns.load(Ordering::Relaxed);
        last != 0 && now_ns.saturating_sub(last) >= limit.as_nanos() as u64
    }

    pub fn last_ns(&self) -> u64 {
        self.last_ns.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn nothing_is_silent_before_the_client_was_ever_heard() {
        let l = Liveness::default();
        assert!(!l.silent_for(100 * S, SILENCE_LIMIT));
    }

    #[test]
    fn a_heartbeat_every_second_is_never_silent() {
        let l = Liveness::default();
        for t in 1..60 {
            l.heard(t * S);
            assert!(!l.silent_for(t * S + S / 2, SILENCE_LIMIT), "second {t}");
        }
    }

    #[test]
    fn a_lossy_stretch_shorter_than_the_limit_is_kept() {
        let l = Liveness::default();
        l.heard(10 * S);
        assert!(!l.silent_for(15 * S, SILENCE_LIMIT));
        l.heard(15 * S + S / 2);
        assert!(!l.silent_for(21 * S, SILENCE_LIMIT));
    }

    #[test]
    fn six_seconds_of_nothing_is_silent() {
        let l = Liveness::default();
        l.heard(10 * S);
        assert!(!l.silent_for(15 * S, SILENCE_LIMIT));
        assert!(l.silent_for(16 * S, SILENCE_LIMIT));
    }

    #[test]
    fn a_late_stamp_never_moves_the_clock_backwards() {
        let l = Liveness::default();
        l.heard(20 * S);
        l.heard(12 * S);
        assert_eq!(l.last_ns(), 20 * S);
        assert!(!l.silent_for(25 * S, SILENCE_LIMIT));
    }
}
