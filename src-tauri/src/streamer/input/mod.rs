use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::streamer::audio::host_now_ns;
use crate::streamer::telemetry::SharedTelemetry;

pub mod protocol;

use self::protocol::InputEvent;

/// What the injector needs to know about the client driving it.
///
/// The display name is which virtual display its coordinates address; the platform decides the
/// modifier layout on macOS, and is ignored everywhere else.
#[derive(Clone, Debug, Default)]
pub struct ClientProfile {
    pub device_name: Option<String>,
    pub platform: ClientPlatform,
}

/// The client's own operating system, as it reported at join.
///
/// Only the Apple/not-Apple distinction is load-bearing today, but the variants are kept whole
/// because guessing from a boolean is how the original bug happened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientPlatform {
    Windows,
    MacOs,
    IOs,
    Android,
    Linux,
    #[default]
    Unknown,
}

impl ClientPlatform {
    /// Parses the `os` string the join request carries (`detectOs()` in the client page).
    pub fn from_reported(os: &str) -> Self {
        match os.trim().to_ascii_lowercase().as_str() {
            "windows" => Self::Windows,
            "macos" => Self::MacOs,
            "ios" => Self::IOs,
            "android" => Self::Android,
            "linux" => Self::Linux,
            _ => Self::Unknown,
        }
    }

    /// Whether the keyboard in front of the user is an Apple one, where Command is the command
    /// modifier and Control is the Emacs-style caret modifier.
    pub fn is_apple(self) -> bool {
        matches!(self, Self::MacOs | Self::IOs)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayRect {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
}

#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod backend;
#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod backend;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod backend;
#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
#[path = "generic.rs"]
mod backend;

pub mod scancode;

/// Which injection path this host can actually take, probed rather than assumed.
///
/// Input was the one subsystem with no tier: audio probes and reports a backend, display probes
/// and reports one, capture probes and reports one, and remote control reported nothing at all.
/// The case that made it matter is macOS without Accessibility, where `CGEventPost` returns
/// `void` and the OS drops every event with no error the process can observe — a client whose
/// remote control is enabled, whose pointer events are being sent and acknowledged, and whose
/// host simply never moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    /// Windows: `SendInput` plus synthetic pen/touch pointer devices.
    SendInput,
    /// macOS: `CGEventPost`, with Accessibility granted.
    CgEvent,
    /// macOS: the API is present but TCC has not granted Accessibility, so injection is a
    /// silent no-op. Recoverable at runtime — the permission can be granted while we run.
    NeedsAccessibility,
    /// No injection backend on this platform.
    Unsupported,
}

impl InputBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SendInput => "SendInput",
            Self::CgEvent => "CGEvent",
            Self::NeedsAccessibility => "NeedsAccessibility",
            Self::Unsupported => "Unsupported",
        }
    }

    /// Whether injected events actually reach the desktop on this tier.
    pub fn injects(self) -> bool {
        matches!(self, Self::SendInput | Self::CgEvent)
    }
}

/// Probe the injection tier. Never cached: on macOS the answer flips the moment the user grants
/// Accessibility, and the old code demanded a relaunch precisely because it asked once.
pub fn probe_backend() -> InputBackend {
    backend::probe()
}

pub fn boost_current_thread() {
    backend::boost_thread();
}

pub fn prime() {
    #[cfg(target_os = "macos")]
    backend::prime_keyboard();
}

/// Depth of the lossy queue, in events.
///
/// This is last-value-wins data: only where the pointer is *now* is observable, so a deep queue
/// buys nothing and costs everything. At 512 a backlog was seconds of history that then got
/// replayed in order, which is strictly worse for a cursor than dropping it — the user watches
/// the pointer crawl through the past instead of jumping to their finger. 64 is a burst
/// absorber, not a buffer; anything that does not clear inside [`MAX_FAST_AGE_NS`] is stale by
/// the time it would be injected anyway.
pub const FAST_CAP: usize = 64;

/// How old a queued positional event may be before injecting it is worse than skipping it.
///
/// Three 60 Hz frames. A pointer position that old has already been superseded on the client;
/// putting it on the screen produces the backwards crawl the queue depth used to cause.
pub const MAX_FAST_AGE_NS: u64 = 50_000_000;

