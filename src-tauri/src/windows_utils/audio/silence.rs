use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};

use super::device::{create_enumerator, default_render_endpoint};
use super::guards::{ComGuard, EventHandle};

pub struct SilenceCompanion {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SilenceCompanion {
    pub fn start() -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        let join = std::thread::Builder::new()
            .name("audio-silence".to_string())
            .spawn(move || run(stop_thread, ready_tx))
            .context("spawning silence companion thread")?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                stop,
                join: Some(join),
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

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
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

fn run(stop: Arc<AtomicBool>, ready_tx: std::sync::mpsc::Sender<Result<()>>) {
    let result = unsafe { setup(&stop) };
    match result {
        Ok((client, render, event, buffer_frames, _com)) => {
            let _ = ready_tx.send(Ok(()));
            unsafe { pump(&client, &render, &event, buffer_frames, &stop) };
            unsafe {
                let _ = client.Stop();
            }
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
        }
    }
}

unsafe fn setup(
    _stop: &Arc<AtomicBool>,
) -> Result<(IAudioClient, IAudioRenderClient, EventHandle, u32, ComGuard)> {
    let com = ComGuard::init_mta()?;
    let enumerator = create_enumerator()?;
    let device = default_render_endpoint(&enumerator)?;

    let client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("Activate IAudioClient (silence companion)")?;
    let pwfx = client.GetMixFormat().context("GetMixFormat (silence)")?;
    let init = client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        0,
        0,
        pwfx,
        None,
    );
    CoTaskMemFree(Some(pwfx as *const _));
    init.context("Initialize render (silence companion)")?;

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
    Ok((client, render, event, buffer_frames, com))
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
            Err(_) => break, // device invalidated; capture thread will re-acquire us
        };
        let avail = buffer_frames.saturating_sub(padding);
        if avail == 0 {
            continue;
        }
        match render.GetBuffer(avail) {
            Ok(_buf) => {
                if render
                    .ReleaseBuffer(avail, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
                    .is_err()
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}
