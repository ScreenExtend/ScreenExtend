use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::{Deserialize, Serialize};
use specta::Type;
use tauri::AppHandle;
use tauri_specta::Event;

const MAX_BACKLOG: usize = 2000;

/// Depth of the hand-off queue between the logging call sites and the writer thread. Deep
/// enough to absorb a startup burst, shallow enough that a wedged writer cannot grow without
/// bound. Overflow drops the line and is reported in-band.
const QUEUE_DEPTH: usize = 4096;

/// Longest a line waits in the writer thread before it is emitted to the webview.
const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Most lines coalesced into one `LogLine` event.
const MAX_BATCH: usize = 64;

static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Whether anything is reading the `LogLine` stream. The Settings log panel is the only
/// consumer; it arms this on mount and disarms it on unmount through `set_log_streaming`.
/// `get_log_backlog` also arms it, so a caller that reads the backlog and subscribes without
/// saying so still gets its events. Tauri 2 exposes no way to ask how many JS listeners an
/// event has, which is why the panel has to tell us.
static STREAMING: AtomicBool = AtomicBool::new(false);

static DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn set_verbose(on: bool) {
    VERBOSE.store(on, Ordering::Relaxed);
}

#[inline]
pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

#[derive(Serialize, Deserialize, Debug, Clone, Type, Event)]
pub struct LogLine(pub String);

struct LogBus {
    backlog: Mutex<VecDeque<String>>,
    app: OnceLock<AppHandle>,
    tx: Sender<String>,
}

static BUS: OnceLock<LogBus> = OnceLock::new();

fn bus() -> &'static LogBus {
    BUS.get_or_init(|| {
        let (tx, rx) = bounded(QUEUE_DEPTH);
        let bus = LogBus {
            backlog: Mutex::new(VecDeque::with_capacity(MAX_BACKLOG)),
            app: OnceLock::new(),
            tx,
        };
        // The writer cannot borrow `bus` yet, so it re-enters through `bus()` once BUS is set.
        // `get_or_init` has not returned at this point, hence the small spin in `writer`.
        std::thread::Builder::new()
            .name("logbus".into())
            .spawn(move || writer(rx))
            .ok();
        bus
    })
}

