use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;
use windows::core::Interface;

use crate::streamer::config::{Config, ScalePercent};
use crate::streamer::telemetry::FrameTiming;
use crate::windows_capture::capture::GraphicsCaptureApiError;
use crate::windows_utils::streamer::keyed::{self, Acquired};
use crate::windows_utils::streamer::pipeline::started_backend_label;
use crate::windows_utils::streamer::pipeline::{
    bitrate_bypasses_backoff, dxgi_needs_scaler, live_encoder_config, recv_frame_within,
    require_owned, resolve_capture_name_with, scaled_dims, settle_forced_idr, take_free_slot,
    wgc_start_error, Backend, DxgiCadence, DxgiControl, EncodedFrame, ProbeRecv, RebuildBudget,
    RetryBackoff, StaleMonitor,
};
use crate::windows_utils::streamer::x264::encoder::X264Encoder;

/// A capture item that cannot be created from the monitor handle is the one WGC start failure
/// `start_on_monitor` retries before falling to DXGI; every other failure goes straight there.
#[test]
fn only_an_item_conversion_failure_is_classified_stale() {
    let stale = wgc_start_error(GraphicsCaptureApiError::ItemConvertFailed);
    assert!(stale.downcast_ref::<StaleMonitor>().is_some());
    assert!(format!("{stale:#}").contains("GraphicsCaptureItem"));

    let other = wgc_start_error(GraphicsCaptureApiError::FailedToInitWinRT);
    assert!(other.downcast_ref::<StaleMonitor>().is_none());
}

#[test]
fn scale_100_is_native_passthrough() {
    assert_eq!(
        scaled_dims(1920, 1080, ScalePercent::new(100)),
        (1920, 1080)
    );
    assert_eq!(
        scaled_dims(1921, 1081, ScalePercent::new(100)),
        (1920, 1080)
    );
}

#[test]
fn scale_90_of_1080p() {
    assert_eq!(scaled_dims(1920, 1080, ScalePercent::new(90)), (1728, 972));
}

#[test]
fn scale_rounds_to_even() {
    assert_eq!(scaled_dims(1920, 1080, ScalePercent::new(75)), (1440, 810));
    assert_eq!(scaled_dims(1366, 768, ScalePercent::new(50)), (682, 384));
}

#[test]
fn scale_clamps_out_of_range() {
    assert_eq!(ScalePercent::new(150).percent(), 100);
    assert_eq!(ScalePercent::new(0).percent(), ScalePercent::MIN);
    assert!(ScalePercent::new(100).is_native());
    assert!(!ScalePercent::new(99).is_native());
}

#[test]
fn scale_parse_accepts_percent_and_bare() {
    assert_eq!(ScalePercent::parse("90"), Some(ScalePercent::new(90)));
    assert_eq!(ScalePercent::parse(" 80% "), Some(ScalePercent::new(80)));
    assert_eq!(ScalePercent::parse("abc"), None);
}

