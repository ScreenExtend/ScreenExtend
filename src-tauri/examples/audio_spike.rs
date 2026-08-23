//! Throwaway spike (PRD §3) — answers the load-bearing WASAPI questions empirically on
//! real hardware. This is *not* part of the shipping build; it exists so the design in
//! `windows_utils/audio/` rests on measured behavior instead of recalled API lore.
//!
//! Run it on the target host:
//!
//! ```sh
//! cargo run --example audio_spike        # from src-tauri/
//! ```
//!
//! It reports, for the current default render endpoint:
//!  - the shared-mode mix format (§3 fast-path check),
//!  - `IAudioClient3::GetSharedModeEnginePeriod` default/fundamental/min/max (§3.3),
//!  - whether `AUDCLNT_STREAMFLAGS_LOOPBACK` is accepted by the IAudioClient3 low-latency
//!    path (§3.1),
//!  - whether the loopback event fires while the host is idle, with and without a silent
//!    render companion (§3.2 / §4.3).
//!
//! Measured answers are written up in `AUDIO_NOTES.md`.

#[cfg(not(windows))]
fn main() {
    eprintln!("audio_spike is Windows-only (system audio capture targets WASAPI loopback).");
}

#[cfg(windows)]
fn main() {
    // SAFETY: single-threaded example; COM is initialized/uninitialized around the run.
    unsafe {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .expect("CoInitializeEx MTA");
        spike::run();
        CoUninitialize();
    }
}

#[cfg(windows)]
#[allow(non_snake_case)]
mod spike {
    use std::time::Instant;

