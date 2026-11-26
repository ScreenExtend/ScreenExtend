//! The `live` stage: the whole host workflow on a real virtual display.
//!
//! One run is one child process (`bench --child live <spec>`) that, unlike every other bench
//! child, runs inside the Tauri runtime, because a virtual display needs `platform::setup` and
//! the `AppState` it builds. `SCREENEXTEND_DATA_DIR` points at a private directory and the
//! listeners are on loopback on non-default ports, so the child never touches the user's config,
//! lock file, control socket or a running host. An in-process webrtc-rs client makes a real WHEP
//! join, a window on the new display shows the workload with a barcode stamp per frame, and the
//! client decodes every frame and reads the stamp back.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded};
use serde_json::{json, Value as Json};
use tauri::Manager;

use crate::macos_utils as platform;
use crate::macos_utils::streamer::{cgds, display, qos, sck, tune};
use crate::streamer::audio::host_now_ns;
use crate::streamer::config::Config;
use crate::streamer::telemetry::SessionTelemetry;
use crate::streamer::test::whep_selftest;

use super::cf;
use super::clip;
use super::live_rx::{self, Assembler, FrameRec};
use super::live_window::{self, Surface, Window};
use super::metrics::summary;
use super::plan::{LiveOpts, RunSpec};
use super::series::{self, Row, Sampler};

pub const SESSION_ID: &str = "BENCHLIVESESSN";
pub const OTP: &str = "000000";
const DEVICE_TOKEN: &str = "be4c0000000000000000000000000001";
/// Test hook: behave as if Screen Recording were denied. Needs `SE_TEST_HOOKS=1` as well.
pub const DENY_ENV: &str = "SE_BENCH_DENY_SCREEN_RECORDING";
const SURFACES: usize = 6;
/// Surfaces kept off the free list after they were presented (Core Animation may still read one).
const HOLD_PRESENTED: usize = 3;
const LATE_NS: u64 = 2_000_000;
/// The minimum fresh-capture rate of an animated workload, in any mode: a quarter of the display's
/// refresh, and never below 5 fps.
const MIN_FRESH_SHARE: f64 = 0.25;
const MIN_FRESH_FPS: f64 = 5.0;

/// `CGSSessionScreenIsLocked` from `CGSessionCopyCurrentDictionary` (CoreGraphics, 10.3), resolved
/// with `dlsym`. `None`: the function or the session dictionary is unavailable. The key is present
/// only while the screen is locked.
fn screen_locked() -> Option<bool> {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
    type CopyDict = unsafe extern "C" fn() -> *const CFDictionary;
    let name = c"CGSessionCopyCurrentDictionary";
    let sym = unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) };
    if sym.is_null() {
        return None;
    }
    let f: CopyDict = unsafe { std::mem::transmute(sym) };
    let dict = std::ptr::NonNull::new(unsafe { f() } as *mut CFDictionary)?;
    let dict = unsafe { CFRetained::from_raw(dict) };
    let key = CFString::from_str("CGSSessionScreenIsLocked");
    let v = unsafe { dict.value((&*key as *const CFString).cast()) } as *const CFType;
    let Some(v) = (unsafe { v.as_ref() }) else {
        return Some(false);
    };
    if let Some(b) = v.downcast_ref::<CFBoolean>() {
        return Some(b.as_bool());
    }
    v.downcast_ref::<CFNumber>()
        .and_then(|n| n.as_i64())
        .map(|n| n != 0)
}

/// The display controller `platform::setup` built, so every exit path can remove what it made.
static DISPLAY: Mutex<Option<crate::streamer::session::SharedVirtualDisplay>> = Mutex::new(None);

fn cleanup_displays() {
    let d = DISPLAY.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(d) = d {
        platform::remove_all_displays(&d);
    }
}

fn arm_cleanup(app: &tauri::AppHandle) {
    {
        let state = app.state::<platform::AppState>();
        *DISPLAY.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.virtual_display.clone());
    }
    let _ = ctrlc::set_handler(|| {
        cleanup_displays();
        super::exit(130);
    });
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        cleanup_displays();
        previous(info);
    }));
}

/// `bench --child live <spec>`: called from `cli::dispatch`, on the main thread inside `setup`.
pub fn child_main(app: &tauri::AppHandle) -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(arg) = args.get(3) else {
        eprintln!("error: bench --child live <spec.json>");
        super::exit(2)
    };
    crate::logbus::set_verbose(true);
    if let Some(mode) = arg.strip_prefix("attach:") {
        attach_child(app, mode);
    }
    let text = match std::fs::read_to_string(arg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: read {arg}: {e}");
            super::exit(2)
        }
    };
    let spec: RunSpec = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: live spec: {e}");
            super::exit(2)
        }
    };
    let result = run(app, &spec);
    cleanup_displays();
    std::thread::sleep(Duration::from_millis(100));
    match &spec.result_path {
        Some(p) => {
            let tmp = format!("{p}.tmp");
            let text = serde_json::to_string(&result).unwrap_or_default();
            if std::fs::write(&tmp, text)
                .and_then(|_| std::fs::rename(&tmp, p))
                .is_err()
            {
                eprintln!("error: cannot write {p}");
                super::exit(1)
            }
        }
        None => println!("{result}"),
    }
    super::exit(0)
}

