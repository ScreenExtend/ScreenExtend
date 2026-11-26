use crossbeam_channel::{bounded, select, unbounded, Receiver, Sender, TrySendError};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::streamer::audio::host_now_ns;
use crate::streamer::lock::LockExt;
use crate::streamer::telemetry::SharedTelemetry;

pub mod protocol;

use self::protocol::InputEvent;

/// What the injector needs to know about the client driving it: which virtual display its
/// coordinates address, the platform (it decides the modifier layout on macOS only), and
/// the two settings the user can change while the session runs.
#[derive(Clone, Debug, Default)]
pub struct ClientProfile {
    pub device_name: Option<String>,
    /// The display's stable device-interface path, when the host recorded one at create.
    #[cfg(windows)]
    pub device_path: Option<String>,
    pub platform: ClientPlatform,
    pub live: SharedLiveInput,
}

/// The two input settings a live session re-reads rather than captures. Neither remote
/// control nor the pointer scope changes a byte the client decodes, so they are two relaxed
/// atomics on the injector's dispatch path: the command writes them, the next event is
/// routed the new way, and nothing renegotiates.
#[derive(Debug)]
pub struct LiveInput {
    control: std::sync::atomic::AtomicBool,
    /// A `PointerScope` as `u8`; the enum is `Copy` and two-valued, so the round trip is total.
    scope: std::sync::atomic::AtomicU8,
    /// The reliable queues of the injectors reading this (Windows). Held weakly, so a queue
    /// still closes when its session lets go of it.
    release_to: std::sync::Mutex<Vec<std::sync::Weak<Sender<Queued>>>>,
}

pub type SharedLiveInput = Arc<LiveInput>;

impl Default for LiveInput {
    fn default() -> Self {
        Self::new(true, PointerScope::default())
    }
}

impl LiveInput {
    pub fn new(control_enabled: bool, scope: PointerScope) -> Self {
        Self {
            control: std::sync::atomic::AtomicBool::new(control_enabled),
            scope: std::sync::atomic::AtomicU8::new(scope.as_u8()),
            release_to: Default::default(),
        }
    }

    pub fn shared(control_enabled: bool, scope: PointerScope) -> SharedLiveInput {
        Arc::new(Self::new(control_enabled, scope))
    }

    pub fn control_enabled(&self) -> bool {
        self.control.load(Ordering::Relaxed)
    }

    pub fn set_control_enabled(&self, enabled: bool) {
        let was = self.control.swap(enabled, Ordering::Relaxed);
        // keys and buttons held when control goes off would otherwise stay down: the gate drops
        // every release the client sends after it
        if cfg!(windows) && was && !enabled {
            self.release_held();
        }
    }

    fn attach_injector(&self, queue: std::sync::Weak<Sender<Queued>>) {
        let mut queues = self.release_to.lock_recover();
        queues.retain(|q| q.strong_count() > 0);
        queues.push(queue);
    }

    fn release_held(&self) {
        let mut queues = self.release_to.lock_recover();
        queues.retain(|q| {
            q.upgrade().is_some_and(|tx| {
                tx.send(Queued {
                    arrival: 0,
                    client_ns: 0,
                    ev: InputEvent::Lifecycle(protocol::Lifecycle::Focus(false)),
                    host_recv_ns: host_now_ns(),
                    seq: None,
                })
                .is_ok()
            })
        });
    }

    pub fn pointer_scope(&self) -> PointerScope {
        PointerScope::from_u8(self.scope.load(Ordering::Relaxed))
    }

    pub fn set_pointer_scope(&self, scope: PointerScope) {
        self.scope.store(scope.as_u8(), Ordering::Relaxed);
    }
}

/// How far a pointer-locked client's relative motion is allowed to travel.
///
/// Absolute pointer frames are mapped inside the client's own display whatever this says;
/// only `MOUSE_DELTA` under pointer lock accumulates a position of its own, and this is what
/// that position is clamped to. `All` is the default (a second monitor whose mouse stops at
/// its own edge is not a second monitor) and `Display` is the opt-in restriction. Read per
/// event out of [`LiveInput`], so a change applies without renegotiating.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PointerScope {
    /// Confine the locked cursor to the display this session is streaming.
    Display,
    /// Let it cross onto the host's real monitors, i.e. the whole virtual desktop.
    #[default]
    All,
}

