use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

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
use super::format::{convert_to_stereo_f32, parse_mix_format, MixFormat, SampleKind};
use super::guards::{ComGuard, EventHandle, MixFormatPtr, MmcssGuard};
use super::process_loopback;
use super::silence::SilenceCompanion;
use crate::streamer::audio::encoder::{OpusEncoder, OpusEncoderConfig, FRAME_SAMPLES};
use crate::streamer::audio::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioSink, AudioStopFn,
    FLAG_DISCONTINUITY, FLAG_SILENT,
};

/// `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`: "A channel matrixer and a sample rate converter are
/// inserted as necessary to convert between the uncompressed format supplied to
/// IAudioClient::Initialize and the audio engine mix format."
const STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
/// `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY`. **The name reads backwards from the meaning.**
/// It does not select the default converter; Microsoft documents it as selecting "a sample
/// rate converter with *better* quality than the default conversion but with a *higher
/// performance cost* ... This should be used if the audio is ultimately intended to be heard
/// by humans." It is the right choice here for that reason, but it is a filter with group
/// delay, and it is only ever reached on a non-48 kHz endpoint.
const STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
// `AUDCLNT_E_DEVICE_INVALIDATED` (`AUDCLNT_ERR(0x004)`)
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;

const OUT_SAMPLE_RATE: u32 = 48000;
const OUT_CHANNELS: u16 = 2;
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * OUT_CHANNELS as usize;
const DIAG_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
/// How long each `WaitForSingleObject` on the capture event may block.
const EVENT_WAIT_MS: u32 = 100;
/// Consecutive unsignalled waits before the endpoint is treated as clockless and re-acquired.
/// 20 x 100 ms = 2 s of nothing at all, against a 10 ms nominal period.
const STARVED_WAITS_BEFORE_REACQUIRE: u32 = 20;

/// Try the process-loopback tier first instead of using it only as a fallback.
///
/// Set by `ScreenExtend probe --audio --process-loopback` and by nothing else. Tier ordering
/// is supposed to be decided by measurement, and on a machine where tier 1 works there is
/// otherwise no way to take tier 2's numbers at all.
pub static PREFER_PROCESS_LOOPBACK: AtomicBool = AtomicBool::new(false);

