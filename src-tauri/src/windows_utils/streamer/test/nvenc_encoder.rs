//! Functional NVENC tests.
//!
//! `nvenc_layout.rs` is 6167 lines of struct-offset assertions that pass without an NVIDIA GPU
//! in the machine and prove nothing about the encoder. These encode real frames on real
//! hardware and assert the properties the transport depends on. They skip loudly (see
//! `super::skip`) rather than reporting `ok` for a path that never ran.

use crate::streamer::config::H264Profile;
use crate::windows_utils::streamer::nvidia::encoder::{
    fill_synthetic_bgra, Encoder, EncoderConfig, RefInvalidation,
};

use super::h264::{is_idr_with_headers, nal_units, parse_sps, starts_with_annexb};

fn cfg(width: u32, height: u32, fps: u32, bitrate: u32) -> EncoderConfig {
    EncoderConfig {
        width,
        height,
        fps,
        bitrate_bps: bitrate,
        max_bitrate_bps: bitrate,
        profile: H264Profile::High,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }
}

fn percentile(sorted: &[u128], pct: usize) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(sorted.len() * pct / 100).min(sorted.len() - 1)]
}

#[test]
fn nvenc_encodes_synthetic_frames() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 60;
    const BITRATE: u32 = 10_000_000;
    const FRAMES: u32 = 120;

    let mut encoder = match Encoder::new(cfg(W, H, FPS, BITRATE)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC smoke test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut per_frame_us: Vec<u128> = Vec::with_capacity(FRAMES as usize);
    let mut total_bytes = 0usize;
    let forced_idr_frame = 60u32;
    let mut first_ok = false;
    let mut forced_ok = false;
    let mut idr_bytes = 0usize;
    let mut p_bytes: Vec<usize> = Vec::new();
    let t0 = std::time::Instant::now();

    for i in 0..FRAMES {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let force = i == 0 || i == forced_idr_frame;
        let start = std::time::Instant::now();
        let au = encoder
            .encode_bgra(&frame, force)
            .unwrap_or_else(|e| panic!("NVENC encode frame {i} failed: {e:?}"));
        per_frame_us.push(start.elapsed().as_micros());

        assert!(
            !au.is_empty(),
            "frame {i}: ULL tuning must emit one access unit per input frame"
        );
        assert!(
            starts_with_annexb(&au),
            "frame {i}: NVENC output must start with an Annex-B start code"
        );
        total_bytes += au.len();

        if i == 0 {
            first_ok = is_idr_with_headers(&au);
            let sps = parse_sps(&au).expect("frame 0 must carry a parseable SPS");
            assert!(
                matches!(sps.profile_idc, 66 | 77 | 100),
                "SPS profile_idc {} is not Baseline/Main/High ({sps:?})",
                sps.profile_idc
            );
            assert_eq!(sps.chroma_format_idc, 1, "NVENC must emit 4:2:0 ({sps:?})");
            assert!(
                sps.bitstream_restriction && sps.max_num_reorder_frames == 0,
                "SPS must carry bitstream_restriction_flag with max_num_reorder_frames=0 so a \
                 decoder does not hold frames waiting for reordering that never happens ({sps:?})"
            );
            eprintln!("NVENC SPS: {sps:?}");
        } else if i == forced_idr_frame {
            forced_ok = is_idr_with_headers(&au);
            idr_bytes = au.len();
        } else if i > 5 {
            p_bytes.push(au.len());
        }
    }

    assert!(first_ok, "frame 0 must contain SPS + PPS + IDR");
    assert!(
        forced_ok,
        "a mid-stream forced keyframe must contain SPS + PPS + IDR (repeatSPSPPS)"
    );

    p_bytes.sort_unstable();
    let median_p = p_bytes[p_bytes.len() / 2];
    per_frame_us.sort_unstable();
    let p50 = percentile(&per_frame_us, 50);
    let p95 = percentile(&per_frame_us, 95);
    let p99 = percentile(&per_frame_us, 99);
    let budget_us = 1_000_000u128 / FPS as u128;

    // `eprintln!` on purpose: `tprintln!` goes into the log-bus ring buffer, which nothing
    // drains under `cargo test`, so a measurement written with it is invisible.
    eprintln!(
        "NVENC smoke OK: {FRAMES} frames {W}x{H}@{FPS}, {total_bytes} B, \
         p50_encode={p50}us p95={p95}us p99={p99}us (frame budget {budget_us}us), \
         idr={idr_bytes}B median_p={median_p}B (ratio {:.1}x), wall={}ms",
        idr_bytes as f64 / median_p.max(1) as f64,
        t0.elapsed().as_millis()
    );

    assert!(
        p95 < budget_us * 4,
        "p95 encode {p95}us is more than 4x the {budget_us}us frame budget"
    );
}