impl PointerScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::All => "all",
        }
    }

    /// Only the explicit restriction restricts: anything unrecognised or absent (a device saved
    /// without the setting) is the default.
    pub fn from_str(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "display" => Self::Display,
            _ => Self::All,
        }
    }

    /// For [`LiveInput`]'s atomic. Same rule as `from_str`: an unexpected byte reads as the
    /// default, not as a lockdown.
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Display => 1,
            Self::All => 0,
        }
    }

    pub fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Display,
            _ => Self::All,
        }
    }
}

/// The client's own operating system, as it reported at join. Only Apple/not-Apple matters
/// today; the variants are kept whole so nothing has to guess from a boolean.
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

/// Which injection path this host can actually take, probed rather than assumed. The case
/// that matters is macOS without Accessibility: `CGEventPost` returns `void` and the OS
/// drops every event with no error the process can observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackend {
    /// Windows: `SendInput` plus synthetic pen/touch pointer devices.
    SendInput,
    /// macOS: `CGEventPost`, with Accessibility granted.
    CgEvent,
    /// macOS: the API is present but TCC has not granted Accessibility, so injection is a
    /// silent no-op. Recoverable: the permission can be granted while the host runs.
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

/// Probe the injection tier. Never cached: on macOS the answer flips the moment the user
/// grants Accessibility.
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

/// Depth of the lossy queue, in events. Last-value-wins data: only where the pointer is *now*
/// matters, and a deep queue (512) replays seconds of history in order. 64 absorbs a burst;
/// anything that does not clear inside [`MAX_FAST_AGE_NS`] is stale anyway.
pub const FAST_CAP: usize = 64;

/// How old a queued positional event may be before injecting it is worse than skipping it:
/// three 60 Hz frames. A position that old is already superseded on the client, and injecting
/// it makes the pointer crawl through the past.
pub const MAX_FAST_AGE_NS: u64 = 50_000_000;

/// Window over which the host keeps its minimum observed client-to-host delay. The same 30 s
/// the client ages its best RTT sample over: a route change must not be remembered for the
/// life of the session.
const OFFSET_WINDOW_NS: u64 = 30_000_000_000;

/// The host's own estimate of how far a stamped input frame ran behind the best case.
///
/// The client never sends back its own `PING`/`PONG` offset estimate. What the host can see
/// unaided is `host_recv - client_send` for every stamped frame: the true clock offset plus
/// that frame's one-way delay. Its minimum over a window is the tightest offset estimate
/// available from one direction, and subtracting it leaves the excess one-way delay (0 on
/// the best sample, growing with queueing).
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

/// Per-event injection trace, off unless `SE_INPUT_TRACE=1`: one line per injected event
/// with the wall clock, to pair against the `Date.now()` the client page recorded at send.
///
/// Read once into a `OnceLock`: an env lookup per event would allocate on the injector
/// thread. The line is printed after `dispatch` returns, so it cannot delay the event it
/// describes.
fn trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::envcfg::var("SE_INPUT_TRACE").as_deref() == Ok("1"))
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn ev_kind(ev: &InputEvent) -> &'static str {
    match ev {
        InputEvent::Pointer { .. } => "pointer",
        InputEvent::PointerBatch { samples, .. } => {
            if samples.len() > 1 {
                "batchN"
            } else {
                "batch1"
            }
        }
        InputEvent::MouseDelta { .. } => "delta",
        InputEvent::Wheel { .. } => "wheel",
        InputEvent::Key { .. } => "key",
        _ => "other",
    }
}

/// An event with the two stamps the telemetry needs, moved through the channel as one unit.
pub struct Queued {
    /// The order it reached the host in, across both queues (Windows); 0 elsewhere.
    pub arrival: u64,
    /// The client's own send stamp, 0 when the client sent none.
    pub client_ns: u64,
    pub ev: InputEvent,
    /// `host_now_ns()` at the moment the transport handed the event over.
    pub host_recv_ns: u64,
    /// The frame's fast-channel sequence number, when it carried one.
    pub seq: Option<u16>,
}

/// Whether an event is positional last-value-wins data that a fresher one supersedes. Keys,
/// buttons, clipboard and lifecycle are state transitions and are never dropped for age.
///
/// Source-aware: a stale cursor position is the wrong place, but a pen or touch sample is
/// the shape of the line, and dropping one leaves a straight segment no later event repairs.
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

