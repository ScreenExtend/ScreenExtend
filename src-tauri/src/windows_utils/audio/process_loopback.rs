//! Process loopback: the preferred Windows capture tier, tried before endpoint loopback.
//!
//! `ActivateAudioInterfaceAsync` on `VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK` with
//! `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` and our own pid captures the whole system
//! except us, in the format we ask for (there is no mix format, so no resampler). It taps the mix
//! before the endpoint's own buffering, so it measured faster than endpoint loopback (click to
//! encoded packet, median 59.4 ms against 80.6 ms), and it works on an exclusive-mode endpoint.
//!
//! Probed by attempting the activation (documented from Windows 10 build 20348), never by build
//! number: the function is exported on builds where the activation type is not supported. It is
//! event-driven (199.8 packets/s measured), needs no silent render companion (999 packets in 5 s
//! on an idle host without one), and delivers real zeros instead of `AUDCLNT_BUFFERFLAGS_SILENT`,
//! so `FLAG_SILENT` is decided from the samples.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// After a transient failure (a timeout, an RPC error, a service still starting), how long
/// acquisitions skip this tier before trying it again.
///
/// Long enough that a host whose audio service keeps timing out does not pay a one-second
/// activation on every re-acquire; short enough that one bad moment at an autostart login does
/// not pin the slower endpoint tier.
pub const TRANSIENT_RETRY_AFTER: Duration = Duration::from_secs(30);

/// The completion handler `ActivateAudioInterfaceAsync` insists on: it only sets an event, and
/// the result is collected on the calling thread.
///
/// It owns a reference to the event. The async operation holds the handler until it completes,
/// which can be after `activate` gave up waiting, and a bare `HANDLE` closed by then would let a
/// late `ActivateCompleted` signal whatever object the recycled handle value names.
#[windows::core::implement(IActivateAudioInterfaceCompletionHandler)]
struct ActivationHandler {
    done: Arc<AtomicBool>,
    event: Arc<EventHandle>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for ActivationHandler_Impl {
    fn ActivateCompleted(
        &self,
        _op: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        self.done.store(true, Ordering::Release);
        unsafe {
            let _ = windows::Win32::System::Threading::SetEvent(self.event.raw());
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

    // VT_BLOB PROPVARIANT: `blob.pBlobData` must outlive the call (`params` is a local, and
    // `ActivateAudioInterfaceAsync` copies it before returning). The variant points at that local
    // and owns nothing, so it must never be cleared: `ManuallyDrop` in case the type gains a
    // `Drop` (`windows` 0.62 has none here, but `windows_core::PROPVARIANT` clears on drop).
    let mut variant = ManuallyDrop::new(PROPVARIANT::default());
    {
        let inner: &mut PROPVARIANT_0_0 = &mut variant.Anonymous.Anonymous;
        inner.vt = VT_BLOB;
        inner.Anonymous.blob = BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &mut params as *mut _ as *mut u8,
        };
    }

    let event = Arc::new(EventHandle::new_auto_reset()?);
    let done = Arc::new(AtomicBool::new(false));
    let handler: IActivateAudioInterfaceCompletionHandler = ActivationHandler {
        done: Arc::clone(&done),
        event: Arc::clone(&event),
    }
    .into();

    let operation = ActivateAudioInterfaceAsync(
        VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
        &IAudioClient::IID,
        Some(&*variant),
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
    Ok(client)
}

/// What a failed activation says about the next one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationVerdict {
    /// The OS said this activation type, interface or parameter block is not supported here.
    /// It will say the same next time, so the tier is skipped for the life of the process.
    Refused,
    /// Anything else: the 1 s timeout, an RPC failure, the service still starting. Worth
    /// another attempt later.
    Transient,
}

/// Classify an activation failure by the `HRESULT` the API returned, if it returned one.
pub fn verdict_for_hresult(hr: i32) -> ActivationVerdict {
    const E_NOTIMPL: i32 = 0x8000_4001u32 as i32;
    const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
    const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
    const REGDB_E_CLASSNOTREG: i32 = 0x8004_0154u32 as i32;
    // HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)
    const E_NOT_SUPPORTED: i32 = 0x8007_0032u32 as i32;
    match hr {
        E_NOTIMPL | E_NOINTERFACE | E_INVALIDARG | REGDB_E_CLASSNOTREG | E_NOT_SUPPORTED => {
            ActivationVerdict::Refused
        }
        _ => ActivationVerdict::Transient,
    }
}

/// Classify an error from [`activate`]. A failure that carries no `HRESULT` (the timeout is the
/// one that matters) is transient.
pub fn classify_activation_error(e: &anyhow::Error) -> ActivationVerdict {
    match e.downcast_ref::<windows::core::Error>() {
        Some(w) => verdict_for_hresult(w.code().0),
        None => ActivationVerdict::Transient,
    }
}

/// Whether acquisitions should try this tier now.
///
/// Only a refusal is cached for the life of the process: caching a timeout would pin the host to
/// the endpoint tier (about 21 ms slower) until it restarted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TierGate {
    refused: bool,
    retry_at: Option<Instant>,
    /// "available" has been logged. Kept apart from failures, so a machine whose first
    /// activation failed transiently still says so once a later one works.
    success_announced: bool,
}

impl TierGate {
    pub const fn new() -> Self {
        Self {
            refused: false,
            retry_at: None,
            success_announced: false,
        }
    }

    pub fn should_try(&self, now: Instant) -> bool {
        !self.refused && self.retry_at.is_none_or(|t| now >= t)
    }

    pub fn is_refused(&self) -> bool {
        self.refused
    }