#[test]
fn scale_never_produces_zero_dims() {
    let (w, h) = scaled_dims(8, 8, ScalePercent::new(ScalePercent::MIN));
    assert!(w >= 2 && h >= 2);
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn encoder_rate_follows_the_display_and_the_configured_ceiling() {
    let mut cfg = Config::default();
    cfg.fps = None;
    cfg.max_fps = 20;
    let enc = live_encoder_config(1920, 1080, 60, &cfg);
    assert_eq!(enc.fps, 20, "the ceiling wins over the display rate");

    cfg.max_fps = 144;
    let enc = live_encoder_config(1920, 1080, 120, &cfg);
    assert_eq!(enc.fps, 120);

    // a 30 Hz display composes 30 times a second; encoding 60 only produces duplicates
    cfg.max_fps = 240;
    let enc = live_encoder_config(1920, 1080, 30, &cfg);
    assert_eq!(enc.fps, 30);

    // an unreported rate assumes 60
    let enc = live_encoder_config(1920, 1080, 0, &cfg);
    assert_eq!(enc.fps, 60);
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn quality_slider_is_a_ceiling_not_a_rate_control_mode() {
    // the slider landing in `Config::qp` would put NVENC in CONSTQP, where every runtime
    // bitrate change is a silent no-op
    let mut cfg = Config::default();
    cfg.quality_qp = Some(28);
    let enc = live_encoder_config(1920, 1080, 60, &cfg);
    assert_eq!(enc.qp, None, "the slider must not select constant-QP");
    assert_eq!(enc.min_qp, Some(28), "the slider is a quality ceiling");
    assert!(enc.bitrate_bps > 0, "bitrate control stays active");
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn explicit_qp_still_selects_constant_qp() {
    let mut cfg = Config::default();
    cfg.qp = Some(26);
    let enc = live_encoder_config(1920, 1080, 60, &cfg);
    assert_eq!(enc.qp, Some(26));
    assert_eq!(enc.min_qp, None);
}

/// A reconfigure the encoder keeps refusing is retried after a growing wait and logged at
/// 1, 2, 4, 8..., not on every frame.
#[test]
fn a_refused_retry_backs_off_and_logs_sparsely() {
    let t0 = Instant::now();
    let mut retry = RetryBackoff::new(true, Duration::from_millis(100), Duration::from_secs(5));
    assert!(retry.ready(t0));

    let mut logged = Vec::new();
    let mut now = t0;
    for n in 1..=12u32 {
        assert!(
            retry.ready(now),
            "attempt {n} should be allowed once the wait has run out"
        );
        if retry.failed(now) {
            logged.push(n);
        }
        // held back right after the failure...
        assert!(!retry.ready(now + Duration::from_millis(50)));
        // ...for 100 ms doubling, capped at 5 s
        let wait =
            Duration::from_millis(100 * (1u64 << (n - 1).min(16))).min(Duration::from_secs(5));
        assert!(!retry.ready(now + wait - Duration::from_millis(1)));
        assert!(retry.ready(now + wait));
        now += wait;
    }
    assert_eq!(logged, vec![1, 2, 4, 8], "only powers of two are logged");

    retry.succeeded();
    assert_eq!(retry.failures(), 0);
    assert!(retry.ready(now));
}

/// With the switch off the backoff holds nothing back and logs every failure.
#[test]
fn a_disabled_retry_is_the_old_every_frame_retry() {
    let t0 = Instant::now();
    let mut retry = RetryBackoff::new(false, Duration::from_millis(100), Duration::from_secs(5));
    for _ in 0..10 {
        assert!(retry.failed(t0));
        assert!(retry.ready(t0));
        assert!(retry.ready_now());
    }
}

#[test]
fn a_forced_keyframe_survives_a_failed_encode_and_is_counted_when_sent() {
    let request = AtomicBool::new(false);
    let sent = AtomicU64::new(0);

    let failed: anyhow::Result<Vec<u8>> = Err(anyhow::anyhow!("device busy"));
    settle_forced_idr(&failed, true, &request, &sent);
    assert!(
        request.load(Ordering::Relaxed),
        "the request must be put back"
    );
    assert_eq!(sent.load(Ordering::Relaxed), 0);

    request.store(false, Ordering::Relaxed);
    let ok: anyhow::Result<Vec<u8>> = Ok(vec![0, 0, 0, 1, 0x65]);
    settle_forced_idr(&ok, true, &request, &sent);
    assert!(!request.load(Ordering::Relaxed));
    assert_eq!(sent.load(Ordering::Relaxed), 1);

    // an unforced encode touches neither, whatever its outcome
    settle_forced_idr(&failed, false, &request, &sent);
    settle_forced_idr(&ok, false, &request, &sent);
    assert!(!request.load(Ordering::Relaxed));
    assert_eq!(sent.load(Ordering::Relaxed), 1);

    // an encoder that took the forced picture and emitted nothing sent no keyframe: every
    // caller drops an empty AU, so the request stands and nothing is counted
    let empty: anyhow::Result<Vec<u8>> = Ok(Vec::new());
    settle_forced_idr(&empty, true, &request, &sent);
    assert!(
        request.load(Ordering::Relaxed),
        "an empty AU carried no keyframe; the request must be put back"
    );
    assert_eq!(
        sent.load(Ordering::Relaxed),
        1,
        "an empty AU is not a sent IDR"
    );
}

/// While a refused target backs off, only a deeper cut than the refused one (and below the
/// rate in force) is tried early; raises and repeats still wait.
#[test]
fn only_a_deeper_cut_skips_a_refused_bitrate_backoff() {
    let current = 10_000_000;
    let refused = 8_000_000;
    assert!(bitrate_bypasses_backoff(6_000_000, current, refused));
    assert!(
        !bitrate_bypasses_backoff(refused, current, refused),
        "the same target waits"
    );
    assert!(
        !bitrate_bypasses_backoff(9_000_000, current, refused),
        "a milder cut waits"
    );
    assert!(
        !bitrate_bypasses_backoff(12_000_000, current, refused),
        "a raise waits"
    );
    assert!(
        !bitrate_bypasses_backoff(0, current, refused),
        "nothing pending"
    );

    // the backoff remembers what it refused, and forgets it on success
    let mut retry = RetryBackoff::new(true, Duration::from_millis(100), Duration::from_secs(5));
    retry.set_refused(refused);
    retry.failed(Instant::now());
    assert_eq!(retry.refused(), refused);
    assert!(!retry.ready_now());
    retry.succeeded();
    assert_eq!(retry.refused(), 0);
}

/// Only `Owned` lets the writer touch the shared texture, and neither refusal reads as a
/// transient "device busy" drop.
#[test]
fn only_ownership_lets_a_keyed_mutex_writer_proceed() {
    assert!(require_owned(Acquired::Owned, "w", 1000).is_ok());
    for outcome in [Acquired::TimedOut, Acquired::Abandoned] {
        let e = require_owned(outcome, "w", 1000).expect_err("must refuse");
        assert!(!format!("{e:#}").contains("device busy"), "{e:#}");
    }
}

/// Against real hardware: a second device acquiring a keyed mutex the first holds gets a
/// timeout, which the `windows` wrapper reports as `Ok(())`.
#[test]
fn a_held_keyed_mutex_times_out_for_the_other_device() {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET,
        D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::{IDXGIKeyedMutex, IDXGIResource};

    fn device() -> Option<ID3D11Device> {
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0][..]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )
        }
        .ok()?;
        device
    }
    let (Some(a), Some(b)) = (device(), device()) else {
        super::skip("keyed mutex timeout", "no hardware D3D11 device");
        return;
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 64,
        Height: 64,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: D3D11_RESOURCE_MISC_SHARED_KEYEDMUTEX.0 as u32,
    };
    let mut tex: Option<ID3D11Texture2D> = None;
    if let Err(e) = unsafe { a.CreateTexture2D(&desc, None, Some(&mut tex)) } {
        super::skip("keyed mutex timeout", e);
        return;
    }
    let tex = tex.expect("texture");
    let handle: HANDLE =
        unsafe { tex.cast::<IDXGIResource>().unwrap().GetSharedHandle() }.expect("shared handle");
    let mut opened: Option<ID3D11Texture2D> = None;
    unsafe { b.OpenSharedResource(handle, &mut opened) }.expect("open on the second device");
    let opened = opened.expect("opened texture");

    let mutex_a: IDXGIKeyedMutex = tex.cast().unwrap();
    let mutex_b: IDXGIKeyedMutex = opened.cast().unwrap();
    assert_eq!(keyed::acquire(&mutex_a, 0, 1000).unwrap(), Acquired::Owned);
    let outcome = keyed::acquire(&mutex_b, 0, 0).unwrap();
    assert_eq!(outcome, Acquired::TimedOut);
    assert!(require_owned(outcome, "second device", 0).is_err());
    // the plain wrapper on the same call reports success
    assert!(unsafe { mutex_b.AcquireSync(0, 0) }.is_ok());
    unsafe { mutex_a.ReleaseSync(0) }.unwrap();
}

/// With the gate on, a display composing at twice the stream rate is decimated 2:1; without
/// it every composed frame is staged.
#[test]
fn the_dxgi_gate_decimates_a_fast_compose_to_the_stream_rate() {
    let period = Duration::from_micros(16_667); // 60 fps
    let compose = Duration::from_micros(8_333); // 120 Hz
    let t0 = Instant::now();

    let staged_over = |gate: bool| -> usize {
        let mut c = DxgiCadence::new(gate, period, t0);
        let mut staged = 0;
        for i in 0..120u32 {
            let now = t0 + compose * i;
            c.on_dirty(now);
            if c.due(now, true, false) {
                c.staged(now);
                staged += 1;
            }
        }
        staged
    };
    assert_eq!(staged_over(false), 120, "the legacy loop never gates");
    let gated = staged_over(true);
    assert_eq!(gated, 60, "120 composed frames over one second at 60 fps");
}

#[test]
fn the_dxgi_gate_flushes_a_pending_frame_at_the_period() {
    let period = Duration::from_millis(16);
    let t0 = Instant::now();
    let mut c = DxgiCadence::new(true, period, t0);

    // nothing pending: idle poll, nothing due
    assert_eq!(c.poll_timeout_ms(t0), 8);
    assert!(!c.due(t0, false, false));

    // the first change goes out at once
    c.on_dirty(t0);
    assert!(c.due(t0, true, false));
    c.staged(t0);

    // a change 4 ms later is early: pending, and the poll waits for the rest of the period
    let early = t0 + Duration::from_millis(4);
    c.on_dirty(early);
    assert!(!c.due(early, true, false));
    assert_eq!(c.poll_timeout_ms(early), 12);
    // nothing new arrives; the timeout fires and the pending frame goes
    let later = t0 + Duration::from_millis(15) + Duration::from_micros(500);
    assert!(
        c.due(later, false, false),
        "within a millisecond of the period counts as due"
    );
    c.staged(later);
    assert!(!c.due(later, false, false));
}

/// While a frame is pending the poll timeout is whole milliseconds to the deadline, so the
/// loop waits instead of spinning; inside the last millisecond the frame is simply due.
#[test]
fn the_dxgi_gate_never_spins_on_a_zero_timeout_while_waiting() {
    let period = Duration::from_millis(16);
    let t0 = Instant::now();
    let mut c = DxgiCadence::new(true, period, t0);
    c.on_dirty(t0);
    c.staged(t0);
    c.on_dirty(t0 + Duration::from_millis(1));
    let mut now = t0 + Duration::from_millis(1);
    while now < t0 + period {
        if c.due(now, false, false) {
            break;
        }
        assert!(c.poll_timeout_ms(now) >= 1, "spinning at {:?}", now - t0);
        now += Duration::from_micros(100);
    }
    assert!(now >= t0 + period - Duration::from_millis(1));
}

