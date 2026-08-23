//! Opus encoder wrapper — lowest-delay configuration (PRD §5.2), RAII cleanup.
//!
//! `OPUS_APPLICATION_RESTRICTED_LOWDELAY` is the whole point: it disables SILK and forces
//! CELT-only, cutting algorithmic delay from ~26.5 ms to ~6.5 ms at 48 kHz. The handle is
//! wrapped so `opus_encoder_destroy` can never be skipped on an early return or `?`.

use anyhow::{bail, Result};

use super::opus_sys::{self, OpusApi};

/// 48 kHz is Opus's native rate and the mix-format fast path (see `AUDIO_NOTES.md`).
pub const OPUS_SAMPLE_RATE: u32 = 48000;
/// Default frame length. 5 ms = 240 samples/channel @ 48 kHz (PRD §5.2 / §3.5 default).
pub const FRAME_MS: u32 = 5;
/// Samples per channel in one 5 ms frame at 48 kHz.
pub const FRAME_SAMPLES: usize = (OPUS_SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;
/// 128 kbps stereo default (PRD §5.2). No UI control for it in v1.
pub const DEFAULT_BITRATE_BPS: i32 = 128_000;
/// Complexity 5, not 10 — quality we won't hear on system audio, for encode time we keep (§5.2).
pub const DEFAULT_COMPLEXITY: i32 = 5;

/// Generous upper bound for a single Opus packet. A 5 ms 128 kbps frame is ~80 bytes; a
/// single Opus frame is capped at 1275 bytes by the format. 4000 covers any VBR spike.
const MAX_PACKET_BYTES: usize = 4000;

#[derive(Debug, Clone, Copy)]
pub struct OpusEncoderConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate_bps: i32,
    pub complexity: i32,
    /// Samples per channel per encoded frame.
    pub frame_samples: usize,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            sample_rate: OPUS_SAMPLE_RATE,
            channels: 2,
            bitrate_bps: DEFAULT_BITRATE_BPS,
            complexity: DEFAULT_COMPLEXITY,
            frame_samples: FRAME_SAMPLES,
        }
    }
}

pub struct OpusEncoder {
    api: OpusApi,
    handle: *mut opus_sys::OpusEncoder,
    channels: usize,
    frame_samples: usize,
    scratch: Vec<u8>,
    lookahead_samples: i32,
}