/// Window over which the host keeps its minimum observed client-to-host delay.
///
/// The same 30 s the client uses to age out its own best RTT sample, for the same reason: a
/// route change should not be remembered for the life of the session.
const OFFSET_WINDOW_NS: u64 = 30_000_000_000;

/// The host's own estimate of how far a stamped input frame ran behind the best case.
///
/// The client already estimates the clock offset NTP-style from the `PING`/`PONG` pair, but it
/// never sends the result back, and the host needs a number to say how long an event spent in
/// flight. What the host can see unaided is `host_recv - client_send` for every stamped frame:
/// the true offset plus that frame's one-way delay. Its *minimum* over a window is therefore the
/// tightest offset estimate obtainable from one direction alone, and subtracting it leaves the
/// excess one-way delay — 0 on the best sample, growing with queueing. That relative reading is
/// exactly what a latency budget wants, and it needs nothing from the client but a stamp.
///
/// Relaxed atomics throughout: these are statistics on the SCTP receive path, not
/// synchronization, and a reader one sample stale reports a percentile one sample stale.
#[derive(Debug)]
struct ClockOffset {
    current: AtomicU64,
    previous: AtomicU64,
    window_start_ns: AtomicU64,
}

impl Default for ClockOffset {
    fn default() -> Self {
        Self {
            current: AtomicU64::new(u64::MAX),
            previous: AtomicU64::new(u64::MAX),
            window_start_ns: AtomicU64::new(0),
        }
    }
}

impl ClockOffset {
    /// Fold one observation in and return this frame's excess delay, or `None` when the frame
    /// carried no client stamp.
    fn observe(&self, client_ns: u64, host_recv_ns: u64) -> Option<u64> {
        if client_ns == 0 {
            return None;
        }
        let raw = host_recv_ns.saturating_sub(client_ns);
        let start = self.window_start_ns.load(Ordering::Relaxed);
        if start == 0 {
            self.window_start_ns.store(host_recv_ns, Ordering::Relaxed);
        } else if host_recv_ns.saturating_sub(start) > OFFSET_WINDOW_NS {
            self.previous
                .store(self.current.load(Ordering::Relaxed), Ordering::Relaxed);
            self.current.store(u64::MAX, Ordering::Relaxed);
            self.window_start_ns.store(host_recv_ns, Ordering::Relaxed);
        }
        self.current.fetch_min(raw, Ordering::Relaxed);
        let base = self
            .current
            .load(Ordering::Relaxed)
            .min(self.previous.load(Ordering::Relaxed));
        Some(raw.saturating_sub(base))
    }
}

/// An event with the two stamps the telemetry needs, moved through the channel as one unit.
pub struct Queued {
    pub ev: InputEvent,
    /// `host_now_ns()` at the moment the transport handed the event over.
    pub host_recv_ns: u64,
    /// The frame's fast-channel sequence number, when it carried one.
    pub seq: Option<u16>,
}

/// Whether an event is positional last-value-wins data that a fresher one supersedes.
///
/// Everything else — keys, buttons, clipboard, lifecycle — is a state transition whose loss the
/// host cannot recover from, so it is never dropped for age however far behind the injector is.
///
/// The decision is source-aware, and that distinction is the whole point. Only where a *cursor*
/// is now is observable, so a 50 ms-old mouse position is worse than nothing. A pen or touch
/// sample is not redundant with its neighbours — it is the shape of the line — so dropping one
/// leaves a permanent straight segment where the user drew a curve and no later event repairs
/// it. Late ink is still the right ink; a late cursor is the wrong place.
fn is_supersedable(ev: &InputEvent) -> bool {
    match ev {
        InputEvent::Pointer {
            phase: protocol::Phase::Move | protocol::Phase::Enter | protocol::Phase::Over,
            source,
            ..
        } => *source == protocol::SRC_MOUSE,
        InputEvent::PointerBatch { source, .. } => *source == protocol::SRC_MOUSE,
        InputEvent::MouseDelta { .. } => true,
        _ => false,
    }
}