/// A fresh poll carrying a pointer move is staged at once, as WGC's gate exempts a cursor-moved
/// frame; a content change at the same moment is still held to the cadence.
#[test]
fn the_dxgi_gate_stages_a_pointer_move_at_once() {
    let period = Duration::from_millis(16);
    let t0 = Instant::now();
    let mut c = DxgiCadence::new(true, period, t0);
    c.on_dirty(t0);
    assert!(c.due(t0, true, false));
    c.staged(t0);

    let early = t0 + Duration::from_millis(3);
    c.on_dirty(early);
    assert!(!c.due(early, true, false), "an early content change waits");
    assert!(c.due(early, true, true), "an early pointer move does not");
    c.staged(early);

    // a pointer sweep composed at 120 Hz for a 60 fps stream is staged at the compose rate
    let compose = Duration::from_micros(8_333);
    let mut staged = 0;
    for i in 1..=60u32 {
        let now = early + compose * i;
        c.on_dirty(now);
        if c.due(now, true, true) {
            c.staged(now);
            staged += 1;
        }
    }
    assert_eq!(staged, 60);
}

/// `start` (the probe harness) lets its DXGI capture run past the control it discards:
/// detaching must not signal the stop that a plain drop does.
#[test]
fn a_detached_dxgi_control_leaves_its_capture_running() {
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let control = DxgiControl::new(Arc::clone(&stop), join);
    control.detach();
    std::thread::sleep(Duration::from_millis(20));
    assert!(
        !stop.load(Ordering::Relaxed),
        "a detached capture must keep running"
    );
    // let the thread go
    stop.store(true, Ordering::Relaxed);
}

/// An encode thread that is gone is an error, not "the encoder is not keeping up".
#[test]
fn a_gone_dxgi_encoder_is_not_a_full_ring() {
    let (free_tx, free_rx) = crossbeam_channel::bounded::<usize>(3);
    assert_eq!(
        take_free_slot(&free_rx).unwrap(),
        None,
        "empty: every slot is busy"
    );
    free_tx.send(2).unwrap();
    assert_eq!(take_free_slot(&free_rx).unwrap(), Some(2));
    drop(free_tx);
    assert!(
        take_free_slot(&free_rx).is_err(),
        "disconnected: the encoder is gone"
    );
}

#[test]
fn a_dropped_dxgi_control_signals_stop_without_joining() {
    let stop = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
        // winding down takes a while; a joining drop would wait this out
        std::thread::sleep(Duration::from_millis(300));
        let _ = done_tx.send(());
    });
    let control = DxgiControl::new(Arc::clone(&stop), join);

    let started = Instant::now();
    drop(control);
    let took = started.elapsed();
    assert!(
        stop.load(Ordering::Relaxed),
        "drop must signal the capture to stop"
    );
    assert!(
        took < Duration::from_millis(200),
        "drop joined the thread ({took:?})"
    );
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the capture thread saw the flag and exited");
}

/// Without a video processor the DXGI path builds no scaler for a CPU-bridge backend, even
/// though that backend would like NV12.
#[test]
fn no_video_processor_means_no_identity_scaler_on_dxgi() {
    let cfg = Config::default();
    let config = live_encoder_config(1280, 720, 60, &cfg);
    let encoder = match X264Encoder::new(config) {
        Ok(e) => e,
        Err(e) => {
            super::skip("dxgi scaler plan (libx264 unavailable)", e);
            return;
        }
    };
    let backend = Backend::X264 { encoder };
    // with a video processor the NV12 conversion is wanted even at identity size...
    assert!(dxgi_needs_scaler(&backend, true, 1280, 720, 1280, 720));
    // ...and without one it is not, so the BGRA `TextureReader` path is used instead
    assert!(!dxgi_needs_scaler(&backend, false, 1280, 720, 1280, 720));
}

/// A probe waiting on a channel nothing is sending on gives up instead of hanging.
#[test]
fn a_probe_wait_ends_on_its_own() {
    let (tx, mut rx) = broadcast::channel::<EncodedFrame>(4);
    let started = Instant::now();
    match recv_frame_within(&mut rx, Duration::from_millis(50)).unwrap() {
        ProbeRecv::TimedOut => {}
        _ => panic!("nothing was sent"),
    }
    assert!(started.elapsed() < Duration::from_secs(2));

    tx.send(EncodedFrame {
        data: bytes::Bytes::from_static(&[0, 0, 0, 1, 0x65]),
        capture: Instant::now(),
        timing: FrameTiming::entering_capture(),
    })
    .ok();
    assert!(matches!(
        recv_frame_within(&mut rx, Duration::from_millis(50)).unwrap(),
        ProbeRecv::Frame(_)
    ));

    drop(tx);
    assert!(recv_frame_within(&mut rx, Duration::from_millis(50)).is_err());
}

/// Capture follows the monitor, not a cached `\\.\DISPLAYn`, and refuses a name that now
/// belongs to a monitor this host did not create.
#[test]
fn capture_resolves_the_owned_monitor_not_the_cached_name() {
    use crate::streamer::session::DisplayTarget;
    use crate::windows_utils::virtual_display::{pick_owned_display, DisplayPath};

    // ours sits on the driver's adapter, the panel on the GPU's
    let path = |gdi: &str, device_path: &str| DisplayPath {
        gdi_name: gdi.to_string(),
        friendly: String::new(),
        device_path: device_path.to_string(),
        target_adapter: if device_path.contains("MTT1337") {
            "idd"
        } else {
            "gpu"
        }
        .to_string(),
        adapter_id: (1, 0),
        source_id: 0,
    };
    let on_driver = |p: &DisplayPath| p.target_adapter == "idd";
    let ours = r"\\?\DISPLAY#MTT1337#ours";
    let panel = r"\\?\DISPLAY#BOE0001#panel";
    let owned = |p: &str| p == ours;

    // our display moved from DISPLAY3 to DISPLAY4, and the host's own panel now holds DISPLAY3
    let now = [path(r"\\.\DISPLAY3", panel), path(r"\\.\DISPLAY4", ours)];

    let by_path = DisplayTarget {
        device_path: ours.to_string(),
        device_name: r"\\.\DISPLAY3".to_string(),
    };
    assert_eq!(
        pick_owned_display(&now, &by_path, &owned, &on_driver)
            .unwrap()
            .gdi_name,
        r"\\.\DISPLAY4",
        "the session's monitor, wherever it answers now"
    );

    // the bare cached name, which is what `start_on_monitor` is given: must not capture the panel
    let by_name = DisplayTarget {
        device_path: String::new(),
        device_name: r"\\.\DISPLAY3".to_string(),
    };
    assert!(pick_owned_display(&now, &by_name, &owned, &on_driver).is_err());
}

#[test]
fn a_capture_item_is_waited_for_and_the_wait_is_bounded() {
    use crate::windows_utils::streamer::pipeline::wait_for_capture_item;
    let target = crate::streamer::session::DisplayTarget {
        device_path: r"\\?\DISPLAY#TEST#1".into(),
        device_name: r"\\.\DISPLAY7".into(),
    };
    let poll = Duration::from_millis(5);
    let wait = Duration::from_secs(5);
    let mut calls = 0;
    let found = wait_for_capture_item(&target, wait, poll, &|| true, &mut |_| {
        calls += 1;
        if calls < 4 {
            anyhow::bail!("the handle is stale");
        }
        Ok(calls)
    });
    assert_eq!(found.unwrap(), 4, "asked again until the item came");

    let started = Instant::now();
    let mut calls = 0;
    let bound = Duration::from_millis(60);
    let never = wait_for_capture_item::<()>(&target, bound, poll, &|| true, &mut |_| {
        calls += 1;
        anyhow::bail!("the handle is stale")
    });
    assert!(never.is_err());
    assert!(calls >= 2, "kept asking within the bound");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "and gave up at it"
    );
}