// SAFETY: the encoder owns its `OpusEncoder*` exclusively; it is never aliased. libopus
// encoder state is not internally synchronized, so this is `Send` (moved to the capture
// thread) but intentionally not `Sync`. Mirrors `X264Encoder`.
unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(config: OpusEncoderConfig) -> Result<Self> {
        let api = OpusApi::load()?;
        Self::with_api(api, config)
    }

    pub fn with_api(api: OpusApi, config: OpusEncoderConfig) -> Result<Self> {
        if config.channels != 1 && config.channels != 2 {
            bail!(
                "opus: unsupported channel count {} (must be 1 or 2)",
                config.channels
            );
        }
        if config.sample_rate != OPUS_SAMPLE_RATE {
            bail!(
                "opus: encoder must run at {OPUS_SAMPLE_RATE} Hz (got {}); resample upstream",
                config.sample_rate
            );
        }
        let channels = config.channels as usize;

        let mut err: std::ffi::c_int = 0;
        // SAFETY: valid Fs/channels/application per opus.h; `err` receives the status.
        let handle = unsafe {
            (api.encoder_create)(
                config.sample_rate as i32,
                config.channels as std::ffi::c_int,
                opus_sys::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if handle.is_null() || err != opus_sys::OPUS_OK {
            bail!("opus_encoder_create failed: {}", api.strerror(err));
        }

        let mut enc = Self {
            api,
            handle,
            channels,
            frame_samples: config.frame_samples,
            scratch: vec![0u8; MAX_PACKET_BYTES],
            lookahead_samples: 0,
        };

        // Lowest-delay configuration (§5.2). Any failure here is fatal — a silently
        // misconfigured encoder is worse than none.
        enc.ctl_set(opus_sys::OPUS_SET_BITRATE_REQUEST, config.bitrate_bps)?;
        enc.ctl_set(opus_sys::OPUS_SET_COMPLEXITY_REQUEST, config.complexity)?;
        enc.ctl_set(
            opus_sys::OPUS_SET_SIGNAL_REQUEST,
            opus_sys::OPUS_SIGNAL_MUSIC,
        )?;
        enc.ctl_set(opus_sys::OPUS_SET_DTX_REQUEST, 0)?; // no DTX — it reintroduces timeline gaps
        enc.ctl_set(opus_sys::OPUS_SET_INBAND_FEC_REQUEST, 0)?; // custom decoder doesn't use FEC yet
        enc.ctl_set(opus_sys::OPUS_SET_VBR_REQUEST, 1)?;
        enc.ctl_set(opus_sys::OPUS_SET_VBR_CONSTRAINT_REQUEST, 0)?; // unconstrained VBR = lowest latency

        enc.lookahead_samples = enc
            .ctl_get(opus_sys::OPUS_GET_LOOKAHEAD_REQUEST)
            .unwrap_or(0);

        Ok(enc)
    }

    fn ctl_set(&self, request: std::ffi::c_int, value: i32) -> Result<()> {
        // SAFETY: `handle` is a live encoder; every SET request we issue takes one i32.
        let rv = unsafe { (self.api.encoder_ctl_set)(self.handle, request, value) };
        if rv != opus_sys::OPUS_OK {
            bail!(
                "opus_encoder_ctl(request={request}, value={value}) failed: {}",
                self.api.strerror(rv)
            );
        }
        Ok(())
    }

    fn ctl_get(&self, request: std::ffi::c_int) -> Result<i32> {
        let mut out: i32 = 0;
        // SAFETY: `handle` is live; the GET requests we issue write one i32 through `out`.
        let rv = unsafe { (self.api.encoder_ctl_get)(self.handle, request, &mut out) };
        if rv != opus_sys::OPUS_OK {
            bail!(
                "opus_encoder_ctl(get request={request}) failed: {}",
                self.api.strerror(rv)
            );
        }
        Ok(out)
    }

    /// Encode one frame of interleaved float32 PCM. `pcm.len()` must be
    /// `frame_samples * channels`. Returns the encoded Opus packet (a borrow of the
    /// encoder's preallocated scratch — no per-frame allocation, per §8.4).
    pub fn encode_float(&mut self, pcm: &[f32]) -> Result<&[u8]> {
        let expected = self.frame_samples * self.channels;
        if pcm.len() != expected {
            bail!(
                "opus encode: expected {expected} interleaved samples ({} frames x{}ch), got {}",
                self.frame_samples,
                self.channels,
                pcm.len()
            );
        }
        // SAFETY: `pcm` holds `frame_samples*channels` floats; `scratch` has MAX_PACKET_BYTES
        // of writable space; `handle` is a live encoder.
        let n = unsafe {
            (self.api.encode_float)(
                self.handle,
                pcm.as_ptr(),
                self.frame_samples as std::ffi::c_int,
                self.scratch.as_mut_ptr(),
                self.scratch.len() as i32,
            )
        };
        if n < 0 {
            bail!("opus_encode_float failed: {}", self.api.strerror(n));
        }
        Ok(&self.scratch[..n as usize])
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Encoder algorithmic look-ahead in samples (for A/V-sync accounting, §6.5).
    pub fn lookahead_samples(&self) -> i32 {
        self.lookahead_samples
    }

    pub fn version(&self) -> String {
        self.api.version()
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        if self.handle.is_null() {
            return;
        }
        // SAFETY: `handle` was returned by opus_encoder_create and not yet destroyed.
        unsafe { (self.api.encoder_destroy)(self.handle) };
        self.handle = std::ptr::null_mut();
    }
}
