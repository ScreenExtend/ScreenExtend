//! When the link governor keeps having to step in for a device, its network is the problem, and
//! the desktop UI proposes the hosted network once per cool-down.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};

use crate::streamer::lock::LockExt;
use crate::streamer::session::{self, DeviceInfo, SharedDeviceReporter, SharedSessions};
use crate::streamer::telemetry::SharedTelemetry;
use crate::streamer::webrtc_session::SessionLocality;

/// The span the engaged time is summed over.
pub(crate) const SLOW_LINK_WINDOW_MS: u64 = 120_000;
/// Engaged this long within the window is consistent, not a passing dip.
pub(crate) const SLOW_LINK_ENGAGED_MS: u64 = 30_000;
/// After a notice the same device is not told again for this long, whichever join it is on.
pub(crate) const SLOW_LINK_COOLDOWN_MS: u64 = 600_000;
/// How often a live join's governor total is read: well inside the window, and nothing per frame.
const SLOW_LINK_TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// The governor's engaged time for one join, over the last [`SLOW_LINK_WINDOW_MS`].
#[derive(Debug, Default)]
pub(crate) struct EngagedWindow {
    /// `(read at, engaged since the reading before)`, only the nonzero ones.
    samples: VecDeque<(u64, u64)>,
    last_total: Option<u64>,
}

impl EngagedWindow {
    /// One reading of the governor's running total. Returns the engaged time inside the window.
    pub(crate) fn observe(&mut self, now_ms: u64, total_ms: u64) -> u64 {
        let delta = match self.last_total {
            // the first reading is the baseline: time engaged before the watch began is not ours
            None => 0,
            Some(prev) if total_ms >= prev => total_ms - prev,
            // a governor built again on this telemetry counts from zero
            Some(_) => total_ms,
        };
        self.last_total = Some(total_ms);
        if delta > 0 {
            self.samples.push_back((now_ms, delta));
        }
        let floor = now_ms.saturating_sub(SLOW_LINK_WINDOW_MS);
        while self.samples.front().is_some_and(|&(t, _)| t <= floor) {
            self.samples.pop_front();
        }
        self.samples.iter().map(|&(_, d)| d).sum()
    }
}

/// A notice is due: the governor held on for long enough within the window, and this device was
/// not told within the cool-down.
pub(crate) fn slow_link_due(
    engaged_in_window_ms: u64,
    last_notice_ms: Option<u64>,
    now_ms: u64,
) -> bool {
    engaged_in_window_ms >= SLOW_LINK_ENGAGED_MS
        && last_notice_ms.is_none_or(|t| now_ms.saturating_sub(t) >= SLOW_LINK_COOLDOWN_MS)
}

/// When each device was last told, by token: a device that rejoins, or holds two tabs, is still
/// one device to the person at the host.
#[derive(Debug, Default)]
pub(crate) struct NoticeBook {
    last: HashMap<String, u64>,
}

impl NoticeBook {
    /// Takes the notice for `token` when one is due.
    pub(crate) fn claim(&mut self, token: &str, engaged_in_window_ms: u64, now_ms: u64) -> bool {
        if !slow_link_due(engaged_in_window_ms, self.last.get(token).copied(), now_ms) {
            return false;
        }
        self.last
            .retain(|_, t| now_ms.saturating_sub(*t) < SLOW_LINK_COOLDOWN_MS);
        self.last.insert(token.to_string(), now_ms);
        true
    }
}

fn notices() -> &'static Mutex<NoticeBook> {
    static BOOK: OnceLock<Mutex<NoticeBook>> = OnceLock::new();
    BOOK.get_or_init(Default::default)
}

/// The join came in on an address the hosted network's adapter holds, so its client is on it.
pub(crate) fn on_hosted_network(host_ip: Option<&str>, hosted_ips: &[String]) -> bool {
    let Some(host) = host_ip.and_then(|h| h.trim().parse::<std::net::IpAddr>().ok()) else {
        return false;
    };
    hosted_ips
        .iter()
        .filter_map(|ip| ip.trim().parse::<std::net::IpAddr>().ok())
        .any(|ip| ip == host)
}

