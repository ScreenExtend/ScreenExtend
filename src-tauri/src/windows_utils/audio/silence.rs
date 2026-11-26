//! The silent render companion for endpoint loopback capture.
//!
//! Clock: with nothing rendering the engine parks the endpoint and the loopback event never fires
//! (measured 0/20 event signals without a companion, 20/20 with one).
//!
//! Period: it is the only stream we own that may ask the engine for a smaller period
//! (`IAudioClient3::InitializeSharedAudioStream` refuses the loopback flag with
//! `AUDCLNT_E_INVALID_STREAM_FLAG`), and the capture stream inherits the result, so the companion
//! starts before loopback is opened. A driver that reports a single period makes this a no-op.

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
use super::loopback::{SetupReporter, StuckSetups};

/// How long the companion's WASAPI open may take before the caller gives up on it. A healthy
/// open is tens of milliseconds; this only bounds a wedged audio service or driver.
const COMPANION_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Companion threads whose open outlived [`COMPANION_START_TIMEOUT`] and are still inside it.
static STUCK_COMPANION_STARTS: StuckSetups = StuckSetups::new();

pub struct SilenceCompanion {
    stop: Arc<AtomicBool>,
    /// Cleared by the pump thread on the way out: a dead companion means the endpoint has no
    /// clock, and the struct itself stays alive inside `Session`.
    alive: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    /// The engine period the companion negotiated, in frames; 0 when it took the default path.
    granted_period_frames: u32,
}

impl SilenceCompanion {
    pub fn start() -> Result<Self> {
        // the capture thread retries a failed acquisition every few seconds, and against a wedged
        // service each retry would park one more thread in the same call
        if STUCK_COMPANION_STARTS.outstanding() > 0 {
            anyhow::bail!(
                "an earlier silence companion is still stuck opening the endpoint; not opening \
                 another until it returns"
            );
        }
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let stop_thread = Arc::clone(&stop);
        let alive_thread = Arc::clone(&alive);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<u32>>();
        let (ready_tx, token) = SetupReporter::new(ready_tx, &STUCK_COMPANION_STARTS);

        let join = std::thread::Builder::new()
            .name("audio-silence".to_string())
            .spawn(move || run(stop_thread, alive_thread, ready_tx))
            .context("spawning silence companion thread")?;

        match super::loopback::await_ready(&ready_rx, COMPANION_START_TIMEOUT) {
            super::loopback::Ready::Started(granted_period_frames) => Ok(Self {
                stop,
                alive,
                join: Some(join),
                granted_period_frames,
            }),
            super::loopback::Ready::Failed(e) => {
                let _ = join.join();
                Err(e)
            }
            super::loopback::Ready::Exited => {
                let _ = join.join();
                anyhow::bail!("silence companion thread exited during setup")
            }
            super::loopback::Ready::TimedOut => {
                // wedged inside the audio service: the thread leaves once the call returns (it
                // checks `stop` before pumping) and is detached, because joining it would wedge
                // the caller and through it the whole audio hub
                stop.store(true, Ordering::Relaxed);
                STUCK_COMPANION_STARTS.abandon(&token);
                drop(join);
                anyhow::bail!(
                    "silence companion did not start within {} s",
                    COMPANION_START_TIMEOUT.as_secs()
                )
            }
        }
    }

    /// False once the pump loop has exited: the endpoint has no clock and capture will starve.
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

fn run(stop: Arc<AtomicBool>, alive: Arc<AtomicBool>, ready_tx: SetupReporter<u32>) {
    let result = unsafe { setup() };
    match result {
        Ok((client, render, event, buffer_frames, granted, com)) => {
            let _ = ready_tx.send(Ok(granted));
            unsafe { pump(&client, &render, &event, buffer_frames, &stop) };
            unsafe {
                let _ = client.Stop();
            }
            // release the COM interfaces before `com`: in the tuple pattern it is declared last
            // and would drop first, and `CoUninitialize` ahead of them is survivable only because
            // `keep_process_com_alive` pins the MTA
            drop(render);
            drop(client);
            drop(event);
            drop(com);
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
        // a refused `InitializeSharedAudioStream` leaves this client in an undefined state for a
        // second `Initialize`, so the fallback gets a fresh one
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
