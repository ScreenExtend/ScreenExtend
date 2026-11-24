//! Time series for long runs: a background sampler (RSS, thermal state, low-power mode every few
//! seconds) and fixed-width buckets over a run's frames, so a 20-minute soak shows drift instead
//! of one whole-run percentile that hides it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use super::cf;
use super::metrics::summary;

/// How often the sampler reads the process and the machine.
pub const SAMPLE_PERIOD: Duration = Duration::from_secs(5);

/// Reads RSS / thermal state / low-power mode every [`SAMPLE_PERIOD`] on its own thread. The
/// first sample is taken at start and the last at `finish`, so even a short run has two points.
pub struct Sampler {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Vec<Json>>>,
    t0: Instant,
}

fn sample(t0: Instant) -> Json {
    let (rss, peak) = cf::rss();
    json!({
        "t_s": (t0.elapsed().as_secs_f64() * 10.0).round() / 10.0,
        "rss_mb": (rss as f64 / 1e5).round() / 10.0,
        "rss_peak_mb": (peak as f64 / 1e5).round() / 10.0,
        "thermal": cf::thermal_state(),
        "low_power": cf::low_power_mode(),
    })
}

impl Sampler {
    pub fn start() -> Sampler {
        Self::start_every(SAMPLE_PERIOD)
    }

    pub fn start_every(period: Duration) -> Sampler {
        let t0 = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("bench-sampler".into())
            .spawn(move || {
                let mut out = vec![sample(t0)];
                let mut next = t0 + period;
                while !flag.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if now >= next {
                        out.push(sample(t0));
                        next += period;
                    } else {
                        std::thread::sleep((next - now).min(Duration::from_millis(100)));
                    }
                }
                out
            })
            .ok();
        Sampler { stop, handle, t0 }
    }

    /// Stop, take a final sample, and return `[{t_s, rss_mb, rss_peak_mb, thermal, low_power}]`.
    pub fn finish(mut self) -> Json {
        self.stop.store(true, Ordering::Relaxed);
        let mut v = self
            .handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        v.push(sample(self.t0));
        Json::Array(v)
    }
}

/// One frame's contribution to a bucket.
#[derive(Clone, Copy, Debug, Default)]
pub struct Row {
    /// When the frame was due (or offered), ns from the run's start.
    pub t_ns: u64,
    /// Latency of the frame's picture, when it produced one.
    pub lat_ms: Option<f64>,
    /// Encoded bytes (0 for no picture).
    pub bytes: u64,
    /// Accepted and returned with no picture (rate control).
    pub dropped: bool,
    /// Failed (non-zero status, or never came back).
    pub errored: bool,
}

/// Fixed-width buckets over `rows`: `[{from_s, to_s, frames, emitted, dropped, errored, mbps,
/// latency_ms}]`. A partial last bucket's rate is over the span it actually covers.
pub fn buckets(width_s: u64, timeline_ns: u64, rows: &[Row]) -> Json {
    let width_ns = width_s.max(1) * 1_000_000_000;
    let n = (timeline_ns.max(1)).div_ceil(width_ns) as usize;
    let mut lat: Vec<Vec<f64>> = vec![Vec::new(); n.max(1)];
    let mut acc: Vec<(usize, usize, usize, usize, u64)> = vec![(0, 0, 0, 0, 0); n.max(1)];
    for r in rows {
        let b = ((r.t_ns / width_ns) as usize).min(acc.len() - 1);
        let a = &mut acc[b];
        a.0 += 1;
        if r.errored {
            a.3 += 1;
        } else if r.dropped {
            a.2 += 1;
        } else if r.bytes > 0 {
            a.1 += 1;
        }
        a.4 += r.bytes;
        if let Some(l) = r.lat_ms {
            lat[b].push(l);
        }
    }
    Json::Array(
        acc.iter()
            .enumerate()
            .map(|(i, &(frames, emitted, dropped, errored, bytes))| {
                let from = i as u64 * width_ns;
                let to = (from + width_ns).min(timeline_ns.max(from + 1));
                let span_s = (to - from) as f64 / 1e9;
                json!({
                    "from_s": from / 1_000_000_000,
                    "to_s": (to as f64 / 1e8).round() / 10.0,
                    "frames": frames,
                    "emitted": emitted,
                    "dropped": dropped,
                    "errored": errored,
                    "mbps": ((bytes as f64 * 8.0 / span_s.max(1e-9) / 1e6) * 1000.0).round() / 1000.0,
                    "latency_ms": summary(&lat[i]),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_land_in_their_bucket_and_a_partial_bucket_is_rated_over_its_span() {
        let rows = [
            Row {
                t_ns: 0,
                lat_ms: Some(5.0),
                bytes: 1000,
                ..Default::default()
            },
            Row {
                t_ns: 9_999_999_999,
                lat_ms: Some(7.0),
                bytes: 1000,
                ..Default::default()
            },
            Row {
                t_ns: 10_000_000_000,
                dropped: true,
                ..Default::default()
            },
            Row {
                t_ns: 12_000_000_000,
                bytes: 500,
                lat_ms: Some(1.0),
                ..Default::default()
            },
        ];
        let b = buckets(10, 15_000_000_000, &rows);
        let b = b.as_array().unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0]["emitted"], json!(2));
        assert_eq!(b[0]["latency_ms"]["n"], json!(2));
        assert_eq!(b[1]["dropped"], json!(1));
        assert_eq!(b[1]["emitted"], json!(1));
        // 500 bytes over the 5 s the last bucket covers.
        assert_eq!(b[1]["mbps"], json!(0.001));
        assert_eq!(b[1]["to_s"], json!(15.0));
    }
}
