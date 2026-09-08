//! Process loopback: the second Windows capture tier.
//!
//! `ActivateAudioInterfaceAsync` on the synthetic device `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK`
//! with `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` yields an `IAudioClient` that captures a
//! *process tree* rather than an endpoint. With
//! `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` and our own pid, that reads as "the whole
//! system except us", which is exactly the semantics this product wants.
//!
//! It is not obviously faster — it rides the same shared engine — so it is not the default. What
//! it buys is robustness the endpoint tier structurally cannot have:
//!
//! - **Endpoint independence.** Endpoint loopback dies on `AUDCLNT_E_DEVICE_INVALIDATED` and on
//!   every default-device change, each of which costs a full re-acquire and a discontinuity. The
//!   synthetic device does not move when the user switches headphones.
//! - **It is the only option on an exclusive-mode endpoint**, where loopback is impossible by
//!   documentation: "A client can enable loopback mode only for a shared-mode stream."
//! - **Self-exclusion is structural**, rather than resting on us happening never to render.
//! - **We choose the format.** The virtual device has no mix format of its own, so we ask for
//!   48 kHz float32 stereo directly and never take the `AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`
//!   resampler — which, despite its name, is the *expensive* converter and has group delay.
//!
//! Availability is Windows 10 build 20348 and up, so this must be probed rather than assumed. It
//! is probed by **attempting the activation**, never by reading a build number: the API's presence
//! is the question, and a version test would be both against the tier doctrine and wrong (the
//! function is exported on builds where the activation type is not supported).
//!
//! Two claims in circulation are contradicted by the primary sources and are not implemented
//! here: that process loopback cannot be event-driven (Microsoft's own `ApplicationLoopback`
//! sample and OBS both pass `AUDCLNT_STREAMFLAGS_EVENTCALLBACK` with a real handle), and that the
//! sample rate is fixed by a mix format (there is no mix format; the caller supplies the
//! `WAVEFORMATEX`). Both hold up: measured on the dev box with
//! `probe --audio --process-loopback`, the event fires and 199.8 packets/s arrive.
//!
//! Two things this tier does *not* need or do, both measured rather than assumed:
//!
//! - **No silent render companion.** `docs/research/audio-capture-tiers.md` §5.7 lists this as
//!   an open question with no published measurement either way. There is none needed: with no
//!   companion anywhere and an idle host, 999 packets arrived in 5 s. There is no endpoint to
//!   park, so the endpoint-parking failure that makes the companion mandatory for tier 1
//!   (measured 0/20 event signals without it) has nothing to act on.
//! - **No `AUDCLNT_BUFFERFLAGS_SILENT`.** The synthetic device delivers real zeros rather than
//!   flagging silence, so the drain's silence shortcut never fires here and `FLAG_SILENT`
//!   never reaches the client on this tier.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use anyhow::{bail, Context, Result};
use windows::core::{Interface, Ref, HRESULT};
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioClient, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0_0};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::System::Variant::VT_BLOB;

use super::guards::EventHandle;

/// How long to wait for `ActivateAudioInterfaceAsync` to call us back. The activation is a
/// round trip to the audio service, not a computation; a second is generous and bounded.
const ACTIVATION_TIMEOUT_MS: u32 = 1000;

/// Signals the caller when the async activation completes.
///
/// `ActivateAudioInterfaceAsync` is the only way to reach the synthetic device and it insists
/// on a completion handler, so this is machinery rather than design. The handler does nothing
/// but set an event; the result is collected on the calling thread.
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: Arc<AtomicBool>,
    event: windows::Win32::Foundation::HANDLE,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        self.done.store(true, Ordering::Release);
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.event);
        }
        Ok(())
    }
}