#[test]
fn nvenc_runtime_bitrate_change() {
    const W: u32 = 1280;
    const H: u32 = 720;
    const FPS: u32 = 60;
    const HIGH: u32 = 12_000_000;
    const LOW: u32 = 2_000_000;

    let mut encoder = match Encoder::new(cfg(W, H, FPS, HIGH)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC runtime-bitrate test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut high_bytes = 0usize;
    for i in 0..90 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder
            .encode_bgra(&frame, i == 0)
            .expect("high-rate encode");
        if i >= 30 {
            high_bytes += au.len();
        }
    }

    assert_eq!(
        encoder
            .set_bitrate(LOW)
            .expect("runtime bitrate reconfigure"),
        crate::streamer::platform::BitrateApplied::Applied,
        "CBR NVENC must report a bitrate change as actually applied"
    );

    let mut low_bytes = 0usize;
    for i in 90..180 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder.encode_bgra(&frame, false).expect("low-rate encode");
        if i >= 120 {
            low_bytes += au.len();
        }
    }

    assert!(
        low_bytes < high_bytes,
        "lowering the target bitrate must shrink output: high={high_bytes}B low={low_bytes}B"
    );
    eprintln!("NVENC runtime bitrate change OK: {high_bytes}B -> {low_bytes}B");
}

/// The static-desktop recovery path: the ring's keepalive re-encodes the held slot, and a
/// pending `request_idr()` has to turn that repeat into a real IDR. Without it a client that
/// joins or loses packets while nothing on screen moves never receives a decodable frame.
#[test]
fn nvenc_repeat_honours_a_pending_idr_request() {
    const W: u32 = 1280;
    const H: u32 = 720;

    let mut encoder = match Encoder::new(cfg(W, H, 60, 8_000_000)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC repeat-IDR test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    let mut frame = vec![0u8; (W * H * 4) as usize];
    for i in 0..10 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        encoder.encode_bgra(&frame, i == 0).expect("priming encode");
    }

    let repeat = encoder.encode_repeat(false).expect("plain repeat");
    assert!(
        !is_idr_with_headers(&repeat),
        "a repeat with no pending request must stay a P frame"
    );

    encoder.request_idr();
    let repaired = encoder
        .encode_repeat(false)
        .expect("repeat after request_idr");
    assert!(
        is_idr_with_headers(&repaired),
        "encode_repeat must honour a pending request_idr(): got NALs {:?}",
        nal_units(&repaired)
    );

    let after = encoder.encode_repeat(false).expect("repeat after the IDR");
    assert!(
        !is_idr_with_headers(&after),
        "the IDR request must be one-shot, not sticky"
    );
    eprintln!("NVENC repeat-IDR OK: {} B IDR", repaired.len());
}

