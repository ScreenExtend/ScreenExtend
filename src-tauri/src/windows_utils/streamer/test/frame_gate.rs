//! `FrameGate` against a frozen reference, on the same sequences, outcome for outcome. The
//! reference is the original inline gate of `LiveCapture::deliver` with the frame and the ring
//! replaced by recorded answers; it must not be edited to follow the gate.

use std::time::{Duration, Instant};

use crate::streamer::telemetry::SessionTelemetry;
use crate::windows_utils::streamer::pipeline::{Admit, FrameGate};

const CADENCE_NUM: u32 = 3;
const CADENCE_DEN: u32 = 4;
const MAX_CONSECUTIVE_UNCHANGED: u32 = 5;

/// One delivered frame and what its surroundings answered.
#[derive(Clone, Copy)]
struct Step {
    capture: Instant,
    cursor_moved: bool,
    timer_delivered: Option<Instant>,
    has_dirty: Option<bool>,
    staged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Stage,
    SkipCadence,
    Hold,
    SkipUnchanged,
}

struct Reference {
    period: Duration,
    auto_frame_rate: bool,
    dirty_region_reporting: bool,
    cadence_counts_staged_only: bool,
    last_delivered: Option<Instant>,
    unchanged_streak: u32,
    skipped_cadence: u64,
    skipped_unchanged: u64,
}

impl Reference {
    fn deliver(&mut self, s: Step, trailing: bool, ring: bool) -> Outcome {
        let last_at = match (self.last_delivered, s.timer_delivered) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        if let Some(last) = last_at {
            if !s.cursor_moved
                && s.capture.duration_since(last) < self.period * CADENCE_NUM / CADENCE_DEN
            {
                if trailing {
                    return Outcome::Hold;
                }
                self.skipped_cadence += 1;
                return Outcome::SkipCadence;
            }
        }
        if self.auto_frame_rate && self.dirty_region_reporting && !s.cursor_moved {
            if let Some(dirty) = s.has_dirty {
                if !dirty && self.unchanged_streak < MAX_CONSECUTIVE_UNCHANGED {
                    self.unchanged_streak += 1;
                    self.skipped_unchanged += 1;
                    return Outcome::SkipUnchanged;
                }
                self.unchanged_streak = 0;
            }
        }
        if !self.cadence_counts_staged_only || !ring {
            self.last_delivered = Some(s.capture);
        }
        if ring && s.staged && self.cadence_counts_staged_only {
            self.last_delivered = Some(s.capture);
        }
        Outcome::Stage
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn chance(&mut self, per_1000: u64) -> bool {
        self.next() % 1000 < per_1000
    }
}

fn sequence(seed: u64, n: usize) -> Vec<Step> {
    let mut r = Rng(seed | 1);
    let t0 = Instant::now();
    let mut t = t0;
    let mut timer: Option<Instant> = None;
    (0..n)
        .map(|_| {
            // 1-40 ms apart: a 120-360 Hz compose with bursts and pauses
            t += Duration::from_micros(1_000 + r.next() % 39_000);
            if r.chance(80) {
                timer = Some(t - Duration::from_micros(r.next() % 9_000));
            }
            Step {
                capture: t,
                cursor_moved: r.chance(120),
                timer_delivered: timer,
                has_dirty: match r.next() % 4 {
                    0 => None,
                    1 => Some(false),
                    _ => Some(r.chance(500)),
                },
                staged: r.chance(850),
            }
        })
        .collect()
}

#[test]
fn the_gate_decides_exactly_as_the_inline_code_did() {
    let mut compared = 0usize;
    for (seed, hz) in [(1u64, 60u64), (7, 120), (42, 30), (99, 144), (2026, 60)] {
        let steps = sequence(seed, 3_000);
        let period = Duration::from_nanos(1_000_000_000 / hz);
        for config in 0..64u32 {
            let auto = config & 1 != 0;
            let reporting = config & 2 != 0;
            let staged_only = config & 4 != 0;
            let trailing = config & 8 != 0;
            let ring = config & 16 != 0;
            let timer_on = config & 32 != 0;
            let mut old = Reference {
                period,
                auto_frame_rate: auto,
                dirty_region_reporting: reporting,
                cadence_counts_staged_only: staged_only,
                last_delivered: None,
                unchanged_streak: 0,
                skipped_cadence: 0,
                skipped_unchanged: 0,
            };
            let mut gate = FrameGate::new(period, auto, reporting, staged_only);
            let tel = SessionTelemetry::new_shared();
            for (i, s) in steps.iter().enumerate() {
                let mut s = *s;
                if !timer_on {
                    s.timer_delivered = None;
                }
                let want = old.deliver(s, trailing, ring);
                let mut asked = false;
                let got = match gate.admit(
                    s.capture,
                    s.cursor_moved,
                    s.timer_delivered,
                    trailing,
                    ring,
                    || {
                        asked = true;
                        s.has_dirty
                    },
                    &tel,
                ) {
                    Admit::Stage => {
                        if ring {
                            gate.staged(s.capture, s.staged);
                        }
                        Outcome::Stage
                    }
                    Admit::SkipCadence => Outcome::SkipCadence,
                    Admit::Hold { .. } => Outcome::Hold,
                    Admit::SkipUnchanged => Outcome::SkipUnchanged,
                };
                assert_eq!(
                    got, want,
                    "seed {seed} {hz} Hz config {config:#b} frame {i}"
                );
                assert_eq!(gate.last_delivered, old.last_delivered, "frame {i}");
                assert_eq!(gate.unchanged_streak, old.unchanged_streak, "frame {i}");
                // the dirty region is queried only where the inline code queried it
                assert_eq!(
                    asked,
                    auto && reporting
                        && !s.cursor_moved
                        && got != Outcome::Hold
                        && got != Outcome::SkipCadence,
                    "frame {i}"
                );
                compared += 1;
            }
            let d = &tel.degradation;
            assert_eq!(
                d.frames_skipped_cadence
                    .load(std::sync::atomic::Ordering::Relaxed),
                old.skipped_cadence
            );
            assert_eq!(
                d.frames_skipped_unchanged
                    .load(std::sync::atomic::Ordering::Relaxed),
                old.skipped_unchanged
            );
        }
    }
    assert_eq!(compared, 5 * 64 * 3_000);
}