/// Whether a frame that arrived behind one already injected may be *dropped*.
///
/// Narrower than [`is_supersedable`] by exactly one case, and the difference is the arithmetic.
/// An absolute pointer position is last-value-wins, so replaying an older one walks the cursor
/// backwards; a `MOUSE_DELTA` carries a relative *increment* that both backends accumulate into a
/// position they track themselves, and increments commute — applying a reordered one was always
/// exactly correct, and dropping it loses that motion permanently, under pointer lock, where the
/// user has the least ability to compensate.
///
/// What a late delta must not do is restate a stale button word: `button_transitions` compares it
/// against the injector's current state and would synthesise a spurious press or release. The
/// caller rewrites that word before dispatch, which is why this can return `false` safely.
fn is_reorder_droppable(ev: &InputEvent) -> bool {
    !matches!(ev, InputEvent::MouseDelta { .. }) && is_supersedable(ev)
}

#[derive(Clone)]
pub struct InjectorTx {
    fast: Sender<Queued>,
    /// The consumer's own end, kept here only so a full queue can be relieved from the head.
    /// crossbeam has no ring-drop, and the channel is MPMC, so popping the oldest from the
    /// producer side is exactly the "newest wins" policy this data wants.
    fast_relief: Receiver<Queued>,
    reliable: Sender<Queued>,
    telemetry: SharedTelemetry,
    offset: Arc<ClockOffset>,
}

impl InjectorTx {
    #[inline]
    pub fn route(&self, ev: InputEvent, hot: bool, client_ns: u64, seq: Option<u16>) {
        let host_recv_ns = host_now_ns();
        if let Some(excess_ns) = self.offset.observe(client_ns, host_recv_ns) {
            self.telemetry.stages.input_recv.record(excess_ns);
        }
        let q = Queued {
            ev,
            host_recv_ns,
            seq: if hot { seq } else { None },
        };
        if hot {
            // Newest wins. Dropping the head rather than the tail is the whole point: when the
            // queue is full the events in it are the stale ones and the arriving event is where
            // the pointer actually is.
            if let Err(TrySendError::Full(q)) = self.fast.try_send(q) {
                let _ = self.fast_relief.try_recv();
                if self.fast.try_send(q).is_err() {
                    self.count(&self.telemetry.degradation.input_queue_drops);
                    return;
                }
                self.count(&self.telemetry.degradation.input_queue_drops);
            }
        } else {
            let _ = self.reliable.send(q);
        }
    }

    #[inline]
    fn count(&self, c: &AtomicU64) {
        c.fetch_add(1, Ordering::Relaxed);
    }

    pub fn release_all(&self) {
        use self::protocol::Lifecycle;
        let _ = self.reliable.send(Queued {
            ev: InputEvent::Lifecycle(Lifecycle::Focus(false)),
            host_recv_ns: host_now_ns(),
            seq: None,
        });
    }
}