/// A capability table read from the driver, not asserted from a header. Prints it so a report
/// from a user's machine says what their GPU actually supports.
#[test]
fn nvenc_reports_its_capabilities() {
    let encoder = match Encoder::new(cfg(1280, 720, 60, 8_000_000)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC capability probe (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    let caps = encoder.caps();
    eprintln!("NVENC caps: {caps:?}");
    assert!(
        caps.width_max >= 1920 && caps.height_max >= 1080,
        "any NVENC engine we support must reach 1080p (got {}x{})",
        caps.width_max,
        caps.height_max
    );
    assert!(
        caps.dyn_bitrate_change,
        "the loss-based bitrate driver requires NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE"
    );
}

/// What the submit/complete split is actually worth. **Measured answer: nothing, and at 4K it
/// is worse.** Kept as the instrument, and so the result is not re-derived.
///
/// The theory was that driving `submit_slot` / `complete_slot` two deep would let the submit of
/// N+1 overlap the hardware's work on N. On an Ampere part it does not, because there is one
/// NVENC engine (`num_encoder_engines = 1`): a second in-flight picture queues behind the
/// first rather than running beside it, so per-frame submit->done can only grow. Throughput was
/// never the constraint either — 3 ms of encode in a 16.6 ms budget at 1080p60.
///
/// RTX 3080 Ti Laptop, driver 580.97, release build, 180 frames:
///
/// ```text
/// 1920x1080  serial p50=3028 p95=3169 p99=6581  | pipelined p50=3067 p95=4364 p99=5954
/// 3840x2160  serial p50=10352 p95=23079 p99=25392 | pipelined p50=13436 p95=23033 p99=28139
/// ```
///
/// So the pipeline should keep calling `encode_input_slot`. This would need re-running on a
/// multi-engine part (Ada/Blackwell desktop, `num_encoder_engines >= 2`) before the conclusion
/// generalises.
#[test]
fn nvenc_pipelined_submit_is_not_faster_than_submit_and_drain() {
    // 4K is where it can matter: encode time scales with area, so at 1080p there is 13 ms of
    // slack in a 60 fps budget and nothing to hide, while at 4K the encode is a large fraction
    // of the frame period.
    for (w, h) in [(1920u32, 1080u32), (3840, 2160)] {
        measure_pipelining(w, h);
    }
}

fn measure_pipelining(w: u32, h: u32) {
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Device1, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

    use crate::windows_utils::streamer::nvidia::encoder::{KEY_ENCODER, KEY_WRITER};

    #[allow(non_snake_case)]
    let (W, H) = (w, h);
    const FPS: u32 = 60;
    const RING: usize = 3;
    const FRAMES: u32 = 180;

    let mut encoder = match Encoder::new_shared_ring(cfg(W, H, FPS, 30_000_000), RING) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC pipelining test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    // Open the ring's shared textures back on the same device. The pipeline opens them on the
    // capture adapter instead; the keyed-mutex protocol is identical either way.
    let device: ID3D11Device1 = encoder
        .device()
        .cast()
        .expect("NVENC device as ID3D11Device1");
    let context = unsafe { encoder.device().GetImmediateContext() }.expect("immediate context");
    let staged: Vec<(ID3D11Texture2D, IDXGIKeyedMutex)> = encoder
        .shared_handles()
        .into_iter()
        .map(|h| {
            let tex: ID3D11Texture2D =
                unsafe { device.OpenSharedResource1(h) }.expect("OpenSharedResource1");
            let mutex: IDXGIKeyedMutex = tex.cast().expect("as IDXGIKeyedMutex");
            (tex, mutex)
        })
        .collect();

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let stage = |slot: usize, i: u32, frame: &mut Vec<u8>| {
        fill_synthetic_bgra(frame, W, H, i);
        let (tex, mutex) = &staged[slot];
        unsafe {
            mutex
                .AcquireSync(KEY_WRITER, 1000)
                .expect("stage AcquireSync(writer)");
            context.UpdateSubresource(
                tex,
                0,
                None,
                frame.as_ptr() as *const std::ffi::c_void,
                W * 4,
                0,
            );
            context.Flush();
            mutex
                .ReleaseSync(KEY_ENCODER)
                .expect("stage ReleaseSync(encoder)");
        }
    };

    // Depth 1: submit and drain the same picture, which is what `encode_input_slot` does.
    let mut serial: Vec<u128> = Vec::with_capacity(FRAMES as usize);
    for i in 0..FRAMES {
        let slot = (i as usize) % RING;
        stage(slot, i, &mut frame);
        let t = std::time::Instant::now();
        encoder
            .submit_slot(slot, i == 0)
            .unwrap_or_else(|e| panic!("submit frame {i}: {e:?}"));
        let au = encoder
            .complete_slot(slot)
            .unwrap_or_else(|e| panic!("complete frame {i}: {e:?}"));
        serial.push(t.elapsed().as_micros());
        assert!(!au.is_empty(), "frame {i} produced no bytes");
    }

    // Depth 2: one picture always in the hardware while the next is being submitted.
    let mut pipelined: Vec<u128> = Vec::with_capacity(FRAMES as usize);
    let mut outstanding: Option<(usize, std::time::Instant)> = None;
    for i in 0..FRAMES {
        let slot = (i as usize) % RING;
        stage(slot, i, &mut frame);
        let t = std::time::Instant::now();
        encoder
            .submit_slot(slot, false)
            .unwrap_or_else(|e| panic!("pipelined submit {i}: {e:?}"));
        if let Some((prev, started)) = outstanding.take() {
            let au = encoder
                .complete_slot(prev)
                .unwrap_or_else(|e| panic!("pipelined complete {i}: {e:?}"));
            pipelined.push(started.elapsed().as_micros());
            assert!(!au.is_empty(), "pipelined frame produced no bytes");
        }
        outstanding = Some((slot, t));
    }
    if let Some((prev, started)) = outstanding.take() {
        let _ = encoder.complete_slot(prev).expect("final drain");
        pipelined.push(started.elapsed().as_micros());
    }

    serial.sort_unstable();
    pipelined.sort_unstable();
    eprintln!(
        "NVENC submit->done at {W}x{H}@{FPS}, {FRAMES} frames, ring={RING}, depth={}: \
         serial p50={}us p95={}us p99={}us | pipelined p50={}us p95={}us p99={}us",
        encoder.pipeline_depth(),
        percentile(&serial, 50),
        percentile(&serial, 95),
        percentile(&serial, 99),
        percentile(&pipelined, 50),
        percentile(&pipelined, 95),
        percentile(&pipelined, 99),
    );
}

/// The loss-repair API: mark a long-term reference, invalidate a frame the client never got,
/// then predict the next picture from the LTR instead of paying for an IDR.
///
/// This is the encoder half of `major-nvenc-reference-invalidation-now-has-its-transport-input`.
/// The transport half — resolving a lost RTP sequence number to a frame index — is W1's; what is
/// asserted here is that the three calls do what their names say on real hardware, and that the
/// repair frame is cheaper than the IDR it replaces.
///
/// The margin here is the pessimistic one and should be read that way. `fill_synthetic_bgra`
/// redraws the whole picture every frame, so predicting frame 40 from the long-term reference
/// marked at frame 1 leaves a residual close to an intra frame's worth — measured at 0.83x the
/// IDR on this RTX 3080 Ti. Real screen content is mostly static between frames, where the same
/// repair should land near a steady P frame. What this pins is that the mechanism works and does
/// not cost *more* than the keyframe it exists to avoid.
#[test]
fn nvenc_ltr_repair_is_cheaper_than_an_idr() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const FPS: u32 = 60;
    const BITRATE: u32 = 10_000_000;

    let mut encoder = match Encoder::new(cfg(W, H, FPS, BITRATE)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC LTR repair test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    let caps = encoder.caps();
    if encoder.ltr_slots() == 0 {
        super::skip(
            "NVENC LTR repair test (engine advertises no long-term references)",
            format!(
                "max_ltr_frames={}, multi_ref={}",
                caps.max_ltr_frames, caps.multiple_ref_frames
            ),
        );
        return;
    }

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut idr_bytes = 0usize;
    let mut steady_p: Vec<usize> = Vec::new();

    // Frame 0 is the IDR everything is measured against. Frame 1 is marked as the long-term
    // reference the repair will point at; the rest are ordinary P frames.
    let ltr_slot = 1u32;
    for i in 0..40u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        if i == ltr_slot {
            let slot = encoder
                .mark_ltr()
                .expect("mark_ltr on an LTR-capable engine");
            assert!(
                slot < encoder.ltr_slots(),
                "mark_ltr returned slot {slot} outside the {} it opened with",
                encoder.ltr_slots()
            );
        }
        let au = encoder.encode_bgra(&frame, i == 0).expect("encode");
        match i {
            0 => idr_bytes = au.len(),
            n if n >= 10 => steady_p.push(au.len()),
            _ => {}
        }
    }

    // Frame 39 is now the reference. Say the client never received it, and repair by predicting
    // from the long-term reference marked back at frame 1.
    let invalidated = encoder.invalidate_reference(39);
    assert_ne!(
        invalidated,
        RefInvalidation::Unsupported,
        "the engine advertised ref-pic invalidation ({}), so the call must not report it \
         unsupported",
        caps.ref_pic_invalidation
    );
    assert!(
        encoder.use_ltr(1 << 0),
        "use_ltr must be accepted on an LTR-capable engine"
    );
    fill_synthetic_bgra(&mut frame, W, H, 40);
    let repair_bytes = encoder
        .encode_bgra(&frame, false)
        .expect("repair encode")
        .len();

    // An empty bitmap is a no-op, not a request for an intra frame.
    assert!(!encoder.use_ltr(0), "an empty LTR bitmap must be refused");

    steady_p.sort_unstable();
    let median_p = steady_p[steady_p.len() / 2];
    eprintln!(
        "NVENC LTR repair OK: slots={}, invalidate={invalidated:?}, \
         idr={idr_bytes}B median_p={median_p}B repair={repair_bytes}B \
         (repair is {:.2}x the IDR, {:.1}x a steady P)",
        encoder.ltr_slots(),
        repair_bytes as f64 / idr_bytes as f64,
        repair_bytes as f64 / median_p as f64,
    );

    assert!(
        repair_bytes < idr_bytes,
        "an LTR repair frame ({repair_bytes} B) that costs more than the IDR it replaces \
         ({idr_bytes} B) is not a repair"
    );
}
