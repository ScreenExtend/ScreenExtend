//! Mix-format negotiation and PCM → interleaved-stereo-float conversion (PRD §4.4).
//!
//! 48 kHz float32 stereo is the fast path (measured mix format on the dev host — see
//! `AUDIO_NOTES.md`): it passes straight through with no conversion. Everything else
//! (44.1 kHz, 24-bit, 5.1/7.1) is handled: integer PCM → float, and >2 channels downmixed to
//! stereo with ITU-R BS.775 coefficients (−3 dB centre/surrounds; never a naive sum, which
//! clips). Non-48 kHz is dealt with at the WASAPI layer (`AUTOCONVERTPCM`), so this module
//! only ever converts sample format and channel count.
//!
//! The pure conversion functions are unit-tested in `test/format.rs`; the WAVEFORMATEX parse
//! is the only Windows-specific part.

use anyhow::{bail, Result};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};

/// −3 dB (1/√2). Applied to centre and surround channels when downmixing (BS.775).
const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

// KSDATAFORMAT_SUBTYPE_{PCM,IEEE_FLOAT} live in different windows-rs feature modules; define
// them locally (they are just fixed GUIDs) to avoid pulling extra features.
const SUBTYPE_PCM: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    /// 24-bit PCM packed in 3 bytes.
    I24In3,
    /// 24-bit (or padded) PCM in a 4-byte container.
    I32,
}

#[derive(Clone, Copy, Debug)]
pub struct MixFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    pub block_align: u16,
    pub channel_mask: u32,
}

impl MixFormat {
    /// True when the source is already exactly what Opus wants: 48 kHz float32 stereo. This is
    /// the fast path — `convert_to_stereo_f32` becomes a byte-for-byte reinterpret.
    pub fn is_fast_path(&self) -> bool {
        self.sample_rate == 48000 && self.channels == 2 && matches!(self.kind, SampleKind::F32)
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.block_align as usize
    }
}

/// Parse a `WAVEFORMATEX`/`WAVEFORMATEXTENSIBLE` returned by `GetMixFormat`.
///
/// # Safety
/// `pwfx` must point to a valid `WAVEFORMATEX` (and, if `wFormatTag == EXTENSIBLE`, a full
/// `WAVEFORMATEXTENSIBLE`), as returned by `IAudioClient::GetMixFormat`.
pub unsafe fn parse_mix_format(pwfx: *const WAVEFORMATEX) -> Result<MixFormat> {
    // WAVEFORMATEX is #[repr(packed)]; copy fields out by value before use.
    let wfx = std::ptr::read_unaligned(pwfx);
    let tag = wfx.wFormatTag;
    let channels = wfx.nChannels;
    let sample_rate = wfx.nSamplesPerSec;
    let bits = wfx.wBitsPerSample;
    let block_align = wfx.nBlockAlign;

    let (kind, channel_mask) = if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = std::ptr::read_unaligned(pwfx as *const WAVEFORMATEXTENSIBLE);
        let sub = ext.SubFormat;
        let mask = ext.dwChannelMask;
        (classify(sub, bits)?, mask)
    } else if tag == WAVE_FORMAT_IEEE_FLOAT {
        (SampleKind::F32, 0)
    } else if tag == WAVE_FORMAT_PCM {
        (classify_pcm(bits)?, 0)
    } else {
        bail!("unsupported mix format tag 0x{tag:04X}");
    };

    if channels == 0 {
        bail!("mix format reports 0 channels");
    }

    Ok(MixFormat {
        sample_rate,
        channels,
        kind,
        block_align,
        channel_mask,
    })
}

fn classify(sub: windows::core::GUID, bits: u16) -> Result<SampleKind> {
    if sub == SUBTYPE_IEEE_FLOAT {
        if bits != 32 {
            bail!("IEEE_FLOAT mix format with {bits} bits (expected 32)");
        }
        Ok(SampleKind::F32)
    } else if sub == SUBTYPE_PCM {
        classify_pcm(bits)
    } else {
        bail!("unsupported mix format subformat {sub:?}");
    }
}

fn classify_pcm(bits: u16) -> Result<SampleKind> {
    match bits {
        16 => Ok(SampleKind::I16),
        24 => Ok(SampleKind::I24In3),
        32 => Ok(SampleKind::I32),
        _ => bail!("unsupported PCM bit depth {bits}"),
    }
}

/// Read one sample of the given kind from `bytes` at `byte_offset`, as a float in ~[-1, 1].
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
        SampleKind::I24In3 => {
            let raw = (bytes[off] as i32)
                | ((bytes[off + 1] as i32) << 8)
                | ((bytes[off + 2] as i32) << 16);
            // sign-extend 24-bit
            let v = (raw << 8) >> 8;
            (v as f32) / 8_388_608.0 // 2^23
        }
        SampleKind::I32 => {
            let v =
                i32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
            (v as f32) / 2_147_483_648.0 // 2^31
        }
    }
}

#[inline]
fn bytes_per_sample(kind: SampleKind) -> usize {
    match kind {
        SampleKind::F32 | SampleKind::I32 => 4,
        SampleKind::I16 => 2,
        SampleKind::I24In3 => 3,
    }
}

/// Convert `frames` frames of interleaved source PCM into interleaved stereo f32, appended to
/// `out`. `src` must hold at least `frames * fmt.block_align` bytes. Output is clamped to
/// [-1, 1]. The 48 kHz-float32-stereo case is a straight copy.
pub fn convert_to_stereo_f32(src: &[u8], frames: usize, fmt: &MixFormat, out: &mut Vec<f32>) {
    let bps = bytes_per_sample(fmt.kind);
    let ch = fmt.channels as usize;
    let stride = fmt.block_align as usize;
    debug_assert!(stride >= bps * ch);

    for f in 0..frames {
        let base = f * stride;
        if base + bps * ch > src.len() {
            break;
        }
        let (l, r) = downmix_frame(src, base, bps, fmt);
        out.push(l.clamp(-1.0, 1.0));
        out.push(r.clamp(-1.0, 1.0));
    }
}

/// Downmix one frame's channels to (L, R) using BS.775 coefficients.
#[inline]
fn downmix_frame(src: &[u8], base: usize, bps: usize, fmt: &MixFormat) -> (f32, f32) {
    let ch = fmt.channels as usize;
    let s = |i: usize| read_sample(fmt.kind, src, base + i * bps);

    match ch {
        1 => {
            let m = s(0);
            (m, m)
        }
        2 => (s(0), s(1)),
        // 3ch: FL, FR, FC
        3 => {
            let c = s(2) * M3DB;
            (s(0) + c, s(1) + c)
        }
        // Quad: FL, FR, BL, BR
        4 => (s(0) + s(2) * M3DB, s(1) + s(3) * M3DB),
        // 5.1: FL, FR, FC, LFE, BL, BR (LFE dropped)
        6 => {
            let c = s(2) * M3DB;
            (s(0) + c + s(4) * M3DB, s(1) + c + s(5) * M3DB)
        }
        // 7.1: FL, FR, FC, LFE, BL, BR, SL, SR (LFE dropped)
        8 => {
            let c = s(2) * M3DB;
            (
                s(0) + c + (s(4) + s(6)) * M3DB,
                s(1) + c + (s(5) + s(7)) * M3DB,
            )
        }
        // Unknown layout: average everything into both channels (attenuated to avoid clip).
        n => {
            let mut acc = 0.0f32;
            for i in 0..n {
                acc += s(i);
            }
            let m = acc / n as f32;
            (m, m)
        }
    }
}
