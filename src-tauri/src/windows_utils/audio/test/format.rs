//! Format conversion, downmix and int-to-float edge cases.

use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVEFORMATEXTENSIBLE};

use crate::windows_utils::audio::format::{
    convert_into, convert_to_stereo_f32, limit_sample, parse_mix_format, MixFormat, SampleKind,
    StereoResampler, HARD_LIMIT, WIDE_LIMIT,
};

fn fmt(sample_rate: u32, channels: u16, kind: SampleKind) -> MixFormat {
    let bps = match kind {
        SampleKind::F32 | SampleKind::I32 => 4,
        SampleKind::I16 => 2,
        SampleKind::I24In3 => 3,
    };
    MixFormat {
        sample_rate,
        channels,
        kind,
        block_align: channels * bps as u16,
        channel_mask: 0,
    }
}

fn f32_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn i16_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

const EPS: f32 = 1e-6;

#[test]
fn stereo_f32_is_passthrough() {
    let f = fmt(48000, 2, SampleKind::F32);
    let src = f32_bytes(&[0.25, -0.5, 0.75, -1.0]); // 2 frames, L/R
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 2, &f, &mut out);
    assert_eq!(out.len(), 4);
    assert!((out[0] - 0.25).abs() < EPS);
    assert!((out[1] + 0.5).abs() < EPS);
    assert!((out[2] - 0.75).abs() < EPS);
    assert!((out[3] + 1.0).abs() < EPS);
    assert!(f.is_fast_path());
}

#[test]
fn int16_min_divides_by_32768_not_32767() {
    let f = fmt(48000, 2, SampleKind::I16);
    let src = i16_bytes(&[i16::MIN, i16::MAX]); // one frame L=-32768, R=32767
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    assert_eq!(out.len(), 2);
    assert!(
        (out[0] + 1.0).abs() < EPS,
        "int16 min should be -1.0, got {}",
        out[0]
    );
    assert!(out[1] < 1.0 && out[1] > 0.9999);
}

#[test]
fn mono_duplicates_to_both_channels() {
    let f = fmt(48000, 1, SampleKind::F32);
    let src = f32_bytes(&[0.42]);
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    assert_eq!(out.len(), 2);
    assert!((out[0] - 0.42).abs() < EPS);
    assert!((out[1] - 0.42).abs() < EPS);
}

#[test]
fn downmix_51_uses_itu_coefficients() {
    let f = fmt(48000, 6, SampleKind::F32);
    let src = f32_bytes(&[0.1, 0.2, 0.3, 0.9, 0.05, 0.06]); // one frame
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    let m3db = std::f32::consts::FRAC_1_SQRT_2;
    let expect_l = 0.1 + 0.3 * m3db + 0.05 * m3db;
    let expect_r = 0.2 + 0.3 * m3db + 0.06 * m3db;
    assert!(
        (out[0] - expect_l).abs() < 1e-5,
        "L={} expected {}",
        out[0],
        expect_l
    );
    assert!(
        (out[1] - expect_r).abs() < 1e-5,
        "R={} expected {}",
        out[1],
        expect_r
    );
    assert!(out[0] < 0.9 && out[1] < 0.9);
}

#[test]
fn downmix_clamps_instead_of_wrapping() {
    let f = fmt(48000, 6, SampleKind::F32);
    let src = f32_bytes(&[1.0, 1.0, 1.0, 0.0, 1.0, 1.0]);
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    assert!(out[0] <= 1.0 && out[0] >= -1.0);
    assert!(out[1] <= 1.0 && out[1] >= -1.0);
    assert!((out[0] - 1.0).abs() < EPS, "should clamp to +1.0");
}

#[test]
fn out_of_range_float_clamps() {
    let f = fmt(48000, 2, SampleKind::F32);
    let src = f32_bytes(&[2.5, -3.0]);
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    assert!((out[0] - 1.0).abs() < EPS);
    assert!((out[1] + 1.0).abs() < EPS);
}

#[test]
fn quad_downmix() {
    let f = fmt(48000, 4, SampleKind::F32);
    let src = f32_bytes(&[0.2, 0.3, 0.1, 0.1]);
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    let m3db = std::f32::consts::FRAC_1_SQRT_2;
    assert!((out[0] - (0.2 + 0.1 * m3db)).abs() < 1e-5);
    assert!((out[1] - (0.3 + 0.1 * m3db)).abs() < 1e-5);
}

#[test]
fn partial_trailing_frame_is_ignored() {
    let f = fmt(48000, 2, SampleKind::F32);
    let src = f32_bytes(&[0.1, 0.2]); // exactly one frame
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 2, &f, &mut out); // ask for 2 frames but only 1 present
    assert_eq!(
        out.len(),
        2,
        "must stop at the available data, not read OOB"
    );
}