pub fn start(sink: AudioSink) -> Result<AudioCapture> {
    let diagnostics = Arc::new(AudioDiagnostics::default());
    let stop = Arc::new(AtomicBool::new(false));

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(AudioFormat, i32)>>();

    let diag_thread = Arc::clone(&diagnostics);
    let stop_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("audio-loopback".to_string())
        .spawn(move || run(sink, diag_thread, stop_thread, ready_tx))
        .context("spawning audio loopback thread")?;

    let (format, lookahead_samples) = match ready_rx.recv() {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = join.join();
            return Err(e);
        }
        Err(_) => {
            let _ = join.join();
            bail!("audio loopback thread exited during setup");
        }
    };

    let stop_flag = Arc::clone(&stop);
    let mut join_holder = Some(join);
    let stop_fn: AudioStopFn = Box::new(move || {
        stop_flag.store(true, Ordering::Relaxed);
        if let Some(j) = join_holder.take() {
            let _ = j.join();
        }
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
    /// Tier 1 only: the endpoint's clock source. Process loopback has no endpoint to park.
    companion: Option<SilenceCompanion>,
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
    ready_tx: std::sync::mpsc::Sender<Result<(AudioFormat, i32)>>,
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

    let enumerator = match create_enumerator() {
        Ok(e) => e,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let (dev_tx, dev_rx) = crossbeam_channel::unbounded::<DeviceEvent>();
    let _notifier = NotifierRegistration::register(&enumerator, dev_tx).ok();

    let mut encoder = match OpusEncoder::new(OpusEncoderConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };
    crate::tprintln!("{}", encoder.describe_state());

    let mut session = match unsafe { acquire(&enumerator) } {
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
    )));

    spawn_diagnostics_reporter(Arc::clone(&diagnostics), Arc::clone(&stop));

    let mut accum: Vec<f32> = Vec::with_capacity(FRAME_INTERLEAVED * 16);
    // Host-clock time of `accum[0]`, taken from the endpoint hardware rather than from
    // whenever this thread got round to draining. `None` while the accumulator is empty.
    let mut accum_start_ns: Option<u64> = None;
    // Prime the QPC-to-host offset off the real-time path: the first call samples both clocks
    // and stores them in a OnceLock, and the drain loop must not be the one to do that.
    let _ = crate::windows_utils::streamer::clock::boot_ns_to_host_ns(1);
    let mut seq: u32 = 0;
    let mut pending_flags: u8 = FLAG_DISCONTINUITY; // first frame is a fresh timeline
    let mut last_wake_ns: Option<u64> = None;
    let mut starved_waits: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Drain the whole burst: a device-change storm used to queue N events and cost N full
        // re-acquisitions, one per loop iteration, when only the last one matters.
        let mut device_changed = false;
        while let Ok(DeviceEvent::DefaultRenderChanged) = dev_rx.try_recv() {
            device_changed = true;
            diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
        }
        if device_changed {
            crate::tprintln!("audio: default render device changed; re-acquiring");
            match unsafe { reacquire(&enumerator, &mut session, &diagnostics) } {
                Ok(()) => {
                    accum.clear();
                    accum_start_ns = None;
                    pending_flags |= FLAG_DISCONTINUITY;
                    starved_waits = 0;
                }
                Err(e) => {
                    crate::teprintln!("audio: re-acquire after device change failed: {e:#}");
                    // brief backoff before retrying to avoid a hot loop
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }
            continue;
        }

        // The companion is the endpoint's clock. If its pump died — three error paths that
        // used to `break` in silence — the capture event stops firing and nothing here would
        // ever notice: the diagnostic line keeps printing with frozen counters, which reads as
        // healthy. Two independent detectors, because either can fire first.
        let signalled = session.event.wait(EVENT_WAIT_MS);
        if signalled {
            let now = host_now_ns();
            if let Some(prev) = last_wake_ns {
                diagnostics.wake_interval.record_span(prev, now);
            }
            last_wake_ns = Some(now);
            starved_waits = 0;
        } else {
            starved_waits += 1;
        }
        // Tier 2 has no companion, so it cannot be the reason capture stalled.
        let companion_dead = session.companion.as_ref().is_some_and(|c| !c.is_alive());
        if companion_dead || starved_waits >= STARVED_WAITS_BEFORE_REACQUIRE {
            if companion_dead {
                crate::teprintln!(
                    "audio: silent render companion died; restarting the endpoint (capture has \
                     no clock without it)"
                );
                diagnostics
                    .companion_restarts
                    .fetch_add(1, Ordering::Relaxed);
            } else {
                crate::teprintln!(
                    "audio: capture event unsignalled for {} ms; treating the endpoint as \
                     starved and re-acquiring",
                    starved_waits * EVENT_WAIT_MS
                );
                diagnostics
                    .event_starvations
                    .fetch_add(1, Ordering::Relaxed);
            }
            starved_waits = 0;
            last_wake_ns = None;
            match unsafe { reacquire(&enumerator, &mut session, &diagnostics) } {
                Ok(()) => {
                    accum.clear();
                    accum_start_ns = None;
                    pending_flags |= FLAG_DISCONTINUITY;
                }
                Err(e) => {
                    crate::teprintln!("audio: re-acquire after starvation failed: {e:#}");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
            continue;
        }

        match unsafe {
            drain(
                &session,
                &mut accum,
                &mut accum_start_ns,
                &mut pending_flags,
                &diagnostics,
            )
        } {
            Ok(()) => {}
            Err(DrainError::DeviceInvalidated) => {
                crate::tprintln!("audio: device invalidated; re-acquiring");
                diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
                last_wake_ns = None;
                match unsafe { reacquire(&enumerator, &mut session, &diagnostics) } {
                    Ok(()) => {
                        accum.clear();
                        accum_start_ns = None;
                        pending_flags |= FLAG_DISCONTINUITY;
                    }
                    Err(e) => {
                        crate::teprintln!("audio: re-acquire failed: {e:#}");
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                }
                continue;
            }
            Err(DrainError::Fatal(e)) => {
                crate::teprintln!("audio: fatal capture error: {e:#}; stopping capture");
                break;
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
                    crate::teprintln!("audio: opus encode failed: {e:#}");
                    continue;
                }
            };
            diagnostics.record_encode_ns(t0.elapsed().as_nanos() as u64);

            let flags = pending_flags;
            pending_flags = 0;
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
/// `summary()` builds a `format!` string and `tprintln!` takes the logbus mutex and emits a
/// Tauri IPC event. Doing that on the MMCSS "Pro Audio" capture thread was a periodic
/// priority inversion on the one thread whose scheduling guarantee the whole feature depends
/// on. `AudioDiagnostics` is all atomics and lock-free histograms, so the reader needs nothing
/// from the writer.
fn spawn_diagnostics_reporter(diagnostics: Arc<AudioDiagnostics>, stop: Arc<AtomicBool>) {
    let _ = std::thread::Builder::new()
        .name("audio-diag".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(DIAG_LOG_INTERVAL);
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                crate::tprintln!("{}", diagnostics.summary());
            }
        });
}

enum DrainError {
    DeviceInvalidated,
    Fatal(anyhow::Error),
}

/// Nanoseconds of audio one interleaved stereo Opus frame represents (5 ms at 48 kHz).
const FRAME_NS: u64 = 5_000_000;

unsafe fn drain(
    session: &Session,
    accum: &mut Vec<f32>,
    accum_start_ns: &mut Option<u64>,
    pending_flags: &mut u8,
    diagnostics: &AudioDiagnostics,
) -> Result<(), DrainError> {
    loop {
        let avail = match session.capture.GetNextPacketSize() {
            Ok(n) => n,
            Err(e) if e.code().0 == AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(DrainError::DeviceInvalidated)
            }
            Err(e) => return Err(DrainError::Fatal(e.into())),
        };
        if avail == 0 {
            return Ok(());
        }

        let mut pdata: *mut u8 = std::ptr::null_mut();
        let mut nframes: u32 = 0;
        let mut flags: u32 = 0;
        let mut devpos: u64 = 0;
        let mut qpcpos: u64 = 0;
        match session.capture.GetBuffer(
            &mut pdata,
            &mut nframes,
            &mut flags,
            Some(&mut devpos),
            Some(&mut qpcpos),
        ) {
            Ok(()) => {}
            Err(e) if e.code().0 == AUDCLNT_E_DEVICE_INVALIDATED => {
                return Err(DrainError::DeviceInvalidated)
            }
            Err(e) => return Err(DrainError::Fatal(e.into())),
        }

        let is_silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
        let is_discont = flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0;
        let ts_error = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;

        // Anchor the accumulator to the endpoint's own timestamp rather than to whenever this
        // thread got round to draining, so the drain interval, the encode time and any
        // scheduling delay stay out of the number the client's A/V sync is built on. Frames
        // within a burst are then spaced by exactly one frame period.
        //
        // `qpcpos` needs care on a **loopback** stream, though. The documented meaning is "the
        // performance-counter value at the time the audio endpoint device read the device
        // position" — and for loopback the endpoint is a *render* device, so that position is
        // its play cursor, not a digitisation instant. The engine hands us mixed audio before
        // the cursor reaches it, so `qpcpos` is a **playout** time, in the future. Measured on
        // the dev box, idle: 1596/1601 packets stamped ahead of `host_now_ns()`, mean −14.7 ms,
        // as far as −20.5 ms, and the offset moves with host load. Left alone that is a
        // straight ~15 ms of surplus audio latency, because the client sizes its jitter buffer
        // from `enqueue − capture_ns` and a capture time in the future makes that look shorter
        // than it is.
        //
        // So take the endpoint's stamp only where it is physically possible: the audio in this
        // read cannot have finished later than now, so the earliest frame is no later than
        // `now − read_duration`. `min` of the two keeps a genuinely-past stamp (what a real
        // capture endpoint reports) and clamps a play-ahead one.
        if accum.is_empty() {
            let now = host_now_ns();
            let read_ns = nframes as u64 * 1_000_000_000 / OUT_SAMPLE_RATE as u64;
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
            *pending_flags |= FLAG_SILENT;
            let silence_samples = nframes as usize * OUT_CHANNELS as usize;
            accum.resize(accum.len() + silence_samples, 0.0f32);
        } else {
            *pending_flags &= !FLAG_SILENT;
            let bytes = nframes as usize * session.mix.bytes_per_frame();
            let src = std::slice::from_raw_parts(pdata, bytes);
            convert_to_stereo_f32(src, nframes as usize, &session.mix, accum);
        }

        if is_discont {
            diagnostics
                .discontinuity_count
                .fetch_add(1, Ordering::Relaxed);
            *pending_flags |= FLAG_DISCONTINUITY;
        }

        if let Err(e) = session.capture.ReleaseBuffer(nframes) {
            if e.code().0 == AUDCLNT_E_DEVICE_INVALIDATED {
                return Err(DrainError::DeviceInvalidated);
            }
            return Err(DrainError::Fatal(e.into()));
        }
    }
}

/// Acquire a capture session, newest workable tier first.
///
/// Endpoint loopback is tried first because it is what every supported Windows has and what
/// this code has always used. Process loopback is the fallback rather than the default because
/// it is *not* measurably faster — both ride the same shared engine — and the invariant here is
/// that the working path is never replaced by an unmeasured one. It takes over exactly when the
/// endpoint tier cannot work at all: the endpoint held in exclusive mode by another app (where
/// loopback is impossible by documentation), or an endpoint that will not activate.
unsafe fn acquire(enumerator: &IMMDeviceEnumerator) -> Result<Session> {
    if PREFER_PROCESS_LOOPBACK.load(Ordering::Relaxed) {
        match acquire_process_loopback() {
            Ok(s) => return Ok(s),
            Err(e) => crate::teprintln!(
                "audio: process loopback was requested but failed ({e:#}); using the endpoint"
            ),
        }
    }
    match acquire_endpoint(enumerator) {
        Ok(s) => Ok(s),
        Err(endpoint_err) => {
            if !process_loopback::available() {
                return Err(endpoint_err);
            }
            crate::teprintln!(
                "audio: endpoint loopback failed ({endpoint_err:#}); falling back to process \
                 loopback"
            );
            acquire_process_loopback().map_err(|e| {
                endpoint_err.context(format!("process loopback fallback also failed: {e:#}"))
            })
        }
    }
}

/// Tier 2: capture every process except this one, off the synthetic loopback device.
///
/// No silent render companion: there is no endpoint to park, so the argument that makes the
/// companion mandatory for tier 1 (measured 0/20 event signals without it) does not apply to a
/// device with no hardware clock. Whether it applies *worse* is unmeasured — this box has no
/// way to force tier 2 — so the starvation detector in the capture loop is what covers it, and
/// the `starvations` counter is what would say so.
unsafe fn acquire_process_loopback() -> Result<Session> {
    let client = process_loopback::activate()?;
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
    // `GetDevicePeriod` has nothing to report on a synthetic device with no hardware clock —
    // it returns 0 here — and the measured delivery cadence matches the buffer exactly
    // (480 frames = 10 ms, one period per buffer), so that is the honest number to report.
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
        // No endpoint, so no companion.
        companion: None,
        period_frames,
        buffer_frames,
    })
}