/// `bench --child live attach:<blocked|pump|pumpreg>:<wait ms>:<displays>`: create and remove
/// displays on a thread without a run loop while the main thread blocks (both as `serve` does),
/// pumps its run loop, or pumps it with the reconfiguration callback registered there. One
/// `ATTACH` line per display.
fn attach_child(app: &tauri::AppHandle, spec: &str) -> ! {
    let p: Vec<&str> = spec.split(':').collect();
    let mode = p[0].to_string();
    let num = |i: usize, d: u64| p.get(i).and_then(|v| v.parse().ok()).unwrap_or(d);
    let (wait, count) = (num(1, 0), num(2, 1));
    if !tauri::async_runtime::block_on(platform::setup(app.clone())) {
        eprintln!("ATTACH setup failed");
        super::exit(1)
    }
    let reg = mode == "pumpreg" && crate::macos_utils::virtual_display::bench_register_here();
    let vd = {
        let state = app.state::<platform::AppState>();
        state.virtual_display.clone()
    };
    let m = mode.clone();
    let worker = std::thread::spawn(move || {
        // the process has looked at the displays before, like a host that has been up a while
        let before = display::active_displays().len();
        std::thread::sleep(Duration::from_millis(wait));
        for k in 0..count {
            let client = &vd;
            let t = Instant::now();
            let r = client.create_display_with_modes(
                "ScreenExtend - attach".into(),
                1920,
                1080,
                60,
                &[],
            );
            let ms = t.elapsed().as_millis();
            let id = r.as_ref().copied().unwrap_or(0);
            println!(
                "ATTACH {m} k={k} reg={reg} before={before} {} in {ms} ms: {}",
                if r.is_ok() { "ok" } else { "FAILED" },
                crate::macos_utils::virtual_display::bench_state(id)
            );
            std::thread::sleep(Duration::from_millis(300));
            if r.is_ok() {
                client.remove_display(id);
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    });
    if mode == "nsapp" {
        // the desktop app's situation: a real NSApplication run on the main thread
        std::thread::spawn(move || {
            let _ = worker.join();
            super::exit(0)
        });
        unsafe {
            use objc2::{class, msg_send, runtime::AnyObject};
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let _: () = msg_send![app, run];
        }
        super::exit(0)
    }
    while !worker.is_finished() {
        if mode == "blocked" {
            std::thread::sleep(Duration::from_millis(20));
        } else {
            live_window::pump_run_loop(0.05);
        }
    }
    super::exit(0)
}

/// `pmset -g ps`'s "Now drawing from 'X'".
fn power_source() -> Option<String> {
    let out = std::process::Command::new("pmset")
        .args(["-g", "ps"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?;
    let a = line.find('\'')?;
    let b = line[a + 1..].find('\'')?;
    Some(line[a + 1..a + 1 + b].to_string())
}

fn port_free(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

struct Skip(String);

/// Everything the run collected that the result reports even when it stops early.
#[derive(Default)]
struct Ctx {
    pre: serde_json::Map<String, Json>,
    applied: serde_json::Map<String, Json>,
    notes: Vec<String>,
}

pub fn run(app: &tauri::AppHandle, spec: &RunSpec) -> Json {
    let wall = Instant::now();
    let sampler = Sampler::start_every(Duration::from_secs(2));
    let power_start = power_source();
    let mut ctx = Ctx::default();
    let out = run_inner(app, spec, &mut ctx);
    let power_end = power_source();
    let base = json!({
        "id": spec.id,
        "stage": "live",
        "preconditions": Json::Object(ctx.pre.clone()),
        "variables": Json::Object(ctx.applied.clone()),
        "notes": ctx.notes,
        "power_source": { "start": power_start, "end": power_end },
        "low_power_mode": cf::low_power_mode(),
        "thermal_state": cf::thermal_state(),
        "tuning": tune::report(),
    });
    let mut v = match out {
        Ok(v) => v,
        Err(Skip(reason)) => json!({
            "result": "skipped",
            "valid": false,
            "skip_reason": reason,
            "invalid_reasons": [format!("skipped: {reason}")],
        }),
    };
    if let (Some(dst), Some(src)) = (v.as_object_mut(), base.as_object()) {
        for (k, val) in src {
            dst.entry(k.clone()).or_insert(val.clone());
        }
    }
    v["series"] = sampler.finish();
    v["wall_s"] = json!(wall.elapsed().as_secs_f64());
    v
}

#[derive(Clone)]
struct Snap {
    t_ns: u64,
    submitted: u64,
    encoded: u64,
    bytes: u64,
    deg: Vec<(&'static str, u64)>,
}

fn snap(t: &SessionTelemetry) -> Snap {
    Snap {
        t_ns: host_now_ns(),
        submitted: t.frames_submitted.load(Ordering::Relaxed),
        encoded: t.frames_encoded.load(Ordering::Relaxed),
        bytes: t.bytes_sent.load(Ordering::Relaxed),
        deg: t.degradation.each(),
    }
}

fn deg_of(s: &Snap, name: &str) -> u64 {
    s.deg
        .iter()
        .find(|(k, _)| *k == name)
        .map_or(0, |(_, v)| *v)
}

/// Fresh (captured, not repeated) frames submitted between two snapshots.
fn fresh_between(a: &Snap, b: &Snap) -> u64 {
    let d = |n: &str| deg_of(b, n).saturating_sub(deg_of(a, n));
    b.submitted
        .saturating_sub(a.submitted)
        .saturating_sub(d("frames_repeated") + d("capture_no_first_frame"))
}

fn snap_delta(a: &Snap, b: &Snap) -> Json {
    let deg: serde_json::Map<String, Json> = b
        .deg
        .iter()
        .map(|(k, v)| (k.to_string(), json!(v.saturating_sub(deg_of(a, k)))))
        .filter(|(_, v)| v.as_u64() != Some(0))
        .collect();
    json!({
        "span_s": (b.t_ns.saturating_sub(a.t_ns)) as f64 / 1e9,
        "frames_submitted": b.submitted.saturating_sub(a.submitted),
        "frames_submitted_fresh": fresh_between(a, b),
        "frames_encoded": b.encoded.saturating_sub(a.encoded),
        "bytes_sent": b.bytes.saturating_sub(a.bytes),
        "degradation": deg,
    })
}

/// One workload frame's pixels.
enum Source {
    Drag {
        desk: Vec<u8>,
        win: Vec<u8>,
        ww: usize,
        wh: usize,
        path: Vec<(usize, usize)>,
    },
    Synthetic(crate::streamer::synthetic::Workload),
    Recording {
        path: std::path::PathBuf,
        info: clip::ClipInfo,
    },
}

/// The window workload: a renderer filling a ring of surfaces ahead of the clock, and a presenter
/// putting one on screen per frame deadline.
struct WindowWorkload {
    stop: Arc<AtomicBool>,
    presented: Arc<AtomicU64>,
    present_ns: Arc<Vec<AtomicU64>>,
    renderer: Option<std::thread::JoinHandle<Result<(), String>>>,
    presenter: Option<std::thread::JoinHandle<(Vec<u64>, usize)>>,
}

impl WindowWorkload {
    #[allow(clippy::too_many_arguments)]
    fn start(
        layer: live_window::LayerRef,
        source: Source,
        w: usize,
        h: usize,
        deadlines_ns: Vec<u64>,
        stamp: bool,
        t_start: Instant,
        period_ns: u64,
    ) -> Result<WindowWorkload, String> {
        let n = deadlines_ns.len();
        let mut surfaces = Vec::with_capacity(SURFACES);
        for _ in 0..SURFACES.min(n.max(1) + HOLD_PRESENTED) {
            surfaces.push(Surface::new(w, h)?);
        }
        let surfaces = Arc::new(surfaces);
        let (free_tx, free_rx) = bounded::<usize>(surfaces.len());
        for i in 0..surfaces.len() {
            let _ = free_tx.send(i);
        }
        let (ready_tx, ready_rx) = bounded::<(usize, usize)>(surfaces.len());
        let stop = Arc::new(AtomicBool::new(false));
        let presented = Arc::new(AtomicU64::new(0));
        let present_ns: Arc<Vec<AtomicU64>> = Arc::new((0..n).map(|_| AtomicU64::new(0)).collect());

        let renderer = {
            let (surfaces, stop, deadlines) = (
                Arc::clone(&surfaces),
                Arc::clone(&stop),
                deadlines_ns.clone(),
            );
            std::thread::Builder::new()
                .name("bench-live-render".into())
                .spawn(move || -> Result<(), String> {
                    qos::pin_current_thread_user_initiated();
                    let mut buf = vec![0u8; w * h * 4];
                    let mut nv12 = Vec::new();
                    let mut reader = None;
                    for (k, &due) in deadlines.iter().enumerate() {
                        let Ok(s) = free_rx.recv() else { return Ok(()) };
                        if stop.load(Ordering::Relaxed) {
                            return Ok(());
                        }
                        match &source {
                            Source::Drag {
                                desk,
                                win,
                                ww,
                                wh,
                                path,
                            } => {
                                buf.copy_from_slice(desk);
                                let (px, py) = path[k % path.len()];
                                for y in 0..*wh {
                                    let d = ((py + y) * w + px) * 4;
                                    buf[d..d + ww * 4]
                                        .copy_from_slice(&win[y * ww * 4..(y + 1) * ww * 4]);
                                }
                            }
                            Source::Synthetic(wl) => crate::streamer::synthetic::fill(
                                &mut buf, w as u32, h as u32, k as u32, *wl,
                            ),
                            Source::Recording { path, info } => {
                                if reader.is_none() || k % info.frames == 0 {
                                    reader = Some(
                                        clip::ClipReader::open(path, info)
                                            .map_err(|e| e.to_string())?,
                                    );
                                }
                                if let Some(r) = reader.as_mut() {
                                    r.next(&mut nv12)
                                        .map_err(|e| format!("recording frame {k}: {e}"))?;
                                }
                                live_window::nv12_to_bgra(
                                    &nv12,
                                    info.width,
                                    info.height,
                                    &mut buf,
                                    w,
                                    h,
                                );
                            }
                        }
                        if stamp {
                            crate::streamer::synthetic::draw_counter(
                                &mut buf,
                                w as u32,
                                h as u32,
                                k as u32,
                                due / 1_000_000,
                            );
                            live_window::draw_stamp(&mut buf, w, h, k as u32);
                        }
                        surfaces[s].write(&buf);
                        if ready_tx.send((k, s)).is_err() {
                            return Ok(());
                        }
                    }
                    Ok(())
                })
                .map_err(|e| e.to_string())?
        };

        let presenter = {
            let (surfaces, stop, presented, present_ns) = (
                Arc::clone(&surfaces),
                Arc::clone(&stop),
                Arc::clone(&presented),
                Arc::clone(&present_ns),
            );
            std::thread::Builder::new()
                .name("bench-live-present".into())
                .spawn(move || {
                    qos::pin_current_thread_time_constraint(period_ns.max(1_000_000));
                    let mut late = Vec::with_capacity(deadlines_ns.len());
                    let mut underruns = 0usize;
                    let mut held: VecDeque<usize> = VecDeque::new();
                    for (k, &due) in deadlines_ns.iter().enumerate() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let deadline = t_start + Duration::from_nanos(due);
                        loop {
                            let now = Instant::now();
                            if now >= deadline {
                                break;
                            }
                            let left = deadline - now;
                            if left > Duration::from_micros(2000) {
                                std::thread::sleep(left - Duration::from_micros(1500));
                            } else {
                                std::hint::spin_loop();
                            }
                        }
                        let (kk, s) = match ready_rx.try_recv() {
                            Ok(x) => x,
                            Err(_) => {
                                underruns += 1;
                                match ready_rx.recv() {
                                    Ok(x) => x,
                                    Err(_) => break,
                                }
                            }
                        };
                        debug_assert_eq!(kk, k);
                        let now = Instant::now();
                        late.push(now.saturating_duration_since(deadline).as_nanos() as u64);
                        live_window::present(layer, &surfaces[s]);
                        present_ns[kk].store(host_now_ns(), Ordering::Relaxed);
                        presented.fetch_add(1, Ordering::Relaxed);
                        held.push_back(s);
                        if held.len() > HOLD_PRESENTED {
                            if let Some(f) = held.pop_front() {
                                let _ = free_tx.send(f);
                            }
                        }
                    }
                    (late, underruns)
                })
                .map_err(|e| e.to_string())?
        };
        Ok(WindowWorkload {
            stop,
            presented,
            present_ns,
            renderer: Some(renderer),
            presenter: Some(presenter),
        })
    }

    /// Stop and join; returns (late ns per presented frame, underruns, renderer error).
    fn finish(&mut self) -> (Vec<u64>, usize, Option<String>) {
        self.stop.store(true, Ordering::Relaxed);
        let (late, under) = self
            .presenter
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let err = self
            .renderer
            .take()
            .and_then(|h| h.join().ok())
            .and_then(|r| r.err());
        (late, under, err)
    }
}

/// The fallback workload: the pointer swept over the display, which both capture backends
/// composite. This moves the user's real cursor, so it is only used when the window cannot be.
struct CursorWorkload {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl CursorWorkload {
    fn start(display: u32, w: u32, h: u32) -> CursorWorkload {
        use objc2_core_foundation::CGPoint;
        use objc2_core_graphics::CGDisplayMoveCursorToPoint;
        unsafe extern "C" {
            fn CGDisplayHideCursor(display: u32) -> i32;
            fn CGDisplayShowCursor(display: u32) -> i32;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let mut i = 0u32;
            while !flag.load(Ordering::Relaxed) {
                let x = f64::from(i.wrapping_mul(7) % w.max(2));
                let y = f64::from(i.wrapping_mul(11) % h.max(2));
                unsafe {
                    let _ = CGDisplayMoveCursorToPoint(display, CGPoint { x, y });
                    CGDisplayHideCursor(display);
                    CGDisplayShowCursor(display);
                }
                i = i.wrapping_add(1);
                std::thread::sleep(Duration::from_micros(2000));
            }
        });
        CursorWorkload {
            stop,
            handle: Some(handle),
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn ms(a: u64, b: u64) -> Option<f64> {
    (a != 0 && b != 0 && b >= a).then(|| (b - a) as f64 / 1e6)
}

/// Lines from the log backlog that say what the encoder and capture turned into.
fn host_log() -> Vec<String> {
    const KEYS: &[&str] = &[
        "[vt]",
        "[sck]",
        "[cgds]",
        "[capture]",
        "[tcc]",
        "[display]",
        "[virtual",
        "negotiat",
        "profile",
        "[pipeline]",
        "[encode",
        "telemetry:",
        "join ",
        "[tune]",
    ];
    let lines = crate::logbus::backlog_snapshot();
    let mut out: Vec<String> = lines
        .into_iter()
        .filter(|l| KEYS.iter().any(|k| l.contains(k)))
        .collect();
    if out.len() > 300 {
        out.drain(..out.len() - 300);
    }
    out
}

#[allow(clippy::too_many_lines)]
fn run_inner(app: &tauri::AppHandle, spec: &RunSpec, ctx: &mut Ctx) -> Result<Json, Skip> {
    let opts: LiveOpts = spec.live.clone().unwrap_or_default();
    let (req_w, req_h) = (opts.width.unwrap_or(1920), opts.height.unwrap_or(1080));
    let refresh = opts.refresh.unwrap_or(60);
    let dpr = opts.dpr.unwrap_or(1.0);
    let video_scale = opts.video_scale.unwrap_or(100).clamp(10, 100);
    let http_port = opts.http_port.unwrap_or(28780);
    let https_port = opts.https_port.unwrap_or(28743);
    let seconds = opts.seconds();
    let warmup = opts.warmup_s();
    let stamp = opts.stamp.unwrap_or(true);
    let pattern = if spec.workload.kind == "recording" {
        "recording".to_string()
    } else {
        spec.workload
            .pattern
            .clone()
            .unwrap_or_else(|| "window_drag_stress".into())
    };
    let is_static = pattern == "static";
    let fps = spec.workload.fps.clamp(1, 240);
    let production = spec.recipe.production.clone();

    // what the plan asked for, and whether it reached the child
    let env = |k: &str| std::env::var(k).ok();
    ctx.applied.insert(
        "recipe".into(),
        json!({
            "name": spec.recipe.recipe.name,
            "kind": if production.is_some() { "production" } else { "recipe" },
            "production": production,
            "se_vt_recipe_env": env("SE_VT_RECIPE"),
            "applied": production.is_some() || env("SE_VT_RECIPE").is_some(),
        }),
    );
    ctx.applied.insert(
        "tune".into(),
        json!({ "on": env("SE_TUNE_ON"), "off": env("SE_TUNE_OFF"),
                "applied": spec.tune_on.is_empty() == env("SE_TUNE_ON").is_none()
                    && spec.tune_off.is_empty() == env("SE_TUNE_OFF").is_none() }),
    );
    ctx.applied.insert(
        "deny".into(),
        json!({ "env": env("SE_VT_DENY"), "applied": spec.deny.is_empty() == env("SE_VT_DENY").is_none() }),
    );
    let knob_env: serde_json::Map<String, Json> = opts
        .capture
        .env()
        .into_iter()
        .map(|(k, v)| {
            let got = env(k);
            (k.to_string(), json!({ "asked": v, "child_env": got, "applied": got.as_deref() == Some(v.as_str()) }))
        })
        .collect();
    ctx.applied
        .insert("capture_env".into(), Json::Object(knob_env));
    ctx.applied.insert(
        "display_request".into(),
        json!({ "width": req_w, "height": req_h, "refresh": refresh, "dpr": dpr, "video_scale": video_scale }),
    );
    ctx.applied.insert(
        "workload".into(),
        json!({ "pattern": pattern, "fps": fps, "seconds": seconds, "warmup_s": warmup, "stamp": stamp }),
    );
    ctx.applied.insert(
        "data_dir".into(),
        json!(crate::data_dir::override_dir().map(|p| p.display().to_string())),
    );

    // preconditions, all silent
    // a locked screen composites nothing new, so the run would measure nothing
    let locked = screen_locked();
    ctx.pre.insert("screen_locked".into(), json!(locked));
    if locked == Some(true) {
        return Err(Skip(
            "the screen is locked (CGSessionCopyCurrentDictionary: CGSSessionScreenIsLocked)"
                .into(),
        ));
    }
    let hooks = env("SE_TEST_HOOKS").as_deref() == Some("1");
    let simulate_deny = hooks && env(DENY_ENV).as_deref() == Some("1");
    if env(DENY_ENV).is_some() && !hooks {
        ctx.notes
            .push(format!("{DENY_ENV} ignored without SE_TEST_HOOKS=1"));
    }
    if crate::data_dir::override_dir().is_none() {
        ctx.pre.insert("data_dir".into(), json!("not isolated"));
        return Err(Skip(
            "SCREENEXTEND_DATA_DIR is not set; a live run never touches the user's data dir".into(),
        ));
    }
    let (preflight, window_probe, live) = if simulate_deny {
        (Some(false), Some(false), Some(false))
    } else {
        (
            platform::permissions::preflight_screen_capture(),
            platform::permissions::window_name_probe(),
            platform::permissions::screen_recording_live(),
        )
    };
    ctx.pre.insert(
        "screen_recording".into(),
        json!({ "preflight": preflight, "window_name_probe": window_probe, "live": live,
                "simulated_denied": simulate_deny, "prompting_api_called": false }),
    );
    match live {
        Some(true) => {}
        Some(false) => {
            return Err(Skip(format!(
                "Screen Recording is not granted to this process{} (checked silently; no prompt)",
                if simulate_deny {
                    " (simulated by the test hook)"
                } else {
                    ""
                }
            )))
        }
        None => {
            return Err(Skip(
                "Screen Recording state could not be read silently; not starting capture".into(),
            ))
        }
    }
    let backend = platform::virtual_display::probe_display_backend();
    ctx.pre
        .insert("display_backend".into(), json!(backend.as_str()));
    if backend != platform::virtual_display::DisplayBackend::CgVirtualDisplay {
        return Err(Skip(format!(
            "display backend {} cannot create a virtual display for a measurement",
            backend.as_str()
        )));
    }
    let ports_ok = port_free(http_port) && port_free(https_port);
    ctx.pre.insert(
        "ports".into(),
        json!({ "http": http_port, "https": https_port, "free": ports_ok }),
    );
    if !ports_ok {
        return Err(Skip(format!("port {http_port} or {https_port} is in use")));
    }
    let main_thread = live_window::is_main_thread();
    let window_api = live_window::available();
    ctx.pre.insert("main_thread".into(), json!(main_thread));
    ctx.pre.insert("window_api".into(), json!(window_api));
    let source = match pattern.as_str() {
        "recording" => {
            let Some(p) = spec.clip_path.clone() else {
                return Err(Skip("recording workload and no --recording given".into()));
            };
            let info = clip::scan(std::path::Path::new(&p))
                .map_err(|e| Skip(format!("recording: {e}")))?;
            Some(Source::Recording {
                path: p.into(),
                info,
            })
        }
        "window_drag_stress" => None, // built once the display size is known
        other => match crate::streamer::synthetic::Workload::parse(other) {
            Some(wl) => Some(Source::Synthetic(wl)),
            None => return Err(Skip(format!("unknown pattern '{other}'"))),
        },
    };

    crate::streamer::platform::set_dpi_awareness();
    crate::streamer::platform::apply_process_tuning();
    let t_setup = Instant::now();
    if !tauri::async_runtime::block_on(platform::setup(app.clone())) {
        return Err(Skip(
            "platform::setup failed (no virtual-display controller)".into(),
        ));
    }
    ctx.pre.insert(
        "setup_ms".into(),
        json!(t_setup.elapsed().as_secs_f64() * 1e3),
    );
    arm_cleanup(app);
    let state = app.state::<platform::AppState>();
    *state
        .session_auth
        .session_id
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = SESSION_ID.into();
    *state
        .session_auth
        .otp
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = OTP.into();
    // a device with saved settings, the way a real one has after its first join: the quality
    // slider is constant QP (the shipped default is 15), the refresh rate is the one asked for
    let qp = production.as_ref().and_then(|p| p.qp);
    let min_qp = production.as_ref().and_then(|p| p.min_qp);
    let seeded = production.is_none() || qp.is_some() || min_qp.is_some();
    if seeded {
        let o = crate::streamer::session::DeviceOverride {
            refresh_rate: refresh,
            video_scale,
            video_quality: min_qp.or(qp).unwrap_or(15),
            adaptive_quality: min_qp.is_some() && qp.is_none(),
            dpr,
            ..crate::streamer::session::DeviceOverride::join_defaults()
        };
        state
            .device_overrides
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(DEVICE_TOKEN.to_string(), o);
    }
    let software = production.as_ref().is_some_and(|p| p.software);
    state.disable_gpu_encode.store(software, Ordering::Relaxed);
    let cfg = Config {
        bind_ip: std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
        lan_ip: Some("127.0.0.1".to_string()),
        local_ips: Some(state.local_ips.clone()),
        port: http_port,
        https_port,
        stun_urls: Vec::new(),
        qp,
        quality_qp: min_qp,
        max_bitrate_kbps: production
            .as_ref()
            .and_then(|p| p.bitrate_bps)
            .map(|b| b / 1000),
        max_fps: refresh,
        disable_gpu_encode: software,
        gpu_encode_toggle: Some(state.disable_gpu_encode.clone()),
        virtual_display: Some(state.virtual_display.clone()),
        session_auth: Some(state.session_auth.clone()),
        device_overrides: Some(state.device_overrides.clone()),
        sessions: Some(state.sessions.clone()),
        audio_sinks: Some(state.audio_sinks.clone()),
        disconnect_grace: Some(state.disconnect_grace.clone()),
        user_turn: Some(state.user_turn.clone()),
        banned_devices: Some(state.banned_devices.clone()),
        approved_devices: Some(state.approved_devices.clone()),
        otp_limiter: Some(state.otp_limiter.clone()),
        audio_hub: Some(state.audio_hub.clone()),
        ..Config::default()
    };
    let profile_offer: Vec<&str> = if opts.baseline_only
        || production.as_ref().and_then(|p| p.profile.as_deref()) == Some("baseline")
    {
        vec!["level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"]
    } else {
        vec![
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=640c1f",
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
        ]
    };
    ctx.applied.insert(
        "encoder_config".into(),
        json!({ "device_override_seeded": seeded, "qp": qp, "min_qp": min_qp,
                "max_bitrate_kbps": cfg.max_bitrate_kbps, "software": software,
                "offered_h264": profile_offer,
                "profile_note": "the profile is negotiated from the offer; ProductionCfg.profile only selects a baseline-only offer" }),
    );

    let transport = crate::streamer::transport_runtime().map_err(|e| Skip(format!("{e:#}")))?;
    let server_handle = axum_server::Handle::new();
    {
        let (cfg, h) = (cfg.clone(), server_handle.clone());
        transport.spawn(async move {
            if let Err(e) = crate::streamer::server::run(cfg, Some(h)).await {
                teprintln!("[live] server exited with error: {e:?}");
            }
        });
    }
    let client_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("bench-live-client")
        // a receiver that reads at background priority overruns webrtc-rs's 1 MB SRTP stream
        // buffer (dropped silently) on a 1.3 MB keyframe, whatever the socket buffer holds
        .on_thread_start(|| {
            qos::pin_current_thread_user_interactive();
        })
        .enable_all()
        .build()
        .map_err(|e| Skip(format!("client runtime: {e}")))?;

    if let Some(churn_opts) = opts.churn.clone() {
        let mode = match (churn_opts.main.as_deref(), churn_opts.block_main) {
            (_, true) => "block",
            (Some(m), _) => m,
            _ => "block",
        }
        .to_string();
        // the joins' own waits never run a run loop: they are not on the main thread
        BLOCK_MAIN.store(true, Ordering::Relaxed);
        let app_state: &platform::AppState = &state;
        let out = std::thread::scope(|sc| {
            let h = sc.spawn(|| {
                churn(
                    app_state,
                    &client_rt,
                    https_port,
                    &profile_offer,
                    &churn_opts,
                    (req_w, req_h),
                    refresh,
                    dpr,
                )
            });
            while !h.is_finished() {
                match mode.as_str() {
                    "block" => std::thread::sleep(Duration::from_millis(20)),
                    "nsapp" => live_window::app_events(0.05),
                    _ => live_window::pump_run_loop(0.05),
                }
            }
            h.join()
        });
        let out = out.unwrap_or_else(
            |_| json!({ "result": "churn_panicked", "valid": false, "invalid_reasons": ["panic"] }),
        );
        server_handle.shutdown();
        cleanup_displays();
        client_rt.shutdown_background();
        return Ok(out);
    }

    // whatever goes wrong before the teardown's own guard: never past the run's expected length
    guard_exit(
        "the join started",
        Duration::from_secs_f64(opts.warmup_s.unwrap_or(1.5) + opts.seconds.unwrap_or(12.0) + 40.0),
    );

    let (au_tx, au_rx) = unbounded();
    let assembler = Arc::new(Mutex::new(Assembler::new(au_tx)));
    // the receive task only timestamps and hands on; reassembly has a thread of its own, so
    // nothing it does can leave datagrams waiting in the socket
    let (pkt_tx, pkt_rx) = unbounded::<(u16, u32, bool, bytes::Bytes, u64)>();
    {
        let assembler = Arc::clone(&assembler);
        std::thread::Builder::new()
            .name("bench-live-assemble".into())
            .spawn(move || {
                qos::pin_current_thread_user_initiated();
                for (seq, ts, marker, payload, now) in pkt_rx {
                    if let Ok(mut a) = assembler.lock() {
                        a.push(seq, ts, marker, &payload, now);
                    }
                }
            })
            .map_err(|e| Skip(format!("assembler thread: {e}")))?;
    }
    let rcvbuf: Arc<Mutex<Vec<i32>>> = Arc::default();
    let rtp_packets = Arc::new(AtomicU64::new(0));
    let first_rtp_ns = Arc::new(AtomicU64::new(0));
    // the client's own connection states, so a join that never carries media says where it stopped
    let pc_states: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let offer_candidates: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let ssrc = Arc::new(AtomicU32::new(0));
    let posted_ns = host_now_ns();
    let want_stats = opts.client_stats.unwrap_or(true);
    type Joined = (
        Arc<webrtc::peer_connection::RTCPeerConnection>,
        String,
        String,
        Option<Arc<webrtc::data_channel::RTCDataChannel>>,
    );
    let joined: anyhow::Result<Joined> = client_rt.block_on(pumped(async {
        use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
        use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
        whep_selftest::wait_for_health(https_port).await?;
        let api = whep_selftest::build_api_with(&profile_offer, opts.srtp_window.unwrap_or(8192))?;
        let pc = Arc::new(
            api.new_peer_connection(
                webrtc::peer_connection::configuration::RTCConfiguration::default(),
            )
            .await?,
        );
        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(webrtc::rtp_transceiver::RTCRtpTransceiverInit {
                direction: RTCRtpTransceiverDirection::Recvonly,
                send_encodings: vec![],
            }),
        )
        .await?;
        // the page's control channel: STATS ride on it, as `input.js` sends them
        let stats_dc = if want_stats {
            Some(
                pc.create_data_channel(
                    "reliable",
                    Some(
                        webrtc::data_channel::data_channel_init::RTCDataChannelInit {
                            ordered: Some(true),
                            ..Default::default()
                        },
                    ),
                )
                .await?,
            )
        } else {
            None
        };
        {
            let (pkt_tx, rtp_packets, first_rtp_ns, ssrc) = (
                pkt_tx.clone(),
                Arc::clone(&rtp_packets),
                Arc::clone(&first_rtp_ns),
                Arc::clone(&ssrc),
            );
            pc.on_track(Box::new(move |track, _r, _t| {
                ssrc.store(track.ssrc(), Ordering::Relaxed);
                let (pkt_tx, rtp_packets, first_rtp_ns) = (
                    pkt_tx.clone(),
                    Arc::clone(&rtp_packets),
                    Arc::clone(&first_rtp_ns),
                );
                Box::pin(async move {
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 1600];
                        while let Ok((pkt, _)) = track.read(&mut buf).await {
                            let now = host_now_ns();
                            if rtp_packets.fetch_add(1, Ordering::Relaxed) == 0 {
                                first_rtp_ns.store(now, Ordering::Relaxed);
                            }
                            let _ = pkt_tx.send((
                                pkt.header.sequence_number,
                                pkt.header.timestamp,
                                pkt.header.marker,
                                pkt.payload,
                                now,
                            ));
                        }
                    });
                })
            }));
        }
        {
            let t0 = host_now_ns();
            let (a, b) = (Arc::clone(&pc_states), Arc::clone(&pc_states));
            pc.on_peer_connection_state_change(Box::new(move |st| {
                let line = format!("+{:.0}ms pc={st}", (host_now_ns() - t0) as f64 / 1e6);
                tprintln!("[live] client {line}");
                if let Ok(mut v) = a.lock() {
                    v.push(line);
                }
                Box::pin(async {})
            }));
            pc.on_ice_connection_state_change(Box::new(move |st| {
                let line = format!("+{:.0}ms ice={st}", (host_now_ns() - t0) as f64 / 1e6);
                tprintln!("[live] client {line}");
                if let Ok(mut v) = b.lock() {
                    v.push(line);
                }
                Box::pin(async {})
            }));
        }
        let fds_before = udp_fds();
        let offer = pc.create_offer(None).await?;
        let mut gather = pc.gathering_complete_promise().await;
        pc.set_local_description(offer).await?;
        let _ = gather.recv().await;
        // the sockets this gather opened are the client's (the host has opened none yet)
        let want = opts.rcvbuf_bytes.unwrap_or(8 << 20);
        if let Ok(mut v) = rcvbuf.lock() {
            *v = udp_fds()
                .into_iter()
                .filter(|fd| !fds_before.contains(fd))
                .map(|fd| raise_rcvbuf(fd, want))
                .collect();
        }
        let sdp = pc
            .local_description()
            .await
            .ok_or_else(|| anyhow::anyhow!("no local description"))?
            .sdp;
        if let Ok(mut v) = offer_candidates.lock() {
            *v = sdp
                .lines()
                .filter(|l| l.starts_with("a=candidate"))
                .map(str::to_string)
                .collect();
        }
        let body = json!({
            "sessionId": SESSION_ID,
            "otp": OTP,
            "deviceName": "bench live v2",
            "deviceToken": DEVICE_TOKEN,
            "attemptId": format!("{:032x}", host_now_ns()),
            "os": "bench",
            "refreshRate": refresh,
            "width": req_w,
            "height": req_h,
            "dpr": dpr,
            "sdp": sdp,
        });
        let (answer, join_id) = whep_selftest::post_join(https_port, &body).await?;
        pc.set_remote_description(
            webrtc::peer_connection::sdp::session_description::RTCSessionDescription::answer(
                answer.clone(),
            )?,
        )
        .await?;
        Ok((pc, join_id, answer, stats_dc))
    }));
    let answered_ns = host_now_ns();
    let teardown_host = |join_id: Option<&str>| {
        if let Some(j) = join_id {
            let req = format!(
                "POST /leave?join={j} HTTP/1.1\r\nHost: 127.0.0.1:{https_port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = client_rt.block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(3),
                    whep_selftest::tls_send_recv(https_port, req.as_bytes()),
                )
                .await
            });
        }
    };
    let (pc, join_id, answer, stats_dc) = match joined {
        Ok(x) => x,
        Err(e) => {
            server_handle.shutdown();
            cleanup_displays();
            client_rt.shutdown_background();
            return Ok(json!({
                "result": "join_failed",
                "valid": false,
                "invalid_reasons": [format!("join: {e:#}")],
                "host_log": host_log(),
            }));
        }
    };
    let negotiated_fmtp: Vec<String> = answer
        .lines()
        .filter(|l| l.starts_with("a=fmtp:") && l.contains("profile-level-id"))
        .map(|l| l.trim().to_string())
        .collect();

    let wait_start = Instant::now();
    while rtp_packets.load(Ordering::Relaxed) == 0 && wait_start.elapsed() < Duration::from_secs(20)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let first_ns = first_rtp_ns.load(Ordering::Relaxed);
    let live_display = state
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&join_id)
        .and_then(|s| s.live_display.clone());
    let telemetry = crate::streamer::telemetry::get(&join_id);
    let display_id = live_display
        .as_ref()
        .and_then(|d| d.device_name.trim().parse::<u32>().ok());
    let connection = json!({
        "client_states": pc_states.lock().map(|v| v.clone()).unwrap_or_default(),
        "offer_candidates": offer_candidates.lock().map(|v| v.clone()).unwrap_or_default(),
        "answer_candidates": answer
            .lines()
            .filter(|l| l.starts_with("a=candidate"))
            .collect::<Vec<_>>(),
    });
    let (Some(display_id), Some(telemetry), true) = (display_id, telemetry, first_ns != 0) else {
        let why = format!(
            "after the join: display={:?} telemetry={} first_rtp={}",
            live_display.as_ref().map(|d| d.device_name.clone()),
            crate::streamer::telemetry::get(&join_id).is_some(),
            first_ns != 0
        );
        teardown_host(Some(&join_id));
        let _ = client_rt.block_on(pc.close());
        server_handle.shutdown();
        cleanup_displays();
        client_rt.shutdown_background();
        return Ok(json!({
            "result": "join_incomplete",
            "valid": false,
            "invalid_reasons": [why],
            "join_id": join_id,
            "connection": connection,
            "host_log": host_log(),
        }));
    };
    let (px_w, px_h, mode_hz) = display::display_geometry(display_id);
    let bounds = display::display_bounds(display_id);
    if opts.teach_origin_left {
        let rc = crate::macos_utils::virtual_display::teach_origin_left(display_id);
        std::thread::sleep(Duration::from_millis(500));
        tprintln!(
            "[live] taught display {display_id} a left-hand origin (CGError {rc}): now at {:?}",
            display::display_bounds(display_id)
        );
    }
    // the display must be the size the client asked for (macOS can restore the size it remembers
    // for that display identity); checked at DPR 1 only, where a backing pixel is a point
    let size_mismatch = (dpr < 1.5 && (px_w, px_h) != (req_w, req_h)).then(|| {
        format!(
            "the display is {px_w}x{px_h}, not the requested {req_w}x{req_h} (the run measured \
             another size)"
        )
    });
    let (overlaps, mirrored) =
        crate::macos_utils::virtual_display::arrangement_conflicts(display_id);
    let arrangement_bad = (!overlaps.is_empty() || mirrored).then(|| {
        format!(
            "display {display_id} at ({},{}) overlaps displays {overlaps:?}{} (capture and input \
             geometry are wrong in that arrangement)",
            bounds.0,
            bounds.1,
            if mirrored { " and is mirrored" } else { "" }
        )
    });
    let input_rect = crate::macos_utils::streamer::pipeline::monitor_rect(&display_id.to_string());
    let display_json = json!({
        "id": display_id,
        "overlaps": overlaps,
        "mirrored": mirrored,
        "input_mapper_rect": input_rect.map(|r| json!([r.0, r.1, r.2, r.3])),
        "pixels": [px_w, px_h],
        "points": [bounds.2, bounds.3],
        "origin_points": [bounds.0, bounds.1],
        "refresh_hz": mode_hz,
        "requested": [req_w, req_h],
        "size_matches_request": if dpr < 1.5 { json!(size_mismatch.is_none()) } else { json!("not checked (HiDPI)") },
        "session_display": live_display.as_ref().map(|d| json!({
            "width": d.width, "height": d.height, "device_name": d.device_name,
        })),
    });

    let decoded = Arc::new(AtomicU64::new(0));
    let rx_stop = Arc::new(AtomicBool::new(false));
    let client_window = live_rx::ClientWindow::new();
    let request_pli: Box<dyn Fn() + Send> =
        {
            let (pc, ssrc, handle) = (
                Arc::clone(&pc),
                Arc::clone(&ssrc),
                client_rt.handle().clone(),
            );
            let cw = Arc::clone(&client_window);
            Box::new(move || {
                cw.note_key_request();
                let (pc, media_ssrc) = (Arc::clone(&pc), ssrc.load(Ordering::Relaxed));
                handle.spawn(async move {
                use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
                let _ = pc
                    .write_rtcp(&[Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc })])
                    .await;
            });
            })
        };
    let decoder = live_rx::spawn_decoder(
        au_rx,
        (px_w as usize, px_h as usize),
        Arc::clone(&decoded),
        Arc::clone(&rx_stop),
        request_pli,
        Some(Arc::clone(&client_window)),
    );
    let stats_sent = Arc::new(AtomicU64::new(0));
    let stats_task = stats_dc.map(|dc| {
        let (cw, sent) = (Arc::clone(&client_window), Arc::clone(&stats_sent));
        client_rt.spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if dc.ready_state()
                    == webrtc::data_channel::data_channel_state::RTCDataChannelState::Open
                {
                    let frame = bytes::Bytes::from(cw.stats_frame());
                    if dc.send(&frame).await.is_ok() {
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
    });

    let (w, h) = (px_w as usize & !1, px_h as usize & !1);
    let period_ns = 1_000_000_000 / fps as u64;
    let run_ns = ((warmup + seconds + 0.5) * 1e9) as u64;
    let deadlines: Vec<u64> = if is_static {
        vec![0]
    } else if let Some(Source::Recording { info, .. }) = &source {
        let loop_ns = info.times[info.frames - 1] + info.median_gap_ns();
        let mut v = Vec::new();
        let mut k = 0usize;
        loop {
            let t = info.times[k % info.frames] + (k / info.frames) as u64 * loop_ns;
            if t > run_ns {
                break;
            }
            v.push(t);
            k += 1;
        }
        v
    } else {
        (0..(run_ns / period_ns).max(1))
            .map(|k| k * period_ns)
            .collect()
    };
    let source = match source {
        Some(s) => s,
        None => {
            let ww = (w * 55 / 100) & !1;
            let wh = (h * 60 / 100) & !1;
            Source::Drag {
                desk: clip::stress_desktop(w, h, spec.workload.seed),
                win: clip::stress_window(ww, wh, spec.workload.seed),
                ww,
                wh,
                path: clip::drag_path(
                    deadlines.len(),
                    fps as f64,
                    w - ww,
                    h - wh,
                    spec.workload.seed,
                ),
            }
        }
    };
    let mut mode = "window";
    let mut window: Option<Window> = None;
    let mut window_info = json!({});
    if opts.force_cursor_fallback {
        mode = "cursor-fallback";
        ctx.notes.push("cursor fallback forced by the plan".into());
    } else if !main_thread || !window_api {
        mode = "cursor-fallback";
        ctx.notes.push(format!(
            "window workload unavailable (main_thread={main_thread} window_api={window_api})"
        ));
    } else {
        match Window::open(display_id) {
            Ok(win) => {
                std::thread::sleep(Duration::from_millis(100));
                let server = win.server_bounds();
                let on_display = server.is_some_and(|(x, y, ww, hh)| {
                    (x - win.target_cg.0).abs() < 2.0
                        && (y - win.target_cg.1).abs() < 2.0
                        && (ww - win.target_cg.2).abs() < 2.0
                        && (hh - win.target_cg.3).abs() < 2.0
                });
                window_info = json!({
                    "window_number": win.window_number,
                    "target_cg": [win.target_cg.0, win.target_cg.1, win.target_cg.2, win.target_cg.3],
                    "server_bounds": server.map(|b| [b.0, b.1, b.2, b.3]),
                    "covers_display": on_display,
                    "surface": [w, h],
                });
                window = Some(win);
            }
            Err(e) => {
                mode = "cursor-fallback";
                ctx.notes
                    .push(format!("window workload failed to open: {e}"));
            }
        }
    }
    let t_w0 = Instant::now() + Duration::from_millis(30);
    let t_w0_ns = crate::streamer::audio::host_instant_to_ns(t_w0);
    let mut win_work: Option<WindowWorkload> = None;
    let mut cursor: Option<CursorWorkload> = None;
    if let Some(win) = &window {
        match WindowWorkload::start(
            win.layer(),
            source,
            w,
            h,
            deadlines.clone(),
            stamp,
            t_w0,
            period_ns,
        ) {
            Ok(ww) => win_work = Some(ww),
            Err(e) => {
                mode = "cursor-fallback";
                ctx.notes
                    .push(format!("window workload failed to start: {e}"));
            }
        }
    }
    if mode == "cursor-fallback" {
        cursor = Some(CursorWorkload::start(display_id, bounds.2, bounds.3));
    }

    // warm-up, with a check that the window's frames actually reach capture
    let sleep_until = |t: Instant| {
        let now = Instant::now();
        if t > now {
            std::thread::sleep(t - now);
        }
    };
    sleep_until(t_w0 + Duration::from_millis(300));
    let probe_a = snap(&telemetry);
    let presented_a = win_work
        .as_ref()
        .map_or(0, |w| w.presented.load(Ordering::Relaxed));
    let t0 = t_w0 + Duration::from_secs_f64(warmup);
    sleep_until(t0);
    let probe_b = snap(&telemetry);
    let presented_b = win_work
        .as_ref()
        .map_or(0, |w| w.presented.load(Ordering::Relaxed));
    let warm_fresh = fresh_between(&probe_a, &probe_b);
    let warm_presented = presented_b.saturating_sub(presented_a);
    let mut window_composited = json!(null);
    let mut t0 = t0;
    if mode == "window" && !is_static {
        let ok = warm_fresh as f64 >= 0.2 * warm_presented as f64 && warm_presented > 0;
        window_composited = json!({ "warmup_presented": warm_presented, "warmup_fresh_captured": warm_fresh, "ok": ok });
        if !ok {
            ctx.notes.push(format!(
                "the window's frames did not reach capture during warm-up ({warm_fresh} fresh of \
                 {warm_presented} presented); switching to the cursor fallback"
            ));
            if let Some(mut ww) = win_work.take() {
                ww.finish();
            }
            if let Some(win) = window.take() {
                win.close();
            }
            mode = "cursor-fallback";
            cursor = Some(CursorWorkload::start(display_id, bounds.2, bounds.3));
            t0 = Instant::now() + Duration::from_secs_f64(warmup);
            sleep_until(t0);
        }
    }

    // the measured window
    let s0 = snap(&telemetry);
    let t0_ns = s0.t_ns;
    let t1 = t0 + Duration::from_secs_f64(seconds);
    sleep_until(t1);
    let s1 = snap(&telemetry);
    let t1_ns = s1.t_ns;
    let telemetry_json = telemetry.to_json();
    let capture_backend = telemetry.capture_backend.get();
    let encoder_backend = telemetry.encoder.get();
    let capture_features = telemetry.capture_features.get();

    let (late, underruns, render_err) = win_work.as_mut().map(|w| w.finish()).unwrap_or_default();
    let present_ns: Vec<u64> = win_work
        .as_ref()
        .map(|w| {
            w.present_ns
                .iter()
                .map(|a| a.load(Ordering::Relaxed))
                .collect()
        })
        .unwrap_or_default();
    if let Some(c) = cursor.as_mut() {
        c.finish();
    }
    // frames in flight when the workload stopped: let them arrive and decode
    std::thread::sleep(Duration::from_millis(400));
    let window_server_bounds_end = window.as_ref().and_then(|w| w.server_bounds());
    if let Some(win) = window.take() {
        win.close();
    }
    // nothing of this process may stay on the user's screen: from here the child has this long
    guard_exit("teardown", Duration::from_secs(8));
    let t_leave = Instant::now();
    teardown_host(Some(&join_id));
    let mut session_gone_ms = None;
    while t_leave.elapsed() < Duration::from_secs(4) {
        let gone = !state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&join_id)
            .is_some_and(|s| s.live_display.is_some());
        if gone {
            session_gone_ms = Some(t_leave.elapsed().as_secs_f64() * 1e3);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(t) = &stats_task {
        t.abort();
    }
    let _ = client_rt
        .block_on(async { tokio::time::timeout(Duration::from_secs(3), pc.close()).await });
    rx_stop.store(true, Ordering::Relaxed);
    let rx = decoder.recv_timeout(Duration::from_secs(3)).ok();
    let asm_stats = assembler
        .lock()
        .map(|a| {
            (
                a.packets,
                a.unsupported_nal,
                a.out_of_order,
                a.too_late,
                a.duplicates,
                a.holes,
                a.never_arrived,
            )
        })
        .unwrap_or((0, 0, 0, 0, 0, 0, 0));
    // the production leave path removes the display on its own (after `LEAVE_SETTLE`): give it
    // the chance, and time it, before the forced cleanup
    let gone_within = |limit: Duration| -> Option<f64> {
        let t = Instant::now();
        while t.elapsed() < limit {
            if !display::active_displays().contains(&display_id) {
                return Some(t_leave.elapsed().as_secs_f64() * 1e3);
            }
            if main_thread {
                live_window::pump_run_loop(0.02);
            } else {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        None
    };
    let display_gone_after_leave_ms = gone_within(Duration::from_secs(4));
    server_handle.shutdown();
    let displays_before_cleanup = display::active_displays().contains(&display_id);
    cleanup_displays();
    let display_left = displays_before_cleanup && gone_within(Duration::from_secs(3)).is_none();
    client_rt.shutdown_timeout(Duration::from_secs(2));

    let Some(rx) = rx else {
        return Ok(json!({ "result": "receiver_panicked", "valid": false,
                          "invalid_reasons": ["the decode thread gave no result in 3 s"], "host_log": host_log() }));
    };
    let in_window = |r: &FrameRec| r.capture_ns >= t0_ns && r.capture_ns <= t1_ns;
    let frames: Vec<&FrameRec> = rx.frames.iter().filter(|r| in_window(r)).collect();
    // freshness by stamp: a frame showing the same presented index as the decoded frame before
    // it is a repeat (the encode loop's idle repeater, or capture redelivering a still picture)
    let mut fresh_flag: HashMap<usize, bool> = HashMap::new();
    let mut prev: Option<u32> = None;
    for r in &rx.frames {
        if let Some(s) = r.stamp {
            fresh_flag.insert(r.seq, prev != Some(s));
            prev = Some(s);
        }
    }
    let (mut c2a, mut c2a_fresh, mut c2first, mut dec, mut c2d, mut c2d_fresh) =
        (vec![], vec![], vec![], vec![], vec![], vec![]);
    let (mut p2c, mut p2d, mut gaps, mut bytes, mut pkts) =
        (vec![], vec![], vec![], vec![], vec![]);
    let mut rows_c2a: Vec<Row> = Vec::new();
    let mut rows_c2d: Vec<Row> = Vec::new();
    let mut rows_dec: Vec<Row> = Vec::new();
    let mut rows_gap: Vec<Row> = Vec::new();
    let (mut complete, mut incomplete, mut skipped, mut decode_ok, mut decode_fail, mut keyframes) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);
    let (mut fresh_n, mut repeat_n, mut unstamped) = (0usize, 0usize, 0usize);
    let mut distinct: HashSet<u32> = HashSet::new();
    let mut total_bytes = 0u64;
    let mut last_arrival: Option<u64> = None;
    let timeline_ns = t1_ns.saturating_sub(t0_ns);
    for r in &frames {
        let t_rel = r.capture_ns.saturating_sub(t0_ns);
        total_bytes += r.bytes as u64;
        bytes.push(r.bytes as f64);
        pkts.push(r.packets as f64);
        if r.keyframe {
            keyframes += 1;
        }
        if !r.complete {
            incomplete += 1;
            continue;
        }
        complete += 1;
        if let Some(prev) = last_arrival {
            if let Some(g) = ms(prev, r.arrival_ns) {
                gaps.push(g);
                rows_gap.push(Row {
                    t_ns: t_rel,
                    lat_ms: Some(g),
                    ..Default::default()
                });
            }
        }
        last_arrival = Some(r.arrival_ns);
        let a = ms(r.capture_ns, r.arrival_ns);
        c2a.extend(a);
        c2first.extend(ms(r.capture_ns, r.first_ns));
        rows_c2a.push(Row {
            t_ns: t_rel,
            lat_ms: a,
            bytes: r.bytes as u64,
            ..Default::default()
        });
        if r.skipped {
            skipped += 1;
            continue;
        }
        let ok = r.submit_status == 0 && r.decode_status == Some(0);
        if !ok {
            decode_fail += 1;
            continue;
        }
        decode_ok += 1;
        let d = ms(r.submit_ns, r.done_ns);
        dec.extend(d);
        rows_dec.push(Row {
            t_ns: t_rel,
            lat_ms: d,
            ..Default::default()
        });
        let cd = ms(r.capture_ns, r.done_ns);
        c2d.extend(cd);
        rows_c2d.push(Row {
            t_ns: t_rel,
            lat_ms: cd,
            ..Default::default()
        });
        match (r.stamp, fresh_flag.get(&r.seq).copied()) {
            (Some(s), Some(true)) => {
                fresh_n += 1;
                distinct.insert(s);
                c2a_fresh.extend(a);
                c2d_fresh.extend(cd);
                if let Some(&pn) = present_ns.get(s as usize) {
                    p2c.extend(ms(pn, r.capture_ns));
                    p2d.extend(ms(pn, r.done_ns));
                }
            }
            (Some(_), _) => repeat_n += 1,
            _ => unstamped += 1,
        }
    }
    let presented_in_window = present_ns
        .iter()
        .filter(|&&p| p >= t0_ns && p <= t1_ns)
        .count();
    let captured_fresh = fresh_between(&s0, &s1);
    let late_count = late.iter().filter(|&&l| l > LATE_NS).count();
    let late_ms: Vec<f64> = late.iter().map(|&l| l as f64 / 1e6).collect();
    let secs = timeline_ns as f64 / 1e9;

    let mut reasons: Vec<String> = Vec::new();
    if let Some(m) = &size_mismatch {
        reasons.push(m.clone());
    }
    if let Some(m) = &arrangement_bad {
        reasons.push(m.clone());
    }
    if frames.is_empty() {
        reasons.push("no frames received in the measured window".into());
    }
    // every animated workload, the cursor fallback included, must actually be captured
    let window_s = t1_ns.saturating_sub(t0_ns) as f64 / 1e9;
    let min_fresh_fps = (MIN_FRESH_SHARE * mode_hz as f64).max(MIN_FRESH_FPS);
    if !is_static && window_s > 0.0 && (captured_fresh as f64 / window_s) < min_fresh_fps {
        reasons.push(format!(
            "only {captured_fresh} fresh captures in {window_s:.1} s ({:.1} fps < {min_fresh_fps:.1}, \
             mode {mode})",
            captured_fresh as f64 / window_s
        ));
    }
    if mode == "window" && !is_static && (captured_fresh as f64) < 0.8 * presented_in_window as f64
    {
        reasons.push(format!(
            "only {captured_fresh} of {presented_in_window} presented frames were captured as new \
             frames (< 80%)"
        ));
    }
    if decode_fail > 0 {
        reasons.push(format!("{decode_fail} frames failed to decode"));
    }
    // why the first keyframe was or was not usable
    let first_keyframe = rx
        .frames
        .iter()
        .find(|r| r.keyframe || r.packets > 200)
        .map(|r| {
            json!({
                "seq": r.seq, "packets": r.packets, "bytes": r.bytes, "complete": r.complete,
                "missing_at_emit": r.missing, "recovered_out_of_order": r.recovered,
                "gave_up_waiting": r.gave_up,
                "first_to_last_packet_ms": (r.arrival_ns.saturating_sub(r.first_ns)) as f64 / 1e6,
                "decoded": r.decode_status == Some(0),
            })
        });
    let frames_recovered = rx
        .frames
        .iter()
        .filter(|r| r.recovered > 0 && r.complete)
        .count();
    let frames_given_up = rx.frames.iter().filter(|r| r.gave_up).count();
    // a run that decoded nothing (or little) measured nothing, however clean its transport looks
    if !frames.is_empty() && (decode_ok as f64) < 0.8 * frames.len() as f64 {
        reasons.push(format!(
            "decoded {decode_ok} of {} frames received in the window (< 80%); {skipped} skipped \
             waiting for a keyframe, {} keyframe requests sent",
            frames.len(),
            rx.plis_requested
        ));
    }
    if incomplete > 0 {
        reasons.push(format!("{incomplete} frames arrived incomplete"));
    }
    if late_count as f64 > 0.01 * late.len().max(1) as f64 {
        reasons.push(format!(
            "{late_count} of {} workload frames presented > 2 ms late by the harness",
            late.len()
        ));
    }
    if let Some(e) = &render_err {
        reasons.push(format!("workload renderer: {e}"));
    }
    if mode == "window" && stamp && fresh_n == 0 && !is_static && decode_ok > 0 {
        reasons.push("no decoded frame carried a readable stamp".into());
    }
    if display_left {
        reasons.push(format!("display {display_id} still present after cleanup"));
    }

    let dist = |rows: &[Row], width: u64| series::buckets(width, timeline_ns, rows);
    let frames_json = json!({
        "presented_total": present_ns.iter().filter(|&&p| p != 0).count(),
        "presented_in_window": presented_in_window,
        // host side: submitted frames that were not the encode loop's idle repeats; capture may
        // redeliver an unchanged picture, so the stamp counts below are what the client saw change
        "captured_fresh_in_window": captured_fresh,
        "host_encoded_in_window": s1.encoded.saturating_sub(s0.encoded),
        "received": frames.len(),
        "complete": complete,
        "incomplete": incomplete,
        "skipped_waiting_keyframe": skipped,
        "decoded": decode_ok,
        "decode_failures": decode_fail,
        "keyframes": keyframes,
        "fresh_by_stamp": fresh_n,
        "repeats_by_stamp": repeat_n,
        "unstamped": unstamped,
        "distinct_presented_seen": distinct.len(),
        "received_total": rx.frames.len(),
        "rtp_packets_total": asm_stats.0,
        "unsupported_nal_packets": asm_stats.1,
        "packets_out_of_order": asm_stats.2,
        "packets_too_late": asm_stats.3,
        "packets_duplicate": asm_stats.4,
        "packets_unique_missing": asm_stats.5,
        "packets_never_arrived": asm_stats.6,
        "nack_requests_whole_run": telemetry.degradation.nacks_received.load(Ordering::Relaxed),
        "client_rcvbuf_effective": rcvbuf.lock().map(|v| v.clone()).unwrap_or_default(),
        "first_keyframe": first_keyframe,
        "frames_recovered_by_repair": frames_recovered,
        "frames_given_up": frames_given_up,
    });
    Ok(json!({
        "result": "completed",
        "valid": reasons.is_empty(),
        "invalid_reasons": reasons,
        "workload_mode": mode,
        "client_stats": { "enabled": want_stats, "frames_sent": stats_sent.load(Ordering::Relaxed) },
        "pattern": pattern,
        "capture_backend": capture_backend,
        "capture_features": capture_features,
        "capture_knobs": {
            "screencapturekit": sck::applied_knobs(),
            "cgdisplaystream": cgds::applied_knobs(),
        },
        "encoder_backend": encoder_backend,
        "negotiated_fmtp": negotiated_fmtp,
        "display": display_json,
        "window": window_info,
        "window_server_bounds_end": window_server_bounds_end.map(|b| [b.0, b.1, b.2, b.3]),
        "window_composited_check": window_composited,
        "join": {
            "join_id": join_id,
            "answer_ms": (answered_ns.saturating_sub(posted_ns)) as f64 / 1e6,
            "first_rtp_after_post_ms": (first_ns.saturating_sub(posted_ns)) as f64 / 1e6,
            "connection": connection,
        },
        "measured_s": secs,
        "frames": frames_json,
        "fps": {
            "presented": presented_in_window as f64 / secs.max(1e-9),
            "captured_fresh": captured_fresh as f64 / secs.max(1e-9),
            "received": frames.len() as f64 / secs.max(1e-9),
            "decoded": decode_ok as f64 / secs.max(1e-9),
            "fresh_decoded": fresh_n as f64 / secs.max(1e-9),
        },
        "client_feedback": {
            "plis_sent_by_client": rx.plis_requested,
            "host_nacks_received": deg_of(&s1, "nacks_received").saturating_sub(deg_of(&s0, "nacks_received")),
            "host_keyframe_requests": deg_of(&s1, "keyframe_requests").saturating_sub(deg_of(&s0, "keyframe_requests")),
            "decoder_create_status": rx.decoder_create_status,
            "format_errors": rx.format_errors,
            "decode_flags": live_rx::DECODE_FLAGS,
        },
        "receiver_ms": {
            "capture_to_arrival": summary(&c2a),
            "capture_to_arrival_fresh": summary(&c2a_fresh),
            "capture_to_first_packet": summary(&c2first),
            "decode": summary(&dec),
            "capture_to_decoded": summary(&c2d),
            "capture_to_decoded_fresh": summary(&c2d_fresh),
            "inter_arrival": summary(&gaps),
            "present_to_capture": summary(&p2c),
            "present_to_decoded": summary(&p2d),
        },
        "latency_ms": summary(&c2d),
        "frame_bytes": summary(&bytes),
        "packets_per_frame": summary(&pkts),
        "avg_mbps": total_bytes as f64 * 8.0 / secs.max(1e-9) / 1e6,
        "buckets_10s": {
            "capture_to_arrival": dist(&rows_c2a, 10),
            "decode": dist(&rows_dec, 10),
            "capture_to_decoded": dist(&rows_c2d, 10),
            "inter_arrival": dist(&rows_gap, 10),
        },
        "buckets_60s": {
            "capture_to_arrival": dist(&rows_c2a, 60),
            "decode": dist(&rows_dec, 60),
            "capture_to_decoded": dist(&rows_c2d, 60),
            "inter_arrival": dist(&rows_gap, 60),
        },
        "host_window_delta": snap_delta(&s0, &s1),
        "telemetry": telemetry_json,
        "workload_pacing": {
            "late_over_2ms": late_count,
            "late_ms": summary(&late_ms),
            "underruns": underruns,
            "frames_scheduled": deadlines.len(),
            "start_host_ns": t_w0_ns,
        },
        "teardown": {
            "leave_to_session_gone_ms": session_gone_ms,
            "leave_to_display_gone_ms": display_gone_after_leave_ms,
            "display_present_before_cleanup": displays_before_cleanup,
            "display_present_after_cleanup": display_left,
        },
        "host_log": host_log(),
    }))
}

/// Display churn: `joins` short real joins back to back on this one host process, one device
/// identity, cycling sizes. Per join: requested vs actual size, origin, overlap, attach time
/// (POST -> first RTP, and POST -> the session's display recorded), outcome. Overall: failures,
/// wrong sizes, overlaps, attach percentiles, and displays / fds / threads before and after.
#[allow(clippy::too_many_arguments)]
fn churn(
    state: &platform::AppState,
    client_rt: &tokio::runtime::Runtime,
    https_port: u16,
    profile_offer: &[&str],
    o: &crate::bench::plan::ChurnOpts,
    default_size: (u32, u32),
    refresh: u32,
    dpr: f64,
) -> Json {
    let sizes = if o.sizes.is_empty() {
        vec![default_size]
    } else {
        o.sizes.clone()
    };
    let hold = Duration::from_secs_f64(o.hold_s.unwrap_or(3.0));
    let gap = Duration::from_millis(o.gap_ms.unwrap_or(750));
    let active = || {
        let mut ids = [0u32; 32];
        let mut n = 0u32;
        unsafe { objc2_core_graphics::CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut n) };
        n as usize
    };
    let (d0, f0, t0) = (
        active(),
        crate::bench::cf::fd_count(),
        crate::bench::cf::thread_count(),
    );
    let files0 = open_file_kinds();
    // the ports the client's own gathers opened, so a leftover socket can be told client or host
    let client_ports: Arc<Mutex<HashSet<u16>>> = Arc::default();
    let mut rows = Vec::new();
    let (mut failures, mut wrong, mut overl) = (0u32, 0u32, 0u32);
    let mut not_gone = 0u32;
    let mut attach_ms: Vec<f64> = Vec::new();
    pause(Duration::from_millis(o.first_wait_ms.unwrap_or(0)));
    for k in 0..o.joins as usize {
        let (w, h) = sizes[k % sizes.len()];
        let posted = Instant::now();
        let rtp = Arc::new(AtomicU64::new(0));
        let joined: anyhow::Result<(Arc<webrtc::peer_connection::RTCPeerConnection>, String)> =
            client_rt.block_on(pumped(async {
                use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
                use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
                let api = whep_selftest::build_api(profile_offer)?;
                let pc = Arc::new(
                    api.new_peer_connection(
                        webrtc::peer_connection::configuration::RTCConfiguration::default(),
                    )
                    .await?,
                );
                pc.add_transceiver_from_kind(
                    RTPCodecType::Video,
                    Some(webrtc::rtp_transceiver::RTCRtpTransceiverInit {
                        direction: RTCRtpTransceiverDirection::Recvonly,
                        send_encodings: vec![],
                    }),
                )
                .await?;
                let r = Arc::clone(&rtp);
                pc.on_track(Box::new(move |track, _r, _t| {
                    let r = Arc::clone(&r);
                    Box::pin(async move {
                        tokio::spawn(async move {
                            let mut buf = vec![0u8; 1600];
                            while track.read(&mut buf).await.is_ok() {
                                r.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                    })
                }));
                let before = udp_fds();
                let offer = pc.create_offer(None).await?;
                let mut gather = pc.gathering_complete_promise().await;
                pc.set_local_description(offer).await?;
                let _ = gather.recv().await;
                if let Ok(mut p) = client_ports.lock() {
                    p.extend(udp_fds().into_iter().filter(|f| !before.contains(f)).map(udp_port));
                }
                let sdp = pc
                    .local_description()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("no local description"))?
                    .sdp;
                let body = json!({
                    "sessionId": SESSION_ID, "otp": OTP, "deviceName": "bench live v2",
                    "deviceToken": DEVICE_TOKEN, "attemptId": format!("{:032x}", host_now_ns()),
                    "os": "bench", "refreshRate": refresh, "width": w, "height": h, "dpr": dpr,
                    "sdp": sdp,
                });
                let (answer, join_id) = whep_selftest::post_join(https_port, &body).await?;
                pc.set_remote_description(
                    webrtc::peer_connection::sdp::session_description::RTCSessionDescription::answer(
                        answer,
                    )?,
                )
                .await?;
                Ok((pc, join_id))
            }));
        let post_ms = posted.elapsed().as_secs_f64() * 1e3;
        let (pc, join_id) = match joined {
            Ok(x) => x,
            Err(e) => {
                failures += 1;
                rows.push(json!({ "k": k, "requested": [w, h], "outcome": format!("join failed: {e:#}"), "post_ms": post_ms }));
                pause(gap);
                continue;
            }
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while rtp.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            pause(Duration::from_millis(5));
        }
        let first_rtp_ms =
            (rtp.load(Ordering::Relaxed) > 0).then(|| posted.elapsed().as_secs_f64() * 1e3);
        let live = state
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&join_id)
            .and_then(|s| s.live_display.clone());
        let id = live
            .as_ref()
            .and_then(|d| d.device_name.trim().parse::<u32>().ok());
        let mut row = json!({ "k": k, "requested": [w, h], "post_ms": post_ms, "first_rtp_ms": first_rtp_ms });
        match (id, first_rtp_ms) {
            (Some(id), Some(ms)) => {
                let (pw, ph, _) = display::display_geometry(id);
                let b = display::display_bounds(id);
                let (ov, mir) = crate::macos_utils::virtual_display::arrangement_conflicts(id);
                let at = (pw, ph) == (w, h);
                if !at {
                    wrong += 1;
                }
                if !ov.is_empty() || mir {
                    overl += 1;
                }
                attach_ms.push(ms);
                let sd = live.as_ref().map(|d| (d.width, d.height));
                row["actual"] = json!([pw, ph]);
                row["session_reports"] = json!(sd);
                row["origin"] = json!([b.0, b.1]);
                row["overlaps"] = json!(ov);
                row["mirrored"] = json!(mir);
                row["outcome"] = json!(if at && ov.is_empty() && !mir {
                    "ok"
                } else {
                    "bad"
                });
            }
            _ => {
                failures += 1;
                row["outcome"] = json!(format!(
                    "no media or no display (display={:?}, rtp={})",
                    id,
                    rtp.load(Ordering::Relaxed)
                ));
            }
        }
        pause(hold);
        let req = format!(
            "POST /leave?join={join_id} HTTP/1.1\r\nHost: 127.0.0.1:{https_port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let _ = client_rt.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(3),
                whep_selftest::tls_send_recv(https_port, req.as_bytes()),
            )
            .await
        });
        let _ = client_rt
            .block_on(async { tokio::time::timeout(Duration::from_secs(3), pc.close()).await });
        // the display goes after LEAVE_SETTLE: wait for it so the next join is a fresh create
        let t_leave = Instant::now();
        while t_leave.elapsed() < Duration::from_secs(6) {
            let alive = id.is_some_and(|i| {
                let mut ids = [0u32; 32];
                let mut n = 0u32;
                unsafe {
                    objc2_core_graphics::CGGetActiveDisplayList(32, ids.as_mut_ptr(), &mut n)
                };
                ids[..n as usize].contains(&i)
            });
            if !alive {
                break;
            }
            pause(Duration::from_millis(20));
        }
        let still = id.is_some_and(|i| display::active_displays().contains(&i));
        row["display_gone"] = json!(!still);
        if still || t_leave.elapsed() > Duration::from_secs(5) {
            not_gone += 1;
        }
        row["display_gone_ms"] = json!(t_leave.elapsed().as_secs_f64() * 1e3);
        row["fds"] = json!(cf::fd_count());
        row["callbacks"] = json!(crate::macos_utils::virtual_display::bench_callbacks());
        tprintln!("[churn] {row}");
        rows.push(row);
        pause(gap);
    }
    pause(Duration::from_secs(2));
    let open_files = open_file_kinds();
    let left: Vec<u16> = udp_fds().into_iter().map(udp_port).collect();
    let of_client = {
        let p = client_ports.lock().unwrap_or_else(|e| e.into_inner());
        left.iter().filter(|x| p.contains(x)).count()
    };
    let mut by_ip: std::collections::BTreeMap<String, u32> = Default::default();
    for fd in udp_fds() {
        *by_ip.entry(udp_ip(fd)).or_default() += 1;
    }
    let leftover = json!({ "client": of_client, "host": left.len() - of_client, "by_ip": by_ip });
    let (d1, f1, t1) = (
        active(),
        crate::bench::cf::fd_count(),
        crate::bench::cf::thread_count(),
    );
    attach_ms.sort_by(|a, b| a.total_cmp(b));
    let pct = |p: f64| {
        (!attach_ms.is_empty())
            .then(|| attach_ms[((p * attach_ms.len() as f64) as usize).min(attach_ms.len() - 1)])
    };
    let leaked = d1 as i64 - d0 as i64;
    let slow = attach_ms.iter().filter(|&&v| v > 4_000.0).count();
    let valid =
        failures == 0 && wrong == 0 && overl == 0 && leaked == 0 && slow == 0 && not_gone == 0;
    json!({
        "result": if valid { "ok" } else { "churn_failed" },
        "valid": valid,
        "invalid_reasons": if valid { json!([]) } else { json!([format!(
            "failures {failures}, wrong sizes {wrong}, overlaps {overl}, joins over 4 s {slow}, displays leaked {leaked}")]) },
        "churn": {
            "joins": o.joins, "sizes": sizes, "hold_s": hold.as_secs_f64(), "gap_ms": gap.as_millis() as u64,
            "displays_not_gone_in_5s": not_gone,
            "failures": failures, "wrong_sizes": wrong, "overlaps": overl,
            "attach_ms_post_to_first_rtp": { "p50": pct(0.5), "p90": pct(0.9), "p99": pct(0.99), "max": attach_ms.last() },
            "active_displays_before_after": [d0, d1],
            "fds_before_after": [f0, f1],
            "threads_before_after": [t0, t1],
            "open_files_before": files0,
            "open_files_after": open_files,
            "udp_sockets_left": leftover,
            "rows": rows,
        },
        "host_log": host_log(),
    })
}

/// This process's descriptors by kind (`lsof`), so a growth can be named.
fn open_file_kinds() -> Json {
    let out = std::process::Command::new("/usr/sbin/lsof")
        .args(["-n", "-P", "-a", "-p", &std::process::id().to_string()])
        .args(["-d", "0-4096", "-F", "tPn"])
        .output();
    let Ok(out) = out else { return Json::Null };
    let mut kinds: std::collections::BTreeMap<String, u32> = Default::default();
    let mut kind = String::new();
    for l in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((k, v)) = l.split_at_checked(1) else {
            continue;
        };
        match k {
            "t" => {
                kind = v.to_string();
                *kinds.entry(kind.clone()).or_default() += 1;
            }
            "P" => *kinds.entry(format!("{kind}/{v}")).or_default() += 1,
            "n" if kind.starts_with("IP") => {
                let shape = if v.contains("->") {
                    "connected"
                } else {
                    "unconnected"
                };
                *kinds.entry(format!("{kind} {shape}")).or_default() += 1;
            }
            _ => {}
        }
    }
    json!(kinds)
}