    use windows::core::{Interface, GUID};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Media::Audio::{
        eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient,
        IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY,
        AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX, WAVEFORMATEXTENSIBLE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
    use windows::Win32::System::Performance::QueryPerformanceFrequency;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

    const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
        GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    const KSDATAFORMAT_SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_PCM: u16 = 0x0001;

    /// # Safety
    /// COM must already be initialized on this thread.
    pub unsafe fn run() {
        println!("==== ScreenExtend audio spike (PRD §3) ====\n");

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).expect("MMDeviceEnumerator");
        let device: IMMDevice = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .expect("GetDefaultAudioEndpoint(eRender, eConsole)");

        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .expect("Activate IAudioClient");
        let pwfx = client.GetMixFormat().expect("GetMixFormat");
        let (samplerate, channels) = describe_format("mix format (GetMixFormat)", pwfx);

        println!("\n---- Q3: IAudioClient3::GetSharedModeEnginePeriod ----");
        match client.cast::<IAudioClient3>() {
            Ok(c3) => {
                let (mut def, mut fund, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
                match c3.GetSharedModeEnginePeriod(pwfx, &mut def, &mut fund, &mut min, &mut max) {
                    Ok(()) => {
                        let ms = |f: u32| f as f64 * 1000.0 / samplerate as f64;
                        println!("  default={def} frames ({:.3} ms)", ms(def));
                        println!("  fundamental={fund} frames ({:.3} ms)", ms(fund));
                        println!(
                            "  minimum={min} frames ({:.3} ms)  <- capture-latency floor",
                            ms(min)
                        );
                        println!("  maximum={max} frames ({:.3} ms)", ms(max));
                        q1_iaudioclient3_loopback(&device, pwfx, samplerate, min);
                    }
                    Err(e) => println!("  GetSharedModeEnginePeriod FAILED: {e}"),
                }
            }
            Err(e) => println!("  IAudioClient3 not available: {e}"),
        }

        q1_legacy_and_q2(&device, pwfx, samplerate, channels);

        CoTaskMemFree(Some(pwfx as *const _));
    }

    /// §3.1 — does `AUDCLNT_STREAMFLAGS_LOOPBACK` work with the IAudioClient3 low-latency path?
    unsafe fn q1_iaudioclient3_loopback(
        device: &IMMDevice,
        pwfx: *const WAVEFORMATEX,
        samplerate: u32,
        min_period: u32,
    ) {
        println!("\n---- Q1: IAudioClient3::InitializeSharedAudioStream(LOOPBACK|EVENTCALLBACK, min) ----");
        let c3: IAudioClient3 = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                println!("  Activate IAudioClient3 failed: {e}");
                return;
            }
        };
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        match c3.InitializeSharedAudioStream(flags, min_period, pwfx, None) {
            Ok(()) => {
                println!(
                    "  SUCCESS: loopback low-latency shared mode at {min_period} frames ({:.3} ms)",
                    min_period as f64 * 1000.0 / samplerate as f64
                );
            }
            Err(e) => {
                println!("  FAILED: {e} (0x{:08X})", e.code().0 as u32);
                println!("  -> loopback NOT supported on the IAudioClient3 low-latency path; use legacy Initialize.");
            }
        }
    }

    /// §3.1 fallback (legacy Initialize) + §3.2 (does the event fire when idle, and does a
    /// silent render companion fix it?).
    unsafe fn q1_legacy_and_q2(
        device: &IMMDevice,
        pwfx: *const WAVEFORMATEX,
        samplerate: u32,
        channels: u16,
    ) {
        println!(
            "\n---- Q1 fallback: IAudioClient::Initialize(SHARED, LOOPBACK|EVENTCALLBACK) ----"
        );
        let client: IAudioClient = match device.Activate(CLSCTX_ALL, None) {
            Ok(c) => c,
            Err(e) => {
                println!("  Activate failed: {e}");
                return;
            }
        };
        let flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        match client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, pwfx, None) {
            Ok(()) => {
                let frames = client.GetBufferSize().unwrap_or(0);
                println!(
                    "  SUCCESS: legacy loopback initialized. Engine buffer = {frames} frames ({:.3} ms)",
                    frames as f64 * 1000.0 / samplerate as f64
                );
                println!("\n---- Q2a: does the loopback event fire when the host is IDLE (no companion)? ----");
                run_capture_probe(&client, samplerate, channels);
            }
            Err(e) => {
                println!("  FAILED: {e}");
                return;
            }
        }

        println!("\n---- Q2b: same idle test WITH a silent render companion stream ----");
        match start_silence_companion(device) {
            Ok(companion) => {
                let client2: IAudioClient = device.Activate(CLSCTX_ALL, None).unwrap();
                client2
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, pwfx, None)
                    .expect("re-init loopback");
                run_capture_probe(&client2, samplerate, channels);
                let _ = client2.Stop();
                drop(companion);
            }
            Err(e) => println!("  could not start silence companion: {e}"),
        }
    }

    unsafe fn run_capture_probe(client: &IAudioClient, samplerate: u32, channels: u16) {
        let event: HANDLE = CreateEventW(None, false, false, None).expect("CreateEventW");
        if let Err(e) = client.SetEventHandle(event) {
            println!("    SetEventHandle failed: {e}");
            let _ = CloseHandle(event);
            return;
        }
        let capture: IAudioCaptureClient =
            client.GetService().expect("GetService IAudioCaptureClient");
        if let Err(e) = client.Start() {
            println!("    Start failed: {e}");
            let _ = CloseHandle(event);
            return;
        }

        let mut qpc_freq = 0i64;
        let _ = QueryPerformanceFrequency(&mut qpc_freq);

        let iters = 20;
        let timeout_ms = 200u32;
        let mut signals = 0u32;
        let mut timeouts = 0u32;
        let mut total_frames = 0u64;
        let mut silent_pkts = 0u32;
        let mut discont = 0u32;
        let mut packets = 0u32;
        let mut intervals: Vec<f64> = Vec::new();
        let mut last_wake = Instant::now();

        for _ in 0..iters {
            let wr = WaitForSingleObject(event, timeout_ms);
            if wr == WAIT_OBJECT_0 {
                signals += 1;
                let now = Instant::now();
                intervals.push(now.duration_since(last_wake).as_secs_f64() * 1000.0);
                last_wake = now;
            } else {
                timeouts += 1;
                last_wake = Instant::now();
            }
            loop {
                let avail = match capture.GetNextPacketSize() {
                    Ok(n) => n,
                    Err(e) => {
                        println!("    GetNextPacketSize error: {e}");
                        break;
                    }
                };
                if avail == 0 {
                    break;
                }
                let mut pdata: *mut u8 = std::ptr::null_mut();
                let mut nframes = 0u32;
                let mut flags = 0u32;
                let mut devpos = 0u64;
                let mut qpcpos = 0u64;
                if capture
                    .GetBuffer(
                        &mut pdata,
                        &mut nframes,
                        &mut flags,
                        Some(&mut devpos),
                        Some(&mut qpcpos),
                    )
                    .is_err()
                {
                    break;
                }
                packets += 1;
                total_frames += nframes as u64;
                if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    silent_pkts += 1;
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                    discont += 1;
                }
                let _ = capture.ReleaseBuffer(nframes);
            }
        }
        let _ = client.Stop();
        let _ = client.Reset();
        let _ = CloseHandle(event);

        let dur_ms = total_frames as f64 * 1000.0 / samplerate as f64;
        println!("    iters={iters} timeout={timeout_ms}ms: event signals={signals}, timeouts={timeouts}");
        println!(
            "    packets={packets}, total_frames={total_frames} ({dur_ms:.1} ms of audio @ {samplerate}Hz x{channels}ch)"
        );
        println!("    silent_packets={silent_pkts}, data_discontinuity={discont}");
        if !intervals.is_empty() {
            let avg = intervals.iter().sum::<f64>() / intervals.len() as f64;
            let max = intervals.iter().cloned().fold(0.0_f64, f64::max);
            println!(
                "    event interval avg={avg:.2} ms, max={max:.2} ms (n={})",
                intervals.len()
            );
        }
        if signals == 0 {
            println!("    VERDICT: event NEVER fired -> need a companion/clock source.");
        } else if timeouts > signals {
            println!("    VERDICT: event fired unreliably (more timeouts than signals).");
        } else {
            println!("    VERDICT: event fired reliably.");
        }
    }

    struct SilenceCompanion {
        client: IAudioClient,
        _event: HANDLE,
    }
    impl Drop for SilenceCompanion {
        fn drop(&mut self) {
            // SAFETY: `client` is a live IAudioClient we started.
            unsafe {
                let _ = self.client.Stop();
            }
        }
    }

    unsafe fn start_silence_companion(
        device: &IMMDevice,
    ) -> windows::core::Result<SilenceCompanion> {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let pwfx = client.GetMixFormat()?;
        let event = CreateEventW(None, false, false, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            0,
            0,
            pwfx,
            None,
        )?;
        client.SetEventHandle(event)?;
        let render: IAudioRenderClient = client.GetService()?;
        let frames = client.GetBufferSize()?;
        let buf = render.GetBuffer(frames)?;
        let block = std::ptr::read_unaligned(pwfx).nBlockAlign as usize;
        std::ptr::write_bytes(buf, 0, frames as usize * block);
        render.ReleaseBuffer(frames, 0)?;
        client.Start()?;
        CoTaskMemFree(Some(pwfx as *const _));
        println!("    silence companion started ({frames} frames buffer)");
        Ok(SilenceCompanion {
            client,
            _event: event,
        })
    }

    unsafe fn describe_format(label: &str, pwfx: *const WAVEFORMATEX) -> (u32, u16) {
        // WAVEFORMATEX is #[repr(packed)]; copy fields out by value before use.
        let wfx = std::ptr::read_unaligned(pwfx);
        let tag = wfx.wFormatTag;
        let n_channels = wfx.nChannels;
        let samplerate = wfx.nSamplesPerSec;
        let bits = wfx.wBitsPerSample;
        let block = wfx.nBlockAlign;
        let subfmt = if tag == WAVE_FORMAT_EXTENSIBLE {
            let ext = std::ptr::read_unaligned(pwfx as *const WAVEFORMATEXTENSIBLE);
            let sub = ext.SubFormat;
            if sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                "IEEE_FLOAT"
            } else if sub == KSDATAFORMAT_SUBTYPE_PCM {
                "PCM"
            } else {
                "OTHER"
            }
        } else if tag == WAVE_FORMAT_IEEE_FLOAT {
            "IEEE_FLOAT(non-ext)"
        } else if tag == WAVE_FORMAT_PCM {
            "PCM(non-ext)"
        } else {
            "UNKNOWN"
        };
        println!("{label}:");
        println!(
            "  wFormatTag=0x{:04X} ({})",
            tag,
            if tag == WAVE_FORMAT_EXTENSIBLE {
                "EXTENSIBLE"
            } else {
                "simple"
            }
        );
        println!("  nChannels={n_channels}");
        println!("  nSamplesPerSec={samplerate}");
        println!("  wBitsPerSample={bits}");
        println!("  nBlockAlign={block}");
        println!("  subformat={subfmt}");
        println!(
            "  => {} kHz is {}",
            samplerate / 1000,
            if samplerate == 48000 {
                "the 48k FAST PATH (no resample)"
            } else {
                "NOT 48k (needs rate conversion)"
            }
        );
        (samplerate, n_channels)
    }
}
