//! Functional AMF tests, for a machine with a Radeon or a Ryzen APU.
//!
//! They encode real frames and assert what the transport depends on, which `amd_layout.rs` and
//! `amd_fake.rs` cannot: neither proves what a real runtime answers. Every test is `#[ignore]`d
//! and skips loudly without the hardware.
//! Run them with `cargo test --lib amd_encoder -- --ignored --nocapture`.

use std::time::Instant;

use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12;

use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::amd::encoder::{Encoder, RateTier};
use crate::windows_utils::streamer::nvidia::encoder::{fill_synthetic_bgra, EncoderConfig};

use super::h264::{is_idr_with_headers, nal_units, parse_sps, starts_with_annexb};

fn cfg(width: u32, height: u32, fps: u32, bitrate: u32, qp: Option<u8>) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: H264Profile::High,
        qp,
        min_qp: None,
        intra_refresh: false,
    }
}

fn open(test: &str, c: EncoderConfig) -> Option<Encoder> {
    match Encoder::new(c) {
        Ok(e) => Some(e),
        Err(e) => {
            super::skip(test, e);
            None
        }
    }
}

fn is_idr(au: &[u8]) -> bool {
    nal_units(au).iter().any(|&(t, _)| t == 5)
}

/// Encodes `n` frames of moving synthetic content, forcing an IDR on the frames in `force`.
fn run(enc: &mut Encoder, c: &EncoderConfig, n: u32, force: &[u32]) -> Vec<Vec<u8>> {
    let mut frame = vec![0u8; (c.width * c.height * 4) as usize];
    (0..n)
        .map(|i| {
            fill_synthetic_bgra(&mut frame, c.width, c.height, i);
            enc.encode_bgra_padded(&frame, c.width * 4, force.contains(&i))
                .unwrap_or_else(|e| panic!("frame {i}: {e:#}"))
        })
        .collect()
}

/// The stream a browser joins: the first access unit is an IDR carrying SPS and PPS, its SPS says
/// the decoder never has to reorder, every later picture is a one-slice P frame, and no
/// unrequested IDR appears (the `IDRPeriod` write took).
#[test]
#[ignore = "needs an AMD GPU: encodes 300 frames through amfrt64.dll"]
fn amf_stream_starts_decodable_and_has_no_periodic_idr() {
    let c = cfg(1920, 1080, 60, 10_000_000, None);
    let Some(mut enc) = open("amf_stream_starts_decodable_and_has_no_periodic_idr", c) else {
        return;
    };
    let aus = run(&mut enc, &c, 300, &[]);
    assert!(aus
        .iter()
        .all(|au| !au.is_empty() && starts_with_annexb(au)));
    assert!(is_idr_with_headers(&aus[0]));
    let sps = parse_sps(&aus[0]).expect("the first SPS parses");
    assert_eq!(sps.chroma_format_idc, 1);
    assert!(sps.vui_present && sps.bitstream_restriction);
    assert_eq!(sps.max_num_reorder_frames, 0);
    let idrs = aus.iter().filter(|au| is_idr(au)).count();
    assert_eq!(idrs, 1, "an IDR nobody requested: IDRPeriod did not take");
    for au in &aus[1..] {
        let nals = nal_units(au);
        assert_eq!(nals.len(), 1, "one slice, no AUD, no SEI: {nals:?}");
        assert_eq!(nals[0].0, 1);
    }
}

/// A forced IDR is an IDR with its parameter sets, on exactly the frame that asked.
#[test]
#[ignore = "needs an AMD GPU: checks ForcePictureType and InsertSPS/InsertPPS on a real runtime"]
fn amf_forced_idr_is_self_contained() {
    let c = cfg(1280, 720, 60, 6_000_000, None);
    let Some(mut enc) = open("amf_forced_idr_is_self_contained", c) else {
        return;
    };
    let aus = run(&mut enc, &c, 60, &[30]);
    assert!(is_idr_with_headers(&aus[30]));
    assert!(!is_idr(&aus[29]) && !is_idr(&aus[31]));
}

