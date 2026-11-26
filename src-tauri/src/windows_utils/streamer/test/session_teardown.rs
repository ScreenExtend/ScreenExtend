//! A session's NVENC ring, built and torn down the way a join and a leave do it, again and again:
//! nothing it creates may outlive it.

use super::*;
use crate::streamer::repair::RepairState;
use crate::streamer::telemetry::{FrameTiming, SessionTelemetry};
use crate::windows_utils::process_handles;
use std::collections::BTreeMap;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};

const W: u32 = 1920;
const H: u32 = 1080;

fn config() -> EncoderConfig {
    EncoderConfig {
        width: W,
        height: H,
        fps: 60,
        bitrate_bps: 8_000_000,
        max_bitrate_bps: 8_000_000,
        profile: crate::streamer::config::H264Profile::High,
        qp: None,
        min_qp: None,
        intra_refresh: false,
    }
}

fn frame_texture(device: &ID3D11Device) -> windows::core::Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: W,
        Height: H,
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
        MiscFlags: 0,
    };
    let mut tex = None;
    // SAFETY: a valid description and an out-pointer to a local.
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex))? };
    Ok(tex.unwrap())
}

/// One session: the ring built as `LiveCapture::new` builds it, on a capture device of the
/// NVIDIA adapter (where `Auto` and `Nvidia` capture), a few frames through it, then the teardown
/// a leave runs: `LiveCapture`'s drop stops the ring, then the capture device goes.
fn one_session(frames: u32) -> anyhow::Result<()> {
    let (device, context) = super::super::nvidia::encoder::create_nvidia_d3d11_device()?;
    let raw = frame_texture(&device)?;
    let (encoder, pairs) = build_zero_copy_ring(config(), &device, 3)?;
    let (textures, mutexes): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let (tx, mut rx) = broadcast::channel(64);
    let (_wake_tx, wake_rx) = crossbeam_channel::bounded::<()>(1);
    let mut ring = AsyncEncodeRing::new(
        RingEncoder::Nvenc(encoder),
        RingTextures::Shared { textures, mutexes },
        context.clone(),
        None,
        None,
        tx,
        Arc::new(AtomicU32::new(8_000_000)),
        8_000_000,
        Arc::new(AtomicU64::new(0)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(RepairState::new()),
        wake_rx,
        Duration::from_millis(16),
        Instant::now(),
        SessionTelemetry::new_shared(),
    );
    let mut emitted = 0;
    for _ in 0..frames {
        ring.stage(&raw, FrameTiming::entering_capture())?;
        std::thread::sleep(Duration::from_millis(16));
        while rx.try_recv().is_ok() {
            emitted += 1;
        }
    }
    // on a loaded machine the first picture can take longer than the frames above
    let deadline = Instant::now() + Duration::from_secs(3);
    while emitted == 0 && Instant::now() < deadline {
        ring.stage(&raw, FrameTiming::entering_capture())?;
        std::thread::sleep(Duration::from_millis(50));
        while rx.try_recv().is_ok() {
            emitted += 1;
        }
    }
    anyhow::ensure!(emitted > 0, "the ring encoded nothing");
    drop(ring);
    drop(raw);
    drop(context);
    drop(device);
    Ok(())
}

/// Long enough for the clock hold's thread to notice its stop flag and release its device.
fn settle() {
    std::thread::sleep(Duration::from_millis(400));
}

/// The two handles the NVIDIA user-mode driver opens again for every D3D11 device on its adapter
/// that allocates, and never closes: a 44 KB section and a mutex the process already holds, so
/// each costs a handle-table entry and nothing more. A session makes three such devices (the
/// encoder's, the capture device, the clock hold's). Only these two are excused.
const DRIVER_REOPENS: &[(&str, &str)] = &[
    ("Section", "{2627E361-24E2-4F14-99ED-A20D0685D8DD}_v23"),
    ("Mutant", "{2627E361-24E2-4F14-99ED-A20D0685D8DD}_v23_0"),
];

/// What it protects: everything the NVENC ring path creates in this process (the encoder session,
/// its devices, textures, NT handles, events, threads, and the GPU clock hold) is given back after
/// each session. What it does not: there is no WGC capture session here, so anything the
/// capture, DWM or the display owns on the host's behalf is outside it.
fn ring_sessions_leave_nothing_behind(cycles: u32) {
    if !nvidia_adapter_present() {
        crate::test_support::skip_because("NVENC ring teardown", "no NVIDIA adapter");
        return;
    }
    // a first session pays for everything created once per process (the driver's libraries,
    // DXGI's factory caches, the clock hold's statics); only what grows after it is a leak
    if let Err(e) = one_session(10) {
        crate::test_support::skip("NVENC ring teardown", e);
        return;
    }
    settle();
    let Some(held0) = process_handles::snapshot() else {
        crate::test_support::skip_because("NVENC ring teardown", "handle table unreadable");
        return;
    };
    let mem0 = process_handles::nvidia_dedicated_bytes();

    // the driver caps concurrent NVENC sessions machine-wide, and other tests and processes
    // hold some of them, so a refusal is waited out rather than failed
    for i in 0..cycles {
        let started = Instant::now();
        loop {
            match one_session(10) {
                Ok(()) => break,
                Err(e)
                    if is_nvenc_session_cap(&e) && started.elapsed() < Duration::from_secs(60) =>
                {
                    std::thread::sleep(Duration::from_millis(500));
                }
                Err(e) => panic!("session {i} failed: {e:#}"),
            }
        }
    }
    settle();

    // a leak is at least one handle per session; 1 MB of memory per two sessions is far below the
    // smallest picture-sized object and above anything the driver's allocator keeps around
    let handle_slack = i64::from(cycles / 2).max(1);
    let mem_slack = 1.0 + f64::from(cycles) * 0.5;
    // beside other tests a sample can catch their threads, events and textures alive; theirs
    // go when those tests end while a leak stays, so the cleanest sample in a minute is judged
    let ok = |grown: &BTreeMap<String, i64>, mem_mb: Option<f64>| {
        let total: i64 = grown.values().filter(|v| **v > 0).sum();
        mem_mb.is_none_or(|m| m < mem_slack)
            && total < handle_slack.max(2)
            && [
                "Section",
                "Mutant",
                "Event",
                "Thread",
                "Semaphore",
                "RawInputManager",
            ]
            .iter()
            .all(|k| grown.get(*k).copied().unwrap_or(0) < handle_slack.clamp(1, 4))
    };
    let mut best: Option<Sample> = None;
    for _ in 0..120 {
        let held1 = process_handles::snapshot().expect("handle table");
        let (excused, rest) = process_handles::split_excused(&held0, &held1, DRIVER_REOPENS);
        let grown = process_handles::growth(
            &process_handles::by_type(&held0),
            &process_handles::by_type(&rest),
        );
        let mem_mb = match (mem0, process_handles::nvidia_dedicated_bytes()) {
            (Some(a), Some(b)) => Some((b as f64 - a as f64) / 1_048_576.0),
            _ => None,
        };
        let total: i64 = grown.values().filter(|v| **v > 0).sum();
        let done = ok(&grown, mem_mb);
        let key = (total, mem_mb.unwrap_or(0.0));
        if best
            .as_ref()
            .is_none_or(|b| key < (b.total, b.mem_mb.unwrap_or(0.0)))
        {
            best = Some(Sample {
                total,
                mem_mb,
                grown,
                excused,
            });
        }
        if done {
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let s = best.expect("at least one sample");
    eprintln!(
        "NVENC ring teardown, {cycles} sessions: handles grown {:?}; driver re-opens {:?}; \
         dedicated memory {:?} MB",
        s.grown, s.excused, s.mem_mb
    );
    if s.mem_mb.is_none() {
        crate::test_support::skip_because(
            "NVENC ring teardown: dedicated memory",
            "the adapter's video memory could not be read; only handles were checked",
        );
    }
    assert!(
        ok(&s.grown, s.mem_mb),
        "over {cycles} sessions: handles grown {:?}, dedicated memory {:?} MB",
        s.grown,
        s.mem_mb
    );
}

/// Three sessions, counted in a process of their own: the rest of the suite opens and closes
/// handles, threads and GPU memory in this one while it runs, and no tolerance tells those apart
/// from a leak. The child is this test binary running [`thirty_nvenc_ring_sessions_leave_nothing_behind`]
/// alone, told by `SE_RING_TEARDOWN_CYCLES` to run three.
#[test]
fn nvenc_ring_sessions_leave_nothing_behind() {
    if !nvidia_adapter_present() {
        crate::test_support::skip_because("NVENC ring teardown", "no NVIDIA adapter");
        return;
    }
    let module = module_path!()
        .split_once("::")
        .map_or(module_path!(), |(_, m)| m);
    let name = format!("{module}::thirty_nvenc_ring_sessions_leave_nothing_behind");
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args([
            name.as_str(),
            "--exact",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SE_RING_TEARDOWN_CYCLES", "3")
        .env("SE_RING_TEARDOWN_PARENT", std::process::id().to_string())
        .output()
        .expect("run the teardown test in its own process");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr
        .lines()
        .filter(|l| l.starts_with("SKIP") || l.contains("teardown"))
    {
        eprintln!("{line}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "the isolated teardown run failed:\n{stdout}\n{stderr}"
    );
}

#[test]
#[ignore = "opens 31 NVENC sessions in a row and competes with any live host for the \
            machine-wide session cap; run with --ignored on an otherwise idle NVIDIA machine"]
fn thirty_nvenc_ring_sessions_leave_nothing_behind() {
    exit_with_parent();
    let cycles = std::env::var("SE_RING_TEARDOWN_CYCLES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    ring_sessions_leave_nothing_behind(cycles);
}

struct Sample {
    total: i64,
    mem_mb: Option<f64>,
    grown: BTreeMap<String, i64>,
    excused: BTreeMap<String, usize>,
}

/// Run as the child of [`nvenc_ring_sessions_leave_nothing_behind`], leave as soon as the parent
/// has gone, so a killed test run does not leave encoder sessions open behind it.
fn exit_with_parent() {
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    let Some(pid) = std::env::var("SE_RING_TEARDOWN_PARENT")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
    else {
        return;
    };
    // SAFETY: a synchronize-only handle to the parent, owned by the watcher below.
    let Ok(parent) = (unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }) else {
        std::process::exit(1);
    };
    let parent = parent.0 as usize;
    std::thread::spawn(move || {
        let h = HANDLE(parent as *mut std::ffi::c_void);
        loop {
            // SAFETY: the handle stays open for the life of this thread.
            if unsafe { WaitForSingleObject(h, 250) } != WAIT_TIMEOUT {
                // SAFETY: as above; the process exits right after.
                let _ = unsafe { CloseHandle(h) };
                std::process::exit(1);
            }
        }
    });
}
