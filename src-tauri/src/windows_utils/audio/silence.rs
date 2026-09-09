//! The silent render companion: the clock source that makes loopback capture fire at all, and
//! the only stream we own that is allowed to ask the engine for a smaller period.
//!
//! Two jobs, and the second one is not obvious:
//!
//! 1. **Clock.** With nothing rendering, the engine parks the endpoint: no buffers, no events.
//!    Measured on this repo's dev box (`docs/recovered/AUDIO_NOTES.md` §3.2, 20 iterations x
//!    200 ms wait, host idle): **0/20** event signals without a companion, **20/20** with one.
//!    OBS carries the same workaround for the same reason.
//! 2. **Periodicity.** The engine runs one period for the whole endpoint and any stream on it
//!    may set that period. Our *capture* stream cannot ask —
//!    `IAudioClient3::InitializeSharedAudioStream` accepts only
//!    `AUDCLNT_STREAMFLAGS_EVENTCALLBACK`, and the loopback flag is refused with
//!    `AUDCLNT_E_INVALID_STREAM_FLAG` (measured, same notes §3.1). The companion has no loopback
//!    flag, so it can ask, and the capture stream inherits the result. `acquire()` already starts
//!    the companion *before* opening loopback, which is exactly the ordering this needs.
//!
//! The period request is a probed tier, never a version check: query
//! `GetSharedModeEnginePeriod`, ask for the minimum only if the driver reports one below its
//! default, and fall back to the legacy `Initialize` on any refusal. On hardware whose driver
//! exposes a single period — the dev box, whose OEM driver reports 480 frames for all four
//! values — the probe correctly declines and nothing changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use windows::core::Interface;
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};

use super::device::{create_enumerator, default_render_endpoint};
use super::engine;
use super::guards::{ComGuard, EventHandle, MixFormatPtr};

pub struct SilenceCompanion {
    stop: Arc<AtomicBool>,
    /// Cleared by the pump thread on the way out, so a caller can tell a live companion from a
    /// dead one. Nothing used to notice: `pump` had three bare `break`s and the struct stayed
    /// alive inside `Session` with the endpoint's clock gone.
    alive: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// The engine period the companion negotiated, in frames; 0 when it took the default path.
    granted_period_frames: u32,
}

impl SilenceCompanion {
    pub fn start() -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let stop_thread = Arc::clone(&stop);
        let alive_thread = Arc::clone(&alive);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32>>();

        let join = std::thread::Builder::new()
            .name("audio-silence".to_string())
            .spawn(move || run(stop_thread, alive_thread, ready_tx))
            .context("spawning silence companion thread")?;

        match ready_rx.recv() {
            Ok(Ok(granted_period_frames)) => Ok(Self {
                stop,
                alive,
                join: Some(join),
                granted_period_frames,
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(e)
            }
            Err(_) => {
                let _ = join.join();
                anyhow::bail!("silence companion thread exited during setup")
            }
        }
    }

    /// False once the pump loop has exited — the endpoint has no clock and capture will starve.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn granted_period_frames(&self) -> u32 {
        self.granted_period_frames
    }
}

impl Drop for SilenceCompanion {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    ready_tx: std::sync::mpsc::Sender<Result<u32>>,
) {
    let result = unsafe { setup() };
    match result {
        Ok((client, render, event, buffer_frames, granted, _com)) => {
            let _ = ready_tx.send(Ok(granted));
            unsafe { pump(&client, &render, &event, buffer_frames, &stop) };
            unsafe {
                let _ = client.Stop();
            }
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
        }
    }
    alive.store(false, Ordering::Relaxed);
}

