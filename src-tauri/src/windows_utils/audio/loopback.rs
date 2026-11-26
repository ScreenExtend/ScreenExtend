use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::CLSCTX_ALL;

use super::device::{
    create_enumerator, default_render_endpoint, DeviceEvent, NotifierRegistration,
};
use super::engine;
use super::format::{
    convert_into, parse_mix_format, MixFormat, SampleKind, StereoResampler, HARD_LIMIT, WIDE_LIMIT,
};
use super::guards::{ComGuard, EventHandle, MixFormatPtr, MmcssGuard};
use super::process_loopback;
use super::silence::SilenceCompanion;
use crate::streamer::audio::encoder::{OpusEncoder, OpusEncoderConfig, FRAME_SAMPLES};
use crate::streamer::audio::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioSink, AudioStopFn,
    FLAG_DISCONTINUITY, FLAG_SILENT,
};
use crate::windows_utils::streamer::tune;

/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`: "A channel matrixer and a sample rate converter are
/// inserted as necessary to convert between the uncompressed format supplied to
/// IAudioClient::Initialize and the audio engine mix format."
const STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
/// `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`. The name reads backwards from the meaning:
/// Microsoft documents it as "a sample rate converter with better quality than the default
/// conversion but with a higher performance cost". It is a filter with group delay, and it is
/// only ever reached on a non-48 kHz endpoint.
const STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
// `AUDCLNT_E_DEVICE_INVALIDATED` (`AUDCLNT_ERR(0x004)`)
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;

const OUT_SAMPLE_RATE: u32 = 48000;
const OUT_CHANNELS: u16 = 2;
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * OUT_CHANNELS as usize;
const DIAG_LOG_INTERVAL: Duration = Duration::from_secs(2);
/// How long each `WaitForSingleObject` on the capture event may block.
const EVENT_WAIT_MS: u32 = 100;
/// Consecutive unsignalled waits before the endpoint is treated as clockless and re-acquired.
/// 20 x 100 ms = 2 s of nothing at all, against a 10 ms nominal period.
const STARVED_WAITS_BEFORE_REACQUIRE: u32 = 20;
/// How long `start` waits for the capture thread to open its device. A healthy open is tens to
/// hundreds of milliseconds (the process-loopback activation is bounded at 1 s, the companion
/// at 5 s); this bounds a WASAPI call wedged inside the audio service or a driver, which would
/// otherwise hold the audio hub `busy`, and with it every session's audio toggle.
const CAPTURE_START_TIMEOUT: Duration = Duration::from_secs(10);
/// First wait after a failed re-acquire; it doubles per consecutive failure up to the max.
const REACQUIRE_BACKOFF_FIRST: Duration = Duration::from_millis(200);
const REACQUIRE_BACKOFF_MAX: Duration = Duration::from_secs(5);
/// While a re-acquire waits out its backoff, how often the thread looks at `stop`.
const PENDING_POLL: Duration = Duration::from_millis(50);
/// How long a re-acquired session must run before the run of failures behind it is forgotten.
/// A session that opens and then fails at once (the audio service restarting, a client that
/// initializes and then errors on every call) is one more failure of the same run and waits
/// out the backoff, instead of re-opening in a tight loop on the real-time thread.
const HEALTHY_SESSION: Duration = Duration::from_secs(10);

/// Try the process-loopback tier even where it would not be tried otherwise: overrides
/// `SE_ENDPOINT_LOOPBACK=1` and a process tier the gate has parked after a failed activation.
/// Set only by `ScreenExtend probe --audio --process-loopback`.
pub static PREFER_PROCESS_LOOPBACK: AtomicBool = AtomicBool::new(false);

/// Which capture tier is running. Every tier is a value, not only a log line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureTier {
    /// No capture is running.
    Idle = 0,
    /// `process_loopback.rs`: every process except this one, off the synthetic device.
    ProcessLoopback = 1,
    /// The default render endpoint, already 48 kHz float stereo.
    EndpointNative = 2,
    /// The default render endpoint, converted by the engine (`AUTOCONVERTPCM`).
    EndpointAutoconvert = 3,
    /// The default render endpoint at its own format, converted (and resampled) in userspace
    /// because the engine refused to.
    EndpointUserspace = 4,
}

impl CaptureTier {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureTier::Idle => "idle",
            CaptureTier::ProcessLoopback => "process-loopback",
            CaptureTier::EndpointNative => "endpoint-native",
            CaptureTier::EndpointAutoconvert => "endpoint-autoconvert",
            CaptureTier::EndpointUserspace => "endpoint-userspace",
        }
    }

    pub fn is_endpoint(self) -> bool {
        matches!(
            self,
            CaptureTier::EndpointNative
                | CaptureTier::EndpointAutoconvert
                | CaptureTier::EndpointUserspace
        )
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => CaptureTier::ProcessLoopback,
            2 => CaptureTier::EndpointNative,
            3 => CaptureTier::EndpointAutoconvert,
            4 => CaptureTier::EndpointUserspace,
            _ => CaptureTier::Idle,
        }
    }
}

static ACTIVE_TIER: AtomicU8 = AtomicU8::new(CaptureTier::Idle as u8);

/// The tier the host-wide capture is on right now. The hub runs at most one capture at a time.
pub fn active_tier() -> CaptureTier {
    CaptureTier::from_u8(ACTIVE_TIER.load(Ordering::Relaxed))
}

/// [`CaptureTier::as_str`] as a reference that lives for the program, which is what the shared
/// diagnostics keep in an atomic.
fn tier_name(tier: CaptureTier) -> &'static &'static str {
    static NAMES: [&str; 5] = [
        "idle",
        "process-loopback",
        "endpoint-native",
        "endpoint-autoconvert",
        "endpoint-userspace",
    ];
    &NAMES[(tier as usize).min(NAMES.len() - 1)]
}

fn set_active_tier(tier: CaptureTier) {
    ACTIVE_TIER.store(tier as u8, Ordering::Relaxed);
}

/// How a capture thread's setup ended, as seen by the thread that is waiting for it.
pub(super) enum Ready<T> {
    Started(T),
    Failed(anyhow::Error),
    /// The thread went away without saying anything.
    Exited,
    /// The thread is still inside an OS call; the caller must not join it.
    TimedOut,
}

/// Wait at most `timeout` for a setup result. Both capture threads (this one and the silent
/// companion) report through this, so neither can wedge its caller.
pub(super) fn await_ready<T>(rx: &mpsc::Receiver<Result<T>>, timeout: Duration) -> Ready<T> {
    match rx.recv_timeout(timeout) {
        Ok(Ok(v)) => Ready::Started(v),
        Ok(Err(e)) => Ready::Failed(e),
        Err(mpsc::RecvTimeoutError::Timeout) => Ready::TimedOut,
        Err(mpsc::RecvTimeoutError::Disconnected) => Ready::Exited,
    }
}

const SETUP_PENDING: u8 = 0;
const SETUP_DONE: u8 = 1;
const SETUP_ABANDONED: u8 = 2;

/// Setup threads a caller gave up on that are still inside an OS call.
///
/// A timed-out start detaches its thread, and a retry against a wedged audio service would park
/// one more thread in the same call. While one is outstanding, the next start is refused
/// without spawning.
pub(super) struct StuckSetups(AtomicUsize);

impl StuckSetups {
    pub(super) const fn new() -> Self {
        Self(AtomicUsize::new(0))
    }

    pub(super) fn outstanding(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    /// The caller stopped waiting for the thread that holds `token`.
    pub(super) fn abandon(&self, token: &AtomicU8) {
        self.0.fetch_add(1, Ordering::AcqRel);
        if token
            .compare_exchange(
                SETUP_PENDING,
                SETUP_ABANDONED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            // the thread got out in between: it is not stuck
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// The setup thread is out of its OS calls, whatever the outcome. Idempotent.
    pub(super) fn finish(&self, token: &AtomicU8) {
        if token.swap(SETUP_DONE, Ordering::AcqRel) == SETUP_ABANDONED {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The setup thread's end of [`await_ready`]: reporting a result, or leaving without one, marks
/// the thread as no longer stuck.
pub(super) struct SetupReporter<T> {
    tx: mpsc::Sender<Result<T>>,
    token: Arc<AtomicU8>,
    stuck: &'static StuckSetups,
}

impl<T> SetupReporter<T> {
    /// Returns the reporter for the thread and the token the caller passes to
    /// [`StuckSetups::abandon`] on a timeout.
    pub(super) fn new(
        tx: mpsc::Sender<Result<T>>,
        stuck: &'static StuckSetups,
    ) -> (Self, Arc<AtomicU8>) {
        let token = Arc::new(AtomicU8::new(SETUP_PENDING));
        (
            Self {
                tx,
                token: Arc::clone(&token),
                stuck,
            },
            token,
        )
    }

    pub(super) fn send(
        &self,
        result: Result<T>,
    ) -> std::result::Result<(), mpsc::SendError<Result<T>>> {
        self.stuck.finish(&self.token);
        self.tx.send(result)
    }
}

impl<T> Drop for SetupReporter<T> {
    fn drop(&mut self) {
        self.stuck.finish(&self.token);
    }
}

static STUCK_CAPTURE_STARTS: StuckSetups = StuckSetups::new();

/// The A/B switches this capture asks, read once when the capture thread starts.
#[derive(Clone, Copy, Debug)]
struct Tuning {
    /// `SE_ENDPOINT_LOOPBACK=1`: endpoint loopback first, for a measurement.
    endpoint_forced: bool,
    /// Candidate `audio_process_ignores_device_change`: on the process tier, a default-device
    /// change is not followed by a re-acquire.
    process_ignores_device_change: bool,
    /// Candidate `audio_process_period_companion`: start a silent render companion beside the
    /// process tier so it can pull the endpoint's engine period down (an experiment).
    process_period_companion: bool,
    /// Candidate `audio_wide_sample_limit`: clamp at +/-2.0 instead of hard-clipping at +/-1.0.
    sample_limit: f32,
}

impl Tuning {
    fn read() -> Self {
        Self {
            endpoint_forced: crate::envcfg::var("SE_ENDPOINT_LOOPBACK").as_deref() == Ok("1"),
            process_ignores_device_change: tune::candidate("audio_process_ignores_device_change"),
            process_period_companion: tune::candidate("audio_process_period_companion"),
            sample_limit: if tune::candidate("audio_wide_sample_limit") {
                WIDE_LIMIT
            } else {
                HARD_LIMIT
            },
        }
    }
}

/// Windows-only counters the shared `AudioDiagnostics` has no field for. Read by the
/// `audio-diag` thread and appended to its line.
#[derive(Debug, Default)]
struct HostStatus {
    notifier_ok: AtomicBool,
    encode_failures: AtomicU64,
    reacquire_failures: AtomicU64,
    capture_errors: AtomicU64,
}

/// Why the capture thread wants a fresh session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReacquireReason {
    DeviceChanged,
    DeviceInvalidated,
    CompanionDied,
    Starved,
    /// Any other `GetNextPacketSize` / `GetBuffer` / `ReleaseBuffer` failure: mostly the audio
    /// service restarting (`AUDCLNT_E_SERVICE_NOT_RUNNING`, an RPC disconnect). Ending the
    /// capture thread on it would leave every session silent while the hub reports it running.
    CaptureError,
}

impl ReacquireReason {
    fn describe(self) -> &'static str {
        match self {
            ReacquireReason::DeviceChanged => "a default-device change",
            ReacquireReason::DeviceInvalidated => "a device invalidation",
            ReacquireReason::CompanionDied => "a dead silent companion",
            ReacquireReason::Starved => "capture starvation",
            ReacquireReason::CaptureError => "a capture error",
        }
    }
}

/// A re-acquire that is wanted, and when it may next be attempted.
///
/// One state for every cause, so a failed attempt is always retried. A device change that fails
/// once and is forgotten leaves the endpoint tier looping back a device nobody plays to: the old
/// companion keeps the old endpoint clocked, so starvation never fires.
#[derive(Debug, Default)]
struct Recovery {
    pending: Option<ReacquireReason>,
    /// Attempts in this run that failed, or that opened a session which then failed before
    /// it had run for [`HEALTHY_SESSION`].
    failures: u32,
    next_at: Option<Instant>,
    /// When the last re-acquire succeeded, while that session has not yet been judged.
    succeeded_at: Option<Instant>,
}

impl Recovery {
    /// Ask for a re-acquire. A request already pending keeps its reason and its backoff.
    ///
    /// A success does not end the run on its own: if the session it opened fails within
    /// [`HEALTHY_SESSION`], that counts as one more failure and the next attempt waits out the
    /// backoff. Only a default-device change is followed at once, because it says nothing
    /// against the session.
    fn request(&mut self, reason: ReacquireReason, now: Instant) {
        if self.pending.is_some() {
            return;
        }
        self.pending = Some(reason);
        if let Some(at) = self.succeeded_at.take() {
            if now.saturating_duration_since(at) >= HEALTHY_SESSION {
                self.failures = 0;
            } else if reason != ReacquireReason::DeviceChanged {
                self.failures = self.failures.saturating_add(1);
                self.next_at = Some(at + reacquire_backoff(self.failures));
            }
        }
    }

    /// Whether the request just made should be logged: the first of a run, then the 1st, 2nd,
    /// 4th, 8th ... repeat, the same thinning as failed attempts.
    fn should_log_request(&self) -> bool {
        self.failures == 0 || should_log_attempt(self.failures)
    }

    fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The pending reason, if an attempt is allowed now.
    fn due(&self, now: Instant) -> Option<ReacquireReason> {
        let reason = self.pending?;
        match self.next_at {
            Some(at) if now < at => None,
            _ => Some(reason),
        }
    }

    /// How long until the next attempt, while one is pending and not yet due.
    fn wait_remaining(&self, now: Instant) -> Option<Duration> {
        self.pending?;
        self.next_at
            .and_then(|at| at.checked_duration_since(now))
            .filter(|d| !d.is_zero())
    }

    /// Record a failed attempt; returns the attempt number and the wait before the next.
    fn on_failure(&mut self, now: Instant) -> (u32, Duration) {
        self.failures = self.failures.saturating_add(1);
        let delay = reacquire_backoff(self.failures);
        self.next_at = Some(now + delay);
        (self.failures, delay)
    }

    /// Record a successful attempt; returns how many failed before it. The count is kept until
    /// the new session has proved itself (see [`Recovery::request`]).
    fn on_success(&mut self, now: Instant) -> u32 {
        self.pending = None;
        self.next_at = None;
        self.succeeded_at = Some(now);
        self.failures
    }
}

/// 200 ms, 400 ms, 800 ms ... capped at 5 s.
fn reacquire_backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(16);
    (REACQUIRE_BACKOFF_FIRST * (1u32 << shift)).min(REACQUIRE_BACKOFF_MAX)
}

/// Log the 1st, 2nd, 4th, 8th ... failure of a run, not every one.
fn should_log_attempt(attempt: u32) -> bool {
    attempt.is_power_of_two()
}

/// Whether a default-device change should replace the session. The endpoint tier is tied to
/// the device it opened; the process tier is not, and re-acquiring it costs an activation, a
/// discontinuity on the client and the audio in between.
fn device_change_needs_reacquire(tier: CaptureTier, process_ignores_device_change: bool) -> bool {
    tier.is_endpoint() || !process_ignores_device_change
}

fn reason_for(err: &DrainError) -> ReacquireReason {
    match err {
        DrainError::DeviceInvalidated => ReacquireReason::DeviceInvalidated,
        DrainError::Fatal(_) => ReacquireReason::CaptureError,
    }
}

/// True when every sample of an encoded frame is digital zero, which is what `FLAG_SILENT`
/// means on the wire. Not `AUDCLNT_BUFFERFLAGS_SILENT`: the process tier never sets it, and it
/// covers only the first 5 ms frame of a silent 10 ms buffer.
#[inline]
fn is_digital_silence(frame: &[f32]) -> bool {
    frame.iter().all(|&s| s == 0.0)
}

fn pad_to_frame(accum: &mut Vec<f32>) {
    let rem = accum.len() % FRAME_INTERLEAVED;
    if rem != 0 {
        accum.resize(accum.len() + FRAME_INTERLEAVED - rem, 0.0);
    }
}

pub fn start(sink: AudioSink) -> Result<AudioCapture> {
    let diagnostics = Arc::new(AudioDiagnostics::default());
    let stop = Arc::new(AtomicBool::new(false));

    if STUCK_CAPTURE_STARTS.outstanding() > 0 {
        bail!(
            "an earlier audio capture start is still stuck inside the audio service; not \
             opening another until it returns"
        );
    }

    let (ready_tx, ready_rx) = mpsc::channel::<Result<(AudioFormat, i32, CaptureTier)>>();
    let (ready_tx, token) = SetupReporter::new(ready_tx, &STUCK_CAPTURE_STARTS);
    // the thread waits for this before it captures, so the tier `start` publishes below can
    // never land after a re-acquire's, and a thread `start` gave up on never captures at all
    let (go_tx, go_rx) = mpsc::channel::<()>();

    let diag_thread = Arc::clone(&diagnostics);
    let stop_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("audio-loopback".to_string())
        .spawn(move || run(sink, diag_thread, stop_thread, ready_tx, go_rx))
        .context("spawning audio loopback thread")?;

    let (format, lookahead_samples, tier) = match await_ready(&ready_rx, CAPTURE_START_TIMEOUT) {
        Ready::Started(v) => v,
        Ready::Failed(e) => {
            let _ = join.join();
            return Err(e);
        }
        Ready::Exited => {
            let _ = join.join();
            bail!("audio loopback thread exited during setup");
        }
        Ready::TimedOut => {
            // still inside a WASAPI call: it sees `stop` when the call returns and leaves without
            // publishing anything, and joining it here would wedge the hub
            stop.store(true, Ordering::Relaxed);
            STUCK_CAPTURE_STARTS.abandon(&token);
            drop(join);
            bail!(
                "audio capture did not start within {} s; the audio service or driver is not \
                 answering",
                CAPTURE_START_TIMEOUT.as_secs()
            );
        }
    };

    set_active_tier(tier);
    diagnostics.set_tier(tier_name(tier));
    let _ = go_tx.send(());

    let stop_flag = Arc::clone(&stop);
    let mut join_holder = Some(join);
    let stop_fn: AudioStopFn = Box::new(move || {
        stop_flag.store(true, Ordering::Relaxed);
        if let Some(j) = join_holder.take() {
            let _ = j.join();
        }
        set_active_tier(CaptureTier::Idle);
    });

    Ok(AudioCapture {
        stop: stop_fn,
        format,
        diagnostics,
        lookahead_samples,
    })
}

struct Session {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: EventHandle,
    mix: MixFormat,
    tier: CaptureTier,
    /// The endpoint tier's clock source; on the process tier only under the
    /// `audio_process_period_companion` experiment.
    companion: Option<SilenceCompanion>,
    /// Userspace tier only, when the endpoint is not at 48 kHz.
    resampler: Option<StereoResampler>,
    period_frames: u32,
    buffer_frames: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
        // companion stopped by its own Drop
    }
}

fn run(
    sink: AudioSink,
    diagnostics: Arc<AudioDiagnostics>,
    stop: Arc<AtomicBool>,
    ready_tx: SetupReporter<(AudioFormat, i32, CaptureTier)>,
    go_rx: mpsc::Receiver<()>,
) {
    let _com = match ComGuard::init_mta() {
        Ok(g) => g,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    let _mmcss = match MmcssGuard::register_pro_audio() {
        Ok(g) => Some(g),
        Err(e) => {
            crate::teprintln!(
                "audio: MMCSS 'Pro Audio' registration failed ({e:#}); continuing without it"
            );
            None
        }
    };
    let tuning = Tuning::read();
    let status = Arc::new(HostStatus::default());

    let enumerator = match create_enumerator() {
        Ok(e) => e,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let (dev_tx, dev_rx) = crossbeam_channel::unbounded::<DeviceEvent>();
    let _notifier = match NotifierRegistration::register(&enumerator, dev_tx) {
        Ok(n) => {
            status.notifier_ok.store(true, Ordering::Relaxed);
            Some(n)
        }
        Err(e) => {
            crate::teprintln!(
                "audio: endpoint notifications unavailable ({e:#}); on the endpoint tier a \
                 default-device change will not be followed, and capture keeps looping back the \
                 old device until it is invalidated"
            );
            None
        }
    };

    let mut encoder = match OpusEncoder::new(OpusEncoderConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    crate::tprintln!("{}", encoder.describe_state());

    let mut session = match unsafe { acquire(&enumerator, &tuning) } {
        Ok(s) => s,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    diagnostics
        .sample_rate
        .store(OUT_SAMPLE_RATE, Ordering::Relaxed);
    diagnostics
        .channels
        .store(OUT_CHANNELS as u32, Ordering::Relaxed);
    diagnostics
        .period_frames
        .store(session.period_frames, Ordering::Relaxed);
    diagnostics
        .buffer_frames
        .store(session.buffer_frames, Ordering::Relaxed);

    let _ = ready_tx.send(Ok((
        AudioFormat {
            sample_rate: OUT_SAMPLE_RATE,
            channels: OUT_CHANNELS,
        },
        encoder.lookahead_samples(),
        session.tier,
    )));
    // `start` publishes the tier and lets the thread go, or it timed out and dropped the
    // sender: then this capture never reached the hub and must not run
    if go_rx.recv().is_err() {
        crate::tprintln!("audio: capture start was abandoned by its caller; closing the session");
        return;
    }

    spawn_diagnostics_reporter(
        Arc::clone(&diagnostics),
        Arc::clone(&stop),
        Arc::clone(&status),
    );

    let mut accum: Vec<f32> = Vec::with_capacity(FRAME_INTERLEAVED * 16);
    // host-clock time of `accum[0]`, from the endpoint's own stamp; `None` while it is empty
    let mut accum_start_ns: Option<u64> = None;
    // prime the QPC-to-host offset off the real-time path: the first call samples both clocks
    // into a OnceLock, and the drain loop must not be the one to do that
    let _ = crate::windows_utils::streamer::clock::boot_ns_to_host_ns(1);
    let mut seq: u32 = 0;
    let mut pending_flags: u8 = FLAG_DISCONTINUITY; // first frame is a fresh timeline
    let mut last_wake_ns: Option<u64> = None;
    let mut starved_waits: u32 = 0;
    let mut recovery = Recovery::default();
    // a drain stopped at a discontinuity and left the packet in the engine: take it on the next
    // pass without waiting for another event
    let mut redrain = false;
    // the packet left behind carried a discontinuity, whatever its flags say when it is re-read
    let mut carry_discont = false;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // drain the whole burst: a device-change storm is N events and only the last one matters
        let mut device_changed = false;
        while let Ok(DeviceEvent::DefaultRenderChanged) = dev_rx.try_recv() {
            device_changed = true;
            diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
        }
        if device_changed {
            if device_change_needs_reacquire(session.tier, tuning.process_ignores_device_change) {
                if !recovery.is_pending() {
                    crate::tprintln!("audio: default render device changed; re-acquiring");
                }
                recovery.request(ReacquireReason::DeviceChanged, Instant::now());
            } else {
                crate::tprintln!(
                    "audio: default render device changed; process loopback is not tied to an \
                     endpoint, keeping the session"
                );
            }
        }

        if let Some(reason) = recovery.due(Instant::now()) {
            starved_waits = 0;
            last_wake_ns = None;
            redrain = false;
            match unsafe { reacquire(&enumerator, &mut session, &diagnostics, &tuning) } {
                Ok(()) => {
                    let failed = recovery.on_success(Instant::now());
                    if failed > 0 && should_log_attempt(failed) {
                        crate::tprintln!(
                            "audio: capture recovered from {} after {failed} failed or \
                             short-lived attempts",
                            reason.describe()
                        );
                    }
                    accum.clear();
                    accum_start_ns = None;
                    pending_flags |= FLAG_DISCONTINUITY;
                    carry_discont = false;
                }
                Err(e) => {
                    let (attempt, delay) = recovery.on_failure(Instant::now());
                    status.reacquire_failures.fetch_add(1, Ordering::Relaxed);
                    if should_log_attempt(attempt) {
                        crate::teprintln!(
                            "audio: re-acquire after {} failed (attempt {attempt}, next in {} \
                             ms): {e:#}",
                            reason.describe(),
                            delay.as_millis()
                        );
                    }
                }
            }
            continue;
        }
        if let Some(wait) = recovery.wait_remaining(Instant::now()) {
            // the current session is the broken one; leave it alone until the next attempt
            std::thread::sleep(wait.min(PENDING_POLL));
            continue;
        }

        if redrain {
            redrain = false;
        } else {
            // The companion is the endpoint's clock: if its pump dies the capture event stops
            // firing, and the diagnostic line keeps printing frozen counters that read as healthy.
            // Two independent detectors (companion liveness, starved waits): either can fire first.
            if session.event.wait(EVENT_WAIT_MS) {
                let now = host_now_ns();
                if let Some(prev) = last_wake_ns {
                    diagnostics.wake_interval.record_span(prev, now);
                }
                last_wake_ns = Some(now);
                starved_waits = 0;
            } else {
                starved_waits += 1;
            }
            let companion_dead = session.companion.as_ref().is_some_and(|c| !c.is_alive());
            if companion_dead || starved_waits >= STARVED_WAITS_BEFORE_REACQUIRE {
                if companion_dead {
                    crate::teprintln!(
                        "audio: silent render companion died; restarting the endpoint (capture \
                         has no clock without it)"
                    );
                    diagnostics
                        .companion_restarts
                        .fetch_add(1, Ordering::Relaxed);
                    recovery.request(ReacquireReason::CompanionDied, Instant::now());
                } else {
                    crate::teprintln!(
                        "audio: capture event unsignalled for {} ms; treating the endpoint as \
                         starved and re-acquiring",
                        starved_waits * EVENT_WAIT_MS
                    );
                    diagnostics
                        .event_starvations
                        .fetch_add(1, Ordering::Relaxed);
                    recovery.request(ReacquireReason::Starved, Instant::now());
                }
                continue;
            }
        }

        let drained = unsafe {
            drain(
                &mut session,
                &mut accum,
                &mut accum_start_ns,
                &mut pending_flags,
                &mut carry_discont,
                tuning.sample_limit,
                &diagnostics,
            )
        };
        match drained {
            Ok(Drained::Complete) => {}
            Ok(Drained::SplitAtDiscontinuity) => redrain = true,
            Err(err) => {
                recovery.request(reason_for(&err), Instant::now());
                // a session that fails again straight after a re-acquire is a run, not news:
                // thin the lines the same way as failed attempts (they are counted anyway)
                let loud = recovery.should_log_request();
                match &err {
                    DrainError::DeviceInvalidated => {
                        if loud {
                            crate::tprintln!("audio: device invalidated; re-acquiring");
                        }
                        diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
                    }
                    DrainError::Fatal(e) => {
                        status.capture_errors.fetch_add(1, Ordering::Relaxed);
                        if loud {
                            crate::teprintln!(
                                "audio: capture error: {e:#}; re-acquiring the capture session"
                            );
                        }
                    }
                }
                continue;
            }
        }

        let full = accum.len() / FRAME_INTERLEAVED;
        for i in 0..full {
            let start = i * FRAME_INTERLEAVED;
            let frame = &accum[start..start + FRAME_INTERLEAVED];
            let t0 = Instant::now();
            let encoded = match encoder.encode_float(frame) {
                Ok(pkt) => pkt,
                Err(e) => {
                    // counted, and printed by `audio-diag`: a persistent fault would otherwise
                    // format and log 200 lines a second on this thread
                    if status.encode_failures.fetch_add(1, Ordering::Relaxed) == 0 {
                        crate::teprintln!(
                            "audio: opus encode failed: {e:#} (further failures are counted in \
                             the audio diag line, not logged)"
                        );
                    }
                    continue;
                }
            };
            diagnostics.record_encode_ns(t0.elapsed().as_nanos() as u64);

            let mut flags = pending_flags;
            pending_flags = 0;
            if is_digital_silence(frame) {
                flags |= FLAG_SILENT;
            }
            let capture_ns = accum_start_ns
                .map(|start| start + i as u64 * FRAME_NS)
                .unwrap_or_else(host_now_ns);
            diagnostics
                .capture_to_packet
                .record_span(capture_ns, host_now_ns());
            let pkt = AudioPacket {
                seq,
                capture_ns,
                flags,
                data: Bytes::copy_from_slice(encoded),
            };
            sink.send(pkt);
            seq = seq.wrapping_add(1);
        }
        if full > 0 {
            accum.drain(0..full * FRAME_INTERLEAVED);
            accum_start_ns = if accum.is_empty() {
                None
            } else {
                accum_start_ns.map(|start| start + full as u64 * FRAME_NS)
            };
        }
    }

    crate::tprintln!("audio: loopback capture stopped");
}

/// Print the 2 s diagnostic from an ordinary thread.
///
/// `summary()` formats a string and `tprintln!` sends it to the log bus, neither of which
/// belongs on the MMCSS "Pro Audio" capture thread. `AudioDiagnostics` is all atomics and
/// lock-free histograms, so the reader needs nothing from the writer. The Windows-only fields
/// (tier, notifier state, failure counters) follow the shared summary.
fn spawn_diagnostics_reporter(
    diagnostics: Arc<AudioDiagnostics>,
    stop: Arc<AtomicBool>,
    status: Arc<HostStatus>,
) {
    let _ = std::thread::Builder::new()
        .name("audio-diag".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(DIAG_LOG_INTERVAL);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                crate::tprintln!(
                    "{} tier={} notify={} encode_failures={} reacquire_failures={} \
                     capture_errors={}",
                    diagnostics.summary(),
                    active_tier().as_str(),
                    if status.notifier_ok.load(Ordering::Relaxed) {
                        "ok"
                    } else {
                        "unavailable"
                    },
                    status.encode_failures.load(Ordering::Relaxed),
                    status.reacquire_failures.load(Ordering::Relaxed),
                    status.capture_errors.load(Ordering::Relaxed),
                );
            }
        });
}

enum DrainError {
    DeviceInvalidated,
    Fatal(anyhow::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Drained {
    /// The engine has no more packets.
    Complete,
    /// A discontinuity arrived while the accumulator held audio from before it. The packet was
    /// handed back to the engine (`ReleaseBuffer(0)`), so the caller encodes what it has and
    /// drains again at once; the post-gap audio then gets an anchor of its own.
    SplitAtDiscontinuity,
}

/// Nanoseconds of audio one interleaved stereo Opus frame represents (5 ms at 48 kHz).
const FRAME_NS: u64 = 5_000_000;

/// Map a `GetNextPacketSize` / `GetBuffer` / `ReleaseBuffer` failure.
fn drain_error(e: windows::core::Error) -> DrainError {
    if e.code().0 == AUDCLNT_E_DEVICE_INVALIDATED {
        DrainError::DeviceInvalidated
    } else {
        DrainError::Fatal(e.into())
    }
}

unsafe fn drain(
    session: &mut Session,
    accum: &mut Vec<f32>,
    accum_start_ns: &mut Option<u64>,
    pending_flags: &mut u8,
    carry_discont: &mut bool,
    sample_limit: f32,
    diagnostics: &AudioDiagnostics,
) -> Result<Drained, DrainError> {
    loop {
        let avail = session.capture.GetNextPacketSize().map_err(drain_error)?;
        if avail == 0 {
            return Ok(Drained::Complete);
        }

        let mut pdata: *mut u8 = std::ptr::null_mut();
        let mut nframes: u32 = 0;
        let mut flags: u32 = 0;
        let mut devpos: u64 = 0;
        let mut qpcpos: u64 = 0;
        session
            .capture
            .GetBuffer(
                &mut pdata,
                &mut nframes,
                &mut flags,
                Some(&mut devpos),
                Some(&mut qpcpos),
            )
            .map_err(drain_error)?;

        let is_silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
        let is_discont =
            flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 || *carry_discont;
        let ts_error = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;

        // Frames in the accumulator are stamped `anchor + i * FRAME_NS`, which holds only while
        // the audio is contiguous. At a discontinuity, pad the pre-gap audio to a frame boundary
        // and hand the packet back (releasing 0 frames leaves it for the next `GetBuffer`), so
        // the caller encodes first and the post-gap audio gets a fresh anchor.
        if is_discont && !accum.is_empty() {
            pad_to_frame(accum);
            session.capture.ReleaseBuffer(0).map_err(drain_error)?;
            *carry_discont = true;
            if let Some(rs) = session.resampler.as_mut() {
                rs.reset();
            }
            return Ok(Drained::SplitAtDiscontinuity);
        }
        *carry_discont = false;

        // Anchor to the endpoint's own stamp, not to when this thread drained, so the drain
        // interval, encode time and scheduling delay stay out of the client's A/V sync. On a
        // loopback stream `qpcpos` is the render endpoint's play cursor, which the mix we are
        // handed has not reached yet: 1596/1601 packets were stamped ahead of `host_now_ns()` on
        // an idle host (mean 14.7 ms, up to 20.5 ms, moving with load). The client sizes its
        // jitter buffer from `enqueue - capture_ns`, so that would be ~15 ms of surplus latency.
        // So clamp to `now - read_duration`, the latest the first frame of this read can be
        // (`nframes` at the capture rate); a genuinely past stamp is kept.
        if accum.is_empty() {
            let now = host_now_ns();
            let read_ns = nframes as u64 * 1_000_000_000 / session.mix.sample_rate.max(1) as u64;
            let latest_possible = now.saturating_sub(read_ns);
            let qpc_host_ns = if ts_error || qpcpos == 0 {
                0
            } else {
                crate::windows_utils::streamer::clock::boot_ns_to_host_ns(qpcpos * 100)
            };
            let anchor = if qpc_host_ns == 0 || qpc_host_ns > latest_possible {
                if qpc_host_ns > latest_possible {
                    diagnostics
                        .capture_ts_clamped
                        .fetch_add(1, Ordering::Relaxed);
                }
                latest_possible
            } else {
                qpc_host_ns
            };
            *accum_start_ns = Some(anchor);
        }

        if is_silent {
            diagnostics.silent_count.fetch_add(1, Ordering::Relaxed);
            match session.resampler.as_mut() {
                Some(rs) => rs.push_silence(nframes as usize, accum),
                None => {
                    let silence_samples = nframes as usize * OUT_CHANNELS as usize;
                    accum.resize(accum.len() + silence_samples, 0.0f32);
                }
            }
        } else {
            let bytes = nframes as usize * session.mix.bytes_per_frame();
            let src = std::slice::from_raw_parts(pdata, bytes);
            convert_into(
                src,
                nframes as usize,
                &session.mix,
                sample_limit,
                session.resampler.as_mut(),
                accum,
            );
        }

        if is_discont {
            diagnostics
                .discontinuity_count
                .fetch_add(1, Ordering::Relaxed);
            *pending_flags |= FLAG_DISCONTINUITY;
        }

        session
            .capture
            .ReleaseBuffer(nframes)
            .map_err(drain_error)?;
    }
}

/// Acquire a capture session, fastest workable tier first.
///
/// Process loopback first: click to encoded packet measured a median 59.4 ms against 80.6 ms for
/// endpoint loopback, which taps the mix after the endpoint's own buffering. The endpoint tier
/// takes over when process loopback cannot be activated (Windows before 10 2004, a refused
/// activation, or one that failed within `process_loopback::TRANSIENT_RETRY_AFTER`), and
/// `SE_ENDPOINT_LOOPBACK=1` forces it for a measurement.
unsafe fn acquire(enumerator: &IMMDeviceEnumerator, tuning: &Tuning) -> Result<Session> {
    let try_process = PREFER_PROCESS_LOOPBACK.load(Ordering::Relaxed)
        || (!tuning.endpoint_forced && process_loopback::worth_trying());
    if try_process {
        match acquire_process_loopback(tuning) {
            Ok(s) => return Ok(s),
            Err(e) => {
                crate::teprintln!("audio: process loopback failed ({e:#}); using endpoint loopback")
            }
        }
    }
    match acquire_endpoint(enumerator) {
        Ok(s) => Ok(s),
        Err(endpoint_err) => {
            if try_process || !process_loopback::worth_trying() {
                return Err(endpoint_err);
            }
            crate::teprintln!(
                "audio: endpoint loopback failed ({endpoint_err:#}); falling back to process \
                 loopback"
            );
            acquire_process_loopback(tuning).map_err(|e| {
                endpoint_err.context(format!("process loopback fallback also failed: {e:#}"))
            })
        }
    }
}

/// The process tier: capture every process except this one, off the synthetic loopback device.
///
/// No silent render companion: there is no endpoint to park, and measured, the event fires with
/// none. The starvation detector in the capture loop still covers this tier.
///
/// A companion could still buy a smaller engine period, which this tier never asks for. Whether
/// the synthetic device follows a period another stream lowered is unmeasured, so
/// `audio_process_period_companion` starts one beside this tier, kept only if it was granted a
/// period below the default.
unsafe fn acquire_process_loopback(tuning: &Tuning) -> Result<Session> {
    // before the capture stream, so the engine already runs at the lowered period when it opens
    let companion = if tuning.process_period_companion {
        match SilenceCompanion::start() {
            Ok(c) if c.granted_period_frames() > 0 => {
                crate::tprintln!(
                    "audio: [audio_process_period_companion] companion holds the endpoint at \
                     {} frames beside process loopback; compare wake= in the audio diag line",
                    c.granted_period_frames()
                );
                Some(c)
            }
            Ok(_) => {
                crate::tprintln!(
                    "audio: [audio_process_period_companion] the endpoint offers no period below \
                     its default; no companion"
                );
                None
            }
            Err(e) => {
                crate::teprintln!(
                    "audio: [audio_process_period_companion] companion failed ({e:#}); \
                     continuing without it"
                );
                None
            }
        }
    } else {
        None
    };

    let client = process_loopback::activate_recorded()?;
    let want = process_loopback::target_format();
    let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            flags,
            0,
            0,
            &want as *const WAVEFORMATEX,
            None,
        )
        .context("Initialize process loopback (48k stereo float)")?;

    let started = (|| -> Result<(EventHandle, IAudioCaptureClient)> {
        let event = EventHandle::new_auto_reset()?;
        client
            .SetEventHandle(event.raw())
            .context("SetEventHandle (process loopback)")?;
        let capture: IAudioCaptureClient = client
            .GetService()
            .context("GetService(IAudioCaptureClient, process loopback)")?;
        Ok((event, capture))
    })();
    let (event, capture) = match started {
        Ok(v) => v,
        Err(e) => {
            let _ = client.Stop();
            let _ = client.Reset();
            return Err(e);
        }
    };

    let buffer_frames = client.GetBufferSize().unwrap_or(0);
    // `GetDevicePeriod` returns 0 on a synthetic device with no hardware clock, and the measured
    // delivery cadence matches the buffer (480 frames = 10 ms, one period per buffer)
    let period_frames = engine::device_period_frames(&client, OUT_SAMPLE_RATE)
        .map(|(default, _min)| default)
        .filter(|&p| p > 0)
        .unwrap_or(buffer_frames);

    if let Err(e) = client.Start() {
        let _ = client.Reset();
        return Err(anyhow::Error::from(e).context("Start process loopback capture"));
    }

    crate::tprintln!(
        "audio: process loopback acquired (48000Hz x2ch F32, every process except this one, \
         period {period_frames} frames, buffer {buffer_frames} frames)"
    );

    Ok(Session {
        client,
        capture,
        event,
        mix: MixFormat {
            sample_rate: OUT_SAMPLE_RATE,
            channels: OUT_CHANNELS,
            kind: SampleKind::F32,
            block_align: 8,
            channel_mask: 0,
        },
        tier: CaptureTier::ProcessLoopback,
        companion,
        resampler: None,
        period_frames,
        buffer_frames,
    })
}

/// The endpoint tier: the default render endpoint in loopback mode, with a silent companion
/// for a clock.
unsafe fn acquire_endpoint(enumerator: &IMMDeviceEnumerator) -> Result<Session> {
    // companion before loopback, so the endpoint is already running
    let companion = SilenceCompanion::start().context("starting silent render companion")?;

    let device = default_render_endpoint(enumerator)?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate IAudioClient (loopback)")?;

    // RAII from here: every `?` below would otherwise leak this `CoTaskMemAlloc`'d format, and
    // `acquire` sits on a retry loop
    let pwfx = MixFormatPtr::from_raw(client.GetMixFormat().context("GetMixFormat")?);
    let header = std::ptr::read_unaligned(pwfx.as_ptr());
    let (source_rate, source_tag) = (header.nSamplesPerSec, header.wFormatTag);
    // a format this host cannot read is not a reason to give up: the engine can still be asked
    // to convert it, and only the userspace fallback needs to understand the source
    let mix = match parse_mix_format(pwfx.as_ptr()) {
        Ok(m) => Some(m),
        Err(e) => {
            crate::teprintln!(
                "audio: endpoint mix format not understood ({e:#}); asking the engine to convert"
            );
            None
        }
    };

    if let Some(periods) = engine::query_engine_periods(&client, pwfx.as_ptr()) {
        crate::tprintln!(
            "audio: endpoint engine periods {}",
            periods.describe(source_rate)
        );
    }

    let base_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let source = match mix {
        Some(m) => format!("{}Hz x{}ch {:?}", m.sample_rate, m.channels, m.kind),
        None => format!("{source_rate}Hz, format tag 0x{source_tag:04X}"),
    };

    let (effective_mix, tier) = match mix {
        Some(m)
            if m.sample_rate == OUT_SAMPLE_RATE
                && m.channels == OUT_CHANNELS
                && matches!(m.kind, SampleKind::F32) =>
        {
            // fast path: 48 kHz float32 stereo, take the mix format verbatim
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    base_flags,
                    0,
                    0,
                    pwfx.as_ptr(),
                    None,
                )
                .context("Initialize loopback (fast path)")?;
            (m, CaptureTier::EndpointNative)
        }
        _ => {
            // non-48k / non-stereo / integer / unparsed: ask the engine to convert to 48 kHz
            // float stereo (AUTOCONVERTPCM)
            let want = target_wave_format();
            let flags = base_flags | STREAMFLAGS_AUTOCONVERTPCM | STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let converted = client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                0,
                0,
                &want as *const WAVEFORMATEX,
                None,
            );
            if let Err(e) = converted {
                // The engine refused to convert: take the endpoint's own format and convert in
                // userspace instead of failing the feature. This is the only path that reaches
                // `convert_into`'s non-fast branch, `downmix_frame` (BS.775), `read_sample`'s
                // integer arms and the `StereoResampler`.
                let Some(m) = mix else {
                    return Err(anyhow::Error::from(e).context(format!(
                        "the engine refused AUTOCONVERTPCM and the endpoint's own format \
                         ({source}) is not one this host can convert itself"
                    )));
                };
                crate::teprintln!(
                    "audio: engine refused AUTOCONVERTPCM ({e}); capturing the endpoint's own \
                     {source} and converting in userspace{}",
                    if m.sample_rate != OUT_SAMPLE_RATE {
                        " (with a linear resampler to 48 kHz)"
                    } else {
                        ""
                    }
                );
                // a refused `Initialize` leaves this client in an unclear state; the retry gets
                // a fresh one
                let retry: IAudioClient = device
                    .Activate(CLSCTX_ALL, None)
                    .context("re-Activate IAudioClient after a refused AUTOCONVERTPCM")?;
                let retry_pwfx =
                    MixFormatPtr::from_raw(retry.GetMixFormat().context("GetMixFormat (retry)")?);
                retry
                    .Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        base_flags,
                        0,
                        0,
                        retry_pwfx.as_ptr(),
                        None,
                    )
                    .context("Initialize loopback with the endpoint's own format")?;
                return finish_endpoint_session(
                    retry,
                    m,
                    CaptureTier::EndpointUserspace,
                    companion,
                );
            }
            crate::tprintln!(
                "audio: source mix format {source}; using engine AUTOCONVERTPCM to 48k stereo \
                 float"
            );
            (
                MixFormat {
                    sample_rate: OUT_SAMPLE_RATE,
                    channels: OUT_CHANNELS,
                    kind: SampleKind::F32,
                    block_align: 8,
                    channel_mask: 0,
                },
                CaptureTier::EndpointAutoconvert,
            )
        }
    };

    finish_endpoint_session(client, effective_mix, tier, companion)
}

/// Everything after a successful `Initialize` on the endpoint tier: event, service, sizes,
/// `Start`.
///
/// Every call between `Initialize` and `Start` is fallible, and dropping an initialized client
/// without stopping it leaves the engine holding a stream we no longer own. `Session::drop`
/// cannot help, because no `Session` exists yet.
unsafe fn finish_endpoint_session(
    client: IAudioClient,
    effective_mix: MixFormat,
    tier: CaptureTier,
    companion: SilenceCompanion,
) -> Result<Session> {
    let started = (|| -> Result<(EventHandle, IAudioCaptureClient)> {
        let event = EventHandle::new_auto_reset()?;
        client
            .SetEventHandle(event.raw())
            .context("SetEventHandle")?;
        let capture: IAudioCaptureClient = client
            .GetService()
            .context("GetService(IAudioCaptureClient)")?;
        Ok((event, capture))
    })();
    let (event, capture) = match started {
        Ok(v) => v,
        Err(e) => {
            let _ = client.Stop();
            let _ = client.Reset();
            return Err(e);
        }
    };

    let buffer_frames = match client.GetBufferSize() {
        Ok(n) => n,
        Err(e) => {
            crate::teprintln!("audio: GetBufferSize failed ({e}); reporting 0");
            0
        }
    };
    // `GetDevicePeriod` is the period; `GetBufferSize` is the whole engine buffer, several
    // periods deep. A smaller period the companion negotiated is what this stream is clocked
    // at, and `GetDevicePeriod` still reports the device default.
    let period_frames = match companion.granted_period_frames() {
        0 => engine::device_period_frames(&client, effective_mix.sample_rate)
            .map(|(default, _min)| default)
            .unwrap_or(0),
        granted => granted,
    };

    if let Err(e) = client.Start() {
        let _ = client.Reset();
        return Err(anyhow::Error::from(e).context("Start loopback capture"));
    }

    crate::tprintln!(
        "audio: loopback acquired ({}Hz x{}ch {:?}, engine period {} frames, engine buffer {} frames, tier {})",
        effective_mix.sample_rate,
        effective_mix.channels,
        effective_mix.kind,
        period_frames,
        buffer_frames,
        tier.as_str()
    );

    // only the userspace tier can arrive here at a rate other than the encoder's
    let resampler = (effective_mix.sample_rate != OUT_SAMPLE_RATE)
        .then(|| StereoResampler::new(effective_mix.sample_rate, OUT_SAMPLE_RATE));

    Ok(Session {
        client,
        capture,
        event,
        mix: effective_mix,
        tier,
        companion: Some(companion),
        resampler,
        period_frames,
        buffer_frames,
    })
}

/// Tear down the current session and acquire a fresh one (device change / invalidation /
/// a dead companion / starvation / a capture error).
///
/// The new session is built *before* the old one is dropped, to keep the endpoint clocked
/// across the swap. Two silent render companions briefly coexist, each with its own COM MTA
/// thread, which is why the caller coalesces a device-change burst into one call.
unsafe fn reacquire(
    enumerator: &IMMDeviceEnumerator,
    session: &mut Session,
    diagnostics: &AudioDiagnostics,
    tuning: &Tuning,
) -> Result<()> {
    let fresh = acquire(enumerator, tuning)?;
    let old = std::mem::replace(session, fresh);
    drop(old); // stops old client + old companion
    set_active_tier(session.tier);
    diagnostics.set_tier(tier_name(session.tier));
    diagnostics
        .period_frames
        .store(session.period_frames, Ordering::Relaxed);
    diagnostics
        .buffer_frames
        .store(session.buffer_frames, Ordering::Relaxed);
    Ok(())
}

/// A 48 kHz float32 stereo `WAVEFORMATEX` to request from the engine when the source isn't
/// already in that format.
fn target_wave_format() -> WAVEFORMATEX {
    let channels: u16 = OUT_CHANNELS;
    let bits: u16 = 32;
    let block_align = channels * (bits / 8);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: OUT_SAMPLE_RATE,
        nAvgBytesPerSec: OUT_SAMPLE_RATE * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tier_reports_its_own_name() {
        for tier in [
            CaptureTier::Idle,
            CaptureTier::ProcessLoopback,
            CaptureTier::EndpointNative,
            CaptureTier::EndpointAutoconvert,
            CaptureTier::EndpointUserspace,
        ] {
            assert_eq!(*tier_name(tier), tier.as_str());
        }
    }

    #[test]
    fn a_capture_error_asks_for_a_reacquire_instead_of_ending_capture() {
        // a `break` on `DrainError::Fatal` would end capture while the hub keeps its sender:
        // every session silent for the life of the host
        let err = DrainError::Fatal(anyhow::anyhow!("AUDCLNT_E_SERVICE_NOT_RUNNING"));
        assert_eq!(reason_for(&err), ReacquireReason::CaptureError);
        let mut r = Recovery::default();
        r.request(reason_for(&err), Instant::now());
        assert_eq!(r.due(Instant::now()), Some(ReacquireReason::CaptureError));
    }

    #[test]
    fn a_session_that_fails_straight_after_a_reacquire_waits_out_the_backoff() {
        // if `on_success` forgot the run, open-then-fail would loop with no delay at all
        let t0 = Instant::now();
        let mut r = Recovery::default();
        r.request(ReacquireReason::CaptureError, t0);
        assert_eq!(r.due(t0), Some(ReacquireReason::CaptureError));
        assert_eq!(r.on_success(t0), 0);

        // the fresh session fails at once: not due immediately
        r.request(ReacquireReason::CaptureError, t0);
        assert_eq!(r.due(t0), None);
        assert_eq!(r.wait_remaining(t0), Some(REACQUIRE_BACKOFF_FIRST));
        let mut now = t0 + REACQUIRE_BACKOFF_FIRST;
        assert_eq!(r.due(now), Some(ReacquireReason::CaptureError));

        // open-then-fail over and over climbs to the cap: never more than one attempt per 5 s
        let mut last_wait = Duration::ZERO;
        for _ in 0..20 {
            assert_eq!(r.due(now), Some(ReacquireReason::CaptureError));
            r.on_success(now);
            r.request(ReacquireReason::CaptureError, now);
            let wait = r
                .wait_remaining(now)
                .expect("a backoff after a short-lived session");
            assert!(wait >= last_wait && wait <= REACQUIRE_BACKOFF_MAX);
            last_wait = wait;
            now += wait;
        }
        assert_eq!(last_wait, REACQUIRE_BACKOFF_MAX);
        assert_eq!(r.should_log_request(), should_log_attempt(r.failures));
    }

    #[test]
    fn a_healthy_session_forgets_the_run_and_a_device_change_is_followed_at_once() {
        let t0 = Instant::now();
        let mut r = Recovery::default();
        r.request(ReacquireReason::Starved, t0);
        r.on_failure(t0);
        r.on_failure(t0);
        assert_eq!(r.on_success(t0), 2);
        // a device change right after a recovery is not evidence against the session
        r.request(ReacquireReason::DeviceChanged, t0);
        assert_eq!(r.due(t0), Some(ReacquireReason::DeviceChanged));
        r.on_success(t0);
        // a session that ran for HEALTHY_SESSION ends the run
        let later = t0 + HEALTHY_SESSION;
        r.request(ReacquireReason::CaptureError, later);
        assert_eq!(r.due(later), Some(ReacquireReason::CaptureError));
        assert_eq!(r.on_success(later), 0);
        assert!(r.should_log_request());
    }

    #[test]
    fn an_abandoned_setup_is_stuck_until_its_thread_returns() {
        // a retry against a wedged service must not leak one thread per try
        static STUCK: StuckSetups = StuckSetups::new();
        let (tx, _rx) = mpsc::channel::<Result<u32>>();
        let (reporter, token) = SetupReporter::new(tx, &STUCK);
        STUCK.abandon(&token);
        assert_eq!(STUCK.outstanding(), 1);
        let _ = reporter.send(Ok(1));
        assert_eq!(STUCK.outstanding(), 0);
        drop(reporter);
        assert_eq!(
            STUCK.outstanding(),
            0,
            "finishing twice must not go below zero"
        );

        // the thread got out first: abandoning it afterwards does not count it
        let (tx, _rx) = mpsc::channel::<Result<u32>>();
        let (reporter, token) = SetupReporter::new(tx, &STUCK);
        drop(reporter);
        STUCK.abandon(&token);
        assert_eq!(STUCK.outstanding(), 0);
    }

    #[test]
    fn a_failed_device_change_reacquire_stays_pending_and_is_retried() {
        let t0 = Instant::now();
        let mut r = Recovery::default();
        r.request(ReacquireReason::DeviceChanged, t0);
        assert_eq!(r.due(t0), Some(ReacquireReason::DeviceChanged));
        let (attempt, delay) = r.on_failure(t0);
        assert_eq!((attempt, delay), (1, REACQUIRE_BACKOFF_FIRST));
        assert!(r.is_pending());
        assert_eq!(r.due(t0), None, "not before the backoff");
        assert_eq!(r.wait_remaining(t0), Some(REACQUIRE_BACKOFF_FIRST));
        assert_eq!(
            r.due(t0 + REACQUIRE_BACKOFF_FIRST),
            Some(ReacquireReason::DeviceChanged)
        );
        // a later event of another kind does not reset the backoff or the reason
        r.request(ReacquireReason::Starved, t0);
        assert_eq!(
            r.due(t0 + REACQUIRE_BACKOFF_FIRST),
            Some(ReacquireReason::DeviceChanged)
        );
        assert_eq!(r.on_success(t0), 1);
        assert!(!r.is_pending());
        assert_eq!(r.due(t0), None);
        assert_eq!(r.wait_remaining(t0), None);
    }

    #[test]
    fn reacquire_backoff_doubles_and_caps() {
        assert_eq!(reacquire_backoff(1), Duration::from_millis(200));
        assert_eq!(reacquire_backoff(2), Duration::from_millis(400));
        assert_eq!(reacquire_backoff(3), Duration::from_millis(800));
        assert_eq!(reacquire_backoff(5), Duration::from_millis(3200));
        assert_eq!(reacquire_backoff(6), REACQUIRE_BACKOFF_MAX);
        assert_eq!(reacquire_backoff(u32::MAX), REACQUIRE_BACKOFF_MAX);
    }

    #[test]
    fn failure_logging_thins_out() {
        let logged: Vec<u32> = (1..=40).filter(|&n| should_log_attempt(n)).collect();
        assert_eq!(logged, vec![1, 2, 4, 8, 16, 32]);
    }

    #[test]
    fn only_the_endpoint_tier_must_follow_a_device_change_when_the_switch_is_on() {
        // behind `audio_process_ignores_device_change`
        for tier in [
            CaptureTier::EndpointNative,
            CaptureTier::EndpointAutoconvert,
            CaptureTier::EndpointUserspace,
        ] {
            assert!(device_change_needs_reacquire(tier, true));
            assert!(device_change_needs_reacquire(tier, false));
        }
        assert!(!device_change_needs_reacquire(
            CaptureTier::ProcessLoopback,
            true
        ));
        // switch off: the process tier re-acquires too
        assert!(device_change_needs_reacquire(
            CaptureTier::ProcessLoopback,
            false
        ));
    }

    #[test]
    fn tiers_round_trip_through_the_atomic() {
        for tier in [
            CaptureTier::Idle,
            CaptureTier::ProcessLoopback,
            CaptureTier::EndpointNative,
            CaptureTier::EndpointAutoconvert,
            CaptureTier::EndpointUserspace,
        ] {
            assert_eq!(CaptureTier::from_u8(tier as u8), tier);
        }
        assert!(!CaptureTier::ProcessLoopback.is_endpoint());
        assert!(!CaptureTier::Idle.is_endpoint());
        assert_eq!(CaptureTier::from_u8(200), CaptureTier::Idle);
    }

    #[test]
    fn setup_wait_is_bounded() {
        // a capture thread that never answers must not hold the caller
        let (_tx, rx) = mpsc::channel::<Result<u32>>();
        let t0 = Instant::now();
        assert!(matches!(
            await_ready(&rx, Duration::from_millis(20)),
            Ready::TimedOut
        ));
        assert!(t0.elapsed() < Duration::from_secs(5));

        let (tx, rx) = mpsc::channel::<Result<u32>>();
        drop(tx);
        assert!(matches!(
            await_ready(&rx, Duration::from_secs(5)),
            Ready::Exited
        ));

        let (tx, rx) = mpsc::channel::<Result<u32>>();
        tx.send(Ok(7)).unwrap();
        assert!(matches!(
            await_ready(&rx, Duration::from_secs(5)),
            Ready::Started(7)
        ));
    }

    #[test]
    fn silent_flag_is_decided_per_frame_from_the_samples() {
        // `AUDCLNT_BUFFERFLAGS_SILENT` covers only the first frame of a silent buffer and is
        // never set on the process tier
        let zeros = vec![0.0f32; FRAME_INTERLEAVED];
        assert!(is_digital_silence(&zeros));
        let mut one = zeros.clone();
        one[FRAME_INTERLEAVED - 1] = 1e-9;
        assert!(!is_digital_silence(&one));
        // -0.0 is digital silence too
        assert!(is_digital_silence(&vec![-0.0f32; FRAME_INTERLEAVED]));
    }

    #[test]
    fn padding_finishes_the_partial_frame_only() {
        // the pre-gap remainder is completed on a frame boundary before the post-gap packet
        // gets its own anchor
        let mut accum = vec![0.5f32; FRAME_INTERLEAVED + 10];
        pad_to_frame(&mut accum);
        assert_eq!(accum.len(), 2 * FRAME_INTERLEAVED);
        assert_eq!(accum[FRAME_INTERLEAVED + 9], 0.5);
        assert_eq!(accum[FRAME_INTERLEAVED + 10], 0.0);
        let mut exact = vec![0.5f32; FRAME_INTERLEAVED];
        pad_to_frame(&mut exact);
        assert_eq!(exact.len(), FRAME_INTERLEAVED);
        let mut empty: Vec<f32> = Vec::new();
        pad_to_frame(&mut empty);
        assert!(empty.is_empty());
    }
}
