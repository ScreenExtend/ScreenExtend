//! Functional NVENC tests: real frames on real hardware, asserting what the transport depends
//! on. Without an NVIDIA GPU they skip loudly (`super::skip`) instead of reporting `ok`.

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

/// `NV_ENC_PARAMS_RC_CONSTQP` is 0 and has no bit in the rate-control capability mask, so a
/// plain bitwise test rejects it on every driver and the quality slider silently becomes CBR.
#[test]
fn constant_qp_is_admitted_by_any_rate_control_mask() {
    use crate::windows_utils::streamer::nvidia::encoder::rc_mode_supported;
    use crate::windows_utils::streamer::nvidia::nvenc_sys::NV_ENC_PARAMS_RC_MODE::*;
    // 0x3f is what an RTX 3080 Ti Laptop reports
    for mask in [0u32, 0x3f, 0x2] {
        assert!(
            rc_mode_supported(mask, NV_ENC_PARAMS_RC_CONSTQP),
            "mask {mask:#x}"
        );
    }
    assert!(rc_mode_supported(0x3f, NV_ENC_PARAMS_RC_CBR));
    assert!(rc_mode_supported(0, NV_ENC_PARAMS_RC_CBR));
    assert!(!rc_mode_supported(0x1, NV_ENC_PARAMS_RC_CBR));
}