/// An attempt a newer offer has overtaken stops waiting at the next turn and says why, so it
/// never goes on to open a second capture on the display.
#[test]
fn an_overtaken_attempt_stops_waiting_for_its_capture() {
    use crate::windows_utils::streamer::pipeline::{wait_for_capture_item, Superseded};
    let target = crate::streamer::session::DisplayTarget {
        device_path: r"\\?\DISPLAY#TEST#1".into(),
        device_name: r"\\.\DISPLAY7".into(),
    };
    let current = std::cell::Cell::new(3);
    let still_current = || {
        current.set(current.get() - 1);
        current.get() > 0
    };
    let mut calls = 0;
    let out = wait_for_capture_item::<()>(
        &target,
        Duration::from_secs(5),
        Duration::from_millis(5),
        &still_current,
        &mut |_| {
            calls += 1;
            anyhow::bail!("the handle is stale")
        },
    );
    assert!(out.unwrap_err().downcast_ref::<Superseded>().is_some());
    assert_eq!(calls, 2, "asked no more once overtaken");
}

/// The readiness test is a delivered frame: a first delivery inside the bound is seen, and a
/// capture that delivers nothing is reported at the bound.
#[test]
fn a_capture_is_ready_when_it_has_delivered_a_frame() {
    use crate::windows_utils::streamer::pipeline::wait_first_frame;
    let arrived = Arc::new(AtomicU64::new(7));
    let late = Arc::clone(&arrived);
    let delivery = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(40));
        late.fetch_add(1, Ordering::Relaxed);
    });
    assert!(wait_first_frame(&arrived, 7, Duration::from_secs(2)));
    delivery.join().unwrap();

    let started = Instant::now();
    assert!(!wait_first_frame(&arrived, 8, Duration::from_millis(60)));
    assert!(started.elapsed() < Duration::from_secs(1));
}

/// The capture start resolves through the ownership test every display write uses: a path that
/// names a monitor this host did not create (here the machine's own) is refused, not captured.
#[test]
fn a_capture_never_resolves_to_a_monitor_this_host_did_not_create() {
    let paths = crate::windows_utils::virtual_display::active_display_paths();
    let Some(panel) = paths.iter().find(|p| !p.device_path.is_empty()) else {
        crate::test_support::skip_because(
            "a_capture_never_resolves_to_a_monitor_this_host_did_not_create",
            "no active display with a device path",
        );
        return;
    };
    let target = crate::streamer::session::DisplayTarget {
        device_path: panel.device_path.clone(),
        device_name: panel.gdi_name.clone(),
    };
    assert!(crate::windows_utils::virtual_display::resolve_owned_display(&target).is_err());
}

/// The capture path's own resolver, on the same fabricated topology. It retries a display that
/// does not resolve yet, follows the monitor to its new name, and refuses the cached name once
/// that name is the panel's, after its retries and without returning the panel.
#[test]
fn capture_name_resolution_retries_then_follows_or_refuses() {
    use crate::streamer::session::DisplayTarget;
    use crate::windows_utils::virtual_display::{pick_owned_display, DisplayPath};

    // ours sits on the driver's adapter, the panel on the GPU's
    let path = |gdi: &str, device_path: &str| DisplayPath {
        gdi_name: gdi.to_string(),
        friendly: String::new(),
        device_path: device_path.to_string(),
        target_adapter: if device_path.contains("MTT1337") {
            "idd"
        } else {
            "gpu"
        }
        .to_string(),
        adapter_id: (1, 0),
        source_id: 0,
    };
    let on_driver = |p: &DisplayPath| p.target_adapter == "idd";
    let ours = r"\\?\DISPLAY#MTT1337#ours";
    let panel = r"\\?\DISPLAY#BOE0001#panel";
    let owned = |p: &str| p == ours;
    let settled = [path(r"\\.\DISPLAY3", panel), path(r"\\.\DISPLAY4", ours)];

    // our monitor is detached for the first two looks (a mode change settling), then is back
    // under a new name
    let by_path = DisplayTarget {
        device_path: ours.to_string(),
        device_name: r"\\.\DISPLAY3".to_string(),
    };
    let mut calls = 0;
    let name = resolve_capture_name_with(&by_path, 5, Duration::ZERO, &mut |t| {
        calls += 1;
        let paths: &[DisplayPath] = if calls <= 2 { &[] } else { &settled };
        pick_owned_display(paths, t, &owned, &on_driver).map(|r| r.gdi_name)
    })
    .expect("resolves once the monitor is back");
    assert_eq!(name, r"\\.\DISPLAY4");
    assert_eq!(calls, 3, "two retries, then the hit");

    // by the cached name alone it is the panel now: every attempt refuses, and so does the call
    let by_name = DisplayTarget {
        device_path: String::new(),
        device_name: r"\\.\DISPLAY3".to_string(),
    };
    let mut calls = 0;
    let err = resolve_capture_name_with(&by_name, 2, Duration::ZERO, &mut |t| {
        calls += 1;
        pick_owned_display(&settled, t, &owned, &on_driver).map(|r| r.gdi_name)
    })
    .expect_err("the panel must never be captured");
    assert_eq!(calls, 3, "the first attempt and two retries");
    assert!(format!("{err:#}").contains("refusing to capture a display this host does not own"));
}

/// The workload window's primary-display guard, on the rectangle test it rests on.
#[test]
fn the_workload_guard_sees_any_overlap_with_the_primary_display() {
    use crate::windows_utils::streamer::workload_window::rects_overlap;
    let primary = (0, 0, 2560, 1600);
    // a virtual display to the right of the panel, edge to edge: no shared pixel
    assert!(!rects_overlap((2560, 0, 4480, 1080), primary));
    // one to the left at negative coordinates
    assert!(!rects_overlap((-1920, 0, 0, 1080), primary));
    // the probe's fallback to display 0, and a rect that straddles the edge
    assert!(rects_overlap((0, 0, 2560, 1600), primary));
    assert!(rects_overlap((2500, 100, 4420, 1180), primary));
}

/// A backend that keeps failing is rebuilt at most three times a minute, then the capture stops;
/// an old rebuild ages out of the window.
#[test]
fn rebuilds_are_bounded_per_minute() {
    let mut budget = RebuildBudget::default();
    let t0 = Instant::now();
    assert!(budget.allow(t0));
    assert!(budget.allow(t0 + Duration::from_secs(1)));
    assert!(budget.allow(t0 + Duration::from_secs(2)));
    assert!(!budget.allow(t0 + Duration::from_secs(3)));
    assert!(!budget.allow(t0 + Duration::from_secs(59)));
    assert!(budget.allow(t0 + Duration::from_secs(60)));
}