/// Activate an `IAudioClient` for "every process except this one".
///
/// # Safety
/// Must be called on a thread with COM initialized.
pub unsafe fn activate() -> Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: GetCurrentProcessId(),
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    // The activation parameters travel as a VT_BLOB PROPVARIANT. Built by hand because the
    // crate exposes the union rather than a constructor for this shape; `blob.pBlobData` must
    // outlive the call, which it does — `params` is a local of this function and
    // `ActivateAudioInterfaceAsync` copies it before returning.
    let mut variant = PROPVARIANT::default();
    {
        let inner: &mut PROPVARIANT_0_0 = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &mut params as *mut _ as *mut u8,
        };
    }

    let event = EventHandle::new_auto_reset()?;
    let done = Arc::new(AtomicBool::new(false));
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
        done: Arc::clone(&done),
        event: event.raw(),
    }
    .into();

    let operation = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&variant),
        &handler,
    )
    .context("ActivateAudioInterfaceAsync(VAD\\Process_Loopback)")?;

    if !event.wait(ACTIVATION_TIMEOUT_MS) && !done.load(Ordering::Acquire) {
        bail!("process loopback activation did not complete within {ACTIVATION_TIMEOUT_MS} ms");
    }

    let mut activate_hr = HRESULT(0);
    let mut interface: Option<windows::core::IUnknown> = None;
    operation
        .GetActivateResult(&mut activate_hr, &mut interface)
        .context("GetActivateResult")?;
    activate_hr
        .ok()
        .context("process loopback activation was refused")?;
    let unknown = interface.context("process loopback returned no interface")?;
    let client = unknown
        .cast::<IAudioClient>()
        .context("cast activated interface to IAudioClient")?;

    // Do not let this `PROPVARIANT` be dropped. windows-rs runs destructor logic for it that
    // assumes the variant owns its payload, and ours points at a stack local — measured, on
    // this machine, as a hard process kill the instant the variant went out of scope, with
    // the activation itself having completed successfully. `forget` leaks nothing: the
    // variant is a stack value whose only pointer is into this frame.
    std::mem::forget(variant);
    Ok(client)
}

/// Does this machine support process loopback?
///
/// Answered by performing a real activation, cached for the process. `ActivateAudioInterfaceAsync`
/// itself exports on builds that do not support the activation *type*, so its presence proves
/// nothing and is not what is tested.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| match unsafe { activate() } {
        Ok(_client) => {
            crate::tprintln!("audio: process loopback is available on this machine");
            true
        }
        Err(e) => {
            crate::tprintln!(
                "audio: process loopback unavailable ({e:#}); endpoint loopback is the only tier"
            );
            false
        }
    })
}

/// The format to ask the synthetic device for.
///
/// There is no mix format to query, so the caller picks — which is the point: 48 kHz float32
/// stereo is what the Opus encoder wants, so this tier never resamples.
pub fn target_format() -> WAVEFORMATEX {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    let channels: u16 = 2;
    let bits: u16 = 32;
    let block_align = channels * (bits / 8);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits,
        cbSize: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_format_is_the_encoder_format() {
        // `WAVEFORMATEX` is `#[repr(packed)]`, so every field has to be copied out before it
        // can be looked at.
        let f = target_format();
        let (rate, channels, bits, block_align, avg) = (
            f.nSamplesPerSec,
            f.nChannels,
            f.wBitsPerSample,
            f.nBlockAlign,
            f.nAvgBytesPerSec,
        );
        assert_eq!(rate, crate::streamer::audio::AUDIO_SAMPLE_RATE);
        assert_eq!(channels, 2);
        assert_eq!(bits, 32);
        // 2 channels x 4 bytes; a wrong block align silently misreads every buffer.
        assert_eq!(block_align, 8);
        assert_eq!(avg, rate * block_align as u32);
    }

    #[test]
    fn activation_params_carry_our_own_pid_and_the_exclude_mode() {
        // `EXCLUDE_TARGET_PROCESS_TREE` with our own pid is what makes this "everything except
        // us" rather than "one app". Getting the mode backwards would capture only ourselves,
        // i.e. silence, with no error anywhere.
        let params = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: unsafe { GetCurrentProcessId() },
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        };
        assert_ne!(params.TargetProcessId, 0);
        assert_eq!(params.ProcessLoopbackMode.0, 1);
    }
}