pub fn spawn(client: ClientProfile, telemetry: SharedTelemetry) -> (InjectorTx, JoinHandle<()>) {
    let (fast_tx, fast_rx) = bounded::<Queued>(FAST_CAP);
    let fast_relief = fast_rx.clone();
    let (rel_tx, rel_rx) = unbounded::<Queued>();

    let thread_telemetry = Arc::clone(&telemetry);
    let join = thread::Builder::new()
        .name("injector".into())
        .spawn(move || {
            backend::boost_thread();
            let mut inj = backend::Injector::new(client);
            tprintln!("injector thread up ({})", backend::NAME);
            let t = thread_telemetry;
            // Highest fast-channel sequence number already injected. The fast channel is
            // unordered with no retransmits, so two moves can arrive swapped; replaying the
            // older one walks the cursor backwards and then forwards again for a frame.
            let mut highest_seq: Option<u16> = None;
            // The button word of the newest `MOUSE_DELTA` actually injected. A late delta's own
            // word is stale by construction, so it is replaced with this before dispatch and the
            // backend's `button_transitions` compare becomes a no-op — the motion is kept, the
            // spurious press or release is not.
            let mut last_delta_buttons: u16 = 0;
            loop {
                let q = select! {
                    recv(rel_rx) -> m => match m {
                        Ok(q) => q,
                        Err(_) => break,
                    },
                    recv(fast_rx) -> m => match m {
                        Ok(q) => q,
                        Err(_) => break,
                    },
                };
                let picked_up_ns = host_now_ns();
                let age_ns = picked_up_ns.saturating_sub(q.host_recv_ns);
                t.stages.input_queue.record(age_ns);
                if age_ns > MAX_FAST_AGE_NS && is_supersedable(&q.ev) {
                    t.degradation
                        .input_stale_drops
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                // Only droppable frames are gated: a reordered button or key is a state
                // transition the host cannot reconstruct, and a reordered relative delta is
                // motion it cannot recover, so both are injected whatever their sequence says.
                let mut ev = q.ev;
                if let Some(seq) = q.seq {
                    let behind =
                        matches!(highest_seq, Some(last) if !protocol::seq_is_newer(seq, last));
                    if behind && is_reorder_droppable(&ev) {
                        t.degradation
                            .input_reorder_drops
                            .fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    if behind {
                        // Kept, but its restatement of the button word is older than what has
                        // already been injected and would be read as a transition.
                        if let InputEvent::MouseDelta { buttons, .. } = &mut ev {
                            *buttons = last_delta_buttons;
                        }
                    } else {
                        // Only a *newer* sequence advances the mark. Letting a late frame set it
                        // walked the high-water backwards and briefly disarmed the gate.
                        highest_seq = Some(seq);
                    }
                }
                if let InputEvent::MouseDelta { buttons, .. } = &ev {
                    last_delta_buttons = *buttons;
                }
                inj.dispatch(&ev);
                t.stages
                    .input_inject
                    .record(host_now_ns().saturating_sub(q.host_recv_ns));
            }
            tprintln!("injector shutting down: releasing all held input");
            inj.release_all();
        })
        .expect("spawn injector thread");

    (
        InjectorTx {
            fast: fast_tx,
            fast_relief,
            reliable: rel_tx,
            telemetry,
            offset: Arc::new(ClockOffset::default()),
        },
        join,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streamer::input::protocol::Phase;

    fn pointer(phase: Phase) -> InputEvent {
        pointer_from(protocol::SRC_MOUSE, phase)
    }

    fn pointer_from(source: u8, phase: Phase) -> InputEvent {
        InputEvent::Pointer {
            source,
            id: 0,
            x: 0.0,
            y: 0.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            twist: 0.0,
            w: 0.0,
            h: 0.0,
            buttons: 0,
            phase,
        }
    }

    #[test]
    fn only_positional_events_may_be_dropped_for_age() {
        assert!(is_supersedable(&pointer(Phase::Move)));
        assert!(is_supersedable(&InputEvent::MouseDelta {
            dx: 1,
            dy: 1,
            buttons: 0,
            unadjusted: false,
        }));
        assert!(!is_supersedable(&pointer(Phase::Down)));
        assert!(!is_supersedable(&pointer(Phase::Up)));
        assert!(!is_supersedable(&InputEvent::Key {
            down: true,
            repeat: false,
            mods: 0,
            code: "KeyA".into(),
            key: "a".into(),
        }));
    }

    fn batch(source: u8) -> InputEvent {
        InputEvent::PointerBatch {
            source,
            id: 1,
            buttons: 0,
            samples: vec![protocol::MoveSample {
                x: 0.1,
                y: 0.1,
                pressure: 0.5,
                tilt_x: 0.0,
                tilt_y: 0.0,
                twist: 0.0,
            }],
            predicted: 0,
        }
    }

    /// A stroke is a shape, so a late pen or touch sample is still the right ink; a late cursor
    /// position is the wrong place. The age gate has to tell the two apart.
    #[test]
    fn ink_is_never_dropped_for_age_but_a_cursor_is() {
        assert!(is_supersedable(&batch(protocol::SRC_MOUSE)));
        assert!(!is_supersedable(&batch(protocol::SRC_PEN)));
        assert!(!is_supersedable(&batch(protocol::SRC_TOUCH)));
        assert!(is_supersedable(&pointer_from(
            protocol::SRC_MOUSE,
            Phase::Move
        )));
        assert!(!is_supersedable(&pointer_from(
            protocol::SRC_PEN,
            Phase::Move
        )));
        assert!(!is_supersedable(&pointer_from(
            protocol::SRC_TOUCH,
            Phase::Move
        )));
    }

    /// The age gate and the reorder gate ask different questions, and a relative delta answers
    /// them differently: it is redundant with its successors (so it may be dropped for *age*)
    /// but its position term is an increment, and increments commute (so it may never be dropped
    /// for *order* — that motion has no other source).
    #[test]
    fn a_late_relative_delta_is_kept_where_a_late_absolute_position_is_dropped() {
        let delta = InputEvent::MouseDelta {
            dx: 3,
            dy: -2,
            buttons: 0,
            unadjusted: true,
        };
        assert!(is_supersedable(&delta), "still droppable for age");
        assert!(!is_reorder_droppable(&delta), "never droppable for order");

        // Everything else the reorder gate covers is unchanged: an absolute cursor position is
        // last-value-wins, and ink and state transitions were never gated at all.
        assert!(is_reorder_droppable(&pointer_from(
            protocol::SRC_MOUSE,
            Phase::Move
        )));
        assert!(is_reorder_droppable(&batch(protocol::SRC_MOUSE)));
        assert!(!is_reorder_droppable(&batch(protocol::SRC_PEN)));
        assert!(!is_reorder_droppable(&pointer_from(
            protocol::SRC_MOUSE,
            Phase::Down
        )));
    }

    /// The high-water mark may only ever move forward. Advancing it from a frame that was itself
    /// behind — which the ungated `else` arm used to do for every non-droppable frame — walks it
    /// backwards and silently disarms the gate for everything in between.
    #[test]
    fn only_a_newer_sequence_advances_the_high_water_mark() {
        fn apply(highest: &mut Option<u16>, seq: u16) -> bool {
            let behind = matches!(*highest, Some(last) if !protocol::seq_is_newer(seq, last));
            if !behind {
                *highest = Some(seq);
            }
            behind
        }
        let mut highest: Option<u16> = None;
        assert!(!apply(&mut highest, 10));
        assert!(!apply(&mut highest, 11));
        assert!(apply(&mut highest, 9), "9 is behind 11");
        assert_eq!(highest, Some(11), "a late frame must not move the mark");
        assert!(
            apply(&mut highest, 11),
            "the mark itself is not newer than the mark"
        );
        assert!(!apply(&mut highest, 12));
        assert_eq!(highest, Some(12));
    }

    #[test]
    fn sequence_comparison_survives_the_u16_wrap() {
        assert!(protocol::seq_is_newer(2, 1));
        assert!(!protocol::seq_is_newer(1, 2));
        assert!(!protocol::seq_is_newer(7, 7));
        // 0 follows 65535.
        assert!(protocol::seq_is_newer(0, u16::MAX));
        assert!(!protocol::seq_is_newer(u16::MAX, 0));
        // Half a space apart is the documented limit; beyond it the sign flips, which is why
        // the gate only ever compares consecutive fast-channel frames.
        assert!(protocol::seq_is_newer(3, u16::MAX - 3));
    }

    #[test]
    fn the_offset_estimator_reports_excess_delay_over_its_best_sample() {
        let c = ClockOffset::default();
        // An unstamped frame contributes nothing and is not measured.
        assert_eq!(c.observe(0, 1_000), None);
        // First sample is by definition the best one seen.
        assert_eq!(c.observe(1_000, 6_000), Some(0));
        // 2 ms further behind than the best sample.
        assert_eq!(c.observe(2_000, 9_000), Some(2_000));
        // A better sample re-bases the estimate.
        assert_eq!(c.observe(3_000, 7_000), Some(0));
        assert_eq!(c.observe(4_000, 9_000), Some(1_000));
    }

    #[test]
    fn a_stale_window_does_not_pin_the_estimate_forever() {
        let c = ClockOffset::default();
        assert_eq!(c.observe(1_000, 6_000), Some(0));
        // Two windows later the 5 000 ns baseline has aged out of both slots, so a link that
        // settled at 8 000 ns reads as its own best case rather than as permanent lateness.
        let far = OFFSET_WINDOW_NS * 2 + 10_000;
        c.observe(far, far + 8_000);
        c.observe(far * 2, far * 2 + 8_000);
        assert_eq!(c.observe(far * 3, far * 3 + 8_000), Some(0));
    }
}
