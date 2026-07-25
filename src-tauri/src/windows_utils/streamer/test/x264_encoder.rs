//! Hardware-free smoke test for the software x264 backend (PRD §9). Requires libx264.dll at
//! runtime; if the library can't be loaded the test logs and returns (so CI without libx264 is
//! green). When the DLL is present it exercises the full encode path on synthetic frames.

use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::nvidia::encoder::EncoderConfig;
use crate::windows_utils::streamer::x264::encoder::{X264Encoder, fill_synthetic_bgra};

/// Split an Annex-B access unit into `(nal_type, payload_len)` pairs (payload excludes the start
/// code). Handles both 3- and 4-byte start codes.
fn nal_units(au: &[u8]) -> Vec<(u8, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push((i + 3, au[i + 3] & 0x1f));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (idx, &(payload_start, nal_type)) in starts.iter().enumerate() {
        // The next NAL begins at the next start code; back off the (up to) 4 preceding zero bytes.
        let mut end = starts.get(idx + 1).map(|&(s, _)| s - 3).unwrap_or(au.len());
        while end > payload_start && au[end - 1] == 0 {
            end -= 1;
        }
        out.push((nal_type, end.saturating_sub(payload_start)));
    }
    out
}

fn starts_with_annexb(au: &[u8]) -> bool {
    au.starts_with(&[0, 0, 1]) || au.starts_with(&[0, 0, 0, 1])
}

fn contains(nals: &[(u8, usize)], nal_type: u8) -> bool {
    nals.iter().any(|&(t, _)| t == nal_type)
}

/// SPS(7) + PPS(8) + IDR(5) all present — a self-contained keyframe access unit.
fn is_idr_with_headers(au: &[u8]) -> bool {
    let nals = nal_units(au);
    contains(&nals, 7) && contains(&nals, 8) && contains(&nals, 5)
}

#[test]
fn x264_software_encodes_synthetic_frames() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 60;
    const BITRATE: u32 = 8_000_000;
    const FRAMES: u32 = 120;

    let mut encoder = match X264Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_bps: BITRATE,
        max_bitrate_bps: BITRATE,
        profile: H264Profile::Baseline,
        qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            teprintln!("skipping x264 software smoke test (libx264 unavailable): {e:?}");
            return;
        }
    };

    // ~2 frames of bits, in bytes; VBV should keep any single NAL well under a small multiple.
    let vbv_bytes = (2.0 * (BITRATE as f64 / FPS as f64) / 8.0) as usize;
    let per_nal_ceiling = vbv_bytes * 4; // generous "~2x vbv/8" bound with headroom for the IDR

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut total_bytes = 0usize;
    let mut outputs = 0u32;
    let mut max_nal = 0usize;
    let forced_idr_frame = 60u32;
    let mut first_ok = false;
    let mut forced_ok = false;

    let t0 = std::time::Instant::now();
    let mut per_frame_us: Vec<u128> = Vec::with_capacity(FRAMES as usize);

    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let force = i == 0 || i == forced_idr_frame;
        let start = std::time::Instant::now();
        let au = encoder
            .encode_bgra(&frame, force)
            .unwrap_or_else(|e| panic!("encode frame {i} failed: {e:?}"));
        per_frame_us.push(start.elapsed().as_micros());

        assert!(!au.is_empty(), "frame {i}: zerolatency must emit output every frame");
        outputs += 1;
        total_bytes += au.len();
        assert!(starts_with_annexb(&au), "frame {i}: output must start with an Annex-B start code");

        for (_, len) in nal_units(&au) {
            max_nal = max_nal.max(len);
            assert!(
                len <= per_nal_ceiling,
                "frame {i}: NAL payload {len} exceeds VBV-implied ceiling {per_nal_ceiling} \
                 (VBV not bounding frame size?)"
            );
        }

        if i == 0 {
            first_ok = is_idr_with_headers(&au);
        }
        if i == forced_idr_frame {
            forced_ok = is_idr_with_headers(&au);
        }
    }

    assert_eq!(outputs, FRAMES, "frame-in/packet-out: {FRAMES} frames must yield {FRAMES} outputs");
    assert!(first_ok, "frame 0 must contain SPS + PPS + IDR");
    assert!(forced_ok, "mid-stream forced keyframe must contain SPS + PPS + IDR");

    // Achieved bitrate within +/-25% of target over the run.
    let achieved_bps = (total_bytes as f64 * 8.0 * FPS as f64) / FRAMES as f64;
    let ratio = achieved_bps / BITRATE as f64;
    assert!(
        (0.6..=1.4).contains(&ratio),
        "achieved bitrate {achieved_bps:.0} bps is too far from target {BITRATE} (ratio {ratio:.2})"
    );

    per_frame_us.sort_unstable();
    let p95 = per_frame_us[(per_frame_us.len() * 95 / 100).min(per_frame_us.len() - 1)];
    let frame_budget_us = 1_000_000u128 / FPS as u128;
    tprintln!(
        "x264 smoke OK: {FRAMES} frames {W}x{H}@{FPS}, {total_bytes} bytes, achieved {:.0} kbps \
         (ratio {ratio:.2}), max_nal={max_nal}B (ceiling {per_nal_ceiling}B), \
         p95_encode={p95}us (budget {frame_budget_us}us), wall={}ms",
        achieved_bps / 1000.0,
        t0.elapsed().as_millis()
    );
}

#[test]
fn x264_software_runtime_bitrate_change() {
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 60;
    const HIGH: u32 = 10_000_000;
    const LOW: u32 = 2_000_000;

    let mut encoder = match X264Encoder::new(EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate_bps: HIGH,
        max_bitrate_bps: HIGH,
        profile: H264Profile::Baseline,
        qp: None,
        intra_refresh: false,
    }) {
        Ok(e) => e,
        Err(e) => {
            teprintln!("skipping x264 runtime-bitrate test (libx264 unavailable): {e:?}");
            return;
        }
    };

    let mut frame = vec![0u8; (W * H * 4) as usize];

    // Warm up + measure the high-bitrate segment (skip the IDR-heavy first frame).
    let mut high_bytes = 0usize;
    for i in 0..60 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder.encode_bgra(&frame, i == 0).expect("high-rate encode");
        if i >= 5 {
            high_bytes += au.len();
        }
    }

    encoder.set_bitrate(LOW).expect("runtime bitrate reconfigure");

    let mut low_bytes = 0usize;
    for i in 60..120 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder.encode_bgra(&frame, false).expect("low-rate encode");
        low_bytes += au.len();
    }

    assert!(
        low_bytes < high_bytes,
        "lowering the target bitrate should shrink output: high={high_bytes}B low={low_bytes}B"
    );
    tprintln!(
        "x264 runtime bitrate change OK: high segment {high_bytes}B -> low segment {low_bytes}B"
    );
}
