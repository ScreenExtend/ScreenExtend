//! WASAPI loopback capture thread (PRD §4.1, §4.2, §4.5).
//!
//! Design settled by the §3 spike (`AUDIO_NOTES.md`):
//!  - **Legacy `IAudioClient::Initialize` loopback path**, not `IAudioClient3` — the low-latency
//!    shared path rejects `AUDCLNT_STREAMFLAGS_LOOPBACK` (measured `AUDCLNT_E_INVALID_STREAM_FLAG`).
//!  - A **silent render companion** runs alongside; it is the clock source that makes the
//!    capture event fire and keeps packets flowing while the host is idle.
//!  - The capture thread is a **dedicated OS thread** with COM MTA + MMCSS "Pro Audio", never a
//!    tokio task (§8.3). It never allocates or locks in the drain path (§8.4) beyond the reused
//!    accumulator/`Bytes` copy of each encoded packet.

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
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};

use super::device::{
    create_enumerator, default_render_endpoint, DeviceEvent, NotifierRegistration,
};
use super::encoder::{OpusEncoder, OpusEncoderConfig, FRAME_SAMPLES};
use super::format::{convert_to_stereo_f32, parse_mix_format, MixFormat, SampleKind};
use super::guards::{ComGuard, EventHandle, MmcssGuard};
use super::silence::SilenceCompanion;
use crate::streamer::audio::{
    host_now_ns, AudioCapture, AudioDiagnostics, AudioFormat, AudioPacket, AudioStopFn,
    FLAG_DISCONTINUITY, FLAG_SILENT,
};

// Stream flags not always surfaced as named consts across feature sets — pin the documented
// values (see docs/wasapi/AUDCLNT_STREAMFLAGS.html).
const STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// `AUDCLNT_E_DEVICE_INVALIDATED` (`AUDCLNT_ERR(0x004)`).
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;

const OUT_SAMPLE_RATE: u32 = 48000;
const OUT_CHANNELS: u16 = 2;
/// Interleaved f32 samples in one Opus frame (`FRAME_SAMPLES` per channel × stereo).
const FRAME_INTERLEAVED: usize = FRAME_SAMPLES * OUT_CHANNELS as usize;
/// How often to recompute encode percentiles and log the diagnostics summary.
const DIAG_LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
const ENCODE_WINDOW: usize = 4096;

/// Start capture. Returns immediately once the first device acquire + encoder init succeed; the
/// dedicated thread then runs until the returned stop closure is invoked.
pub fn start() -> Result<AudioCapture> {
    let (pkt_tx, pkt_rx) = crossbeam_channel::unbounded::<AudioPacket>();
    let diagnostics = Arc::new(AudioDiagnostics::default());
    let stop = Arc::new(AtomicBool::new(false));

    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(AudioFormat, i32)>>();

    let diag_thread = Arc::clone(&diagnostics);
    let stop_thread = Arc::clone(&stop);
    let join = std::thread::Builder::new()
        .name("audio-loopback".to_string())
        .spawn(move || run(pkt_tx, diag_thread, stop_thread, ready_tx))
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
        rx: pkt_rx,
        stop: stop_fn,
        format,
        diagnostics,
        lookahead_samples,
    })
}

/// One acquired capture endpoint + its silent companion. Dropping it stops both.
struct Session {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    event: EventHandle,
    mix: MixFormat,
    companion: SilenceCompanion,
    period_frames: u32,
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: stopping a stream we started.
        unsafe {
            let _ = self.client.Stop();
        }
        // companion stopped by its own Drop
    }
}