#[test]
fn nan_becomes_silence_not_a_nan_sample() {
    // `f32::clamp` passes NaN through; the encoder must never see one
    let f = fmt(48000, 2, SampleKind::F32);
    let src = f32_bytes(&[f32::NAN, 0.5]);
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f, &mut out);
    assert_eq!(out[0], 0.0);
    assert!((out[1] - 0.5).abs() < EPS);

    let f6 = fmt(48000, 6, SampleKind::F32);
    let src = f32_bytes(&[f32::NAN, 0.0, 0.0, 0.0, 0.0, 0.0]);
    let mut out = Vec::new();
    convert_to_stereo_f32(&src, 1, &f6, &mut out);
    assert_eq!(out[0], 0.0);
}

#[test]
fn wide_limit_passes_overs_up_to_two() {
    // behind `audio_wide_sample_limit`
    let f = fmt(48000, 2, SampleKind::F32);
    let src = f32_bytes(&[1.5, -3.0]);
    let mut out = Vec::new();
    convert_into(&src, 1, &f, WIDE_LIMIT, None, &mut out);
    assert!((out[0] - 1.5).abs() < EPS);
    assert!((out[1] + 2.0).abs() < EPS);
    // the switch-off branch still applies the hard ceiling
    let mut out = Vec::new();
    convert_into(&src, 1, &f, HARD_LIMIT, None, &mut out);
    assert!((out[0] - 1.0).abs() < EPS);
    assert_eq!(limit_sample(f32::INFINITY, WIDE_LIMIT), WIDE_LIMIT);
}

/// Frames and zero-crossing frequency of the left channel of interleaved stereo at 48 kHz.
fn measure(out: &[f32]) -> (usize, f64) {
    let frames = out.len() / 2;
    let mut crossings = 0usize;
    for i in 1..frames {
        if (out[2 * (i - 1)] < 0.0) != (out[2 * i] < 0.0) {
            crossings += 1;
        }
    }
    (frames, crossings as f64 / 2.0 / (frames as f64 / 48_000.0))
}

#[test]
fn userspace_tier_resamples_44k1_to_48k_at_the_right_pitch() {
    // without resampling, a 44.1 kHz endpoint fed to the 48 kHz encoder plays 8.8 % fast and sharp
    let src_rate = 44_100u32;
    let f = fmt(src_rate, 2, SampleKind::I16);
    let samples: Vec<i16> = (0..src_rate)
        .flat_map(|n| {
            let v = (2.0 * std::f64::consts::PI * 1000.0 * n as f64 / src_rate as f64).sin();
            let s = (v * 16_000.0) as i16;
            [s, s]
        })
        .collect();
    let src = i16_bytes(&samples);
    // fed in 441-frame wakes, as the engine would
    let mut rs = StereoResampler::new(src_rate, 48_000);
    let mut out = Vec::new();
    let block = 441 * 4;
    for chunk in src.chunks(block) {
        convert_into(
            chunk,
            chunk.len() / 4,
            &f,
            HARD_LIMIT,
            Some(&mut rs),
            &mut out,
        );
    }
    let (frames, hz) = measure(&out);
    assert!(
        (47_998..=48_002).contains(&frames),
        "one second in must be 48000 frames out, got {frames}"
    );
    assert!((hz - 1000.0).abs() < 2.0, "tone moved to {hz} Hz");
}

#[test]
fn resampler_downsamples_96k_and_handles_silence() {
    let mut rs = StereoResampler::new(96_000, 48_000);
    let mut out = Vec::new();
    rs.push_silence(9600, &mut out);
    assert_eq!(out.len() / 2, 4800);
    assert!(out.iter().all(|&s| s == 0.0));
    rs.reset();
    let mut out = Vec::new();
    for n in 0..96_000u32 {
        let v = (2.0 * std::f64::consts::PI * 440.0 * n as f64 / 96_000.0).sin() as f32;
        rs.push(v, v, &mut out);
    }
    let (frames, hz) = measure(&out);
    assert!((47_999..=48_001).contains(&frames), "{frames}");
    assert!((hz - 440.0).abs() < 2.0, "{hz}");
}

#[test]
fn extensible_header_without_its_extension_is_refused() {
    // a `WAVE_FORMAT_EXTENSIBLE` tag whose cbSize does not cover the extension must not be read
    // as one: the bytes after this header are valid float, in memory the format does not own
    let mut ext: WAVEFORMATEXTENSIBLE = unsafe { std::mem::zeroed() };
    ext.Format.wFormatTag = 0xFFFE;
    ext.Format.nChannels = 2;
    ext.Format.nSamplesPerSec = 48_000;
    ext.Format.wBitsPerSample = 32;
    ext.Format.nBlockAlign = 8;
    ext.Format.cbSize = 0;
    ext.SubFormat = windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
    let p = &ext as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
    assert!(unsafe { parse_mix_format(p) }.is_err());

    ext.Format.cbSize = 22;
    let p = &ext as *const WAVEFORMATEXTENSIBLE as *const WAVEFORMATEX;
    let m = unsafe { parse_mix_format(p) }.expect("a complete extensible format parses");
    assert_eq!(m.kind, SampleKind::F32);
    assert!(m.is_fast_path());
}