/// Whether a frame that arrived behind one already injected may be dropped. Narrower than
/// [`is_supersedable`] by one case: replaying an old absolute position walks the cursor
/// backwards, but a `MOUSE_DELTA` is an increment, increments commute, and dropping one
/// loses that motion for good.
///
/// A late delta must not restate a stale button word (`button_transitions` would synthesise
/// a spurious press or release), so the caller rewrites that word before dispatch.
fn is_reorder_droppable(ev: &InputEvent) -> bool {
    !matches!(ev, InputEvent::MouseDelta { .. }) && is_supersedable(ev)
}

/// One `SE_INPUT_TRACE` line, taken on the injector thread and printed wherever it is cheapest.
#[derive(Clone, Copy)]
struct TraceRec {
    kind: &'static str,
    recv_ns: u64,
    queue_us: u64,
    inject_us: u64,
    wall_ms: u64,
    qdepth: usize,
}

impl TraceRec {
    fn print(&self) {
        tprintln!(
            "input-trace kind={} recv_ns={} queue_us={} inject_us={} wall_ms={} qdepth={}",
            self.kind,
            self.recv_ns,
            self.queue_us,
            self.inject_us,
            self.wall_ms,
            self.qdepth
        );
    }
}

/// The `input_trace_thread` writer: the injector hands each record over without waiting, and a
/// record that finds the queue full is dropped rather than delaying the next event.
#[cfg(windows)]
fn spawn_trace_writer() -> Option<Sender<TraceRec>> {
    let (tx, rx) = bounded::<TraceRec>(1024);
    thread::Builder::new()
        .name("input-trace".into())
        .spawn(move || {
            while let Ok(rec) = rx.recv() {
                rec.print();
            }
        })
        .map_err(|e| teprintln!("input: trace thread did not start ({e}); tracing inline"))
        .ok()?;
    Some(tx)
}

/// A pen or touch position older than this when the injector reaches it is dropped (Windows):
/// after a stall the client gets the ink it is drawing now, not seconds of old strokes replayed.
/// Downs, ups and cancels are never dropped, so no stroke is left open.
const CONTACT_MAX_AGE_NS: u64 = 1_000_000_000;

/// Memory guard only: positions beyond this many queued reliable frames are refused at the door.
/// The age rule above keeps the queue far shorter than this whenever the injector is running.
const CONTACT_BACKLOG: usize = 8192;

/// A pen or touch position the injector should skip because the stroke has moved on.
fn contact_sample_is_stale(ev: &InputEvent, age_ns: u64) -> bool {
    age_ns > CONTACT_MAX_AGE_NS && is_contact_sample(ev)
}

/// A pen or touch position, as opposed to its down, up or cancel.
fn is_contact_sample(ev: &InputEvent) -> bool {
    match ev {
        InputEvent::Pointer { source, phase, .. } => {
            *source != protocol::SRC_MOUSE
                && matches!(
                    phase,
                    protocol::Phase::Move | protocol::Phase::Over | protocol::Phase::Enter
                )
        }
        InputEvent::PointerBatch { source, .. } => *source != protocol::SRC_MOUSE,
        _ => false,
    }
}

/// What is still injected while remote control is off: anything that lets go.
fn is_release(ev: &InputEvent) -> bool {
    match ev {
        InputEvent::Key { down, .. } => !down,
        InputEvent::Pointer { phase, .. } => matches!(
            phase,
            protocol::Phase::Up
                | protocol::Phase::Cancel
                | protocol::Phase::Leave
                | protocol::Phase::Out
        ),
        InputEvent::Lifecycle(_) => true,
        _ => false,
    }
}

/// The frame each queue has handed over but the injector has not taken yet: fast, reliable.
type Held = [Option<Queued>; 2];