/// The compatibility row names a started session's tier in its own words, one per pipeline path.
#[test]
fn every_started_path_has_a_compatibility_label() {
    let cases = [
        ("nvenc-zero-copy-ring", "nvenc"),
        ("zero-copy", "nvenc"),
        ("cpu-bridge", "nvenc (cpu bridge)"),
        ("intel-same-adapter", "quicksync"),
        ("intel-cpu-bridge", "quicksync (cpu bridge)"),
        ("amd-amf-ring", "amf"),
        ("amd-amf-same-adapter", "amf"),
        ("amd-amf-cpu-bridge", "amf (cpu bridge)"),
        ("software-x264", "x264"),
    ];
    for (path, label) in cases {
        assert_eq!(started_backend_label(path), label, "{path}");
    }
}

/// The scale wait reports what it saw: the first reading, and when the written scale showed.
#[test]
fn the_scale_wait_reports_what_the_system_applied() {
    use crate::windows_utils::streamer::capture::wait_for_scale_with;
    let mut reads = vec![Some(100), Some(100), Some(150)].into_iter();
    let seen = wait_for_scale_with(
        150,
        Duration::from_secs(2),
        Duration::from_millis(1),
        &mut || reads.next().unwrap_or(Some(150)),
    );
    assert_eq!((seen.first, seen.last), (Some(100), Some(150)));
    assert!(seen.landed_after.is_some());

    let seen = wait_for_scale_with(
        150,
        Duration::from_millis(30),
        Duration::from_millis(5),
        &mut || Some(100),
    );
    assert_eq!(
        (seen.first, seen.last, seen.landed_after),
        (Some(100), Some(100), None)
    );
}

/// One opening of a capture, for the first-frame loop: which ones were opened and stopped, and
/// how many were alive at once.
struct FakeOpening {
    id: usize,
    log: Arc<std::sync::Mutex<OpeningLog>>,
}

#[derive(Default)]
struct OpeningLog {
    opened: usize,
    stopped: Vec<usize>,
    alive: usize,
    most_alive: usize,
}

impl crate::windows_utils::streamer::pipeline::Opening for FakeOpening {
    fn backend(&self) -> &'static str {
        "wgc"
    }
    fn stop(self) {
        let mut log = self.log.lock().unwrap();
        log.alive -= 1;
        log.stopped.push(self.id);
    }
}

struct LoopRun {
    log: Arc<std::sync::Mutex<OpeningLog>>,
    reopened: u64,
    result: anyhow::Result<(FakeOpening, Option<&'static str>)>,
}

/// Run the real first-frame loop over fake openings. `delivers_on` is the opening (0-based) that
/// hands over a frame; `withheld` and `current` stand in for the desktop and the join.
fn run_first_frame_loop(delivers_on: Option<usize>, withheld: bool, current_for: usize) -> LoopRun {
    use crate::windows_utils::streamer::pipeline::{open_until_first_frame, FirstFrameRules};
    let rules = FirstFrameRules {
        bound: Duration::from_millis(20),
        reopens: 2,
        floor: Duration::from_millis(1),
    };
    let arrived = AtomicU64::new(0);
    let reopened = AtomicU64::new(0);
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let asked = std::sync::atomic::AtomicUsize::new(0);
    let still_current = || asked.fetch_add(1, Ordering::Relaxed) < current_for;
    let opener = Arc::clone(&log);
    let result = open_until_first_frame(
        &rules,
        &arrived,
        &reopened,
        &still_current,
        &mut || {
            let mut l = opener.lock().unwrap();
            let id = l.opened;
            l.opened += 1;
            l.alive += 1;
            l.most_alive = l.most_alive.max(l.alive);
            if delivers_on == Some(id) {
                arrived.fetch_add(1, Ordering::Relaxed);
            }
            Ok(FakeOpening {
                id,
                log: Arc::clone(&opener),
            })
        },
        &|_| withheld,
        &|| 0,
        "test display",
    );
    LoopRun {
        log,
        reopened: reopened.load(Ordering::Relaxed),
        result,
    }
}

/// A fresh monitor whose first opening never delivers: the shell's work-area broadcast after the
/// opening started, with no frame yet, reopens it at once instead of at the bound.
#[test]
fn a_work_area_broadcast_before_the_first_frame_reopens_at_once() {
    use crate::windows_utils::streamer::pipeline::{open_until_first_frame, FirstFrameRules};
    let rules = FirstFrameRules {
        bound: Duration::from_secs(30),
        reopens: 2,
        floor: Duration::from_millis(1),
    };
    let arrived = AtomicU64::new(0);
    let reopened = AtomicU64::new(0);
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let opener = Arc::clone(&log);
    let started = Instant::now();
    // the shell commits 50 ms in; the second opening delivers
    let shell = || u64::from(started.elapsed() >= Duration::from_millis(50));
    let result = open_until_first_frame(
        &rules,
        &arrived,
        &reopened,
        &|| true,
        &mut || {
            let mut l = opener.lock().unwrap();
            let id = l.opened;
            l.opened += 1;
            l.alive += 1;
            l.most_alive = l.most_alive.max(l.alive);
            if id == 1 {
                arrived.fetch_add(1, Ordering::Relaxed);
            }
            Ok(FakeOpening {
                id,
                log: Arc::clone(&opener),
            })
        },
        &|_| false,
        &shell,
        "test display",
    );
    let (opening, first) = result.expect("a capture");
    assert_eq!((opening.id, first), (1, Some("wgc")));
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "reopened on the broadcast, not at the 30 s bound"
    );
    assert_eq!(reopened.load(Ordering::Relaxed), 1);
    assert_eq!(log.lock().unwrap().stopped, vec![0]);
}

/// A capture that delivers on its third opening: the two before it are stopped, in order, each
/// before the next is opened, and the reopens are counted.
#[test]
fn a_dark_opening_is_stopped_before_the_next_one_opens() {
    let run = run_first_frame_loop(Some(2), false, usize::MAX);
    let (opening, first) = run.result.expect("a capture");
    assert_eq!(first, Some("wgc"));
    assert_eq!(opening.id, 2);
    let log = run.log.lock().unwrap();
    assert_eq!(log.opened, 3);
    assert_eq!(log.stopped, vec![0, 1]);
    assert_eq!(
        log.most_alive, 1,
        "never two openings, or two encoders, at once"
    );
    assert_eq!(run.reopened, 2);
}

/// Out of reopens, the start keeps the last opening and answers with it instead of failing.
#[test]
fn a_capture_that_stays_dark_is_kept_rather_than_failing_the_join() {
    let run = run_first_frame_loop(None, false, usize::MAX);
    let (opening, first) = run.result.expect("the start does not fail");
    assert_eq!(first, None);
    assert_eq!(opening.id, 2);
    let log = run.log.lock().unwrap();
    assert_eq!((log.opened, log.stopped.len()), (3, 2));
    assert_eq!(run.reopened, 2);
}

/// A locked or secure desktop delivers to no capture: nothing is reopened, and the join is
/// answered with the first opening.
#[test]
fn a_withheld_desktop_waives_the_first_frame() {
    let run = run_first_frame_loop(None, true, usize::MAX);
    let (opening, first) = run.result.expect("answered");
    assert_eq!((opening.id, first), (0, None));
    let log = run.log.lock().unwrap();
    assert_eq!(log.opened, 1);
    assert!(log.stopped.is_empty());
    assert_eq!(run.reopened, 0);
}

#[test]
fn an_attempt_overtaken_during_the_wait_stops_its_opening() {
    let run = run_first_frame_loop(None, false, 1);
    let err = run.result.err().expect("superseded");
    assert!(err
        .downcast_ref::<crate::windows_utils::streamer::pipeline::Superseded>()
        .is_some());
    let log = run.log.lock().unwrap();
    assert_eq!((log.opened, log.stopped.clone()), (1, vec![0]));
}