/// The IPv4 address a UDP descriptor is bound to ("" for anything else).
fn udp_ip(fd: i32) -> String {
    unsafe {
        let mut sa: libc::sockaddr_storage = std::mem::zeroed();
        let mut sl = size_of::<libc::sockaddr_storage>() as u32;
        if libc::getsockname(fd, (&raw mut sa).cast(), &mut sl) != 0
            || sa.ss_family as i32 != libc::AF_INET
        {
            return String::new();
        }
        let v4 = &*(&raw const sa).cast::<libc::sockaddr_in>();
        std::net::Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr)).to_string()
    }
}

/// The local port of a UDP descriptor (0 when unbound).
fn udp_port(fd: i32) -> u16 {
    unsafe {
        let mut sa: libc::sockaddr_storage = std::mem::zeroed();
        let mut sl = size_of::<libc::sockaddr_storage>() as u32;
        if libc::getsockname(fd, (&raw mut sa).cast(), &mut sl) != 0 {
            return 0;
        }
        // `sin_port` and `sin6_port` are at the same offset
        u16::from_be((*(&raw const sa).cast::<libc::sockaddr_in>()).sin_port)
    }
}

fn udp_fds() -> Vec<i32> {
    (0..4096)
        .filter(|&fd| unsafe {
            let (mut ty, mut len) = (0i32, 4u32);
            let ok = libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                (&raw mut ty).cast(),
                &mut len,
            ) == 0;
            let mut sa: libc::sockaddr_storage = std::mem::zeroed();
            let mut sl = size_of::<libc::sockaddr_storage>() as u32;
            ok && ty == libc::SOCK_DGRAM
                && libc::getsockname(fd, (&raw mut sa).cast(), &mut sl) == 0
                && matches!(sa.ss_family as i32, libc::AF_INET | libc::AF_INET6)
        })
        .collect()
}