/// Whether proposing the hosted network can help this join: not a client reaching the host from
/// another network (a relayed or remote join is never on the host's adapter), and not one
/// already on it.
pub(crate) fn hosted_network_helps(
    locality: SessionLocality,
    host_ip: Option<&str>,
    hosted_ips: &[String],
) -> bool {
    locality != SessionLocality::CrossNetwork && !on_hosted_network(host_ip, hosted_ips)
}

fn now_ms() -> u64 {
    crate::streamer::audio::host_now_ns() / 1_000_000
}

/// Watch one attempt's link governor for as long as the attempt owns its join. Spawned only when
/// the link governor can run at all; a renegotiation starts a new watch on the new telemetry.
pub(crate) async fn watch_slow_link(
    sessions: SharedSessions,
    seq: u64,
    telemetry: SharedTelemetry,
    reporter: SharedDeviceReporter,
    device: DeviceInfo,
    host_ip: Option<String>,
    locality: SessionLocality,
) {
    let mut window = EngagedWindow::default();
    loop {
        tokio::time::sleep(SLOW_LINK_TICK).await;
        if !session::is_current_session(&sessions, &device.join_id, seq) {
            return;
        }
        let now = now_ms();
        let engaged = window.observe(now, telemetry.link.engaged_ms());
        if !notices().lock_recover().claim(&device.token, engaged, now) {
            continue;
        }
        // three WMI queries, once per cool-down at most and none for a remote client: on the
        // blocking pool
        let hosted_ips = if locality == SessionLocality::CrossNetwork {
            Vec::new()
        } else {
            tokio::task::spawn_blocking(crate::windows_utils::networking::hosted_network_ipv4)
                .await
                .unwrap_or_default()
        };
        if !hosted_network_helps(locality, host_ip.as_deref(), &hosted_ips) {
            tprintln!(
                "slow connection to {} (join={}): governor engaged {}s of the last {}s, {}, \
                 no notice",
                device.name,
                device.join_id,
                engaged / 1000,
                SLOW_LINK_WINDOW_MS / 1000,
                if locality == SessionLocality::CrossNetwork {
                    "a client on another network"
                } else {
                    "already on the hosted network"
                }
            );
            continue;
        }
        reporter.report_slow_connection(
            device.clone(),
            (engaged / 1000) as u32,
            (SLOW_LINK_WINDOW_MS / 1000) as u32,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1000;

    /// Feeds one reading a second from `from` to `to` (seconds), with the governor engaged
    /// while `engaged(t)` holds; returns the largest windowed total seen and the running total.
    fn run(
        w: &mut EngagedWindow,
        total: &mut u64,
        from: u64,
        to: u64,
        engaged: impl Fn(u64) -> bool,
    ) -> u64 {
        let mut most = 0;
        for t in from..to {
            if engaged(t) {
                *total += S;
            }
            most = most.max(w.observe(t * S, *total));
        }
        most
    }

    #[test]
    fn a_session_that_never_engaged_never_gets_a_notice() {
        let (mut w, mut total) = (EngagedWindow::default(), 0);
        let most = run(&mut w, &mut total, 0, 600, |_| false);
        assert_eq!(most, 0);
        assert!(!slow_link_due(most, None, 600 * S));
    }

    #[test]
    fn one_engagement_of_a_few_seconds_is_not_consistent() {
        let (mut w, mut total) = (EngagedWindow::default(), 0);
        let most = run(&mut w, &mut total, 0, 300, |t| (50..56).contains(&t));
        assert_eq!(most, 6 * S);
        assert!(!slow_link_due(most, None, 300 * S));
    }

    #[test]
    fn sustained_engagement_is_consistent() {
        let (mut w, mut total) = (EngagedWindow::default(), 0);
        let before = run(&mut w, &mut total, 0, 39, |t| t >= 10);
        assert!(!slow_link_due(before, None, 39 * S));
        let after = run(&mut w, &mut total, 39, 41, |t| t >= 10);
        assert!(slow_link_due(after, None, 41 * S));
    }

    #[test]
    fn repeated_short_engagements_within_the_window_add_up() {
        // 8 s engaged every 25 s
        let (mut w, mut total) = (EngagedWindow::default(), 0);
        let most = run(&mut w, &mut total, 0, 120, |t| t % 25 < 8);
        assert!(slow_link_due(most, None, 120 * S), "{most}");
    }

    #[test]
    fn engagements_spread_wider_than_the_window_never_add_up() {
        // 8 s engaged every 60 s: at most three of them fit in the window
        let (mut w, mut total) = (EngagedWindow::default(), 0);
        let most = run(&mut w, &mut total, 0, 900, |t| t % 60 < 8);
        assert!(most <= 24 * S, "{most}");
        assert!(!slow_link_due(most, None, 900 * S));
    }

    #[test]
    fn a_notice_fires_once_then_waits_out_the_cooldown_for_that_device() {
        let mut book = NoticeBook::default();
        let t0 = 1_000 * S;
        assert!(book.claim("phone", 31 * S, t0));
        assert!(!book.claim("phone", 60 * S, t0 + S));
        assert!(!book.claim("phone", 60 * S, t0 + SLOW_LINK_COOLDOWN_MS - S));
        // another device has its own cool-down
        assert!(book.claim("tablet", 31 * S, t0 + S));
        assert!(book.claim("phone", 31 * S, t0 + SLOW_LINK_COOLDOWN_MS));
    }

    #[test]
    fn a_device_below_the_bar_does_not_take_its_notice() {
        let mut book = NoticeBook::default();
        assert!(!book.claim("phone", 29 * S, 10 * S));
        assert!(book.claim("phone", 30 * S, 11 * S));
    }

    #[test]
    fn a_new_session_starts_with_an_empty_window() {
        let (mut w, mut total) = (EngagedWindow::default(), 0);
        let most = run(&mut w, &mut total, 0, 60, |_| true);
        assert!(slow_link_due(most, None, 60 * S));
        // the session ended; its successor's governor starts from zero and so does its window
        let mut next = EngagedWindow::default();
        assert_eq!(next.observe(61 * S, 0), 0);
        assert_eq!(next.observe(62 * S, S), S);
    }

    #[test]
    fn the_first_reading_is_a_baseline_and_a_restarted_governor_counts_from_zero() {
        let mut w = EngagedWindow::default();
        assert_eq!(w.observe(0, 50 * S), 0);
        assert_eq!(w.observe(S, 51 * S), S);
        // rebuilt: its total went back to 2 s
        assert_eq!(w.observe(2 * S, 2 * S), 3 * S);
    }

    #[test]
    fn a_join_over_the_hosted_networks_adapter_is_already_on_it() {
        let hosted = vec!["192.168.137.1".to_string()];
        assert!(on_hosted_network(Some("192.168.137.1"), &hosted));
        assert!(!on_hosted_network(Some("192.168.1.20"), &hosted));
        assert!(!on_hosted_network(None, &hosted));
        assert!(!on_hosted_network(Some("192.168.137.1"), &[]));
    }

    #[test]
    fn the_hosted_network_is_never_proposed_to_a_client_on_another_network() {
        let hosted = vec!["192.168.137.1".to_string()];
        let lan = Some("192.168.1.20");
        assert!(hosted_network_helps(
            SessionLocality::SameNetwork,
            lan,
            &hosted
        ));
        assert!(hosted_network_helps(SessionLocality::Unknown, lan, &hosted));
        assert!(!hosted_network_helps(
            SessionLocality::CrossNetwork,
            lan,
            &hosted
        ));
        assert!(!hosted_network_helps(
            SessionLocality::CrossNetwork,
            None,
            &[]
        ));
        assert!(!hosted_network_helps(
            SessionLocality::SameNetwork,
            Some("192.168.137.1"),
            &hosted
        ));
    }
}
