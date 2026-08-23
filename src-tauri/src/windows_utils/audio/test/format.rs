//! Format conversion + downmix + int→float edge cases (PRD §4.4, §9).

use crate::windows_utils::audio::format::{convert_to_stereo_f32, MixFormat, SampleKind};

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
    // The asymmetric int16 min (-32768) must map to exactly -1.0 (divide by 32768).
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
    // 32767/32768 ≈ 0.99997, and never exceeds +1.0.
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
    // 5.1 order: FL, FR, FC, LFE, BL, BR. LFE dropped; C/surrounds at -3 dB (0.7071).
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
    // LFE (0.9) must not leak in.
    assert!(out[0] < 0.9 && out[1] < 0.9);
}

#[test]
fn downmix_clamps_instead_of_wrapping() {
    // Overloud 5.1 frame: naive sum would exceed +1.0; output must clamp, never wrap/clip hard.
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
    // Quad: FL, FR, BL, BR — surrounds at -3 dB.
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
    // If the source has fewer bytes than a whole frame, don't read past the end.
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