/// A constant-QP request must reach the hardware: the session reports the QP it opened with, and
/// a lower QP spends more bytes on the same picture than a higher one.
#[test]
fn nvenc_constant_qp_request_is_applied() {
    let open = |qp: u8| {
        Encoder::new(EncoderConfig {
            qp: Some(qp),
            ..cfg(1280, 720, 60, 8_000_000)
        })
    };
    let (mut fine, mut coarse) = match (open(18), open(42)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            super::skip("NVENC constant-QP test (no NVIDIA GPU / driver)", e);
            return;
        }
    };
    assert_eq!(fine.constant_qp(), Some(18), "constant QP fell back to CBR");
    assert_eq!(
        coarse.constant_qp(),
        Some(42),
        "constant QP fell back to CBR"
    );
    let mut frame = vec![0u8; 1280 * 720 * 4];
    fill_synthetic_bgra(&mut frame, 1280, 720, 7);
    let a = fine.encode_bgra(&frame, true).expect("encode qp18").len();
    let b = coarse.encode_bgra(&frame, true).expect("encode qp42").len();
    assert!(
        a > b * 2,
        "QP 18 IDR {a} B is not clearly larger than QP 42 IDR {b} B"
    );
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

    // `eprintln!` on purpose: nothing drains the `tprintln!` log bus under `cargo test`
    eprintln!(
        "NVENC smoke OK: {FRAMES} frames {W}x{H}@{FPS}, {total_bytes} B, \
         p50_encode={p50}us p95={p95}us p99={p99}us (frame budget {budget_us}us), \
         idr={idr_bytes}B median_p={median_p}B (ratio {:.1}x), wall={}ms",
        idr_bytes as f64 / median_p.max(1) as f64,
        t0.elapsed().as_millis()
    );

    // p50 with a wide multiple, not p95: the suite runs its hardware tests in parallel, and
    // the same encoder measures p50 3.5 ms idle against 14 ms contended. Software H.264 at
    // 1080p is tens of milliseconds a frame, so a p50 inside ten budgets still rules it out
    assert!(
        p50 < budget_us * 10,
        "p50 encode {p50}us is more than 10x the {budget_us}us frame budget: that is not a \
         hardware encode"
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

/// Submit/complete two deep against submit-and-drain: no gain, and worse at 4K. There is one
/// NVENC engine (`num_encoder_engines = 1`), so a second in-flight picture queues behind the
/// first, and the pipeline keeps calling `encode_input_slot`.
///
/// RTX 3080 Ti Laptop, driver 580.97, release build, 180 frames:
///
/// ```text
/// 1920x1080  serial p50=3028 p95=3169 p99=6581  | pipelined p50=3067 p95=4364 p99=5954
/// 3840x2160  serial p50=10352 p95=23079 p99=25392 | pipelined p50=13436 p95=23033 p99=28139
/// ```
///
/// Not measured on a multi-engine part (`num_encoder_engines >= 2`).
#[test]
fn nvenc_pipelined_submit_is_not_faster_than_submit_and_drain() {
    // 4K is where it could matter: there the encode is a large fraction of the frame period
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

    // opened back on the same device; the pipeline opens them on the capture adapter, with
    // the same keyed-mutex protocol
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
        assert_eq!(
            crate::windows_utils::streamer::keyed::acquire(mutex, KEY_WRITER, 1000)
                .expect("stage AcquireSync(writer)"),
            crate::windows_utils::streamer::keyed::Acquired::Owned,
            "stage: the encoder did not hand slot {slot} back"
        );
        unsafe {
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

    // depth 1: submit and drain the same picture, as `encode_input_slot` does
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

    // depth 2: one picture always in the hardware while the next is being submitted
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
/// The margin is the pessimistic one: `fill_synthetic_bgra` redraws the whole picture every
/// frame, so the repair measured 0.83x the IDR on an RTX 3080 Ti. Real screen content is mostly
/// static between frames, where the same repair should land near a steady P frame.
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
            "NVENC LTR repair test (encoder opened without long-term references)",
            format!(
                "LTR is opt-in via SE_NVENC_LTR=1 because its SPS under-declares the DPB; \
                 max_ltr_frames={}, multi_ref={}",
                caps.max_ltr_frames, caps.multiple_ref_frames
            ),
        );
        return;
    }

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut idr_bytes = 0usize;
    let mut steady_p: Vec<usize> = Vec::new();

    // frame 0 is the IDR everything is measured against, frame 1 the long-term reference the
    // repair points at
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

    // frame 39 is now the reference: say the client never received it, and repair from the
    // long-term reference marked at frame 1
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

    // an empty bitmap is a no-op, not a request for an intra frame
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

/// Consumer drivers cap concurrent NVENC sessions per system (three historically, eight since
/// r551), and there is one encoder per streaming session, so the cap is a number of clients.
/// The client past it must degrade: `build_backend` catches the error and moves to Quick Sync
/// or x264, and the error must not read as "no NVENC on this machine".
///
/// The cap is probed by attempting, never asserted as a number: it varies by driver branch and
/// by what else on the machine is encoding.
#[test]
fn nvenc_session_limit_degrades_instead_of_panicking() {
    const W: u32 = 640;
    const H: u32 = 360;
    // well past any documented cap, so the loop ends on the driver's refusal, not this bound
    const CEILING: usize = 24;

    let mut held: Vec<Encoder> = Vec::new();
    let mut refusal = None;
    for _ in 0..CEILING {
        match Encoder::new(cfg(W, H, 30, 2_000_000)) {
            Ok(e) => held.push(e),
            Err(e) => {
                refusal = Some(format!("{e:#}"));
                break;
            }
        }
    }

    if held.is_empty() {
        super::skip(
            "NVENC session-limit test (no NVIDIA GPU / driver)",
            refusal.unwrap_or_default(),
        );
        return;
    }

    // an exhausted session pool must not disturb the sessions that already exist
    let mut frame = vec![0u8; (W * H * 4) as usize];
    fill_synthetic_bgra(&mut frame, W, H, 0);
    for (i, enc) in held.iter_mut().enumerate() {
        let au = enc.encode_bgra(&frame, true).unwrap_or_else(|e| {
            panic!("session {i} should still encode after the pool filled: {e:#}")
        });
        assert!(
            is_idr_with_headers(&au),
            "session {i} produced no IDR after the session pool filled"
        );
    }

    match &refusal {
        Some(msg) => {
            assert!(
                msg.contains("nvEncOpenEncodeSessionEx"),
                "the refusal past the session cap must name the call that refused, got: {msg}"
            );
            assert!(
                msg.contains("concurrent-session limit"),
                "a refusal caused by the session cap must say so, or the log reads as \
                 'no NVENC on this machine'; got: {msg}"
            );
            eprintln!(
                "NVENC session limit OK: {} concurrent sessions opened, the next refused with: {msg}",
                held.len()
            );
        }
        None => eprintln!(
            "NVENC session limit OK: {CEILING} concurrent sessions opened without refusal \
             (this driver's cap is above the probe ceiling)"
        ),
    }
}

/// What the per-frame `nvEncMapInputResource` / `nvEncUnmapInputResource` pair costs, for
/// deciding whether to hold the mapping across frames. Printed, not asserted.
#[test]
fn nvenc_map_unmap_cost_per_frame() {
    const W: u32 = 1920;
    const H: u32 = 1080;
    const FRAMES: u32 = 200;

    let mut encoder = match Encoder::new(cfg(W, H, 60, 10_000_000)) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC map/unmap cost test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    let mut frame = vec![0u8; (W * H * 4) as usize];
    fill_synthetic_bgra(&mut frame, W, H, 0);
    // warm-up: the first-frame IDR and any lazy driver allocation are out of the way
    for _ in 0..10 {
        encoder.encode_bgra(&frame, false).expect("warmup encode");
    }

    // the same picture every time, so the spread is the fixed driver round trips
    let mut full: Vec<u128> = Vec::with_capacity(FRAMES as usize);
    for _ in 0..FRAMES {
        let t = std::time::Instant::now();
        encoder.encode_bgra(&frame, false).expect("steady encode");
        full.push(t.elapsed().as_micros());
    }
    // a repeat goes through the same map/unmap pair without a new upload, so the difference
    // between the two is the `UpdateSubresource`
    let mut repeat: Vec<u128> = Vec::with_capacity(FRAMES as usize);
    for _ in 0..FRAMES {
        let t = std::time::Instant::now();
        encoder.encode_repeat(false).expect("repeat encode");
        repeat.push(t.elapsed().as_micros());
    }

    full.sort_unstable();
    repeat.sort_unstable();
    eprintln!(
        "NVENC per-frame cost at {W}x{H}: upload+map+encode p50={}us p95={}us | \
         map+encode only p50={}us p95={}us",
        percentile(&full, 50),
        percentile(&full, 95),
        percentile(&repeat, 50),
        percentile(&repeat, 95),
    );
}

/// The NVENC adapter (and the clock hold's, which uses the same function) is chosen by PCI vendor
/// id, not by the word "NVIDIA" in the description.
#[test]
fn nvenc_adapter_is_chosen_by_pci_vendor_id() {
    use crate::windows_utils::streamer::nvidia::encoder::is_nvenc_adapter;
    const SOFTWARE: u32 = 2; // DXGI_ADAPTER_FLAG_SOFTWARE
    assert!(is_nvenc_adapter(0x10DE, 0), "an NVIDIA hardware adapter");
    assert!(
        !is_nvenc_adapter(0x10DE, SOFTWARE),
        "a software adapter is never NVENC"
    );
    assert!(!is_nvenc_adapter(0x8086, 0), "Intel");
    assert!(!is_nvenc_adapter(0x1002, 0), "AMD");
    assert!(
        !is_nvenc_adapter(0x1414, SOFTWARE),
        "Microsoft Basic Render Driver"
    );
}

/// A session opened before the `Encoder` exists is destroyed if construction fails, and handed
/// over intact if it succeeds.
#[test]
fn an_unfinished_encoder_destroys_its_session() {
    use crate::windows_utils::streamer::nvidia::encoder::SessionGuard;
    use crate::windows_utils::streamer::nvidia::nvenc_sys::NVENCSTATUS;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DESTROYED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn fake_destroy(_encoder: *mut std::ffi::c_void) -> NVENCSTATUS {
        DESTROYED.fetch_add(1, Ordering::SeqCst);
        NVENCSTATUS::NV_ENC_SUCCESS
    }
    let fake: *mut std::ffi::c_void = std::ptr::without_provenance_mut(0x1000);

    drop(SessionGuard::new(fake, Some(fake_destroy)));
    assert_eq!(
        DESTROYED.load(Ordering::SeqCst),
        1,
        "an abandoned session must be destroyed"
    );

    let raw = SessionGuard::new(fake, Some(fake_destroy)).into_raw();
    assert_eq!(raw, fake);
    assert_eq!(
        DESTROYED.load(Ordering::SeqCst),
        1,
        "a session handed to its owner must not be destroyed by the guard"
    );
}

/// `nvenc_rate_budget_observed_fps`: the CBR budget follows the rate pictures really arrive at,
/// only upward, and not for a wobble inside 10 %.
#[test]
fn rate_budget_follows_the_observed_rate_upward_only() {
    use crate::windows_utils::streamer::nvidia::encoder::{budget_fps, scaled_bitrate};
    // a 60 fps session receiving 113 pictures a second (cursor-exempt frames)
    assert_eq!(budget_fps(60, 113, 60), Some(113));
    // slower than configured needs no correction
    assert_eq!(budget_fps(60, 30, 60), None);
    // back to the configured rate once the burst is over
    assert_eq!(budget_fps(60, 58, 113), Some(60));
    // inside the band: no reconfigure
    assert_eq!(budget_fps(60, 65, 60), None);
    assert_eq!(budget_fps(60, 110, 113), None);

    // 113 pictures each given avg/60 must add up to the 10 Mbps target
    let avg = scaled_bitrate(10_000_000, 60, 113);
    assert_eq!(avg, 5_309_734);
    let spent = u64::from(avg) / 60 * 113;
    assert!((9_900_000..=10_000_000).contains(&spent), "{spent}");
    assert_eq!(
        scaled_bitrate(10_000_000, 60, 60),
        10_000_000,
        "no scaling at the configured rate"
    );
    assert_eq!(scaled_bitrate(0, 60, 120), 1, "never zero");
}

/// `nvenc_single_copy_au`: the buffer allocated before the bitstream lock fits ordinary
/// frame-to-frame growth.
#[test]
fn the_output_buffer_is_sized_ahead_of_the_lock() {
    use crate::windows_utils::streamer::nvidia::encoder::next_au_capacity;
    assert_eq!(next_au_capacity(0), 64 * 1024);
    assert_eq!(next_au_capacity(30_000), 64 * 1024);
    assert_eq!(next_au_capacity(200_000), 400_000);
    assert_eq!(next_au_capacity(usize::MAX), usize::MAX);
}

/// A ring slot the writer never handed over must not be encoded.
///
/// `AcquireSync` reports a timeout as a success code, so an unchecked acquire would encode a
/// texture it does not own and free the slot with the key still wrong, where the ring's
/// `Poisoned` recovery never runs. Nothing here stages slot 0, so its key is still `KEY_WRITER`
/// and the encoder's `KEY_ENCODER` acquire must time out.
#[test]
fn nvenc_ring_slot_the_writer_never_released_is_not_encoded() {
    use crate::windows_utils::streamer::nvidia::encoder::{SlotEncodeError, KEY_ENCODER};
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Device1, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

    let mut encoder = match Encoder::new_shared_ring(cfg(640, 360, 60, 2_000_000), 2) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC keyed-mutex test (no NVIDIA GPU / driver)", e);
            return;
        }
    };

    // the `windows` wrapper itself reports a timed-out acquire as `Ok`
    let device: ID3D11Device1 = encoder.device().cast().expect("ID3D11Device1");
    let handle = encoder.shared_handles()[1];
    let tex: ID3D11Texture2D =
        unsafe { device.OpenSharedResource1(handle) }.expect("OpenSharedResource1");
    let mutex: IDXGIKeyedMutex = tex.cast().expect("as IDXGIKeyedMutex");
    assert!(
        unsafe { mutex.AcquireSync(KEY_ENCODER, 10) }.is_ok(),
        "the windows wrapper reports WAIT_TIMEOUT as success; if this changed, keyed.rs can go"
    );

    let t = std::time::Instant::now();
    match encoder.encode_input_slot(0, true) {
        Err(SlotEncodeError::Poisoned(e)) => {
            let msg = format!("{e:#}");
            assert!(msg.contains("timed out"), "{msg}");
            eprintln!(
                "NVENC unowned slot refused after {} ms: {msg}",
                t.elapsed().as_millis()
            );
        }
        Ok(au) => panic!(
            "slot 0 was never released to the encoder, yet it was encoded ({} B)",
            au.len()
        ),
        Err(e) => panic!("expected Poisoned, got {e:?}"),
    }

    // the refused acquire took nothing: slot 0 is still the writer's and the keepalive (which
    // takes it at KEY_WRITER) encodes it, and the IDR asked for above is still pending
    let au = encoder
        .encode_repeat_slot(0)
        .expect("slot 0 is still at KEY_WRITER");
    assert!(
        is_idr_with_headers(&au),
        "the IDR request must survive a refused acquire: NALs {:?}",
        nal_units(&au)
    );
}

