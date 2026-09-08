//! The Windows shared-mode engine period: what it is on this machine, and how to lower it.
//!
//! The engine runs **one periodicity for the whole endpoint**, and Microsoft documents that any
//! stream may set it: "if one application requests the usage of small buffers, then the audio
//! engine will start transferring audio using that particular buffer size. In that case, all
//! applications that use the same endpoint and mode will automatically switch to that small
//! buffer size" ([Low Latency Audio], FAQ).
//!
//! That matters because our loopback capture stream *cannot* ask for itself.
//! `IAudioClient3::InitializeSharedAudioStream` lists its supported flags exhaustively and
//! `AUDCLNT_STREAMFLAGS_LOOPBACK` is not among them; this repo measured the refusal directly
//! (`docs/recovered/AUDIO_NOTES.md` §3.1, `AUDCLNT_E_INVALID_STREAM_FLAG` 0x88890021). The
//! silent render companion has no loopback flag, so it can, and dragging the engine down drags
//! the capture stream with it.
//!
//! Everything here is a query first: nothing is assumed about what a driver supports, because
//! it varies per machine. On the dev box (Windows 11 26200) the driver reports
//! `default = fundamental = min = max = 480 frames`, i.e. no small buffer exists at all and the
//! negotiation is a correctly-detected no-op.
//!
//! [Low Latency Audio]: https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/low-latency-audio

use windows::core::Interface;
use windows::Win32::Media::Audio::{IAudioClient, IAudioClient3, WAVEFORMATEX};

/// `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` — another client already pinned the engine period.
pub const AUDCLNT_E_ENGINE_PERIODICITY_LOCKED: i32 = 0x8889_0028u32 as i32;
/// `AUDCLNT_E_INVALID_DEVICE_PERIOD`
pub const AUDCLNT_E_INVALID_DEVICE_PERIOD: i32 = 0x8889_0020u32 as i32;

/// The four frame counts `IAudioClient3::GetSharedModeEnginePeriod` reports.
///
/// Legal requests are multiples of `fundamental` between `min` and `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnginePeriods {
    pub default_frames: u32,
    pub fundamental_frames: u32,
    pub min_frames: u32,
    pub max_frames: u32,
}

impl EnginePeriods {
    /// The smallest legal period at or above `min`, or `None` when the driver exposes no
    /// variant below its default — the common case, and not a failure.
    pub fn lower_than_default(&self) -> Option<u32> {
        if self.min_frames == 0 || self.min_frames >= self.default_frames {
            return None;
        }
        Some(self.min_frames)
    }

    pub fn describe(&self, sample_rate: u32) -> String {
        let ms = |f: u32| {
            if sample_rate == 0 {
                0.0
            } else {
                f as f64 * 1000.0 / sample_rate as f64
            }
        };
        format!(
            "default={}fr ({:.3}ms) fundamental={}fr ({:.3}ms) min={}fr ({:.3}ms) max={}fr ({:.3}ms)",
            self.default_frames,
            ms(self.default_frames),
            self.fundamental_frames,
            ms(self.fundamental_frames),
            self.min_frames,
            ms(self.min_frames),
            self.max_frames,
            ms(self.max_frames),
        )
    }
}

/// Query the endpoint's engine periods. `None` on pre-1607 (no `IAudioClient3`) or any refusal.
///
/// # Safety
/// `pwfx` must point at a valid `WAVEFORMATEX` for this endpoint.
pub unsafe fn query_engine_periods(
    client: &IAudioClient,
    pwfx: *const WAVEFORMATEX,
) -> Option<EnginePeriods> {
    let c3 = client.cast::<IAudioClient3>().ok()?;
    let (mut default_frames, mut fundamental_frames, mut min_frames, mut max_frames) = (0, 0, 0, 0);
    c3.GetSharedModeEnginePeriod(
        pwfx,
        &mut default_frames,
        &mut fundamental_frames,
        &mut min_frames,
        &mut max_frames,
    )
    .ok()?;
    Some(EnginePeriods {
        default_frames,
        fundamental_frames,
        min_frames,
        max_frames,
    })
}

/// `IAudioClient::GetDevicePeriod`, in **frames** at `sample_rate`.
///
/// Returns `(default, minimum)`. This is the period, not `GetBufferSize`'s total engine buffer,
/// which is several periods deep — the two were being conflated in the diagnostics.
///
/// # Safety
/// `client` must be a live `IAudioClient` for the endpoint.
pub unsafe fn device_period_frames(client: &IAudioClient, sample_rate: u32) -> Option<(u32, u32)> {
    let mut default_100ns: i64 = 0;
    let mut min_100ns: i64 = 0;
    client
        .GetDevicePeriod(Some(&mut default_100ns), Some(&mut min_100ns))
        .ok()?;
    let to_frames = |hns: i64| -> u32 {
        if hns <= 0 || sample_rate == 0 {
            return 0;
        }
        // REFERENCE_TIME is 100 ns units; frames = seconds * rate.
        ((hns as i128 * sample_rate as i128) / 10_000_000i128) as u32
    };
    Some((to_frames(default_100ns), to_frames(min_100ns)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_small_buffer_reports_none() {
        // The dev box: one period, no low-latency variant.
        let p = EnginePeriods {
            default_frames: 480,
            fundamental_frames: 480,
            min_frames: 480,
            max_frames: 480,
        };
        assert_eq!(p.lower_than_default(), None);
    }

    #[test]
    fn in_box_hdaudio_range_reports_the_minimum() {
        // The in-box High Definition Audio Device driver: 128..480 frames.
        let p = EnginePeriods {
            default_frames: 480,
            fundamental_frames: 128,
            min_frames: 128,
            max_frames: 480,
        };
        assert_eq!(p.lower_than_default(), Some(128));
    }

    #[test]
    fn a_zero_minimum_is_not_a_request() {
        let p = EnginePeriods {
            default_frames: 480,
            fundamental_frames: 0,
            min_frames: 0,
            max_frames: 480,
        };
        assert_eq!(p.lower_than_default(), None);
    }

    #[test]
    fn describe_converts_frames_to_milliseconds() {
        let p = EnginePeriods {
            default_frames: 480,
            fundamental_frames: 480,
            min_frames: 128,
            max_frames: 480,
        };
        let s = p.describe(48_000);
        assert!(s.contains("default=480fr (10.000ms)"), "{s}");
        assert!(s.contains("min=128fr (2.667ms)"), "{s}");
    }
}
