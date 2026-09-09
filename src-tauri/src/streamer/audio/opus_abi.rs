//! libopus ABI, measured against the library that is actually loaded.
//!
//! `windows_utils/audio/test/opus_layout.rs` asserts the constants in `opus_sys.rs` against
//! literals. That is a change-detector for our own file: it passes whatever libopus thinks, and
//! nothing in it links or loads the library. The numbers it guards are exactly the kind that
//! fail silently — a wrong CTL request reads or writes a *different* property and still returns
//! `OPUS_OK`, so a mis-numbered `OPUS_SET_BITRATE_REQUEST` would leave the encoder at its
//! default bitrate with the bitrate controller believing it had been obeyed.
//!
//! These tests ask the library instead. A CTL number is proved by setting a distinctive value
//! and reading it back through its paired getter; an error constant is proved by the string
//! `opus_strerror` returns for it; and the encoder configuration is proved by the TOC byte of a
//! real packet, which carries the mode and the channel count the encoder actually used.
//!
//! Cross-platform on purpose: `libopus.dll` and `libopus.dylib` are separate artifacts built
//! from the same commit, and this is the only test that would notice them diverging.

use std::ffi::c_int;

use super::opus_sys::{self as o, OpusApi};
use crate::test_support::skip;

const SR: i32 = 48_000;
const CHANNELS: c_int = 2;
/// 5 ms at 48 kHz, the frame size the host encodes at.
const FRAME_SAMPLES: usize = 240;

struct Encoder {
    api: OpusApi,
    enc: *mut o::OpusEncoder,
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.enc.is_null() {
            unsafe { (self.api.encoder_destroy)(self.enc) };
        }
    }
}

impl Encoder {
    fn new() -> Option<Self> {
        let api = match OpusApi::load() {
            Ok(api) => api,
            Err(e) => {
                skip("libopus ABI (library unavailable)", e);
                return None;
            }
        };
        let mut err: c_int = 0;
        let enc = unsafe {
            (api.encoder_create)(
                SR,
                CHANNELS,
                o::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                &mut err,
            )
        };
        if enc.is_null() || err != o::OPUS_OK {
            skip("libopus ABI (encoder_create failed)", api.strerror(err));
            return None;
        }
        Some(Encoder { api, enc })
    }

    fn set(&self, request: c_int, value: c_int) -> c_int {
        unsafe { (self.api.encoder_ctl_set)(self.enc, request, value) }
    }

    fn get(&self, request: c_int) -> Result<c_int, c_int> {
        let mut out: c_int = i32::MIN;
        let rc = unsafe { (self.api.encoder_ctl_get)(self.enc, request, &mut out) };
        if rc == o::OPUS_OK {
            Ok(out)
        } else {
            Err(rc)
        }
    }
}

#[test]
fn the_library_identifies_itself() {
    let Ok(api) = OpusApi::load() else {
        skip("libopus ABI", "library unavailable");
        return;
    };
    let version = api.version();
    assert!(
        version.to_lowercase().contains("opus"),
        "opus_get_version_string returned {version:?}"
    );
}

#[test]
fn every_error_constant_is_the_number_libopus_names() {
    // `opus_strerror` is a switch over the real error values, so a constant we got wrong falls
    // through to its "unknown error" arm. This is the cheapest possible check that our error
    // table is the library's, and it needs no encoder.
    let Ok(api) = OpusApi::load() else {
        skip("libopus ABI", "library unavailable");
        return;
    };
    for (code, name) in [
        (o::OPUS_OK, "success"),
        (o::OPUS_BAD_ARG, "argument"),
        (o::OPUS_BUFFER_TOO_SMALL, "buffer"),
        (o::OPUS_INTERNAL_ERROR, "internal"),
        (o::OPUS_INVALID_PACKET, "corrupted"),
        (o::OPUS_UNIMPLEMENTED, "not implemented"),
        (o::OPUS_INVALID_STATE, "state"),
        (o::OPUS_ALLOC_FAIL, "memory"),
    ] {
        let text = api.strerror(code).to_lowercase();
        assert!(
            !text.contains("unknown"),
            "libopus does not recognise error {code}: {text:?}"
        );
        assert!(
            text.contains(name),
            "error {code} reads as {text:?}, which does not mention {name:?}"
        );
    }
    // And a value that really is not an error code must come back as unknown, or the assertion
    // above proves nothing.
    assert!(api.strerror(-9999).to_lowercase().contains("unknown"));
}