/// The next frame in the order it reached the host, whichever queue it is on. A cursor move
/// dropped from the fast queue's head is the only thing ever skipped. `None` to stop.
fn next_in_arrival_order(
    held: &mut Held,
    fast: &Receiver<Queued>,
    reliable: &Receiver<Queued>,
    stop: &Receiver<()>,
) -> Option<Queued> {
    loop {
        if stop.try_recv().is_ok() {
            return None;
        }
        if held[0].is_none() {
            held[0] = fast.try_recv().ok();
        }
        if held[1].is_none() {
            held[1] = reliable.try_recv().ok();
        }
        let pick = match (&held[0], &held[1]) {
            (Some(f), Some(r)) => Some(if f.arrival <= r.arrival { 0 } else { 1 }),
            (Some(_), None) => Some(0),
            (None, Some(_)) => Some(1),
            (None, None) => None,
        };
        if let Some(i) = pick {
            return held[i].take();
        }
        select! {
            recv(stop) -> _ => return None,
            recv(reliable) -> m => held[1] = Some(m.ok()?),
            recv(fast) -> m => held[0] = Some(m.ok()?),
        }
    }
}

/// A press, a release or a cancel: the transitions a late frame must not undo.
#[cfg(windows)]
fn is_button_transition(ev: &InputEvent) -> bool {
    matches!(
        ev,
        InputEvent::Pointer {
            phase: protocol::Phase::Down | protocol::Phase::Up | protocol::Phase::Cancel,
            ..
        }
    )
}

#[derive(Clone)]
pub struct InjectorTx {
    fast: Sender<Queued>,
    /// The consumer's own end, kept so a full queue can be relieved from the head: crossbeam
    /// has no ring-drop, but the channel is MPMC, so the producer can pop the oldest itself.
    fast_relief: Receiver<Queued>,
    reliable: Arc<Sender<Queued>>,
    /// A one-shot the injector also selects on. Its only other exit is both input channels
    /// disconnecting, and the sender clones live in peer-connection handlers that outlive the
    /// `SessionHandle`, so "drop the sender and join" would wait forever inside the task that
    /// removes the virtual display.
    stop: Sender<()>,
    telemetry: SharedTelemetry,
    offset: Arc<ClockOffset>,
    /// Arrival order across both queues, so the injector can keep it (Windows).
    arrival: Arc<AtomicU64>,
}