    /// Returns true the first time this process learns the tier works.
    pub fn on_success(&mut self) -> bool {
        self.retry_at = None;
        !std::mem::replace(&mut self.success_announced, true)
    }

    pub fn on_failure(&mut self, verdict: ActivationVerdict, now: Instant) {
        match verdict {
            ActivationVerdict::Refused => self.refused = true,
            ActivationVerdict::Transient => self.retry_at = Some(now + TRANSIENT_RETRY_AFTER),
        }
    }
}

static GATE: Mutex<TierGate> = Mutex::new(TierGate::new());

fn gate() -> std::sync::MutexGuard<'static, TierGate> {
    // taken only at acquisition time, never on the packet path; a poisoned gate is a stale
    // `Option<Instant>` at worst
    GATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Should an acquisition attempt process loopback now?
pub fn worth_trying() -> bool {
    gate().should_try(Instant::now())
}

/// Has this machine definitively refused the activation type?
pub fn refused() -> bool {
    gate().is_refused()
}

/// Does process loopback activate here right now? One real activation, recorded like any other;
/// the client is dropped. For the bench's environment report, which runs before any capture,
/// on a thread that has initialized COM.
#[cfg_attr(not(feature = "bench"), allow(dead_code))]
pub fn available() -> bool {
    if refused() {
        return false;
    }
    // SAFETY: on a thread without COM the activation returns CO_E_NOTINITIALIZED; nothing
    // else is required of the caller.
    unsafe { activate_recorded() }.is_ok()
}

/// [`activate`], with the outcome recorded for [`worth_trying`].
///
/// # Safety
/// Must be called on a thread with COM initialized.
pub unsafe fn activate_recorded() -> Result<IAudioClient> {
    let result = activate();
    let mut g = gate();
    match &result {
        Ok(_) => {
            if g.on_success() {
                crate::tprintln!("audio: process loopback is available on this machine");
            }
        }
        Err(e) => {
            let verdict = classify_activation_error(e);
            g.on_failure(verdict, Instant::now());
            match verdict {
                ActivationVerdict::Refused => crate::tprintln!(
                    "audio: process loopback refused by this machine ({e:#}); endpoint loopback \
                     is the only tier from now on"
                ),
                ActivationVerdict::Transient => crate::tprintln!(
                    "audio: process loopback activation failed ({e:#}); endpoint loopback for \
                     now, the process tier is tried again after {} s",
                    TRANSIENT_RETRY_AFTER.as_secs()
                ),
            }
        }
    }
    result
}

/// The format to ask the synthetic device for.
///
/// There is no mix format to query, so the caller picks: 48 kHz float32 stereo is what the Opus
/// encoder wants, so this tier never resamples.
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
        // `WAVEFORMATEX` is `#[repr(packed)]`: copy every field out before looking at it
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
        // 2 channels x 4 bytes; a wrong block align silently misreads every buffer
        assert_eq!(block_align, 8);
        assert_eq!(avg, rate * block_align as u32);
    }

    #[test]
    fn activation_params_carry_our_own_pid_and_the_exclude_mode() {
        // the mode backwards would capture only ourselves: silence, with no error anywhere
        let params = AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
            TargetProcessId: unsafe { GetCurrentProcessId() },
            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
        };
        assert_ne!(params.TargetProcessId, 0);
        assert_eq!(params.ProcessLoopbackMode.0, 1);
    }

    #[test]
    fn a_timeout_is_transient_and_is_retried_after_the_window() {
        // a timeout must not be cached as "unavailable" for the process lifetime
        let timeout =
            anyhow::anyhow!("process loopback activation did not complete within 1000 ms");
        assert_eq!(
            classify_activation_error(&timeout),
            ActivationVerdict::Transient
        );
        let t0 = Instant::now();
        let mut g = TierGate::new();
        assert!(g.should_try(t0));
        g.on_failure(ActivationVerdict::Transient, t0);
        assert!(!g.should_try(t0 + Duration::from_secs(1)));
        assert!(g.should_try(t0 + TRANSIENT_RETRY_AFTER));
        assert!(!g.is_refused());
    }

    #[test]
    fn an_rpc_failure_is_transient() {
        // RPC_E_DISCONNECTED and AUDCLNT_E_SERVICE_NOT_RUNNING: the service went away, which a
        // later attempt can survive
        for hr in [0x8001_0108u32 as i32, 0x8889_0010u32 as i32] {
            let e = anyhow::Error::from(windows::core::Error::from_hresult(HRESULT(hr)))
                .context("GetActivateResult");
            assert_eq!(classify_activation_error(&e), ActivationVerdict::Transient);
        }
    }

    #[test]
    fn a_definitive_refusal_is_cached_through_context() {
        let e = anyhow::Error::from(windows::core::Error::from_hresult(HRESULT(
            0x8000_4001u32 as i32,
        )))
        .context("process loopback activation was refused");
        assert_eq!(classify_activation_error(&e), ActivationVerdict::Refused);
        let t0 = Instant::now();
        let mut g = TierGate::new();
        g.on_failure(ActivationVerdict::Refused, t0);
        assert!(!g.should_try(t0 + Duration::from_secs(3600)));
        assert!(g.is_refused());
    }

    #[test]
    fn success_clears_a_pending_retry_and_announces_once() {
        let t0 = Instant::now();
        let mut g = TierGate::new();
        g.on_failure(ActivationVerdict::Transient, t0);
        assert!(
            g.on_success(),
            "a success after a transient failure still announces the tier once"
        );
        assert!(!g.on_success());
        assert!(g.should_try(t0));
        let mut fresh = TierGate::new();
        assert!(fresh.on_success());
        assert!(!fresh.on_success());
    }
}
