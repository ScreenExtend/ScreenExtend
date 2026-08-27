use anyhow::{bail, Result};
use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsSignedInteger,
    AudioStreamBasicDescription,
};

const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

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

#[derive(Clone, Copy, Debug)]
pub struct AudioFormatDesc {
    pub sample_rate: u32,
    pub channels: u16,
    pub kind: SampleKind,
    pub non_interleaved: bool,
}

impl AudioFormatDesc {
    /// source is already Opus-ready: 48 kHz, stereo, float32, interleaved
    pub fn is_fast_path(&self) -> bool {
        self.sample_rate == 48000
            && self.channels == 2
            && self.kind == SampleKind::F32
            && !self.non_interleaved
    }
}

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

/// BS.775 downmix to (L, R); mirror of `windows_utils/audio/format.rs::downmix_frame`
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

/// interleaved source PCM → interleaved stereo f32; returns f32 written. `out` holds ≥ `2*frames`,
/// `src` holds ≥ `frames * channels * bytes_per_sample`
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

/// planar source PCM (one slice per channel) → interleaved stereo f32; the SCK / non-interleaved tap layout
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