fn run(
    pkt_tx: crossbeam_channel::Sender<AudioPacket>,
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
    // MMCSS is important but not worth failing the whole feature if unavailable (e.g. locked-
    // down environments); log and continue.
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
    crate::tprintln!("audio: {} loaded", encoder.version());

    // SAFETY: all WASAPI calls run on this MTA thread.
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

    let _ = ready_tx.send(Ok((
        AudioFormat {
            sample_rate: OUT_SAMPLE_RATE,
            channels: OUT_CHANNELS,
        },
        encoder.lookahead_samples(),
    )));

    let mut accum: Vec<f32> = Vec::with_capacity(FRAME_INTERLEAVED * 16);
    let mut seq: u32 = 0;
    let mut pending_flags: u8 = FLAG_DISCONTINUITY; // first frame is a fresh timeline
    let mut encode_ns: Vec<u64> = Vec::with_capacity(ENCODE_WINDOW);
    let mut last_diag = Instant::now();

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // React to the user switching the default output device (§4.5).
        if let Ok(DeviceEvent::DefaultRenderChanged) = dev_rx.try_recv() {
            crate::tprintln!("audio: default render device changed; re-acquiring");
            diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
            match unsafe { reacquire(&enumerator, &mut session) } {
                Ok(()) => {
                    accum.clear();
                    pending_flags |= FLAG_DISCONTINUITY;
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

        session.event.wait(100);

        // Drain everything currently available.
        match unsafe { drain(&session, &mut accum, &mut pending_flags, &diagnostics) } {
            Ok(()) => {}
            Err(DrainError::DeviceInvalidated) => {
                crate::tprintln!("audio: device invalidated; re-acquiring");
                diagnostics.device_changes.fetch_add(1, Ordering::Relaxed);
                match unsafe { reacquire(&enumerator, &mut session) } {
                    Ok(()) => {
                        accum.clear();
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

        // Encode all whole frames sitting in the accumulator.
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
            let dt = t0.elapsed().as_nanos() as u64;
            if encode_ns.len() < ENCODE_WINDOW {
                encode_ns.push(dt);
            } else {
                encode_ns[(seq as usize) % ENCODE_WINDOW] = dt;
            }

            let flags = pending_flags;
            pending_flags = 0;
            let pkt = AudioPacket {
                seq,
                capture_ns: host_now_ns(),
                flags,
                data: Bytes::copy_from_slice(encoded),
            };
            if pkt_tx.send(pkt).is_err() {
                // no bridge/subscribers — capture is being torn down
                return;
            }
            seq = seq.wrapping_add(1);
        }
        if full > 0 {
            accum.drain(0..full * FRAME_INTERLEAVED);
        }

        if last_diag.elapsed() >= DIAG_LOG_INTERVAL {
            update_encode_percentiles(&encode_ns, &diagnostics);
            crate::tprintln!("{}", diagnostics.summary());
            last_diag = Instant::now();
        }
    }

    crate::tprintln!("audio: loopback capture stopped");
    // `session` Drop stops the client + companion; guards revert COM/MMCSS.
}

enum DrainError {
    DeviceInvalidated,
    Fatal(anyhow::Error),
}

/// Pull all currently-available packets, convert to interleaved stereo f32, and append to
/// `accum`. Sets `pending_flags` bits for silence/discontinuity as it goes.
unsafe fn drain(
    session: &Session,
    accum: &mut Vec<f32>,
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
        let _ts_error = flags & AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0;
        // (QPC position unreliable on _ts_error; we timestamp on the host Instant timebase
        // rather than QPC, so a bad QPC does not corrupt the audio timeline.)

        if is_silent {
            diagnostics.silent_count.fetch_add(1, Ordering::Relaxed);
            *pending_flags |= FLAG_SILENT;
            // AUDCLNT_BUFFERFLAGS_SILENT: buffer contents are UNDEFINED — emit real silence.
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

/// Acquire the default render endpoint for loopback, starting a fresh silent companion first.
unsafe fn acquire(enumerator: &IMMDeviceEnumerator) -> Result<Session> {
    // Start the companion *before* opening loopback so the endpoint is already running.
    let companion = SilenceCompanion::start().context("starting silent render companion")?;

    let device = default_render_endpoint(enumerator)?;
    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate IAudioClient (loopback)")?;

    let pwfx = client.GetMixFormat().context("GetMixFormat")?;
    let mix = parse_mix_format(pwfx)?;

    let base_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;

    let effective_mix = if mix.sample_rate == OUT_SAMPLE_RATE
        && mix.channels == OUT_CHANNELS
        && matches!(mix.kind, SampleKind::F32)
    {
        // Fast path (§4.4): 48 kHz float32 stereo — take the mix format verbatim.
        let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, base_flags, 0, 0, pwfx, None);
        CoTaskMemFree(Some(pwfx as *const _));
        init.context("Initialize loopback (fast path)")?;
        mix
    } else {
        // Non-48k / non-stereo / integer: ask the engine to convert to 48 kHz float stereo
        // (§4.4 option 1: AUTOCONVERTPCM). No userspace resampler needed.
        CoTaskMemFree(Some(pwfx as *const _));
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

    let event = EventHandle::new_auto_reset()?;
    client
        .SetEventHandle(event.raw())
        .context("SetEventHandle")?;
    let capture: IAudioCaptureClient = client
        .GetService()
        .context("GetService(IAudioCaptureClient)")?;
    let period_frames = client.GetBufferSize().unwrap_or(0);
    client.Start().context("Start loopback capture")?;

    crate::tprintln!(
        "audio: loopback acquired ({}Hz x{}ch {:?}, engine buffer {} frames)",
        effective_mix.sample_rate,
        effective_mix.channels,
        effective_mix.kind,
        period_frames
    );

    Ok(Session {
        client,
        capture,
        event,
        mix: effective_mix,
        companion,
        period_frames,
    })
}

/// Tear down the current session and acquire a fresh one (device change / invalidation).
unsafe fn reacquire(enumerator: &IMMDeviceEnumerator, session: &mut Session) -> Result<()> {
    // Replace with a placeholder-free swap: build the new one first, then drop the old.
    let fresh = acquire(enumerator)?;
    let old = std::mem::replace(session, fresh);
    drop(old); // stops old client + old companion
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

fn update_encode_percentiles(samples: &[u64], diagnostics: &AudioDiagnostics) {
    if samples.is_empty() {
        return;
    }
    let mut sorted: Vec<u64> = samples.to_vec();
    sorted.sort_unstable();
    let p = |q: f64| sorted[((sorted.len() as f64 * q) as usize).min(sorted.len() - 1)];
    diagnostics.encode_p50_ns.store(p(0.50), Ordering::Relaxed);
    diagnostics.encode_p99_ns.store(p(0.99), Ordering::Relaxed);
}