#[test]
fn an_encoder_state_has_a_real_size() {
    let Ok(api) = OpusApi::load() else {
        skip("libopus ABI", "library unavailable");
        return;
    };
    let mono = unsafe { (api.encoder_get_size)(1) };
    let stereo = unsafe { (api.encoder_get_size)(2) };
    assert!(mono > 0, "opus_encoder_get_size(1) = {mono}");
    assert!(
        stereo > mono,
        "stereo state should be larger: {stereo} vs {mono}"
    );
    // A negative return is an error code, which would mean the signature is wrong.
    assert!(
        unsafe { (api.encoder_get_size)(0) } <= 0,
        "0 channels is not valid"
    );
}

#[test]
fn each_ctl_request_number_reaches_the_property_it_names() {
    // The whole point: a wrong request number is not an error, it is a different property.
    // Every pair below writes a value nothing else would produce and reads it back.
    let Some(enc) = Encoder::new() else { return };

    assert_eq!(enc.set(o::OPUS_SET_BITRATE_REQUEST, 96_000), o::OPUS_OK);
    assert_eq!(enc.get(o::OPUS_GET_BITRATE_REQUEST), Ok(96_000), "bitrate");

    assert_eq!(enc.set(o::OPUS_SET_COMPLEXITY_REQUEST, 3), o::OPUS_OK);
    assert_eq!(enc.get(o::OPUS_GET_COMPLEXITY_REQUEST), Ok(3), "complexity");

    assert_eq!(enc.set(o::OPUS_SET_VBR_REQUEST, 0), o::OPUS_OK);
    assert_eq!(enc.get(o::OPUS_GET_VBR_REQUEST), Ok(0), "vbr off");
    assert_eq!(enc.set(o::OPUS_SET_VBR_REQUEST, 1), o::OPUS_OK);
    assert_eq!(enc.get(o::OPUS_GET_VBR_REQUEST), Ok(1), "vbr on");

    assert_eq!(enc.set(o::OPUS_SET_VBR_CONSTRAINT_REQUEST, 0), o::OPUS_OK);
    assert_eq!(
        enc.get(o::OPUS_GET_VBR_CONSTRAINT_REQUEST),
        Ok(0),
        "vbr constraint"
    );

    assert_eq!(
        enc.set(o::OPUS_SET_SIGNAL_REQUEST, o::OPUS_SIGNAL_MUSIC),
        o::OPUS_OK
    );
    assert_eq!(
        enc.get(o::OPUS_GET_SIGNAL_REQUEST),
        Ok(o::OPUS_SIGNAL_MUSIC),
        "signal"
    );

    assert_eq!(enc.set(o::OPUS_SET_LSB_DEPTH_REQUEST, 16), o::OPUS_OK);
    assert_eq!(enc.get(o::OPUS_GET_LSB_DEPTH_REQUEST), Ok(16), "lsb depth");

    assert_eq!(
        enc.set(o::OPUS_SET_APPLICATION_REQUEST, o::OPUS_APPLICATION_AUDIO),
        o::OPUS_OK
    );
    assert_eq!(
        enc.get(o::OPUS_GET_APPLICATION_REQUEST),
        Ok(o::OPUS_APPLICATION_AUDIO),
        "application"
    );
    assert_eq!(
        enc.set(
            o::OPUS_SET_APPLICATION_REQUEST,
            o::OPUS_APPLICATION_RESTRICTED_LOWDELAY
        ),
        o::OPUS_OK
    );

    // Read-only requests, which prove their own numbers by answering something specific.
    assert_eq!(
        enc.get(o::OPUS_GET_SAMPLE_RATE_REQUEST),
        Ok(SR),
        "the encoder was created at 48 kHz and must say so"
    );
    let lookahead = enc.get(o::OPUS_GET_LOOKAHEAD_REQUEST).expect("lookahead");
    assert!(
        (0..SR / 100).contains(&lookahead),
        "lookahead of {lookahead} samples is not a codec delay"
    );

    // A request number that is not a request must be refused, or the reads above could be
    // hitting a permissive default path.
    assert_eq!(enc.get(9999), Err(o::OPUS_UNIMPLEMENTED));
}