/// With `SE_INTRA_REFRESH`'s path open, every picture stays one slice: the single-slice cap is
/// requested, not only checked.
#[test]
fn nvenc_intra_refresh_keeps_one_slice_per_picture() {
    const W: u32 = 1280;
    const H: u32 = 720;
    let mut encoder = match Encoder::new(EncoderConfig {
        intra_refresh: true,
        ..cfg(W, H, 60, 8_000_000)
    }) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC intra-refresh slice test (no NVIDIA GPU / driver)", e);
            return;
        }
    };
    let caps = encoder.caps();
    if !(caps.intra_refresh && caps.single_slice_intra_refresh) {
        crate::test_support::skip_because(
            "NVENC intra-refresh slice test",
            "this engine does not advertise single-slice intra refresh",
        );
        return;
    }
    assert_eq!(
        encoder.intra_refresh_config(),
        (true, true),
        "intra refresh must be opened with singleSliceIntraRefresh"
    );

    let mut frame = vec![0u8; (W * H * 4) as usize];
    let mut worst = 0usize;
    for i in 0..120u32 {
        fill_synthetic_bgra(&mut frame, W, H, i);
        let au = encoder.encode_bgra(&frame, i == 0).expect("encode");
        let slices = nal_units(&au)
            .iter()
            .filter(|(t, _)| *t == 1 || *t == 5)
            .count();
        worst = worst.max(slices);
        assert_eq!(
            slices,
            1,
            "frame {i}: {slices} slices, NALs {:?}",
            nal_units(&au)
        );
    }
    eprintln!("NVENC intra refresh OK: 120 frames, at most {worst} slice per picture");
}