/// The SPS level is the one the SDP answer advertises, and the encoder opens at a size and rate
/// AMF's default level 4.2 does not cover.
#[test]
#[ignore = "needs an AMD GPU: reads the level a real runtime writes at 2560x1440@120"]
fn amf_level_matches_the_sdp_at_1440p120() {
    let c = cfg(2560, 1440, 120, 30_000_000, None);
    let Some(mut enc) = open("amf_level_matches_the_sdp_at_1440p120", c) else {
        return;
    };
    let aus = run(&mut enc, &c, 10, &[]);
    let sps = parse_sps(&aus[0]).expect("SPS");
    assert_eq!(
        u32::from(sps.level_idc),
        crate::streamer::negotiate::level_for(2560, 1440, 120) as u32
    );
}

/// A 240 Hz client keeps the hardware path: either the runtime takes 240 fps or it is held at
/// 120 with the target scaled; it is never a fallback to x264.
#[test]
#[ignore = "needs an AMD GPU: probes whether a real runtime enforces FrameRate <= 120"]
fn amf_a_240_fps_client_keeps_the_hardware_encoder() {
    let c = cfg(1920, 1080, 240, 20_000_000, None);
    let Some(mut enc) = open("amf_a_240_fps_client_keeps_the_hardware_encoder", c) else {
        return;
    };
    let aus = run(&mut enc, &c, 240, &[]);
    let bytes: usize = aus.iter().map(Vec::len).sum();
    let mbps = bytes as f64 * 8.0 / 1e6;
    eprintln!("240 frames at 240 fps: {mbps:.2} Mbit for a 20 Mbit/s target");
    assert!(
        mbps < 40.0,
        "the rate controller budgets a frame at 1/120 s"
    );
}

/// A live bitrate cut shrinks the stream within a few frames and costs no keyframe.
#[test]
#[ignore = "needs an AMD GPU: measures a dynamic TargetBitrate change on a real runtime"]
fn amf_a_live_bitrate_cut_shrinks_the_stream_without_a_keyframe() {
    let c = cfg(1920, 1080, 60, 20_000_000, None);
    let Some(mut enc) = open(
        "amf_a_live_bitrate_cut_shrinks_the_stream_without_a_keyframe",
        c,
    ) else {
        return;
    };
    assert_eq!(enc.rate_tier(), RateTier::LinkCbr);
    let before: usize = run(&mut enc, &c, 120, &[])
        .iter()
        .skip(60)
        .map(Vec::len)
        .sum();
    enc.set_bitrate(2_000_000).expect("applied");
    let after_aus = run(&mut enc, &c, 120, &[]);
    assert!(
        after_aus.iter().all(|au| !is_idr(au)),
        "a bitrate change cost an IDR"
    );
    let after: usize = after_aus.iter().skip(60).map(Vec::len).sum();
    assert!(
        after * 4 < before,
        "20 -> 2 Mbit/s: {before} B then {after} B"
    );
}

/// A live `QPI`/`QPP` write on the constant-QP tier: the first picture after it reports the new
/// QP in its statistics (on RX 5000 / Ryzen 2000 and newer), the frames shrink, and no keyframe
/// is spent. A failure means the write does not take on this runtime, and `amf_qp_live` should
/// ship off (the re-init path, `amf_a_reinit_qp_change_takes_with_one_idr`).
#[test]
#[ignore = "needs an AMD GPU: checks that dynamic QPI/QPP apply per frame on a real runtime"]
fn amf_a_live_qp_change_shrinks_the_stream_without_a_keyframe() {
    let c = cfg(1920, 1080, 60, 20_000_000, Some(18));
    let Some(mut enc) = open(
        "amf_a_live_qp_change_shrinks_the_stream_without_a_keyframe",
        c,
    ) else {
        return;
    };
    eprintln!("rate tier: {:?}", enc.rate_tier());
    let before = run(&mut enc, &c, 30, &[]);
    assert!(enc.set_qp(40).expect("answers"), "constant QP is in force");
    let after = run(&mut enc, &c, 10, &[]);
    eprintln!(
        "first picture after the change: reported QP {:?}; unconfirmed {}, unchecked {}",
        enc.last_frame_qp(),
        enc.qp_change_unconfirmed(),
        enc.qp_change_unchecked()
    );
    // per picture, so "the next picture" can be told from "later"
    let mut sizes: Vec<usize> = before[20..].iter().map(Vec::len).collect();
    let mut tail: Vec<usize> = after.iter().map(Vec::len).collect();
    eprintln!("sizes before the change: {sizes:?}");
    eprintln!("sizes after the change:  {tail:?}");
    sizes.sort_unstable();
    let median_before = sizes[sizes.len() / 2];
    assert_eq!(
        enc.qp_change_unconfirmed(),
        0,
        "the live write did not take"
    );
    assert!(after.iter().all(|au| !is_idr(au)));
    assert!(
        tail[0] * 2 < median_before,
        "the first picture after the change is not smaller ({} B against a median of {} B): \
         the change landed late or not at all",
        tail[0],
        median_before
    );
    tail.sort_unstable();
    assert!(tail[tail.len() / 2] * 2 < median_before);
}