#[test]
fn a_restricted_lowdelay_packet_is_celt_only_and_stereo() {
    // The TOC byte is the codec's own statement of what it produced: bits 7..3 are the config
    // (16..31 are the CELT-only modes), bit 2 is the stereo flag. `RESTRICTED_LOWDELAY` is
    // documented to force CELT, and this is the assertion behind that claim — and behind the
    // decision not to advertise in-band FEC, which is SILK-only and therefore inert here.
    let Some(enc) = Encoder::new() else { return };
    assert_eq!(enc.set(o::OPUS_SET_BITRATE_REQUEST, 128_000), o::OPUS_OK);

    // A tone rather than silence: DTX is off, but an all-zero frame still encodes to almost
    // nothing and says less about the mode.
    let mut pcm = vec![0f32; FRAME_SAMPLES * CHANNELS as usize];
    for (i, frame) in pcm.chunks_mut(CHANNELS as usize).enumerate() {
        let s = (i as f32 * 0.13).sin() * 0.5;
        frame[0] = s;
        frame[1] = -s;
    }

    let mut out = vec![0u8; 4000];
    let len = unsafe {
        (enc.api.encode_float)(
            enc.enc,
            pcm.as_ptr(),
            FRAME_SAMPLES as c_int,
            out.as_mut_ptr(),
            out.len() as i32,
        )
    };
    assert!(
        len > 2,
        "opus_encode_float returned {len} ({})",
        enc.api.strerror(len)
    );

    let toc = out[0];
    let config = toc >> 3;
    let stereo = (toc >> 2) & 1;
    assert!(
        config >= 16,
        "TOC config {config} is a SILK or hybrid mode; RESTRICTED_LOWDELAY must be CELT-only"
    );
    assert_eq!(stereo, 1, "a stereo encoder produced a mono packet");

    // The same call through the int16 entry point, which the capture path uses when the engine
    // hands it PCM. Two different symbols, one signature each; a mismatch in either is a crash
    // or a garbage packet.
    let pcm16: Vec<i16> = pcm.iter().map(|s| (s * 32767.0) as i16).collect();
    let len16 = unsafe {
        (enc.api.encode)(
            enc.enc,
            pcm16.as_ptr(),
            FRAME_SAMPLES as c_int,
            out.as_mut_ptr(),
            out.len() as i32,
        )
    };
    assert!(len16 > 2, "opus_encode returned {len16}");
    assert!(
        out[0] >> 3 >= 16,
        "the int16 path produced a non-CELT packet"
    );
}

#[test]
fn a_frame_size_the_codec_does_not_support_is_refused() {
    // 241 samples is not one of Opus's frame sizes. It must come back as an error rather than
    // as a short packet, because the capture path checks the return value and nothing else.
    let Some(enc) = Encoder::new() else { return };
    let pcm = vec![0f32; 241 * CHANNELS as usize];
    let mut out = vec![0u8; 4000];
    let len = unsafe {
        (enc.api.encode_float)(
            enc.enc,
            pcm.as_ptr(),
            241,
            out.as_mut_ptr(),
            out.len() as i32,
        )
    };
    assert_eq!(len, o::OPUS_BAD_ARG, "241 samples should be OPUS_BAD_ARG");
}

#[test]
fn the_frame_math_the_host_encodes_at_is_the_math_opus_accepts() {
    use super::encoder::{FRAME_MS, FRAME_SAMPLES as HOST_FRAME_SAMPLES, OPUS_SAMPLE_RATE};
    assert_eq!(OPUS_SAMPLE_RATE, 48_000);
    assert_eq!(FRAME_MS, 5);
    assert_eq!(
        HOST_FRAME_SAMPLES,
        OPUS_SAMPLE_RATE as usize * FRAME_MS as usize / 1000
    );
    assert_eq!(HOST_FRAME_SAMPLES, FRAME_SAMPLES);
}
