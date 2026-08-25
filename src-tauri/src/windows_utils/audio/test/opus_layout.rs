//! Pin the hand-written libopus FFI constants against the real header values (PRD §5.1, §9).
//!
//! In the spirit of `streamer/test/nvenc_layout.rs`: a DLL upgrade or a careless edit that
//! changes one of these numbers silently misconfigures the encoder instead of failing to
//! compile. Every value below is transcribed from `.sources/repos/opus/include/opus_defines.h`.

use crate::streamer::audio::opus_sys as o;

#[test]
fn application_and_signal_values() {
    assert_eq!(o::OPUS_APPLICATION_VOIP, 2048);
    assert_eq!(o::OPUS_APPLICATION_AUDIO, 2049);
    // The whole point of the low-delay path (§5.2).
    assert_eq!(o::OPUS_APPLICATION_RESTRICTED_LOWDELAY, 2051);
    assert_eq!(o::OPUS_SIGNAL_VOICE, 3001);
    assert_eq!(o::OPUS_SIGNAL_MUSIC, 3002);
    assert_eq!(o::OPUS_AUTO, -1000);
    assert_eq!(o::OPUS_BITRATE_MAX, -1);
}

#[test]
fn ctl_request_numbers() {
    // SETs are even, GETs odd (opus_defines.h comment).
    assert_eq!(o::OPUS_SET_APPLICATION_REQUEST, 4000);
    assert_eq!(o::OPUS_SET_BITRATE_REQUEST, 4002);
    assert_eq!(o::OPUS_SET_MAX_BANDWIDTH_REQUEST, 4004);
    assert_eq!(o::OPUS_SET_VBR_REQUEST, 4006);
    assert_eq!(o::OPUS_SET_BANDWIDTH_REQUEST, 4008);
    assert_eq!(o::OPUS_SET_COMPLEXITY_REQUEST, 4010);
    assert_eq!(o::OPUS_SET_INBAND_FEC_REQUEST, 4012);
    assert_eq!(o::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 4014);
    assert_eq!(o::OPUS_SET_DTX_REQUEST, 4016);
    assert_eq!(o::OPUS_SET_VBR_CONSTRAINT_REQUEST, 4020);
    assert_eq!(o::OPUS_SET_FORCE_CHANNELS_REQUEST, 4022);
    assert_eq!(o::OPUS_SET_SIGNAL_REQUEST, 4024);
    assert_eq!(o::OPUS_GET_LOOKAHEAD_REQUEST, 4027);
    assert_eq!(o::OPUS_RESET_STATE, 4028);
    assert_eq!(o::OPUS_GET_SAMPLE_RATE_REQUEST, 4029);
    assert_eq!(o::OPUS_SET_LSB_DEPTH_REQUEST, 4036);
}

#[test]
fn error_codes() {
    assert_eq!(o::OPUS_OK, 0);
    assert_eq!(o::OPUS_BAD_ARG, -1);
    assert_eq!(o::OPUS_BUFFER_TOO_SMALL, -2);
    assert_eq!(o::OPUS_INTERNAL_ERROR, -3);
    assert_eq!(o::OPUS_INVALID_PACKET, -4);
    assert_eq!(o::OPUS_UNIMPLEMENTED, -5);
    assert_eq!(o::OPUS_INVALID_STATE, -6);
    assert_eq!(o::OPUS_ALLOC_FAIL, -7);
}

#[test]
fn frame_math() {
    use crate::streamer::audio::encoder::{FRAME_MS, FRAME_SAMPLES, OPUS_SAMPLE_RATE};
    // 5 ms @ 48 kHz = 240 samples/channel — a valid Opus frame size.
    assert_eq!(OPUS_SAMPLE_RATE, 48000);
    assert_eq!(FRAME_MS, 5);
    assert_eq!(FRAME_SAMPLES, 240);
}