/// Teardown with a picture still in the hardware: `Drop` completes it, flushes with an
/// end-of-stream picture and only then frees, and does so promptly. (Reopening a session after
/// the drop is not asserted: the suite's session-limit test holds the whole pool in parallel.)
#[test]
fn nvenc_drop_with_a_picture_in_flight_is_prompt() {
    use crate::windows_utils::streamer::keyed::{acquire, Acquired};
    use crate::windows_utils::streamer::nvidia::encoder::{
        EosFlush, KEY_ENCODER, KEY_WRITER, LAST_TEARDOWN,
    };
    use windows::core::Interface as _;
    use windows::Win32::Graphics::Direct3D11::{ID3D11Device1, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::IDXGIKeyedMutex;

    let mut encoder = match Encoder::new_shared_ring(cfg(1280, 720, 60, 8_000_000), 2) {
        Ok(e) => e,
        Err(e) => {
            super::skip("NVENC teardown test (no NVIDIA GPU / driver)", e);
            return;
        }
    };
    let device: ID3D11Device1 = encoder.device().cast().expect("ID3D11Device1");
    let tex: ID3D11Texture2D = unsafe { device.OpenSharedResource1(encoder.shared_handles()[0]) }
        .expect("OpenSharedResource1");
    let mutex: IDXGIKeyedMutex = tex.cast().expect("as IDXGIKeyedMutex");
    assert_eq!(
        acquire(&mutex, KEY_WRITER, 1000).expect("acquire"),
        Acquired::Owned
    );
    unsafe { mutex.ReleaseSync(KEY_ENCODER) }.expect("release to the encoder");
    drop(mutex);
    drop(tex);

    encoder
        .submit_slot(0, true)
        .expect("submit without completing");
    LAST_TEARDOWN.with(|r| r.set(None));
    let t = std::time::Instant::now();
    drop(encoder);
    let took = t.elapsed();
    let record = LAST_TEARDOWN
        .with(|r| r.get())
        .expect("Drop recorded its teardown");
    eprintln!(
        "NVENC drop with one picture in flight: {} ms, {record:?}",
        took.as_millis()
    );
    // the order matters, not only the speed: the picture in flight completes before anything
    // is unregistered, and an end-of-stream picture is submitted (and, in async mode,
    // signalled) before the session is freed
    assert_eq!(
        record.inflight_at_drop, 1,
        "the test put one picture in flight"
    );
    assert_eq!(
        record.inflight_at_unregister, 0,
        "a picture was still in flight when the ring was unregistered"
    );
    let expected = if record.async_mode {
        EosFlush::Signalled
    } else {
        EosFlush::Submitted
    };
    assert_eq!(record.eos, expected, "the end-of-stream flush");
    assert!(
        took < std::time::Duration::from_millis(400),
        "teardown took {took:?}: the end-of-stream flush did not signal"
    );
}
