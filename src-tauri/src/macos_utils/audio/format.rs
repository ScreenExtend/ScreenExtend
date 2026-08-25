//! `AudioStreamBasicDescription` parsing and PCM → interleaved-stereo-f32 conversion for the
//! macOS backends (PRD §5.4).
//!
//! Unlike the Windows path, macOS delivers either an interleaved or a **planar (non-interleaved)**
//! `AudioBufferList` described by an `AudioStreamBasicDescription` (ASBD), not a WASAPI
//! `WAVEFORMATEXTENSIBLE`. ScreenCaptureKit in particular hands us 48 kHz stereo float32 in
//! *separate per-channel buffers* (`kAudioFormatFlagIsNonInterleaved`), a layout Windows never
//! produces — so this module parses the real ASBD and handles both layouts.
//!
//! The channel **downmix** math (ITU-R BS.775: −3 dB centre/surrounds, never a naive sum) is the
//! same as the Windows sibling `windows_utils/audio/format.rs`. It is deliberately duplicated
//! here rather than shared: the surrounding format-descriptor parse (ASBD vs WAVEFORMATEX) and the
//! planar-vs-interleaved reader diverge enough that a shared abstraction would be leaky (PRD §5.4
//! — "duplication with a comment pointing at the sibling file is better than a leaky shared
//! abstraction"). If you touch a downmix coefficient here, touch it there too.
//!
//! The converters write into a caller-provided output slice (not a growing `Vec`) so the
//! real-time capture callback never allocates (PRD §9.2).

use anyhow::{bail, Result};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsSignedInteger,
    AudioStreamBasicDescription,
};

/// −3 dB (1/√2). Applied to centre and surround channels when downmixing (BS.775).
const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Opus's target: 48 kHz stereo. The output of every converter here.
pub const OUT_CHANNELS: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    I32,
}

impl SampleKind {
    #[inline]
    fn bytes(self) -> usize {
        match self {
            SampleKind::F32 | SampleKind::I32 => 4,
            SampleKind::I16 => 2,
        }
    }
}

/// A parsed ASBD, reduced to what the converters need.
#[derive(Clone, Copy, Debug)]
pub struct AudioFormatDesc {
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    /// True when each channel is a separate buffer (`kAudioFormatFlagIsNonInterleaved`).
    pub non_interleaved: bool,
}

impl AudioFormatDesc {
    /// True when the source is already exactly Opus-ready: 48 kHz, stereo, float32, interleaved.
    pub fn is_fast_path(&self) -> bool {
        self.sample_rate == 48000
            && self.channels == 2
            && self.kind == SampleKind::F32
            && !self.non_interleaved
    }
}

/// Parse an `AudioStreamBasicDescription` (from `kAudioTapPropertyFormat` on the Process Tap side,
/// or `CMAudioFormatDescriptionGetStreamBasicDescription` on the SCK side).
pub fn parse_asbd(asbd: &AudioStreamBasicDescription) -> Result<AudioFormatDesc> {
    let flags = asbd.mFormatFlags;
    let bits = asbd.mBitsPerChannel;
    let channels = asbd.mChannelsPerFrame;
    if channels == 0 {
        bail!("ASBD reports 0 channels");
    }

    let is_float = flags & kAudioFormatFlagIsFloat != 0;
    let is_signed_int = flags & kAudioFormatFlagIsSignedInteger != 0;
    let non_interleaved = flags & kAudioFormatFlagIsNonInterleaved != 0;

    let kind = if is_float {
        if bits != 32 {
            bail!("float ASBD with {bits} bits (expected 32)");
        }
        SampleKind::F32
    } else if is_signed_int {
        match bits {
            16 => SampleKind::I16,
            32 => SampleKind::I32,
            other => bail!("unsupported signed-int ASBD bit depth {other}"),
        }
    } else {
        bail!("unsupported ASBD format flags 0x{flags:08x} ({bits}-bit) — not float or signed int");
    };

    Ok(AudioFormatDesc {
        sample_rate: asbd.mSampleRate as u32,
        channels: channels as u16,
        kind,
        non_interleaved,
    })
}

/// Read one sample of the given kind from `bytes` at `off`, as a float in ~[-1, 1].
#[inline]
fn read_sample(kind: SampleKind, bytes: &[u8], off: usize) -> f32 {
    match kind {
        SampleKind::F32 => {
            f32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        }
        // Divide by 32768, not 32767: the int16 range is asymmetric (−32768..=32767).
        SampleKind::I16 => {
            let v = i16::from_le_bytes([bytes[off], bytes[off + 1]]);
            (v as f32) / 32768.0
        }
        SampleKind::I32 => {
            let v =
                i32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
            (v as f32) / 2_147_483_648.0 // 2^31
        }
    }
}