/// Settles what `AMF_NEED_MORE_INPUT` means on this runtime. The low-latency usages will
/// probably never return it, so this provokes it: the TRANSCODING usage needs several inputs
/// before any output. In the printed log, a picture whose submit answered `AMF_NEED_MORE_INPUT`
/// and whose identity appears among the outputs was taken (OBS's reading; ship as is); one that
/// never appears was not (AMD's samples' reading; the probe-and-resubmit path is what matters).
/// `AMF_INPUT_FULL` answers are the queue, not this question.
#[test]
#[ignore = "needs an AMD GPU: provokes AMF_NEED_MORE_INPUT in the TRANSCODING usage and reports \
            which submitted pictures come back"]
fn amf_need_more_input_meaning_probe() {
    let c = cfg(1920, 1080, 60, 10_000_000, None);
    let Some(mut enc) = open("amf_need_more_input_meaning_probe", c) else {
        return;
    };
    for line in enc.need_more_input_probe().expect("the probe runs") {
        eprintln!("{line}");
    }
}

/// OBS's way of changing a constant QP: flush, re-initialise, one IDR. If the live QP test fails
/// and this passes, ship `amf_qp_live` off.
#[test]
#[ignore = "needs an AMD GPU: checks Flush + ReInit with a new QP on a real runtime"]
fn amf_a_reinit_qp_change_takes_with_one_idr() {
    let c = cfg(1920, 1080, 60, 20_000_000, Some(18));
    let Some(mut enc) = open("amf_a_reinit_qp_change_takes_with_one_idr", c) else {
        return;
    };
    let before: usize = run(&mut enc, &c, 60, &[])
        .iter()
        .skip(30)
        .map(Vec::len)
        .sum();
    assert!(enc.set_qp_reinit(40).expect("answers"));
    let after_aus = run(&mut enc, &c, 60, &[]);
    assert!(
        is_idr_with_headers(&after_aus[0]),
        "the re-init starts on an IDR"
    );
    assert!(after_aus[1..].iter().all(|au| !is_idr(au)));
    let after: usize = after_aus.iter().skip(30).map(Vec::len).sum();
    assert!(after * 2 < before, "QP 18 -> 40: {before} B then {after} B");
}

/// Over 600 pictures, every output should carry the identity written on its surface
/// (`unlabelled` 0 means AMF carries surface properties to the output) and nothing should be
/// dropped as a duplicate or a stale picture. Any `AMF_NEED_MORE_INPUT` is counted by what
/// followed it.
#[test]
#[ignore = "needs an AMD GPU: reports how a real runtime answers SubmitInput and labels outputs"]
fn amf_outputs_carry_their_identity_and_need_more_input_is_counted() {
    let c = cfg(1920, 1080, 60, 10_000_000, None);
    let Some(mut enc) = open(
        "amf_outputs_carry_their_identity_and_need_more_input_is_counted",
        c,
    ) else {
        return;
    };
    run(&mut enc, &c, 600, &[]);
    let counts = enc.identity_counts();
    eprintln!("identity counts after 600 pictures: {counts:?}");
    assert_eq!(
        counts.unlabelled, 0,
        "AMF did not carry the surface identity"
    );
    assert_eq!(counts.duplicates_dropped + counts.stale_dropped, 0);
}