/// Queue a line for the backlog and the UI.
///
/// This is the only thing a `tprintln!` call site pays for, and it is deliberately cheap: one
/// bounded-channel send that moves the already-formatted `String`. No lock the capture, encode
/// or audio threads can contend on, no second allocation, and no JSON serialization — Tauri
/// serializes an event payload before it checks whether anyone is listening, so emitting from
/// here would cost that on every line whether or not the log panel is open.
pub fn push_line(line: String) {
    match bus().tx.try_send(line) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn writer(rx: Receiver<String>) {
    // `bus()` is mid-`get_or_init` on the spawning thread; wait for it to publish.
    let bus = loop {
        if let Some(bus) = BUS.get() {
            break bus;
        }
        std::thread::yield_now();
    };

    let mut batch: Vec<String> = Vec::with_capacity(MAX_BATCH);
    while let Ok(first) = rx.recv() {
        batch.push(first);

        let streaming = STREAMING.load(Ordering::Relaxed);
        if streaming {
            // Trade up to FLUSH_INTERVAL of latency for one IPC hop per batch.
            let deadline = Instant::now() + FLUSH_INTERVAL;
            while batch.len() < MAX_BATCH {
                match rx.recv_deadline(deadline) {
                    Ok(line) => batch.push(line),
                    Err(_) => break,
                }
            }
        } else {
            // Nothing is listening; keep the backlog as fresh as possible instead.
            while batch.len() < MAX_BATCH {
                match rx.try_recv() {
                    Ok(line) => batch.push(line),
                    Err(_) => break,
                }
            }
        }

        let dropped = DROPPED.swap(0, Ordering::Relaxed);
        if dropped > 0 {
            batch.push(format!(
                "[logbus] dropped {dropped} line(s): the log queue overflowed"
            ));
        }

        // Whether to emit is decided under the backlog lock, not from `streaming` above: that
        // was read before this batch reached the ring, so a panel that armed the stream and read
        // the backlog in between saw these lines in neither.
        if commit(&bus.backlog, &STREAMING, &batch) {
            if let Some(app) = bus.app.get() {
                let payload = if batch.len() == 1 {
                    batch.pop().unwrap()
                } else {
                    batch.join("\n")
                };
                let _ = LogLine(payload).emit(app);
            }
        }

        batch.clear();
    }
}

/// Append one batch to the ring and say whether it must also be emitted.
///
/// The answer is read under the ring's lock, the same lock [`arm_and_snapshot`] arms the stream
/// under, so every line lands on exactly one side of a panel's backlog read: either it was in the
/// ring when the snapshot was taken, or the stream was armed when it was committed. Reading the
/// flag outside the lock lost the lines of any batch that straddled the panel opening.
fn commit(backlog: &Mutex<VecDeque<String>>, streaming: &AtomicBool, batch: &[String]) -> bool {
    let mut backlog = backlog.lock().unwrap_or_else(|e| e.into_inner());
    for line in batch {
        if backlog.len() >= MAX_BACKLOG {
            backlog.pop_front();
        }
        backlog.push_back(line.clone());
    }
    streaming.load(Ordering::Relaxed)
}

/// Arm the stream and copy the ring in one critical section. See [`commit`].
fn arm_and_snapshot(backlog: &Mutex<VecDeque<String>>, streaming: &AtomicBool) -> Vec<String> {
    let backlog = backlog.lock().unwrap_or_else(|e| e.into_inner());
    streaming.store(true, Ordering::Relaxed);
    backlog.iter().cloned().collect()
}

pub fn attach(app: AppHandle) {
    let _ = bus().app.set(app);
}

#[tauri::command]
#[specta::specta]
pub fn get_log_backlog() -> Vec<String> {
    arm_and_snapshot(&bus().backlog, &STREAMING)
}

/// The backlog for a reader that is *not* going to listen: the control server's `LOGS` verb,
/// i.e. `ScreenExtend logs`. It used to call [`get_log_backlog`], whose arming is a side effect
/// meant for the panel — so one `ScreenExtend logs` against the desktop app left the writer
/// serialising every line into a `LogLine` IPC event for the rest of the process's life, which is
/// exactly the cost `set_log_streaming(false)` exists to switch off. Under `serve` it switched
/// the writer to 50 ms coalescing, so the newest lines reached the ring later.
pub fn backlog_snapshot() -> Vec<String> {
    bus()
        .backlog
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Whether the `LogLine` stream is armed. For the control server's tests.
#[cfg(test)]
pub fn streaming_armed() -> bool {
    STREAMING.load(Ordering::Relaxed)
}

/// Arm or disarm the `LogLine` event stream. Called by the Settings log panel on mount and,
/// crucially, on unmount: without the second call the panel being opened once kept the writer
/// thread emitting batched IPC events for the rest of the process's life.
#[tauri::command]
#[specta::specta]
pub fn set_log_streaming(on: bool) {
    STREAMING.store(on, Ordering::Relaxed);
}

/// The `-v` echo of one log line. `println!` panics when stdout is a pipe nobody reads any more
/// (a closed terminal, `serve -v | Select -First 20`, a supervising script that exited), and the
/// echo runs on whichever thread logged — the control server's accept loop, a join, the capture
/// or audio thread — so one vanished reader took those threads down mid-line. The line never
/// reached the backlog either, because the echo comes first. A failed echo is now dropped.
pub fn echo_stdout(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stdout().lock(), "{line}");
}

/// [`echo_stdout`] for stderr.
pub fn echo_stderr(line: &str) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{line}");
}

#[macro_export]
macro_rules! tprintln {
    () => {{
        if $crate::logbus::verbose() {
            $crate::logbus::echo_stdout("");
        }
        $crate::logbus::push_line(String::new());
    }};
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        if $crate::logbus::verbose() {
            $crate::logbus::echo_stdout(&__line);
        }
        $crate::logbus::push_line(__line);
    }};
}

