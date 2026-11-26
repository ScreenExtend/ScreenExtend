use anyhow::{bail, Result};
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};

const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
/// Bytes a `WAVEFORMATEXTENSIBLE` carries after its `WAVEFORMATEX` header.
const EXTENSIBLE_CB_SIZE: usize =
    std::mem::size_of::<WAVEFORMATEXTENSIBLE>() - std::mem::size_of::<WAVEFORMATEX>();

const SUBTYPE_PCM: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleKind {
    F32,
    I16,
    I24In3,
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
    pub fn is_fast_path(&self) -> bool {
        self.sample_rate == 48000 && self.channels == 2 && matches!(self.kind, SampleKind::F32)
    }

    pub fn bytes_per_frame(&self) -> usize {
        self.block_align as usize
    }
}

pub unsafe fn parse_mix_format(pwfx: *const WAVEFORMATEX) -> Result<MixFormat> {
    let wfx = std::ptr::read_unaligned(pwfx);
    let tag = wfx.wFormatTag;
    let channels = wfx.nChannels;
    let sample_rate = wfx.nSamplesPerSec;
    let bits = wfx.wBitsPerSample;
    let block_align = wfx.nBlockAlign;

    let (kind, channel_mask) = if tag == WAVE_FORMAT_EXTENSIBLE {
        // the extension is there only if `cbSize` says so: a `WAVEFORMATEXTENSIBLE` read through
        // a shorter `CoTaskMemAlloc` block is out of bounds
        let cb_size = wfx.cbSize;
        if (cb_size as usize) < EXTENSIBLE_CB_SIZE {
            bail!(
                "WAVE_FORMAT_EXTENSIBLE mix format with cbSize {cb_size} (needs {EXTENSIBLE_CB_SIZE})"
            );
        }
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

#[inline]
fn read_sample(kind: SampleKind, bytes: &[u8], off: usize) -> f32 {
    match kind {
        SampleKind::F32 => {
            f32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        }
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

/// The default sample ceiling: a hard clip at full scale.
pub const HARD_LIMIT: f32 = 1.0;
/// The wider ceiling `audio_wide_sample_limit` selects. The shared-mode float mix can exceed
/// 0 dBFS and `opus_encode_float` accepts it; this only keeps a garbage value from reaching the
/// encoder.
pub const WIDE_LIMIT: f32 = 2.0;

/// Clamp to `+/-limit`, and turn a NaN into silence: `f32::clamp` passes NaN straight through.
#[inline]
pub fn limit_sample(x: f32, limit: f32) -> f32 {
    if x.is_nan() {
        0.0
    } else {
        x.clamp(-limit, limit)
    }
}

/// Convert to interleaved stereo f32 at full scale. Kept for the tests and any caller that
/// does not resample; the capture path uses [`convert_into`].
pub fn convert_to_stereo_f32(src: &[u8], frames: usize, fmt: &MixFormat, out: &mut Vec<f32>) {
    convert_into(src, frames, fmt, HARD_LIMIT, None, out);
}

/// Convert `frames` frames of `fmt` to interleaved stereo f32, limited to `+/-limit`, and
/// resampled on the way into `out` when the source is not at the output rate.
///
/// Runs on the real-time capture thread: no allocation beyond `out`'s own growth.
pub fn convert_into(
    src: &[u8],
    frames: usize,
    fmt: &MixFormat,
    limit: f32,
    resampler: Option<&mut StereoResampler>,
    out: &mut Vec<f32>,
) {
    if fmt.is_fast_path() && resampler.is_none() {
        let n = (frames * 2).min(src.len() / 4);
        out.reserve(n);
        for c in src[..n * 4].as_chunks::<4>().0 {
            out.push(limit_sample(f32::from_le_bytes(*c), limit));
        }
        return;
    }

    let bps = bytes_per_sample(fmt.kind);
    let ch = fmt.channels as usize;
    let stride = fmt.block_align as usize;
    debug_assert!(stride >= bps * ch);

    let mut resampler = resampler;
    for f in 0..frames {
        let base = f * stride;
        if base + bps * ch > src.len() {
            break;
        }
        let (l, r) = downmix_frame(src, base, bps, fmt);
        let (l, r) = (limit_sample(l, limit), limit_sample(r, limit));
        match resampler.as_deref_mut() {
            Some(rs) => rs.push(l, r, out),
            None => {
                out.push(l);
                out.push(r);
            }
        }
    }
}

/// A streaming linear-interpolation rate converter for interleaved stereo.
///
/// Only the userspace fallback needs one: when the engine refuses `AUTOCONVERTPCM`, capture runs
/// at the endpoint's own rate, while the Opus encoder and every timestamp downstream assume
/// 48 kHz (unconverted, a 44.1 kHz endpoint plays 8.8 % fast). Linear interpolation images above
/// the source band, but it needs no allocation and no lookahead.
#[derive(Debug, Clone, Copy)]
pub struct StereoResampler {
    /// Source frames per output frame.
    step: f64,
    /// Position of the next output frame, in source frames after `prev`.
    pos: f64,
    prev: [f32; 2],
}

impl StereoResampler {
    pub fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            step: src_rate.max(1) as f64 / dst_rate.max(1) as f64,
            pos: 0.0,
            prev: [0.0, 0.0],
        }
    }

    /// Feed one source frame; push every output frame that falls before it.
    #[inline]
    pub fn push(&mut self, l: f32, r: f32, out: &mut Vec<f32>) {
        while self.pos < 1.0 {
            let t = self.pos as f32;
            out.push(self.prev[0] + (l - self.prev[0]) * t);
            out.push(self.prev[1] + (r - self.prev[1]) * t);
            self.pos += self.step;
        }
        self.pos -= 1.0;
        self.prev = [l, r];
    }

    /// Feed `frames` frames of digital silence.
    pub fn push_silence(&mut self, frames: usize, out: &mut Vec<f32>) {
        for _ in 0..frames {
            self.push(0.0, 0.0, out);
        }
    }

    /// Forget the interpolation history, so a gap in the source is not bridged by a ramp.
    pub fn reset(&mut self) {
        self.pos = 0.0;
        self.prev = [0.0, 0.0];
    }
}

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
        // unknown layout: average everything into both channels (attenuated to avoid clip)
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