/// Downmix one frame's channel values to (L, R) using BS.775 coefficients. Mirrors
/// `windows_utils/audio/format.rs::downmix_frame` — keep the two in sync (see module docs).
#[inline]
fn downmix_channels(ch: &[f32]) -> (f32, f32) {
    match ch.len() {
        0 => (0.0, 0.0),
        1 => (ch[0], ch[0]),
        2 => (ch[0], ch[1]),
        // 3ch: FL, FR, FC
        3 => {
            let c = ch[2] * M3DB;
            (ch[0] + c, ch[1] + c)
        }
        // Quad: FL, FR, BL, BR
        4 => (ch[0] + ch[2] * M3DB, ch[1] + ch[3] * M3DB),
        // 5.1: FL, FR, FC, LFE, BL, BR (LFE dropped)
        6 => {
            let c = ch[2] * M3DB;
            (ch[0] + c + ch[4] * M3DB, ch[1] + c + ch[5] * M3DB)
        }
        // 7.1: FL, FR, FC, LFE, BL, BR, SL, SR (LFE dropped)
        8 => {
            let c = ch[2] * M3DB;
            (
                ch[0] + c + (ch[4] + ch[6]) * M3DB,
                ch[1] + c + (ch[5] + ch[7]) * M3DB,
            )
        }
        // Unknown layout: average everything into both channels (attenuated to avoid clip).
        n => {
            let m = ch.iter().sum::<f32>() / n as f32;
            (m, m)
        }
    }
}

/// Convert `frames` frames of **interleaved** source PCM into interleaved stereo f32, writing
/// `2 * frames` samples into `out`. Returns the number of f32 written. `out` must hold at least
/// `2 * frames`; `src` must hold at least `frames * channels * bytes_per_sample`.
pub fn convert_interleaved(
    src: &[u8],
    frames: usize,
    desc: &AudioFormatDesc,
    out: &mut [f32],
) -> usize {
    let ch = desc.channels as usize;
    let bps = desc.kind.bytes();
    let stride = ch * bps;
    let mut scratch = [0.0f32; 8];
    let mut written = 0;
    for f in 0..frames {
        let base = f * stride;
        if base + stride > src.len() || written + OUT_CHANNELS > out.len() {
            break;
        }
        let (l, r) = if ch <= scratch.len() {
            for (c, slot) in scratch[..ch].iter_mut().enumerate() {
                *slot = read_sample(desc.kind, src, base + c * bps);
            }
            downmix_channels(&scratch[..ch])
        } else {
            // >8 channels: average without the fixed scratch.
            let mut acc = 0.0f32;
            for c in 0..ch {
                acc += read_sample(desc.kind, src, base + c * bps);
            }
            let m = acc / ch as f32;
            (m, m)
        };
        out[written] = l.clamp(-1.0, 1.0);
        out[written + 1] = r.clamp(-1.0, 1.0);
        written += OUT_CHANNELS;
    }
    written
}

/// Convert `frames` frames of **planar (non-interleaved)** source PCM — one byte slice per
/// channel — into interleaved stereo f32, writing `2 * frames` samples into `out`. Returns the
/// number of f32 written. This is the ScreenCaptureKit / non-interleaved Process Tap layout.
pub fn convert_planar(
    planes: &[&[u8]],
    frames: usize,
    desc: &AudioFormatDesc,
    out: &mut [f32],
) -> usize {
    let ch = planes.len().min(desc.channels as usize);
    let bps = desc.kind.bytes();
    let mut scratch = [0.0f32; 8];
    let mut written = 0;
    for f in 0..frames {
        let off = f * bps;
        if written + OUT_CHANNELS > out.len() {
            break;
        }
        let mut short = false;
        let n = ch.min(scratch.len());
        for (c, slot) in scratch[..n].iter_mut().enumerate() {
            if off + bps > planes[c].len() {
                short = true;
                break;
            }
            *slot = read_sample(desc.kind, planes[c], off);
        }
        if short {
            break;
        }
        let (l, r) = downmix_channels(&scratch[..n]);
        out[written] = l.clamp(-1.0, 1.0);
        out[written + 1] = r.clamp(-1.0, 1.0);
        written += OUT_CHANNELS;
    }
    written
}