impl InjectorTx {
    #[inline]
    pub fn route(&self, ev: InputEvent, hot: bool, client_ns: u64, seq: Option<u16>) {
        let host_recv_ns = host_now_ns();
        if let Some(excess_ns) = self.offset.observe(client_ns, host_recv_ns) {
            self.telemetry.stages.input_recv.record(excess_ns);
        }
        // only a frame a fresher one supersedes may be dropped from the head of a full queue;
        // pen, touch, leave, wheel and zoom go in order with everything else
        let hot = hot && (!cfg!(windows) || is_supersedable(&ev));
        if cfg!(windows) && !hot && is_contact_sample(&ev) && self.reliable.len() >= CONTACT_BACKLOG
        {
            self.count(&self.telemetry.degradation.input_contact_stale_drops);
            return;
        }
        let q = Queued {
            arrival: self.arrival.fetch_add(1, Ordering::Relaxed),
            client_ns,
            ev,
            host_recv_ns,
            seq: if hot { seq } else { None },
        };
        if hot {
            // newest wins: drop the head, not the tail. In a full queue the queued events are the
            // stale ones and the arriving one is where the pointer is.
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

    /// Tell the injector to leave. Idempotent, and never blocks: the channel holds one slot and
    /// a second call finds it full, which is the same instruction already in flight.
    pub fn shutdown(&self) {
        let _ = self.stop.try_send(());
    }

    pub fn release_all(&self) {
        use self::protocol::Lifecycle;
        let _ = self.reliable.send(Queued {
            arrival: 0,
            client_ns: 0,
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
    let rel_tx = Arc::new(rel_tx);
    if cfg!(windows) {
        client.live.attach_injector(Arc::downgrade(&rel_tx));
    }
    let (stop_tx, stop_rx) = bounded::<()>(1);

    let thread_telemetry = Arc::clone(&telemetry);
    let live = Arc::clone(&client.live);
    let join = thread::Builder::new()
        .name("injector".into())
        .spawn(move || {
            backend::boost_thread();
            let mut inj = backend::Injector::new(client);
            tprintln!("injector thread up ({})", backend::NAME);
            let t = thread_telemetry;
            // highest fast-channel sequence number already injected: the channel is unordered with
            // no retransmits, and replaying a swapped older move walks the cursor backwards
            let mut highest_seq: Option<u16> = None;
            // button word of the newest `MOUSE_DELTA` injected. A late delta's own word is stale,
            // so it is replaced with this before dispatch: the motion is kept, the spurious press
            // or release is not.
            let mut last_delta_buttons: u16 = 0;
            let tracing = trace_on();
            // Windows candidates: a frame the client sent before the newest button transition
            // already injected is late, whichever channel it came on; and the trace lines are
            // written by a thread of their own
            #[cfg(windows)]
            let order_by_client =
                crate::windows_utils::streamer::tune::candidate("input_order_by_client_time");
            #[cfg(windows)]
            let mut last_button_ns: u64 = 0;
            #[cfg(windows)]
            let trace_tx = if tracing
                && crate::windows_utils::streamer::tune::candidate("input_trace_thread")
            {
                spawn_trace_writer()
            } else {
                None
            };
            let mut held: Held = [None, None];
            loop {
                let q = if cfg!(windows) {
                    match next_in_arrival_order(&mut held, &fast_rx, &rel_rx, &stop_rx) {
                        Some(q) => q,
                        None => break,
                    }
                } else {
                    select! {
                        recv(stop_rx) -> _ => break,
                        recv(rel_rx) -> m => match m {
                            Ok(q) => q,
                            Err(_) => break,
                        },
                        recv(fast_rx) -> m => match m {
                            Ok(q) => q,
                            Err(_) => break,
                        },
                    }
                };
                // control may have gone off after the transport's gate let this through: a
                // press is not injected then, a release always is
                if cfg!(windows) && !live.control_enabled() && !is_release(&q.ev) {
                    continue;
                }
                let picked_up_ns = host_now_ns();
                let age_ns = picked_up_ns.saturating_sub(q.host_recv_ns);
                t.stages.input_queue.record(age_ns);
                if cfg!(windows) && contact_sample_is_stale(&q.ev, age_ns) {
                    t.degradation
                        .input_contact_stale_drops
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                if age_ns > MAX_FAST_AGE_NS && is_supersedable(&q.ev) {
                    t.degradation
                        .input_stale_drops
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                // only droppable frames are gated: a reordered button or key is a state
                // transition the host cannot reconstruct, and a reordered relative delta is
                // motion it cannot recover, so both are injected whatever their sequence says
                let mut ev = q.ev;
                #[cfg(windows)]
                let before_button = order_by_client && {
                    if is_button_transition(&ev) && q.client_ns != 0 {
                        last_button_ns = last_button_ns.max(q.client_ns);
                    }
                    q.client_ns != 0 && q.client_ns < last_button_ns
                };
                #[cfg(not(windows))]
                let before_button = false;
                if before_button && is_reorder_droppable(&ev) {
                    t.degradation
                        .input_reorder_drops
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
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
                        // kept, but its button word is older than what was injected and would
                        // read as a transition
                        if let InputEvent::MouseDelta { buttons, .. } = &mut ev {
                            *buttons = last_delta_buttons;
                        }
                    } else {
                        // only a newer sequence advances the mark: a late frame setting it
                        // would disarm the gate
                        highest_seq = Some(seq);
                    }
                }
                #[cfg(windows)]
                if before_button {
                    // kept for its motion, but its button word is older than the transition
                    // already injected; restate what the injector actually holds
                    if let InputEvent::MouseDelta { buttons, .. } = &mut ev {
                        *buttons = inj.buttons();
                    }
                }
                if let InputEvent::MouseDelta { buttons, .. } = &ev {
                    last_delta_buttons = *buttons;
                }
                inj.dispatch(&ev);
                let done_ns = host_now_ns();
                t.stages
                    .input_inject
                    .record(done_ns.saturating_sub(q.host_recv_ns));
                if tracing {
                    let rec = TraceRec {
                        kind: ev_kind(&ev),
                        recv_ns: q.host_recv_ns,
                        queue_us: age_ns / 1_000,
                        inject_us: done_ns.saturating_sub(q.host_recv_ns) / 1_000,
                        wall_ms: wall_ms(),
                        qdepth: fast_rx.len(),
                    };
                    #[cfg(windows)]
                    match &trace_tx {
                        Some(tx) => {
                            let _ = tx.try_send(rec);
                        }
                        None => rec.print(),
                    }
                    #[cfg(not(windows))]
                    rec.print();
                }
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
            stop: stop_tx,
            telemetry,
            offset: Arc::new(ClockOffset::default()),
            arrival: Arc::new(AtomicU64::new(0)),
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

    /// A sender pair whose consumer is the test, not an injector thread.
    #[cfg(windows)]
    fn bare_tx(fast_cap: usize) -> (InjectorTx, Receiver<Queued>, Receiver<Queued>) {
        let (fast, fast_rx) = bounded::<Queued>(fast_cap);
        let (reliable, rel_rx) = unbounded::<Queued>();
        let (stop, _stop_rx) = bounded::<()>(1);
        let tx = InjectorTx {
            fast,
            fast_relief: fast_rx.clone(),
            reliable: Arc::new(reliable),
            stop,
            telemetry: crate::streamer::telemetry::SessionTelemetry::new_shared(),
            offset: Arc::new(ClockOffset::default()),
            arrival: Arc::new(AtomicU64::new(0)),
        };
        (tx, fast_rx, rel_rx)
    }

    #[cfg(windows)]
    #[test]
    fn a_full_fast_queue_drops_only_a_cursor_position() {
        let (tx, fast_rx, rel_rx) = bare_tx(1);
        tx.route(pointer(Phase::Move), true, 0, Some(1));
        // pen ink arriving on the fast channel while it is full keeps its place
        tx.route(
            pointer_from(protocol::SRC_PEN, Phase::Move),
            true,
            0,
            Some(2),
        );
        assert_eq!(fast_rx.len(), 1, "the cursor position was not displaced");
        assert_eq!(
            rel_rx.len(),
            1,
            "the pen sample went in order with the rest"
        );
        // a fresher cursor position still replaces the stale one
        tx.route(pointer(Phase::Move), true, 0, Some(3));
        assert_eq!(fast_rx.recv().unwrap().seq, Some(3));
    }

    fn wheel() -> InputEvent {
        InputEvent::Wheel {
            source: protocol::SRC_MOUSE,
            dx: 0.0,
            dy: 120.0,
            dz: 0.0,
            mode: 0,
        }
    }

    /// Under a backlog the fast queue drops its oldest cursor move, and nothing else changes
    /// order: a scroll that came between two moves is injected between them.
    #[cfg(windows)]
    #[test]
    fn frames_are_injected_in_the_order_they_arrived_across_both_queues() {
        let (tx, fast_rx, rel_rx) = bare_tx(2);
        let (_stop_tx, stop_rx) = bounded::<()>(1);
        tx.route(pointer(Phase::Move), true, 0, Some(1));
        tx.route(wheel(), true, 0, Some(2));
        tx.route(pointer(Phase::Move), true, 0, Some(3));
        tx.route(pointer(Phase::Move), true, 0, Some(4));
        // the fast queue held two; the first move was the one dropped
        let mut held: Held = [None, None];
        let order: Vec<(u64, bool)> = (0..3)
            .map(|_| {
                let q = next_in_arrival_order(&mut held, &fast_rx, &rel_rx, &stop_rx).unwrap();
                (q.arrival, matches!(q.ev, InputEvent::Wheel { .. }))
            })
            .collect();
        assert_eq!(order, vec![(1, true), (2, false), (3, false)]);
    }

    /// Old ink goes from the old end; a stroke's down, up and cancel never do.
    #[test]
    fn old_ink_is_dropped_from_the_old_end_and_strokes_stay_closed() {
        let pen = |phase| pointer_from(protocol::SRC_PEN, phase);
        let second = 1_000_000_000;
        assert!(contact_sample_is_stale(&pen(Phase::Move), second + 1));
        assert!(!contact_sample_is_stale(&pen(Phase::Move), second / 2));
        assert!(!contact_sample_is_stale(&pen(Phase::Down), 10 * second));
        assert!(!contact_sample_is_stale(&pen(Phase::Up), 10 * second));
        assert!(!contact_sample_is_stale(&pen(Phase::Cancel), 10 * second));
        assert!(!contact_sample_is_stale(&pointer(Phase::Move), 10 * second));
    }

    /// A stalled injector cannot grow the pen backlog without bound, and never loses a contact's
    /// down or up to it.
    #[cfg(windows)]
    #[test]
    fn a_stalled_pen_backlog_is_bounded_and_keeps_its_downs_and_ups() {
        let (tx, _fast_rx, rel_rx) = bare_tx(2);
        tx.route(pointer_from(protocol::SRC_PEN, Phase::Down), false, 0, None);
        for _ in 0..(CONTACT_BACKLOG + 100) {
            tx.route(pointer_from(protocol::SRC_PEN, Phase::Move), true, 0, None);
        }
        tx.route(pointer_from(protocol::SRC_PEN, Phase::Up), false, 0, None);
        assert_eq!(
            rel_rx.len(),
            CONTACT_BACKLOG + 1,
            "bounded, with the up kept"
        );
        let last = rel_rx.try_iter().last().unwrap();
        assert!(matches!(
            last.ev,
            InputEvent::Pointer {
                phase: Phase::Up,
                ..
            }
        ));
    }

    /// Only a release passes once control is off, whatever the transport let through before.
    #[test]
    fn with_control_off_only_releases_are_injected() {
        assert!(is_release(&pointer(Phase::Up)));
        assert!(is_release(&InputEvent::Lifecycle(
            protocol::Lifecycle::Focus(false)
        )));
        assert!(!is_release(&pointer(Phase::Down)));
        assert!(!is_release(&pointer(Phase::Move)));
        assert!(!is_release(&wheel()));
    }

    #[cfg(windows)]
    #[test]
    fn turning_control_off_lets_go_of_what_is_held() {
        let live = LiveInput::shared(true, PointerScope::All);
        let (tx, rx) = unbounded::<Queued>();
        let tx = Arc::new(tx);
        live.attach_injector(Arc::downgrade(&tx));

        live.set_control_enabled(false);
        let q = rx.try_recv().expect("a release was queued");
        assert!(matches!(
            q.ev,
            InputEvent::Lifecycle(protocol::Lifecycle::Focus(false))
        ));
        live.set_control_enabled(false);
        assert!(
            rx.try_recv().is_err(),
            "only the true-to-false edge releases"
        );

        live.set_control_enabled(true);
        drop(tx);
        live.set_control_enabled(false);
        assert_eq!(
            live.release_to.lock().unwrap().len(),
            0,
            "a finished session's queue is forgotten, not kept open"
        );
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

    /// The age gate and the reorder gate ask different questions. A relative delta is redundant
    /// with its successors, so it may be dropped for age, but it is an increment and increments
    /// commute, so it is never dropped for order: that motion has no other source.
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

        // an absolute cursor position is last-value-wins; ink and state transitions are never gated
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

    /// The high-water mark may only move forward: advancing it from a frame that was itself
    /// behind walks it backwards and disarms the gate for everything in between.
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
        // 0 follows 65535
        assert!(protocol::seq_is_newer(0, u16::MAX));
        assert!(!protocol::seq_is_newer(u16::MAX, 0));
        // half a space apart is the documented limit; beyond it the sign flips, which is why
        // the gate only ever compares consecutive fast-channel frames
        assert!(protocol::seq_is_newer(3, u16::MAX - 3));
    }

    #[test]
    fn the_offset_estimator_reports_excess_delay_over_its_best_sample() {
        let c = ClockOffset::default();
        // an unstamped frame contributes nothing and is not measured
        assert_eq!(c.observe(0, 1_000), None);
        // the first sample is by definition the best one seen
        assert_eq!(c.observe(1_000, 6_000), Some(0));
        // 2000 ns further behind than the best sample
        assert_eq!(c.observe(2_000, 9_000), Some(2_000));
        // a better sample re-bases the estimate
        assert_eq!(c.observe(3_000, 7_000), Some(0));
        assert_eq!(c.observe(4_000, 9_000), Some(1_000));
    }

    #[test]
    fn a_stale_window_does_not_pin_the_estimate_forever() {
        let c = ClockOffset::default();
        assert_eq!(c.observe(1_000, 6_000), Some(0));
        // two windows later the 5 000 ns baseline has aged out of both slots, so a link that
        // settled at 8 000 ns reads as its own best case, not as permanent lateness
        let far = OFFSET_WINDOW_NS * 2 + 10_000;
        c.observe(far, far + 8_000);
        c.observe(far * 2, far * 2 + 8_000);
        assert_eq!(c.observe(far * 3, far * 3 + 8_000), Some(0));
    }
}