/// Raise `fd`'s receive buffer towards `want` (halving until the kernel takes it; 0 = leave it)
/// and return the size in effect.
fn raise_rcvbuf(fd: i32, want: u32) -> i32 {
    let mut v = want as i32;
    unsafe {
        while v >= 1 << 18
            && libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_RCVBUF,
                (&raw const v).cast(),
                4,
            ) != 0
        {
            v /= 2;
        }
        let (mut got, mut len) = (0i32, 4u32);
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&raw mut got).cast(),
            &mut len,
        );
        got
    }
}

/// If this process is still alive after `after`, sample it (the stacks go to
/// `/tmp/se-live-hang-<pid>.txt`) and leave at once, which takes its window and displays with it.
fn guard_exit(what: &'static str, after: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(after);
        eprintln!("[live] guard: still alive {after:?} after {what}; sampling and exiting");
        let pid = std::process::id();
        let _ = std::process::Command::new("/usr/bin/sample")
            .args([
                &pid.to_string(),
                "1",
                "-f",
                &format!("/tmp/se-live-hang-{pid}.txt"),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        unsafe { libc::_exit(86) }
    });
}

/// The churn control: main-thread waits block instead of pumping.
static BLOCK_MAIN: AtomicBool = AtomicBool::new(false);

/// Sleep on the main thread with its run loop running (`pump_main_run_loop`), as the desktop app
/// and `serve` do all the time.
fn pause(d: Duration) {
    if BLOCK_MAIN.load(Ordering::Relaxed) {
        std::thread::sleep(d);
        return;
    }
    let t = Instant::now();
    while t.elapsed() < d {
        let left = (d - t.elapsed().min(d)).as_secs_f64();
        crate::macos_utils::virtual_display::pump_main_run_loop(left.min(0.02));
    }
}

/// `fut`, with the main run loop pumped beside it: `block_on` would otherwise park the main thread
/// for the whole join.
async fn pumped<T>(fut: impl std::future::Future<Output = T>) -> T {
    if BLOCK_MAIN.load(Ordering::Relaxed) {
        return fut.await;
    }
    tokio::pin!(fut);
    loop {
        tokio::select! {
            v = &mut fut => return v,
            _ = tokio::time::sleep(Duration::from_millis(10)) => {
                crate::macos_utils::virtual_display::pump_main_run_loop(0.005);
            }
        }
    }
}