/// The capture-item wait is the display's, not the attempt's: a retry on a display disturbed four
/// seconds ago waits only for what is left of the bound, and a later write starts it again.
#[test]
fn a_retry_inherits_the_capture_item_wait() {
    use crate::windows_utils::streamer::capture::{
        item_wait_remaining, note_display_disturbed, note_display_disturbed_at,
    };
    let target = crate::streamer::session::DisplayTarget {
        device_path: format!("test-path-{}", crate::streamer::session::mint_join_id()),
        device_name: String::new(),
    };
    let bound = Duration::from_secs(6);
    assert_eq!(
        item_wait_remaining(&target, bound),
        bound,
        "no record: the whole bound"
    );

    note_display_disturbed_at(&target.device_path, Instant::now() - Duration::from_secs(4));
    let left = item_wait_remaining(&target, bound);
    assert!(
        left <= Duration::from_secs(2) && left > Duration::from_millis(1500),
        "{left:?}"
    );

    note_display_disturbed_at(
        &target.device_path,
        Instant::now() - Duration::from_secs(10),
    );
    assert!(
        item_wait_remaining(&target, bound) <= Duration::from_secs(2),
        "an older event never moves the clock back"
    );

    note_display_disturbed(&target.device_path);
    assert!(item_wait_remaining(&target, bound) > Duration::from_millis(5900));

    let nameless = crate::streamer::session::DisplayTarget {
        device_path: String::new(),
        device_name: String::new(),
    };
    assert_eq!(item_wait_remaining(&nameless, bound), bound);
}

fn new_opening(log: &Arc<std::sync::Mutex<OpeningLog>>) -> FakeOpening {
    let mut l = log.lock().unwrap();
    let id = l.opened;
    l.opened += 1;
    l.alive += 1;
    l.most_alive = l.most_alive.max(l.alive);
    FakeOpening {
        id,
        log: Arc::clone(log),
    }
}

fn idle(d: Duration) -> bool {
    std::thread::sleep(d);
    false
}

/// The session holds its capture while the item is refused (the join has been answered by then),
/// and the capture opens, once, as soon as the item is handed out.
#[test]
fn a_deferred_capture_waits_for_its_item_and_then_opens_once() {
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let slot = Arc::new(DeferredSlot::<FakeOpening>::new());
    let ready = Arc::new(AtomicBool::new(false));
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let waiting = {
        let (slot, ready, log) = (Arc::clone(&slot), Arc::clone(&ready), Arc::clone(&log));
        std::thread::spawn(move || {
            run_deferred(
                &slot,
                &budget(Duration::from_secs(10)),
                &|| true,
                &mut || ready.load(Ordering::Relaxed),
                &mut idle,
                &mut || Ok(new_opening(&log)),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(250));
    assert!(!slot.running(), "nothing opens while the item is refused");
    assert_eq!(log.lock().unwrap().opened, 0);
    ready.store(true, Ordering::Relaxed);
    let end = waiting.join().unwrap();
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
    assert!(slot.running());
    assert_eq!(log.lock().unwrap().opened, 1);
    // the session's stop takes it out exactly once
    let opened = slot.end().expect("the running capture");
    crate::windows_utils::streamer::pipeline::Opening::stop(opened);
    assert!(slot.end().is_none());
    assert_eq!(log.lock().unwrap().stopped, vec![0]);
}

/// A newer attempt owns the join: the wait stands down and opens nothing.
#[test]
fn a_superseded_attempt_stops_waiting_for_its_capture() {
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let slot = DeferredSlot::<FakeOpening>::new();
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let started = Instant::now();
    let current = move || started.elapsed() < Duration::from_millis(150);
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(10)),
        &current,
        &mut || false,
        &mut idle,
        &mut || Ok(new_opening(&log)),
    );
    assert!(matches!(end, DeferredEnd::Superseded), "{end:?}");
    assert_eq!(log.lock().unwrap().opened, 0);

    // overtaken while the opening was being made: that opening is stopped, never handed over
    let slot = DeferredSlot::<FakeOpening>::new();
    let flip = std::sync::atomic::AtomicBool::new(true);
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(10)),
        &|| flip.load(Ordering::Relaxed),
        &mut || true,
        &mut idle,
        &mut || {
            flip.store(false, Ordering::Relaxed);
            Ok(new_opening(&log))
        },
    );
    assert!(matches!(end, DeferredEnd::Superseded), "{end:?}");
    assert!(!slot.running());
    let l = log.lock().unwrap();
    assert_eq!((l.opened, l.stopped.clone(), l.alive), (1, vec![0], 0));
}

/// A monitor that never hands out an item ends the wait at the budget, once, with nothing open;
/// there is no other bound inside it.
#[test]
fn the_budget_ends_a_capture_that_never_becomes_ready() {
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let slot = DeferredSlot::<FakeOpening>::new();
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let started = Instant::now();
    let mut polls = 0u32;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_millis(300)),
        &|| true,
        &mut || {
            polls += 1;
            false
        },
        &mut idle,
        &mut || Ok(new_opening(&log)),
    );
    assert!(matches!(end, DeferredEnd::OutOfBudget), "{end:?}");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(polls >= 2, "it kept asking until the budget");
    assert_eq!(log.lock().unwrap().opened, 0);
    assert!(slot.end().is_none(), "nothing to stop");
}

/// An opening that fails for a reason waiting cannot cure is tried again, a bounded number of
/// times, before the session is ended; one that succeeds on a later try is used.
#[test]
fn a_failed_opening_is_retried_before_the_session_is_ended() {
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let slot = DeferredSlot::<FakeOpening>::new();
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(30)),
        &|| true,
        &mut || true,
        &mut idle,
        &mut || {
            tries += 1;
            if tries < 3 {
                return Err(anyhow::anyhow!("encoder session limit"));
            }
            Ok(new_opening(&log))
        },
    );
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
    assert_eq!(tries, 3);

    let slot = DeferredSlot::<FakeOpening>::new();
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(30)),
        &|| true,
        &mut || true,
        &mut idle,
        &mut || {
            tries += 1;
            Err(anyhow::anyhow!("encoder session limit"))
        },
    );
    assert!(matches!(end, DeferredEnd::Failed(_)), "{end:?}");
    assert_eq!(tries, 3, "bounded");
}

/// A wait that panics still clears the waiting flag and gives up, once.
#[test]
fn a_capture_wait_that_panics_ends_its_session() {
    use crate::windows_utils::streamer::pipeline::WaitGuard;
    let t = crate::streamer::telemetry::SessionTelemetry::new_shared();
    t.capture_waiting.store(true, Ordering::Relaxed);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let guard_t = Arc::clone(&t);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = WaitGuard::new(
            guard_t,
            Box::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
            }),
        );
        panic!("in the wait");
    }));
    assert!(r.is_err());
    assert!(!t.capture_waiting.load(Ordering::Relaxed));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // an ordinary end clears the flag without giving up
    t.capture_waiting.store(true, Ordering::Relaxed);
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    drop(WaitGuard::new(
        Arc::clone(&t),
        Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }),
    ));
    assert!(!t.capture_waiting.load(Ordering::Relaxed));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

