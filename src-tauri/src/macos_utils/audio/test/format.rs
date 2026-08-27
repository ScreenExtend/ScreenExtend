use objc2_core_audio_types::{
    kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatFlagIsPacked,
    kAudioFormatLinearPCM, AudioStreamBasicDescription,
};

use crate::macos_utils::audio::format::{
    convert_interleaved, convert_planar, parse_asbd, AudioFormatDesc, SampleKind,
};

const M3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

fn asbd(
    rate: f64,
    channels: u32,
    flags: u32,
    bits: u32,
    interleaved_bytes_per_frame: u32,
) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: flags,
        mBytesPerPacket: interleaved_bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: interleaved_bytes_per_frame,
        mChannelsPerFrame: channels,
        mBitsPerChannel: bits,
        mReserved: 0,
    }
}

fn f32_bytes(vals: &[f32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[test]
fn parses_sck_planar_float_stereo() {
    let a = asbd(
        48000.0,
        2,
        kAudioFormatFlagIsFloat | kAudioFormatFlagIsNonInterleaved,
        32,
        4,
    );
    let d = parse_asbd(&a).unwrap();
    assert_eq!(d.sample_rate, 48000);
    assert_eq!(d.channels, 2);
    assert_eq!(d.kind, SampleKind::F32);
    assert!(d.non_interleaved);
    assert!(!d.is_fast_path()); // planar is not the interleaved fast path
}

#[test]
fn parses_interleaved_float_stereo_fast_path() {
    let a = asbd(
        48000.0,
        2,
        kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        32,
        8,
    );
    let d = parse_asbd(&a).unwrap();
    assert!(d.is_fast_path());
}

#[test]
fn interleaved_stereo_f32_passthrough() {
    let d = AudioFormatDesc {
        sample_rate: 48000,
        channels: 2,
        kind: SampleKind::F32,
        non_interleaved: false,
    };
    let src = f32_bytes(&[0.25, -0.5, 0.75, -1.0]);
    let mut out = [0.0f32; 4];
    let n = convert_interleaved(&src, 2, &d, &mut out);
    assert_eq!(n, 4);
    assert_eq!(out, [0.25, -0.5, 0.75, -1.0]);
}

#[test]
fn interleaved_mono_duplicates_to_stereo() {
    let d = AudioFormatDesc {
        sample_rate: 48000,
        channels: 1,
        kind: SampleKind::F32,
        non_interleaved: false,
    };
    let src = f32_bytes(&[0.3, -0.6]);
    let mut out = [0.0f32; 4];
    let n = convert_interleaved(&src, 2, &d, &mut out);
    assert_eq!(n, 4);
    assert_eq!(out, [0.3, 0.3, -0.6, -0.6]);
}

#[test]
fn i16_scales_by_32768_not_32767() {
    let d = AudioFormatDesc {
        sample_rate: 48000,
        channels: 2,
        kind: SampleKind::I16,
        non_interleaved: false,
    };
    let mut src = Vec::new();
    src.extend_from_slice(&i16::MIN.to_le_bytes()); // L = -1.0
    src.extend_from_slice(&i16::MAX.to_le_bytes()); // R ≈ 0.99997
    let mut out = [0.0f32; 2];
    let n = convert_interleaved(&src, 1, &d, &mut out);
    assert_eq!(n, 2);
    assert!((out[0] - -1.0).abs() < 1e-6);
    assert!((out[1] - (32767.0 / 32768.0)).abs() < 1e-6);
}

#[test]
fn downmix_5_1_uses_bs775_coefficients() {
    // 5.1 order: FL, FR, FC, LFE, BL, BR. LFE dropped; FC/BL/BR at -3 dB.
    let d = AudioFormatDesc {
        sample_rate: 48000,
        channels: 6,
        kind: SampleKind::F32,
        non_interleaved: false,
    };
    let (fl, fr, fc, lfe, bl, br) = (0.1f32, 0.2, 0.4, 0.9, 0.05, 0.06);
    let src = f32_bytes(&[fl, fr, fc, lfe, bl, br]);
    let mut out = [0.0f32; 2];
    convert_interleaved(&src, 1, &d, &mut out);
    let want_l = (fl + fc * M3DB + bl * M3DB).clamp(-1.0, 1.0);
    let want_r = (fr + fc * M3DB + br * M3DB).clamp(-1.0, 1.0);
    assert!((out[0] - want_l).abs() < 1e-6, "L {} vs {}", out[0], want_l);
    assert!((out[1] - want_r).abs() < 1e-6, "R {} vs {}", out[1], want_r);
}

#[test]
fn clamps_out_of_range_sums() {
    // Quad where the back channels push the sum past 1.0 → must clamp, not wrap.
    let d = AudioFormatDesc {
        sample_rate: 48000,
        channels: 4,
        kind: SampleKind::F32,
        non_interleaved: false,
    };
    let src = f32_bytes(&[0.9, 0.9, 0.9, 0.9]);
    let mut out = [0.0f32; 2];
    convert_interleaved(&src, 1, &d, &mut out);
    assert_eq!(out[0], 1.0);
    assert_eq!(out[1], 1.0);
}

#[test]
fn planar_stereo_f32_interleaves() {
    // Two separate channel planes (the SCK layout).
    let d = AudioFormatDesc {
        sample_rate: 48000,
        channels: 2,
        kind: SampleKind::F32,
        non_interleaved: true,
    };
    let left = f32_bytes(&[0.1, 0.2, 0.3]);
    let right = f32_bytes(&[-0.1, -0.2, -0.3]);
    let planes: [&[u8]; 2] = [&left, &right];
    let mut out = [0.0f32; 6];
    let n = convert_planar(&planes, 3, &d, &mut out);
    assert_eq!(n, 6);
    assert_eq!(out, [0.1, -0.1, 0.2, -0.2, 0.3, -0.3]);
}

#[test]
fn parse_rejects_unknown_format() {
    // No float, no signed-int flag → unsupported.
    let a = asbd(48000.0, 2, 0, 24, 6);
    assert!(parse_asbd(&a).is_err());
}