#[macro_export]
macro_rules! teprintln {
    () => {{
        if $crate::logbus::verbose() {
            $crate::logbus::echo_stderr("");
        }
        $crate::logbus::push_line(String::new());
    }};
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        if $crate::logbus::verbose() {
            $crate::logbus::echo_stderr(&__line);
        }
        $crate::logbus::push_line(__line);
    }};
}

const MAX_FRONTEND_ERROR_CHARS: usize = 2000;

/// The webview's `console.error` has nowhere to go in a release build -- there are no
/// devtools attached and nothing captures it -- so failures in the desktop UI simply
/// vanished. This puts them on the same bus as everything else, which means they reach
/// the Settings log terminal, `get_log_backlog` and `ScreenExtend logs`.
///
/// Defined below the macros because a `macro_rules!` is only in scope after its definition
/// within its own file.
#[tauri::command]
#[specta::specta]
pub fn log_frontend_error(message: String) {
    let mut text = message.replace(['\r', '\n'], " ");
    if text.chars().count() > MAX_FRONTEND_ERROR_CHARS {
        text = text.chars().take(MAX_FRONTEND_ERROR_CHARS).collect();
        text.push('…');
    }
    teprintln!("[ui] {}", text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_settled() {
        // The writer thread wakes on every send; give it room to catch up before reading.
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(5));
            if bus().tx.is_empty() {
                std::thread::sleep(Duration::from_millis(5));
                return;
            }
        }
    }

    /// One test, not two: the ring is process-global, so a sibling test pushing at the same
    /// time would evict this one's lines out from under it.
    #[test]
    fn lines_reach_the_backlog_in_order_and_the_ring_stays_bounded() {
        for i in 0..10 {
            push_line(format!("logbus-order-{i}"));
        }
        drain_settled();
        let backlog = bus().backlog.lock().unwrap().clone();
        let seen: Vec<&String> = backlog
            .iter()
            .filter(|l| l.starts_with("logbus-order-"))
            .collect();
        assert_eq!(seen.len(), 10, "every line reached the ring");
        for (i, line) in seen.iter().enumerate() {
            assert_eq!(**line, format!("logbus-order-{i}"));
        }

        for i in 0..(MAX_BACKLOG + 500) {
            push_line(format!("logbus-cap-{i}"));
        }
        drain_settled();
        assert!(bus().backlog.lock().unwrap().len() <= MAX_BACKLOG);
    }

    /// The log panel reads the backlog and then listens. Every line the writer commits while
    /// that happens must reach the panel exactly one way: in the snapshot, or emitted. A writer
    /// commits numbered batches on its own thread while the panel arms the stream mid-flight;
    /// the snapshot followed by everything emitted after it must be the whole sequence, with no
    /// gap. Before the fix the emit decision was a flag read *before* the commit, and the batch
    /// that straddled the snapshot fell between the two.
    #[test]
    fn a_panel_opening_mid_stream_loses_no_line() {
        use std::sync::Arc;
        const BATCHES: usize = 400;
        const PER: usize = 3;
        for attempt in 0..20 {
            let ring = Arc::new(Mutex::new(VecDeque::<String>::new()));
            let streaming = Arc::new(AtomicBool::new(false));
            let writer = {
                let (ring, streaming) = (Arc::clone(&ring), Arc::clone(&streaming));
                std::thread::spawn(move || {
                    let mut emitted = Vec::new();
                    for b in 0..BATCHES {
                        let batch: Vec<String> =
                            (0..PER).map(|i| format!("{}", b * PER + i)).collect();
                        if commit(&ring, &streaming, &batch) {
                            emitted.extend(batch);
                        }
                        if b % 7 == 0 {
                            std::thread::yield_now();
                        }
                    }
                    emitted
                })
            };
            // Open the panel somewhere in the middle of the stream.
            std::thread::sleep(Duration::from_micros(50 * attempt as u64));
            let snapshot = arm_and_snapshot(&ring, &streaming);
            let emitted = writer.join().unwrap();

            let mut seen: Vec<usize> = snapshot.iter().map(|s| s.parse().unwrap()).collect();
            seen.extend(emitted.iter().map(|s| s.parse::<usize>().unwrap()));
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(
                seen,
                (0..BATCHES * PER).collect::<Vec<_>>(),
                "attempt {attempt}: a line reached the panel neither in the backlog nor as an \
                 event (snapshot {} lines, emitted {})",
                snapshot.len(),
                emitted.len()
            );
        }
    }

    /// Cost of one `tprintln!` at the call site, against the shape this module used to have.
    /// Run with `cargo test --lib logbus::tests::bench_push_line -- --ignored --nocapture`.
    ///
    /// The "before" arm reproduces the old `push_line` exactly — take the global ring mutex,
    /// clone the line, push, evict — minus the `LogLine(line).emit(app)` that followed it,
    /// which a unit test cannot reach because no `AppHandle` exists here. That emit was the
    /// larger half (a JSON serialization plus two Tauri-internal mutexes, paid whether or not
    /// the log panel was mounted), so the ratio below is a lower bound on the improvement.
    #[test]
    #[ignore = "benchmark, not an assertion; run with --ignored --nocapture"]
    fn bench_push_line() {
        const N: usize = 200_000;

        let old_ring: Mutex<VecDeque<String>> = Mutex::new(VecDeque::with_capacity(MAX_BACKLOG));
        let t0 = Instant::now();
        for i in 0..N {
            let line = format!("[bench] line {i} of {N}");
            let mut backlog = old_ring.lock().unwrap();
            if backlog.len() >= MAX_BACKLOG {
                backlog.pop_front();
            }
            backlog.push_back(line.clone());
        }
        let before = t0.elapsed();

        // Warm the bus and the writer thread so the spawn is not in the measurement.
        push_line("warmup".into());
        drain_settled();
        DROPPED.store(0, Ordering::Relaxed);

        let t1 = Instant::now();
        for i in 0..N {
            push_line(format!("[bench] line {i} of {N}"));
        }
        let after = t1.elapsed();

        let before_ns = before.as_nanos() as f64 / N as f64;
        let after_ns = after.as_nanos() as f64 / N as f64;
        println!("push_line, {N} lines, one thread, log panel closed:");
        println!("  before (ring lock + clone, no emit) {before_ns:.0} ns/line");
        println!("  after  (bounded try_send)           {after_ns:.0} ns/line");
        println!("  ratio  {:.2}x", before_ns / after_ns);
        drain_settled();
        println!("  dropped {}", DROPPED.load(Ordering::Relaxed));

        // The shape that actually matters: capture, encode, audio and the tokio workers all
        // log, so the old global ring mutex was contended across threads that must not block
        // each other. Four writers is a conservative stand-in.
        const THREADS: usize = 4;
        const PER: usize = 50_000;

        let old_ring =
            std::sync::Arc::new(Mutex::new(VecDeque::<String>::with_capacity(MAX_BACKLOG)));
        let t2 = Instant::now();
        let mut handles = Vec::new();
        for t in 0..THREADS {
            let ring = old_ring.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..PER {
                    let line = format!("[bench] thread {t} line {i}");
                    let mut backlog = ring.lock().unwrap();
                    if backlog.len() >= MAX_BACKLOG {
                        backlog.pop_front();
                    }
                    backlog.push_back(line.clone());
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let before_mt = t2.elapsed();

        DROPPED.store(0, Ordering::Relaxed);
        let t3 = Instant::now();
        let mut handles = Vec::new();
        for t in 0..THREADS {
            handles.push(std::thread::spawn(move || {
                for i in 0..PER {
                    push_line(format!("[bench] thread {t} line {i}"));
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let after_mt = t3.elapsed();

        let total = (THREADS * PER) as f64;
        let before_mt_ns = before_mt.as_nanos() as f64 / total;
        let after_mt_ns = after_mt.as_nanos() as f64 / total;
        println!("push_line, {total} lines, {THREADS} threads, log panel closed:");
        println!("  before (contended ring lock)        {before_mt_ns:.0} ns/line");
        println!("  after  (bounded try_send)           {after_mt_ns:.0} ns/line");
        println!("  ratio  {:.2}x", before_mt_ns / after_mt_ns);
        drain_settled();
        println!("  dropped {}", DROPPED.load(Ordering::Relaxed));
    }
}