/// Submit to access unit, per frame, at 1080p60. Printed in the shape the perf gate parses.
#[test]
#[ignore = "needs an AMD GPU: times SubmitInput to QueryOutput on VCN"]
fn amf_encode_latency_1080p60() {
    crate::test_support::show_measurements();
    let c = cfg(1920, 1080, 60, 10_000_000, None);
    let Some(mut enc) = open("amf_encode_latency_1080p60", c) else {
        return;
    };
    let mut frame = vec![0u8; (c.width * c.height * 4) as usize];
    let mut samples = Vec::new();
    for i in 0..600u32 {
        fill_synthetic_bgra(&mut frame, c.width, c.height, i);
        let t = Instant::now();
        enc.encode_bgra_padded(&frame, c.width * 4, false)
            .expect("encodes");
        if i >= 60 {
            samples.push(t.elapsed().as_micros());
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
    samples.sort_unstable();
    let p = crate::test_support::percentiles(&samples);
    tprintln!(
        "amf encode_us 1080p60 p50={} p90={} p99={} max={}",
        p.p50,
        p.p90,
        p.p99,
        p.max
    );
    assert!(p.p50 < 8_000, "p50 {} us", p.p50);
}

/// The CPU bridge: built in NV12, it encodes its first frame without a second `Init()`.
#[test]
#[ignore = "needs an AMD GPU: opens the NV12 CPU-bridge encoder on a real runtime"]
fn amf_cpu_bridge_encodes_nv12_from_the_first_frame() {
    let c = cfg(1280, 720, 60, 6_000_000, None);
    let mut enc = match Encoder::new_with_input_format(c, DXGI_FORMAT_NV12) {
        Ok(e) => e,
        Err(e) => {
            super::skip("amf_cpu_bridge_encodes_nv12_from_the_first_frame", e);
            return;
        }
    };
    let nv12 = vec![0x80u8; (c.width * c.height * 3 / 2) as usize];
    let au = enc
        .encode_nv12_padded(&nv12, c.width, false)
        .expect("encodes");
    assert!(is_idr_with_headers(&au));
    let sps = parse_sps(&au).expect("SPS");
    assert!(sps.vui_present);
}

/// Teardown with a frame just encoded returns promptly: the drain is bounded.
#[test]
#[ignore = "needs an AMD GPU: times Drop on a real component"]
fn amf_teardown_is_prompt() {
    let c = cfg(1920, 1080, 60, 10_000_000, None);
    let Some(mut enc) = open("amf_teardown_is_prompt", c) else {
        return;
    };
    run(&mut enc, &c, 10, &[]);
    let t = Instant::now();
    drop(enc);
    assert!(t.elapsed().as_millis() < 250, "drop took {:?}", t.elapsed());
}

/// Long-term references on the capture device, as the async ring opens them with
/// `SE_TUNE_OFF=amf_ring_ltr_off`: granted only when the SPS has room, and a repair that names a
/// marked slot yields a P frame, not an IDR.
#[test]
#[ignore = "needs an AMD GPU: exercises MaxOfLTRFrames and ForceLTRReferenceBitfield"]
fn amf_long_term_repair_is_a_p_frame() {
    let c = cfg(1280, 720, 60, 6_000_000, None);
    let Some(probe) = open("amf_long_term_repair_is_a_p_frame", c) else {
        return;
    };
    let device = probe.device().clone();
    // SAFETY: a live device; the immediate context is the one the probe encoder used.
    let context = unsafe { device.GetImmediateContext() }.expect("immediate context");
    drop(probe);
    let mut enc = Encoder::new_on_device_with_ltr(c, &device, &context, 2, 3).expect("opens");
    eprintln!("LTR slots granted: {}", enc.ltr_slots());
    if enc.ltr_slots() == 0 {
        crate::test_support::skip_because(
            "amf_long_term_repair_is_a_p_frame",
            "this runtime's SPS has no room for long-term references",
        );
        return;
    }
    let mark = enc.mark_ltr().expect("a slot");
    let first = enc.encode_slot(0, false).expect("frame 0");
    assert!(is_idr(&first));
    for i in 1..10 {
        enc.encode_slot(i % 3, false).expect("frame");
    }
    assert!(enc.use_ltr(1 << mark));
    let repair = enc.encode_slot(1, false).expect("repair frame");
    assert!(
        !is_idr(&repair),
        "a repair from a held reference is a P frame"
    );
}