/// The session ended while the capture was being opened: the late opening is stopped, never
/// installed, and a refusal between the poll and the opening only means waiting on.
#[test]
fn a_capture_that_opens_after_its_session_ended_is_stopped() {
    use crate::windows_utils::streamer::pipeline::{
        run_deferred, DeferredEnd, DeferredSlot, StaleMonitor,
    };
    let slot = DeferredSlot::<FakeOpening>::new();
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(10)),
        &|| true,
        &mut || true,
        &mut idle,
        &mut || {
            tries += 1;
            if tries == 1 {
                return Err(anyhow::anyhow!("refused a capture item").context(StaleMonitor));
            }
            let opened = new_opening(&log);
            assert!(slot.end().is_none());
            Ok(opened)
        },
    );
    assert!(matches!(end, DeferredEnd::Ended), "{end:?}");
    assert_eq!(tries, 2, "a not-ready refusal is waited on, not fatal");
    let l = log.lock().unwrap();
    assert_eq!((l.opened, l.stopped.clone(), l.alive), (1, vec![0], 0));
}

/// An opening stood down because the session ended is reported as ended, not superseded.
#[test]
fn a_stand_down_for_an_ended_session_says_so() {
    use crate::windows_utils::streamer::pipeline::{
        run_deferred, DeferredEnd, DeferredSlot, Superseded,
    };
    let slot = DeferredSlot::<FakeOpening>::new();
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(10)),
        &|| true,
        &mut || true,
        &mut idle,
        &mut || {
            assert!(slot.end().is_none());
            Err(anyhow::Error::new(Superseded))
        },
    );
    assert!(matches!(end, DeferredEnd::Ended), "{end:?}");
}

/// Duplication from the start: the monitor's name resolves at once, but the opening waits until
/// DXGI lists its output, and a monitor whose output never appears ends at the budget, unopened.
#[test]
fn duplication_waits_for_its_output_to_be_listed() {
    use crate::windows_utils::streamer::pipeline::{
        duplication_ready, run_deferred, DeferredEnd, DeferredSlot,
    };
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let slot = DeferredSlot::<FakeOpening>::new();
    let started = Instant::now();
    let listed_after = Duration::from_millis(250);
    let mut resolved = 0u32;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(10)),
        &|| true,
        &mut || {
            duplication_ready(
                &mut || {
                    resolved += 1;
                    Ok(r"\\.\DISPLAY9".to_string())
                },
                &mut |_| started.elapsed() >= listed_after,
            )
        },
        &mut idle,
        &mut || Ok(new_opening(&log)),
    );
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
    assert!(
        started.elapsed() >= listed_after,
        "opened before the output was listed"
    );
    assert!(resolved >= 2, "it kept asking");
    assert_eq!(log.lock().unwrap().opened, 1);

    let slot = DeferredSlot::<FakeOpening>::new();
    let end = run_deferred(
        &slot,
        &budget(Duration::from_millis(300)),
        &|| true,
        &mut || duplication_ready(&mut || Ok(r"\\.\DISPLAY9".to_string()), &mut |_| false),
        &mut idle,
        &mut || Ok(new_opening(&log)),
    );
    assert!(matches!(end, DeferredEnd::OutOfBudget), "{end:?}");
    assert_eq!(log.lock().unwrap().opened, 1, "nothing more was opened");
    // a name that does not resolve is not ready either
    assert!(!duplication_ready(
        &mut || Err(anyhow::anyhow!("gone")),
        &mut |_| true
    ));
}

fn budget(d: Duration) -> crate::windows_utils::streamer::pipeline::ReadyBudget {
    crate::windows_utils::streamer::pipeline::ReadyBudget::new(d)
}

fn refused(kind: crate::windows_utils::streamer::dxgi::NotReady) -> anyhow::Error {
    anyhow::Error::new(crate::windows_utils::streamer::dxgi::DuplicationNotReady {
        kind,
        detail: "no adapter can duplicate X".to_string(),
    })
}

/// Which duplication codes stand for "not ready", and which one decides when several adapters
/// were tried.
#[test]
fn duplication_start_codes_are_classed_on_the_deciding_adapter() {
    use crate::windows_utils::streamer::dxgi::{not_ready_code, start_verdict, NotReady};
    use windows::Win32::Foundation::{E_ACCESSDENIED, E_OUTOFMEMORY};
    use windows::Win32::Graphics::Dxgi::{
        DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_INVALID_CALL,
        DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_SESSION_DISCONNECTED,
        DXGI_ERROR_UNSUPPORTED,
    };
    let settling = Some(NotReady::Settling);
    assert_eq!(not_ready_code(DXGI_ERROR_NOT_CURRENTLY_AVAILABLE), settling);
    assert_eq!(not_ready_code(DXGI_ERROR_ACCESS_LOST), settling);
    assert_eq!(not_ready_code(DXGI_ERROR_INVALID_CALL), settling);
    assert_eq!(not_ready_code(E_ACCESSDENIED), Some(NotReady::Withheld));
    assert_eq!(
        not_ready_code(DXGI_ERROR_SESSION_DISCONNECTED),
        Some(NotReady::Withheld)
    );
    assert_eq!(
        not_ready_code(DXGI_ERROR_UNSUPPORTED),
        Some(NotReady::Unsupported)
    );
    assert_eq!(not_ready_code(DXGI_ERROR_DEVICE_REMOVED), None);
    assert_eq!(not_ready_code(E_OUTOFMEMORY), None);

    let (nv, igpu) = (7i64, 9i64);
    let unsupported = Some(NotReady::Unsupported);
    // the composing adapter's real error is not hidden by another adapter's UNSUPPORTED
    assert_eq!(
        start_verdict(&[(nv, None), (igpu, unsupported)], Some(nv)),
        None
    );
    assert_eq!(
        start_verdict(&[(igpu, unsupported), (nv, None)], None),
        None
    );
    // only the composing adapter's answer counts when it was tried
    assert_eq!(
        start_verdict(&[(nv, settling), (igpu, unsupported)], Some(nv)),
        settling
    );
    assert_eq!(
        start_verdict(&[(nv, unsupported), (igpu, None)], Some(nv)),
        unsupported
    );
    // composed somewhere that does not list it: every listing adapter refuses
    assert_eq!(start_verdict(&[(nv, unsupported)], Some(igpu)), unsupported);
    assert_eq!(
        start_verdict(&[(nv, unsupported), (igpu, Some(NotReady::Withheld))], None),
        Some(NotReady::Withheld)
    );
    assert_eq!(start_verdict(&[], None), None);
}

/// A failure that is not the duplicator's typed "not ready" is a failed try whatever its text
/// says, an encoder or Direct3D error carrying one of the not-ready codes included; a typed one
/// that is still settling is waited on within the budget.
#[test]
fn only_a_typed_not_ready_duplication_start_is_not_a_failed_try() {
    use crate::windows_utils::streamer::dxgi::NotReady;
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let slot = DeferredSlot::<FakeOpening>::new();
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(30)),
        &|| true,
        &mut || true,
        &mut idle,
        &mut || {
            tries += 1;
            if tries <= 5 {
                return Err(refused(NotReady::Settling).context("starting capture"));
            }
            Ok(new_opening(&log))
        },
    );
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
    assert_eq!(tries, 6, "five not-ready starts, then the one that opened");

    for text in [
        "building the encoder: NvEncOpenEncodeSessionEx: Error { code: HRESULT(0x887A0001) }",
        "CreateTexture2D (duplication target): Error { code: HRESULT(0x80070005) }",
        "no adapter can duplicate X (Intel: Error { code: HRESULT(0x887A0004) })",
    ] {
        let slot = DeferredSlot::<FakeOpening>::new();
        let mut tries = 0;
        let started = Instant::now();
        let end = run_deferred(
            &slot,
            &budget(Duration::from_secs(30)),
            &|| true,
            &mut || true,
            &mut idle,
            &mut || {
                tries += 1;
                Err(anyhow::anyhow!("{text}"))
            },
        );
        assert!(matches!(end, DeferredEnd::Failed(_)), "{text}: {end:?}");
        assert_eq!(tries, 3, "{text}");
        assert!(started.elapsed() < Duration::from_secs(5), "{text}");
    }
}