/// Initialize the companion, asking for the smallest engine period the driver admits to.
///
/// Returns the granted period in frames, or 0 when the default path was taken.
unsafe fn init_render_stream(
    device: &IMMDevice,
    client: &IAudioClient,
    pwfx: &MixFormatPtr,
    sample_rate: u32,
) -> Result<(IAudioClient, u32)> {
    let periods = engine::query_engine_periods(client, pwfx.as_ptr());
    if let Some(p) = periods {
        crate::tprintln!(
            "audio: companion sees engine periods {}",
            p.describe(sample_rate)
        );
    }

    if let Some(min_frames) = periods.and_then(|p| p.lower_than_default()) {
        // A refused `InitializeSharedAudioStream` leaves this client in an undefined state as
        // far as a second `Initialize` is concerned, so the fallback gets a fresh one.
        if let Ok(c3) = client.cast::<IAudioClient3>() {
            match c3.InitializeSharedAudioStream(
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                min_frames,
                pwfx.as_ptr(),
                None,
            ) {
                Ok(()) => {
                    let granted = read_back_current_period(&c3).unwrap_or(min_frames);
                    crate::tprintln!(
                        "audio: companion pulled the endpoint engine period to {granted} frames \
                         ({:.3} ms); the loopback capture stream inherits it",
                        granted as f64 * 1000.0 / sample_rate.max(1) as f64
                    );
                    return Ok((client.clone(), granted));
                }
                Err(e) => {
                    let code = e.code().0;
                    let why = if code == engine::AUDCLNT_E_ENGINE_PERIODICITY_LOCKED {
                        "another client has pinned the engine period"
                    } else if code == engine::AUDCLNT_E_INVALID_DEVICE_PERIOD {
                        "the driver rejected the period"
                    } else {
                        "unexpected failure"
                    };
                    crate::teprintln!(
                        "audio: companion could not request {min_frames}-frame periods ({why}, \
                         0x{:08X}); using the default engine period",
                        code as u32
                    );
                }
            }
        }
        // Fall through to the default path on a fresh client.
        let fresh: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .context("re-Activate IAudioClient after a refused low-latency request")?;
        let fresh_pwfx = MixFormatPtr::from_raw(
            fresh
                .GetMixFormat()
                .context("GetMixFormat (silence retry)")?,
        );
        fresh
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,
                0,
                fresh_pwfx.as_ptr(),
                None,
            )
            .context("Initialize render (silence companion, default period)")?;
        return Ok((fresh, 0));
    }

    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            pwfx.as_ptr(),
            None,
        )
        .context("Initialize render (silence companion)")?;
    Ok((client.clone(), 0))
}

/// What the engine actually granted, rather than what we asked for.
unsafe fn read_back_current_period(c3: &IAudioClient3) -> Option<u32> {
    let mut fmt: *mut windows::Win32::Media::Audio::WAVEFORMATEX = std::ptr::null_mut();
    let mut frames: u32 = 0;
    c3.GetCurrentSharedModeEnginePeriod(&mut fmt, &mut frames)
        .ok()?;
    if !fmt.is_null() {
        CoTaskMemFree(Some(fmt as *const _));
    }
    if frames == 0 {
        None
    } else {
        Some(frames)
    }
}

unsafe fn setup() -> Result<(
    IAudioClient,
    IAudioRenderClient,
    EventHandle,
    u32,
    u32,
    ComGuard,
)> {
    let com = ComGuard::init_mta()?;
    let enumerator = create_enumerator()?;
    let device = default_render_endpoint(&enumerator)?;

    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate IAudioClient (silence companion)")?;
    let pwfx = MixFormatPtr::from_raw(client.GetMixFormat().context("GetMixFormat (silence)")?);
    let sample_rate = (*pwfx.as_ptr()).nSamplesPerSec;

    let (client, granted) = init_render_stream(&device, &client, &pwfx, sample_rate)?;

    let event = EventHandle::new_auto_reset()?;
    client
        .SetEventHandle(event.raw())
        .context("SetEventHandle (silence)")?;

    let render: IAudioRenderClient = client
        .GetService()
        .context("GetService(IAudioRenderClient)")?;
    let buffer_frames = client.GetBufferSize().context("GetBufferSize (silence)")?;

    let _buf = render
        .GetBuffer(buffer_frames)
        .context("render GetBuffer pre-roll")?;
    render
        .ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
        .context("render ReleaseBuffer pre-roll")?;

    client.Start().context("Start render (silence)")?;
    Ok((client, render, event, buffer_frames, granted, com))
}

unsafe fn pump(
    client: &IAudioClient,
    render: &IAudioRenderClient,
    event: &EventHandle,
    buffer_frames: u32,
    stop: &Arc<AtomicBool>,
) {
    while !stop.load(Ordering::Relaxed) {
        event.wait(100);
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let padding = match client.GetCurrentPadding() {
            Ok(p) => p,
            Err(e) => {
                crate::teprintln!(
                    "audio: silence companion GetCurrentPadding failed ({e}); the endpoint has \
                     lost its clock and capture will starve until it is restarted"
                );
                break;
            }
        };
        let avail = buffer_frames.saturating_sub(padding);
        if avail == 0 {
            continue;
        }
        match render.GetBuffer(avail) {
            Ok(_buf) => {
                if let Err(e) = render.ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) {
                    crate::teprintln!(
                        "audio: silence companion ReleaseBuffer failed ({e}); stopping"
                    );
                    break;
                }
            }
            Err(e) => {
                crate::teprintln!("audio: silence companion GetBuffer failed ({e}); stopping");
                break;
            }
        }
    }
}