/// Tier 1: the default render endpoint in loopback mode, with a silent companion for a clock.
unsafe fn acquire_endpoint(enumerator: &IMMDeviceEnumerator) -> Result<Session> {
    // Start the companion *before* opening loopback so the endpoint is already running.
    let companion = SilenceCompanion::start().context("starting silent render companion")?;

    let device = default_render_endpoint(enumerator)?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate IAudioClient (loopback)")?;

    // RAII from here: every `?` below used to leak this `CoTaskMemAlloc`'d format, and
    // `acquire` sits on a 200 ms retry loop.
    let pwfx = MixFormatPtr::from_raw(client.GetMixFormat().context("GetMixFormat")?);
    let mix = parse_mix_format(pwfx.as_ptr())?;

    if let Some(periods) = engine::query_engine_periods(&client, pwfx.as_ptr()) {
        crate::tprintln!(
            "audio: endpoint engine periods {}",
            periods.describe(mix.sample_rate)
        );
    }

    let base_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

    let effective_mix = if mix.sample_rate == OUT_SAMPLE_RATE
        && mix.channels == OUT_CHANNELS
        && matches!(mix.kind, SampleKind::F32)
    {
        // Fast path (§4.4): 48 kHz float32 stereo — take the mix format verbatim.
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
        mix
    } else {
        // Non-48k / non-stereo / integer: ask the engine to convert to 48 kHz float stereo
        // (§4.4 option 1: AUTOCONVERTPCM). No userspace resampler needed.
        let want = target_wave_format();
        let flags = base_flags | STREAMFLAGS_AUTOCONVERTPCM | STREAMFLAGS_SRC_DEFAULT_QUALITY;
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                flags,
                0,
                0,
                &want as *const WAVEFORMATEX,
                None,
            )
            .with_context(|| {
                format!(
                    "Initialize loopback with AUTOCONVERTPCM to 48k stereo float \
                     (source was {}Hz x{}ch {:?})",
                    mix.sample_rate, mix.channels, mix.kind
                )
            })?;
        crate::tprintln!(
            "audio: source mix format {}Hz x{}ch {:?}; using engine AUTOCONVERTPCM to 48k stereo float",
            mix.sample_rate,
            mix.channels,
            mix.kind
        );
        MixFormat {
            sample_rate: OUT_SAMPLE_RATE,
            channels: OUT_CHANNELS,
            kind: SampleKind::F32,
            block_align: 8,
            channel_mask: 0,
        }
    };

    // Everything between `Initialize` and `Start` is fallible, and dropping an initialized
    // client without stopping it leaves the engine holding a stream we no longer own —
    // `Session::drop` cannot help because no `Session` exists yet.
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
    // periods deep. The two were reported as the same number under the name `period_frames`.
    // When the companion negotiated a smaller engine period, that is what this stream is
    // actually clocked at, and `GetDevicePeriod` still reports the device default.
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
        "audio: loopback acquired ({}Hz x{}ch {:?}, engine period {} frames, engine buffer {} frames)",
        effective_mix.sample_rate,
        effective_mix.channels,
        effective_mix.kind,
        period_frames,
        buffer_frames
    );

    Ok(Session {
        client,
        capture,
        event,
        mix: effective_mix,
        companion: Some(companion),
        period_frames,
        buffer_frames,
    })
}

/// Tear down the current session and acquire a fresh one (device change / invalidation /
/// a dead companion).
///
/// The new session is built *before* the old one is dropped, deliberately: it keeps the
/// endpoint clocked across the swap. The cost is that two silent render companions briefly
/// coexist on the endpoint, each with its own COM MTA thread — correct, but worth saying out
/// loud, because a device-change storm would otherwise stack them (which is why the caller
/// coalesces the whole event burst into one call).
unsafe fn reacquire(
    enumerator: &IMMDeviceEnumerator,
    session: &mut Session,
    diagnostics: &AudioDiagnostics,
) -> Result<()> {
    let fresh = acquire(enumerator)?;
    let old = std::mem::replace(session, fresh);
    drop(old); // stops old client + old companion
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