/// A withheld desktop is waited on outside the budget, without a failed try, and the capture
/// opens once it is back: the same as a dark WGC capture that paints on the unlock.
#[test]
fn a_withheld_desktop_is_waited_on_outside_the_budget() {
    use crate::windows_utils::streamer::dxgi::NotReady;
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let slot = DeferredSlot::<FakeOpening>::new();
    let b = budget(Duration::from_millis(500));
    let started = Instant::now();
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &b,
        &|| true,
        &mut || true,
        &mut |d| idle(d.min(Duration::from_millis(100))),
        &mut || {
            tries += 1;
            match tries {
                // locked for about a second, well past the budget
                1..=10 => Err(refused(NotReady::Withheld)),
                // back, and settling for a moment: this part counts
                11..=12 => Err(refused(NotReady::Settling)),
                _ => Ok(new_opening(&log)),
            }
        },
    );
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
    assert_eq!(tries, 13);
    assert!(started.elapsed() >= Duration::from_millis(900));
    assert!(!b.spent(), "the locked second was not counted");
    assert_eq!(log.lock().unwrap().opened, 1);
}

/// `DXGI_ERROR_UNSUPPORTED` that persists ends the wait at its own bound, long before the
/// budget, as a failure that says so; one that clears inside the bound is waited on.
#[test]
fn a_persistent_unsupported_duplication_fails_at_its_own_bound() {
    use crate::windows_utils::streamer::dxgi::NotReady;
    use crate::windows_utils::streamer::pipeline::{
        run_deferred, DeferredEnd, DeferredSlot, UNSUPPORTED_BOUND,
    };
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let slot = DeferredSlot::<FakeOpening>::new();
    let started = Instant::now();
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(30)),
        &|| true,
        &mut || true,
        &mut |d| idle(d.min(Duration::from_millis(50))),
        &mut || {
            tries += 1;
            Err(refused(NotReady::Unsupported))
        },
    );
    let DeferredEnd::Failed(why) = end else {
        panic!("{end:?}");
    };
    assert!(why.contains("unsupported for this display"), "{why}");
    assert!(started.elapsed() >= UNSUPPORTED_BOUND);
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(tries > 3, "not counted as failed tries");

    let slot = DeferredSlot::<FakeOpening>::new();
    let started = Instant::now();
    let end = run_deferred(
        &slot,
        &budget(Duration::from_secs(30)),
        &|| true,
        &mut || true,
        &mut |d| idle(d.min(Duration::from_millis(50))),
        &mut || {
            if started.elapsed() < UNSUPPORTED_BOUND / 2 {
                return Err(refused(NotReady::Unsupported));
            }
            Ok(new_opening(&log))
        },
    );
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
}

/// The turn that finds the desktop withheld is off the budget as a whole, the opening's own time
/// included: a long lock leaves the budget where it was.
#[test]
fn a_long_withheld_desktop_leaves_the_budget_where_it_was() {
    use crate::windows_utils::streamer::dxgi::NotReady;
    use crate::windows_utils::streamer::pipeline::{run_deferred, DeferredEnd, DeferredSlot};
    let log = Arc::new(std::sync::Mutex::new(OpeningLog::default()));
    let slot = DeferredSlot::<FakeOpening>::new();
    let b = budget(Duration::from_secs(2));
    let before = b.left();
    let mut tries = 0;
    let end = run_deferred(
        &slot,
        &b,
        &|| true,
        &mut || true,
        &mut |d| idle(d.min(Duration::from_millis(50))),
        &mut || {
            tries += 1;
            if tries <= 20 {
                // an opening that builds devices before it is refused
                std::thread::sleep(Duration::from_millis(150));
                return Err(refused(NotReady::Withheld));
            }
            Ok(new_opening(&log))
        },
    );
    assert!(matches!(end, DeferredEnd::Running), "{end:?}");
    let used = before.saturating_sub(b.left());
    assert!(
        used < Duration::from_millis(300),
        "a 4 s lock cost the budget {used:?}"
    );
}

/// The render claim of duplication from the start lives exactly as long as the capture that
/// holds it, and a failed opening, which never hands it over, gives it back at once.
#[test]
fn the_duplication_claim_lives_as_long_as_its_capture() {
    use crate::windows_utils::streamer::pipeline::{
        duplication_claims, duplication_owns_render, dxgi_is_capture_tier, DuplicationClaim,
    };
    let base = duplication_claims();
    {
        // an opening that fails drops its claim with it
        let _claim = DuplicationClaim::new();
        assert_eq!(duplication_claims(), base + 1);
        assert!(duplication_owns_render());
    }
    assert_eq!(duplication_claims(), base);

    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let join = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let mut control = DxgiControl::new(Arc::clone(&stop), join);
    control.hold(DuplicationClaim::new());
    assert_eq!(duplication_claims(), base + 1);
    drop(control);
    assert_eq!(
        duplication_claims(),
        base,
        "the session's end gives it back"
    );
    assert!(stop.load(Ordering::Relaxed));
    if base == 0 && !dxgi_is_capture_tier() {
        assert!(
            !duplication_owns_render(),
            "later sessions follow the encoder again"
        );
    }
}

/// A duplication error that reaches the fall-back after WGC failed keeps its text and can never
/// be read as the typed "not ready".
#[test]
fn the_fallback_flattens_a_typed_duplication_error_to_text() {
    use crate::windows_utils::streamer::dxgi::{DuplicationNotReady, NotReady};
    use crate::windows_utils::streamer::pipeline::fallback_error;
    for kind in [
        NotReady::Settling,
        NotReady::Withheld,
        NotReady::Unsupported,
    ] {
        let typed = refused(kind).context("starting capture");
        assert!(typed.downcast_ref::<DuplicationNotReady>().is_some());
        let e = fallback_error(&anyhow::anyhow!("WGC refused"), typed);
        assert!(
            e.downcast_ref::<DuplicationNotReady>().is_none(),
            "{kind:?}"
        );
        let text = format!("{e:#}");
        assert!(text.contains("no adapter can duplicate X"), "{text}");
        assert!(text.contains("WGC refused"), "{text}");
    }
}

/// The driver is asked to compose a display once per attempt, whatever the number of openings,
/// and again only when the adapter DXGI lists it under changes.
#[test]
fn the_composing_adapter_is_asked_for_once_per_attempt() {
    use crate::windows_utils::streamer::pipeline::ComposeOnce;
    let once = ComposeOnce::default();
    let mut asked = Vec::new();
    for listing in [&[7i64][..], &[7], &[7], &[9], &[9], &[], &[7, 9], &[7]] {
        if let Some(a) = once.take(listing) {
            asked.push(a);
        }
    }
    assert_eq!(asked, vec![7, 9, 7]);
    // a new attempt starts over
    assert_eq!(ComposeOnce::default().take(&[7]), Some(7));
}
