use anyhow::{bail, Result};

use super::opus_sys::{self, OpusApi};

pub const OPUS_SAMPLE_RATE: u32 = 48000;

/// Frame length. A deliberate latency-for-overhead trade that was previously unrecorded.
///
/// 5 ms is 200 packets/s. At 128 kbps the payload measures ~118 bytes of speech (3 bytes of
/// pure silence) against a 13-byte app header plus roughly 50-70 bytes of SCTP/DTLS/UDP/IP,
/// so well over a third of the audio bitrate on the wire is framing. 10 ms would halve that
/// for +5 ms of latency; 2.5 ms is legal in CELT-only and would save 2.5 ms of the 7.5 ms
/// codec delay while doubling an already overhead-dominated packet rate. 5 ms is the balance
/// point for this product, and the total algorithmic delay is frame + lookahead = 5 + 2.5 ms,
/// not 5.
pub const FRAME_MS: u32 = 5;
pub const FRAME_SAMPLES: usize = (OPUS_SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;
pub const DEFAULT_BITRATE_BPS: i32 = 128_000;
/// Encode time at this setting measures p50 0.07-0.1 ms / p99 0.33 ms on the dev box, three
/// orders of magnitude under the frame it produces, so there is nothing to buy by lowering it.
pub const DEFAULT_COMPLEXITY: i32 = 5;
/// Source depth. The mix arrives as float32, so 24 is the honest answer and also libopus's
/// default; stated explicitly because the alternative is a silent assumption.
pub const DEFAULT_LSB_DEPTH: i32 = 24;

const MAX_PACKET_BYTES: usize = 4000;

#[derive(Debug, Clone, Copy)]
pub struct OpusEncoderConfig {
    pub sample_rate: u32,
    pub channels: u16,
    pub bitrate_bps: i32,
    pub complexity: i32,
    pub frame_samples: usize,
    /// `OPUS_SET_PREDICTION_DISABLED`. Makes every frame nearly independent, which is what a
    /// zero-retransmit unordered transport wants — a lost frame then does not degrade its
    /// successors. It costs bitrate at fixed quality, so it is a listening-test decision, not
    /// a free one, and it stays off until someone can A/B it on real hardware. Flipping this
    /// default is the whole change.
    pub disable_prediction: bool,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            sample_rate: OPUS_SAMPLE_RATE,
            channels: 2,
            bitrate_bps: DEFAULT_BITRATE_BPS,
            complexity: DEFAULT_COMPLEXITY,
            frame_samples: FRAME_SAMPLES,
            disable_prediction: false,
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
        // SAFETY: valid Fs/channels/application per opus.h; `err` receives the status
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

        enc.ctl_set(opus_sys::OPUS_SET_BITRATE_REQUEST, config.bitrate_bps)?;
        enc.ctl_set(opus_sys::OPUS_SET_COMPLEXITY_REQUEST, config.complexity)?;
        enc.ctl_set(
            opus_sys::OPUS_SET_SIGNAL_REQUEST,
            opus_sys::OPUS_SIGNAL_MUSIC,
        )?;
        enc.ctl_set(opus_sys::OPUS_SET_LSB_DEPTH_REQUEST, DEFAULT_LSB_DEPTH)?;
        enc.ctl_set(opus_sys::OPUS_SET_VBR_REQUEST, 1)?;
        // Constrained VBR (libopus's own default, which the previous code explicitly turned
        // off). Unconstrained VBR lets one frame's byte count run well above the nominal rate
        // on a transient, and at 200 packets/s over Wi-Fi a size spike is an airtime spike is
        // arrival jitter — which the client answers by *growing* its jitter buffer. Encoder
        // peakiness converts fairly directly into client-side latency, so the bounded mode is
        // the one a real-time stream wants.
        enc.ctl_set(opus_sys::OPUS_SET_VBR_CONSTRAINT_REQUEST, 1)?;
        if config.disable_prediction {
            enc.ctl_set(opus_sys::OPUS_SET_PREDICTION_DISABLED_REQUEST, 1)?;
        }
        // `OPUS_SET_DTX` and `OPUS_SET_INBAND_FEC` are deliberately *not* issued. libopus
        // documents both as "only applicable to the LPC layer"; the LPC layer is SILK, and
        // `OPUS_APPLICATION_RESTRICTED_LOWDELAY` plus a sub-10 ms frame both force CELT-only.
        // Setting either to 0 was writing down a default, and setting either to 1 would change
        // nothing at all — so neither is an answer to packet loss here, and the code should
        // not imply that it is. The answer is `protocol::FLAG_REDUNDANT`.

        enc.lookahead_samples = match enc.ctl_get(opus_sys::OPUS_GET_LOOKAHEAD_REQUEST) {
            Ok(v) => v,
            Err(e) => {
                // A silent 0 here would be a claim of zero algorithmic delay, and the value is
                // reported to the client and used to align playout.
                crate::teprintln!("opus: could not read lookahead ({e:#}); reporting 0");
                0
            }
        };

        Ok(enc)
    }

    /// What the encoder says it is, rather than what we asked for.
    ///
    /// `ctl_set` bails on a rejected value, which is better than most FFI wrappers manage, but
    /// nothing ever asked the encoder to confirm its state — and Windows and macOS load
    /// different libopus builds (`libopus.dll` / `libopus.dylib`), so "it worked on the other
    /// platform" is not evidence.
    pub fn describe_state(&self) -> String {
        let get = |req| {
            self.ctl_get(req)
                .map(|v| v.to_string())
                .unwrap_or_else(|_| "?".to_string())
        };
        format!(
            "opus: {} — application={} bitrate={} complexity={} signal={} vbr={} \
             vbr_constraint={} lsb_depth={} prediction_disabled={} lookahead={} samples \
             ({:.2} ms), frame {} ms",
            self.api.version(),
            get(opus_sys::OPUS_GET_APPLICATION_REQUEST),
            get(opus_sys::OPUS_GET_BITRATE_REQUEST),
            get(opus_sys::OPUS_GET_COMPLEXITY_REQUEST),
            get(opus_sys::OPUS_GET_SIGNAL_REQUEST),
            get(opus_sys::OPUS_GET_VBR_REQUEST),
            get(opus_sys::OPUS_GET_VBR_CONSTRAINT_REQUEST),
            get(opus_sys::OPUS_GET_LSB_DEPTH_REQUEST),
            get(opus_sys::OPUS_GET_PREDICTION_DISABLED_REQUEST),
            self.lookahead_samples,
            self.lookahead_samples as f64 * 1000.0 / OPUS_SAMPLE_RATE as f64,
            FRAME_MS,
        )
    }

    fn ctl_set(&self, request: std::ffi::c_int, value: i32) -> Result<()> {
        // SAFETY: `handle` is a live encoder; every SET request we issue takes one i32
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
        // SAFETY: `handle` is live; the GET requests we issue write one i32 through `out`
        let rv = unsafe { (self.api.encoder_ctl_get)(self.handle, request, &mut out) };
        if rv != opus_sys::OPUS_OK {
            bail!(
                "opus_encoder_ctl(get request={request}) failed: {}",
                self.api.strerror(rv)
            );
        }
        Ok(out)
    }

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
        unsafe { (self.api.encoder_destroy)(self.handle) };
        self.handle = std::ptr::null_mut();
    }
}
