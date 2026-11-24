//! The orchestrator: output directory, stages, one child process per unit of work with a
//! watchdog, the JSONL ledger, resume, and the single results file.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};

use super::cf;
use super::clip;
use super::env::CLEARED_VARS;
use super::plan::{Plan, ProbeRow, RunSpec};
use super::probe::ProbeSpec;

pub struct Opts {
    pub plan: Option<String>,
    pub quick: bool,
    pub recording: Option<PathBuf>,
    pub out: Option<PathBuf>,
    pub resume: Option<PathBuf>,
    pub stages: Vec<String>,
    pub only: Option<String>,
    pub list: bool,
    pub per_frame: bool,
    pub keep_clips: bool,
}

struct ChildOutcome {
    outcome: &'static str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    wall_s: f64,
    result: Option<Json>,
    tail: Vec<String>,
}

fn load_plan(o: &Opts) -> Result<(Plan, String), String> {
    let (text, label) = if let Some(p) = &o.plan {
        match Plan::builtin(p) {
            Some(t) => (t.to_string(), format!("builtin:{p}")),
            None => (
                fs::read_to_string(p).map_err(|e| format!("read plan {p}: {e}"))?,
                p.clone(),
            ),
        }
    } else if o.quick {
        (super::plan::QUICK.to_string(), "builtin:quick".into())
    } else if let Some(saved) = o
        .resume
        .as_ref()
        .map(|d| d.join("plan.json"))
        .filter(|p| p.exists())
    {
        (
            fs::read_to_string(&saved).map_err(|e| e.to_string())?,
            saved.display().to_string(),
        )
    } else {
        (super::plan::DEFAULT.to_string(), "builtin:default".into())
    };
    Ok((Plan::parse(&text)?, label))
}

fn default_catalogue() -> super::plan::Catalogue {
    Plan::parse(super::plan::DEFAULT)
        .ok()
        .and_then(|p| p.catalogue)
        .unwrap_or_default()
}

/// Seconds a run should take, for the watchdog and `--list`.
fn estimate_s(r: &RunSpec, rec: Option<&clip::ClipInfo>) -> f64 {
    let w = &r.workload;
    if let Some(c) = r.live.as_ref().and_then(|l| l.churn.as_ref()) {
        // Each join: the POST and first media (up to ~10 s on a bad one), the hold, the leave
        // (up to ~6 s), the gap; plus setup.
        let per = c.hold_s.unwrap_or(3.0) + c.gap_ms.unwrap_or(750) as f64 / 1e3 + 4.0;
        return c.joins as f64 * (per + 4.0) + c.first_wait_ms.unwrap_or(0) as f64 / 1e3 + 20.0;
    }
    if let Some(l) = &r.live {
        // Setup + join (~3 s), the warm-up, the measured span, drain and teardown (~6 s).
        return l.warmup_s() + l.seconds() + 10.0;
    }
    if let Some(c) = &r.churn {
        // Each cycle: the frames at the workload's rate plus a create and a teardown.
        return c.cycles as f64 * (c.frames as f64 / w.fps.max(1) as f64 + 0.5) + 6.0;
    }
    let (clip_s, frames) = if w.kind == "recording" {
        match rec {
            Some(i) => {
                let n = w.max_frames.map_or(i.frames, |m| m.min(i.frames));
                (i.times[n.max(1) - 1] as f64 / 1e9, n)
            }
            None => (30.0, 1800),
        }
    } else {
        let n = (w.seconds * w.fps as f64) as usize;
        let n = w.max_frames.map_or(n, |m| m.min(n));
        (n as f64 / w.fps.max(1) as f64, n)
    };
    let speed = if w.speed > 0.0 { w.speed } else { 1.0 };
    let timed = clip_s * r.loops as f64 / speed;
    let psnr = if r.psnr {
        frames as f64 * r.loops as f64 * 0.03
    } else {
        0.0
    };
    // Every extra session is another create and another collector, not more wall time.
    timed + psnr + 6.0 + 0.5 * r.sessions.saturating_sub(1) as f64
}

/// The watchdog: the estimate, plus 10 % of it (a long looped soak drifts by more than a fixed
/// slack would cover), plus the plan's slack.
fn watchdog(estimate_s: f64, slack: Duration) -> Duration {
    Duration::from_secs_f64(estimate_s * 1.1) + slack
}

/// The child kind for a run.
fn child_kind(r: &RunSpec) -> &'static str {
    if r.churn.is_some() {
        "churn"
    } else if r.stage == "live" {
        "live"
    } else if r.stage == "pipe" {
        "pipe"
    } else {
        "run"
    }
}

/// The environment a run's child gets from its group: the deny hook (which also needs
/// `SE_TEST_HOOKS=1`) and the tuning switches. Everything in `CLEARED_VARS` is removed first.
fn child_env(r: &RunSpec) -> Vec<(&'static str, String)> {
    let mut env = Vec::new();
    if !r.deny.is_empty() {
        env.push(("SE_VT_DENY", r.deny.join(",")));
        env.push(("SE_TEST_HOOKS", "1".to_string()));
    }
    if !r.tune_off.is_empty() {
        env.push(("SE_TUNE_OFF", r.tune_off.join(",")));
    }
    if !r.tune_on.is_empty() {
        env.push(("SE_TUNE_ON", r.tune_on.join(",")));
        // A test-hook candidate is honoured only with the gate (pipeline.rs).
        if r.tune_on.iter().any(|t| t == "hold_push_start_immediate")
            && !env.iter().any(|(k, _)| *k == "SE_TEST_HOOKS")
        {
            env.push(("SE_TEST_HOOKS", "1".to_string()));
        }
    }
    if let Some(l) = &r.live {
        env.extend(l.capture.env());
        if l.simulate_screen_recording_denied {
            env.push((super::live::DENY_ENV, "1".to_string()));
            if !env.iter().any(|(k, _)| *k == "SE_TEST_HOOKS") {
                env.push(("SE_TEST_HOOKS", "1".to_string()));
            }
        }
    }
    env
}

/// The live child's private state directory and, for a recipe run, the recipe file every session
/// of that child is built from (`SE_VT_RECIPE`).
fn live_env(r: &RunSpec, out: &Path) -> Vec<(&'static str, String)> {
    let mut env = Vec::new();
    let data = out.join(format!("work/{}.data", r.id));
    let _ = fs::remove_dir_all(&data);
    let _ = fs::create_dir_all(&data);
    env.push(("SCREENEXTEND_DATA_DIR", data.display().to_string()));
    if r.recipe.production.is_none() {
        let rp = out.join(format!("work/{}.recipe.json", r.id));
        if fs::write(
            &rp,
            serde_json::to_string(&r.recipe.recipe).unwrap_or_default(),
        )
        .is_ok()
        {
            env.push(("SE_VT_RECIPE", rp.display().to_string()));
            env.push(("SE_TEST_HOOKS", "1".to_string())); // SE_VT_RECIPE needs it (encoder.rs)
        }
    }
    env
}

fn read_tail(path: &Path, lines: usize) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(16 * 1024)));
    let mut s = String::new();
    let _ = f.read_to_string(&mut s);
    let all: Vec<String> = s.lines().map(str::to_string).collect();
    all[all.len().saturating_sub(lines)..].to_vec()
}

fn spawn_child(
    kind: &str,
    spec_path: &Path,
    result_path: &Path,
    log: &Path,
    timeout: Duration,
    env: &[(&'static str, String)],
) -> ChildOutcome {
    let started = Instant::now();
    let _ = fs::remove_file(result_path);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ScreenExtend"));
    let logf = match File::create(log) {
        Ok(f) => f,
        Err(e) => {
            return ChildOutcome {
                outcome: "error",
                exit_code: None,
                signal: None,
                wall_s: 0.0,
                result: None,
                tail: vec![format!("log: {e}")],
            }
        }
    };
    let mut cmd = Command::new(exe);
    cmd.arg("bench").arg("--child").arg(kind).arg(spec_path);
    for v in CLEARED_VARS {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(logf.try_clone().expect("dup log")))
        .stderr(Stdio::from(logf));
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ChildOutcome {
                outcome: "error",
                exit_code: None,
                signal: None,
                wall_s: 0.0,
                result: None,
                tail: vec![format!("spawn: {e}")],
            }
        }
    };
    let mut hung = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {}
            Err(_) => break None,
        }
        if started.elapsed() > timeout {
            hung = true;
            let _ = child.kill();
            break child.wait().ok();
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let wall_s = started.elapsed().as_secs_f64();
    let result = fs::read_to_string(result_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Json>(&t).ok());
    let (exit_code, signal) = status.map_or((None, None), |s| (s.code(), s.signal()));
    let outcome = if hung {
        "hang"
    } else if signal.is_some() {
        "crash"
    } else if exit_code == Some(0) && result.is_some() {
        "ok"
    } else if exit_code == Some(101) {
        "crash" // a Rust panic
    } else {
        "error"
    };
    let tail = if outcome == "ok" {
        Vec::new()
    } else {
        read_tail(log, 25)
    };
    ChildOutcome {
        outcome,
        exit_code,
        signal,
        wall_s,
        result,
        tail,
    }
}

fn append_line(path: &Path, v: &Json) -> std::io::Result<()> {
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    let mut line = serde_json::to_string(v).unwrap_or_default();
    line.push('\n');
    f.write_all(line.as_bytes())?;
    f.sync_all()
}

fn read_lines(path: &Path) -> Vec<Json> {
    let Ok(f) = File::open(path) else {
        return Vec::new();
    };
    BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn write_json(path: &Path, v: &Json, pretty: bool) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let text = if pretty {
        serde_json::to_string_pretty(v)
    } else {
        serde_json::to_string(v)
    }
    .unwrap_or_default();
    fs::write(&tmp, text)?;
    File::open(&tmp)?.sync_all()?;
    fs::rename(&tmp, path)
}

fn fmt_dur(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{}h{:02}m{:02}s", s / 3600, (s / 60) % 60, s % 60)
}

pub fn run(o: Opts) -> i32 {
    let (plan, plan_label) = match load_plan(&o) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    let mut runs = match plan.expand() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 2;
        }
    };
    if let Some(sub) = &o.only {
        runs.retain(|r| r.id.contains(sub.as_str()));
    }
    if o.per_frame {
        for r in &mut runs {
            r.per_frame = true;
        }
    }
    let rec_info = match &o.recording {
        Some(p) => match clip::scan(p) {
            Ok(i) => Some(i),
            Err(e) => {
                eprintln!("error: --recording {}: {e}", p.display());
                return 2;
            }
        },
        None => None,
    };
    let stages: Vec<String> = if o.stages.is_empty() {
        vec!["env".into(), "probe".into(), "lab".into()]
    } else {
        o.stages.clone()
    };
    let probe_rows = plan
        .probe_rows
        .clone()
        .unwrap_or_else(ProbeRow::default_rows);
    let cooldown = plan.order.cooldown_ms as f64 / 1e3;

    if o.list {
        let mut total = 0.0;
        if stages.iter().any(|s| s == "probe") {
            println!("probe: {} rows (~{} each)", probe_rows.len(), fmt_dur(8.0));
            total += probe_rows.len() as f64 * 8.0;
        }
        if stages.iter().any(|s| s == "lab") {
            for r in &runs {
                let skip =
                    r.workload.kind == "recording" && r.churn.is_none() && rec_info.is_none();
                let e = estimate_s(r, rec_info.as_ref());
                if !skip {
                    total += e + cooldown;
                }
                println!(
                    "{:<64} {:>8}{}",
                    r.id,
                    fmt_dur(e),
                    if skip {
                        "  (skipped: needs --recording)"
                    } else {
                        ""
                    }
                );
            }
        }
        println!("{} runs, estimated {}", runs.len(), fmt_dur(total));
        return 0;
    }

    let host = cf::hostname();
    let out = o
        .resume
        .clone()
        .or_else(|| o.out.clone())
        .unwrap_or_else(|| {
            PathBuf::from(format!("screenextend-bench-{host}-{}", cf::local_stamp()))
        });
    for d in ["", "logs", "work", "cache"] {
        if let Err(e) = fs::create_dir_all(out.join(d)) {
            eprintln!("error: create {}: {e}", out.join(d).display());
            return 2;
        }
    }
    let out = fs::canonicalize(&out).unwrap_or(out);
    let _ = write_json(
        &out.join("plan.json"),
        &serde_json::to_value(&plan).unwrap_or(Json::Null),
        true,
    );
    let runs_path = out.join("runs.jsonl");
    let probe_path = out.join("probe.jsonl");
    let slack = Duration::from_secs(plan.order.watchdog_slack_s);
    println!("[bench] output: {}", out.display());
    println!("[bench] plan: {plan_label} ({} runs)", runs.len());

    let parent_env: serde_json::Map<String, Json> = CLEARED_VARS
        .iter()
        .filter_map(|v| std::env::var(v).ok().map(|val| (v.to_string(), json!(val))))
        .collect();

    // ---- env ------------------------------------------------------------------------------
    let env_path = out.join("env.json");
    if stages.iter().any(|s| s == "env") && !(o.resume.is_some() && env_path.exists()) {
        let spec = json!({ "result_path": out.join("work/env.result.json"), "out_dir": out, "parent_env": parent_env });
        let sp = out.join("work/env.json");
        let _ = write_json(&sp, &spec, false);
        let c = spawn_child(
            "env",
            &sp,
            &out.join("work/env.result.json"),
            &out.join("logs/env.log"),
            Duration::from_secs(120),
            &[],
        );
        let v = c
            .result
            .unwrap_or_else(|| json!({ "outcome": c.outcome, "tail": c.tail }));
        let _ = write_json(&env_path, &v, true);
        println!(
            "[bench] env: {} {} {} arch={} translated={}",
            v["hw"]["model"].as_str().unwrap_or("?"),
            v["hw"]["cpu_brand"].as_str().unwrap_or("?"),
            v["os"]["product_version"].as_str().unwrap_or("?"),
            v["process"]["arch"].as_str().unwrap_or("?"),
            v["process"]["translated"]
        );
    }

    // ---- probe ----------------------------------------------------------------------------
    if stages.iter().any(|s| s == "probe") {
        let done: HashSet<String> = read_lines(&probe_path)
            .iter()
            .filter_map(|v| v["id"].as_str().map(str::to_string))
            .collect();
        let catalogue = plan.catalogue.clone().unwrap_or_else(default_catalogue);
        for row in &probe_rows {
            let id = row.id();
            if done.contains(&id) {
                continue;
            }
            let rp = out.join(format!("work/probe-{id}.result.json"));
            let spec = ProbeSpec {
                row: row.clone(),
                catalogue: catalogue.clone(),
                result_path: rp.display().to_string(),
            };
            let sp = out.join(format!("work/probe-{id}.json"));
            let _ = write_json(
                &sp,
                &serde_json::to_value(&spec).unwrap_or(Json::Null),
                false,
            );
            let c = spawn_child(
                "probe",
                &sp,
                &rp,
                &out.join(format!("logs/probe-{id}.log")),
                Duration::from_secs(300),
                &[],
            );
            let mut v = c.result.unwrap_or_else(|| json!({ "id": id, "row": row }));
            v["outcome"] = json!(c.outcome);
            if c.outcome != "ok" {
                v["exit_code"] = json!(c.exit_code);
                v["signal"] = json!(c.signal);
                v["stderr_tail"] = json!(c.tail);
            }
            let _ = append_line(&probe_path, &v);
            println!(
                "[bench] probe {id:<32} {} create={} encoder={} keys={} ({:.1}s)",
                c.outcome,
                v["session_create_status"],
                v["identity"]["encoder_id"].as_str().unwrap_or("-"),
                v["supported_key_count"],
                c.wall_s
            );
        }
    }

    // ---- lab ------------------------------------------------------------------------------
    if stages.iter().any(|s| s == "lab") {
        let done: HashSet<String> = read_lines(&runs_path)
            .iter()
            .filter(|v| v["outcome"].as_str() != Some("skipped"))
            .filter_map(|v| v["id"].as_str().map(str::to_string))
            .collect();
        // Last run (by position) that needs each rendered clip, so it can be deleted after.
        let mut last_use: HashMap<String, usize> = HashMap::new();
        for (i, r) in runs.iter().enumerate() {
            if r.workload.kind == "synthetic" && r.churn.is_none() && !done.contains(&r.id) {
                last_use.insert(r.workload.clip_key(), i);
            }
        }
        let todo = runs.iter().filter(|r| !done.contains(&r.id)).count();
        let est: f64 = runs
            .iter()
            .filter(|r| !done.contains(&r.id))
            .map(|r| estimate_s(r, rec_info.as_ref()) + cooldown)
            .sum();
        println!("[bench] lab: {todo} runs to do, estimated {}", fmt_dur(est));
        let lab_start = Instant::now();
        let mut n = 0usize;
        for (i, r) in runs.iter().enumerate() {
            if done.contains(&r.id) {
                continue;
            }
            n += 1;
            let echo = json!({
                "group": r.group, "group_name": r.group_name, "repeat": r.repeat,
                "recipe": r.recipe, "workload": r.workload, "loops": r.loops,
                "psnr": r.psnr, "events": r.events, "stage": r.stage,
                "sessions": r.sessions, "deny": r.deny, "tune_on": r.tune_on,
                "tune_off": r.tune_off, "churn": r.churn, "ltr": r.ltr, "live": r.live,
            });
            let skip = |reason: String| {
                let v = json!({ "id": r.id, "outcome": "skipped", "reason": reason, "spec": echo });
                let _ = append_line(&runs_path, &v);
                println!(
                    "[bench] {n}/{todo} {} skipped: {}",
                    r.id,
                    v["reason"].as_str().unwrap_or("")
                );
            };
            let clip_path: PathBuf = match r.workload.kind.as_str() {
                // A churn run paints its own frames; so does a live run's window.
                _ if r.churn.is_some() => PathBuf::new(),
                "synthetic" if r.live.is_some() => PathBuf::new(),
                "recording" => match &o.recording {
                    Some(p) => p.clone(),
                    None => {
                        skip("recording workload and no --recording given".into());
                        continue;
                    }
                },
                "synthetic" => {
                    let key = r.workload.clip_key();
                    let p = out.join("cache").join(format!("{key}.raw"));
                    if !p.exists() {
                        let need = clip::synthetic_bytes(&r.workload);
                        if need > clip::CLIP_BUDGET_BYTES {
                            skip(format!("clip would be {need} bytes, over the {} byte budget; lower seconds or size", clip::CLIP_BUDGET_BYTES));
                            continue;
                        }
                        // One clip on disk at a time keeps the cache inside the budget.
                        if let Ok(rd) = fs::read_dir(out.join("cache")) {
                            for e in rd.flatten() {
                                let _ = fs::remove_file(e.path());
                            }
                        }
                        let t = Instant::now();
                        println!(
                            "[bench] rendering clip {} ({:.2} GB)…",
                            r.workload.id,
                            need as f64 / 1e9
                        );
                        if let Err(e) = clip::render(&r.workload, &p) {
                            skip(format!("render failed: {e}"));
                            continue;
                        }
                        println!("[bench] rendered in {:.1}s", t.elapsed().as_secs_f64());
                    }
                    p
                }
                other => {
                    skip(format!("unknown workload kind '{other}'"));
                    continue;
                }
            };
            let mut spec = r.clone();
            let rp = out.join(format!("work/{}.result.json", r.id));
            spec.clip_path = (r.churn.is_none() && !clip_path.as_os_str().is_empty())
                .then(|| clip_path.display().to_string());
            spec.result_path = Some(rp.display().to_string());
            if spec.per_frame {
                spec.per_frame_path = Some(
                    out.join(format!("logs/{}.frames.csv", r.id))
                        .display()
                        .to_string(),
                );
            }
            let sp = out.join(format!("work/{}.json", r.id));
            let _ = write_json(
                &sp,
                &serde_json::to_value(&spec).unwrap_or(Json::Null),
                false,
            );
            let info_for_est = if r.workload.kind == "recording" {
                rec_info.as_ref()
            } else {
                None
            };
            let timeout = watchdog(estimate_s(r, info_for_est), slack);
            let mut env = child_env(r);
            if r.live.is_some() {
                env.extend(live_env(r, &out));
            }
            let c = spawn_child(
                child_kind(r),
                &sp,
                &rp,
                &out.join(format!("logs/{}.log", r.id)),
                timeout,
                &env,
            );
            let mut v = c.result.clone().unwrap_or_else(|| json!({ "id": r.id }));
            v["outcome"] = json!(c.outcome);
            // A live precondition that did not hold is a recorded skip, retried on --resume.
            if c.outcome == "ok" && v["result"] == "skipped" {
                v["outcome"] = json!("skipped");
                v["reason"] = v["skip_reason"].clone();
            }
            v["child_wall_s"] = json!(c.wall_s);
            v["watchdog_s"] = json!(timeout.as_secs_f64());
            v["spec"] = echo;
            if c.outcome != "ok" {
                v["exit_code"] = json!(c.exit_code);
                v["signal"] = json!(c.signal);
                v["stderr_tail"] = json!(c.tail);
                v["valid"] = json!(false);
            }
            let _ = append_line(&runs_path, &v);
            if r.live.is_some() {
                let f = &v["frames"];
                let rx = &v["receiver_ms"];
                println!(
                    "[bench] {n}/{todo} {:<56} {} valid={} {} capture={} presented={} captured={} received={} decoded={} c2arrival p50={} p99={} decode p50={} c2decoded p50={} p99={} {:.1} Mbps ({:.0}s){}",
                    r.id,
                    v["outcome"].as_str().unwrap_or("?"),
                    v["valid"],
                    v["workload_mode"].as_str().unwrap_or("-"),
                    v["capture_backend"].as_str().unwrap_or("-"),
                    f["presented_in_window"],
                    f["captured_fresh_in_window"],
                    f["received"],
                    f["decoded"],
                    rx["capture_to_arrival"]["p50"],
                    rx["capture_to_arrival"]["p99"],
                    rx["decode"]["p50"],
                    rx["capture_to_decoded"]["p50"],
                    rx["capture_to_decoded"]["p99"],
                    v["avg_mbps"].as_f64().unwrap_or(0.0),
                    c.wall_s,
                    v["reason"].as_str().map(|s| format!(" skipped: {s}")).unwrap_or_default(),
                );
                if cooldown > 0.0 {
                    std::thread::sleep(Duration::from_secs_f64(cooldown));
                }
                continue;
            }
            println!(
                "[bench] {n}/{todo} {:<56} {} valid={} drop={}/{} lat p50={} p99={} {:.1} Mbps level={} late={}/{}enc ({:.0}s, {} elapsed)",
                r.id,
                c.outcome,
                v["valid"],
                v["dropped"],
                v["submitted"],
                v["latency_ms"]["p50"],
                v["latency_ms"]["p99"],
                v["avg_mbps"].as_f64().unwrap_or(0.0),
                v["sps"]["level_idc"].as_u64().or(v["sps"]["general_level_idc"].as_u64()).map_or("-".into(), |l| l.to_string()),
                v["pacing"]["late_over_2ms_harness"],
                v["pacing"]["late_over_2ms_encoder_blocked"],
                c.wall_s,
                fmt_dur(lab_start.elapsed().as_secs_f64()),
            );
            if r.workload.kind == "synthetic"
                && r.churn.is_none()
                && !o.keep_clips
                && last_use.get(&r.workload.clip_key()) == Some(&i)
            {
                let _ = fs::remove_file(&clip_path);
            }
            if cooldown > 0.0 {
                std::thread::sleep(Duration::from_secs_f64(cooldown));
            }
        }
        if !o.keep_clips {
            if let Ok(rd) = fs::read_dir(out.join("cache")) {
                for e in rd.flatten() {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    // ---- results --------------------------------------------------------------------------
    let env = fs::read_to_string(&env_path)
        .ok()
        .and_then(|t| serde_json::from_str::<Json>(&t).ok());
    let probe = read_lines(&probe_path);
    // A resumed run that was first skipped appears twice; the last line is the truth.
    let mut all_runs: Vec<Json> = Vec::new();
    for v in read_lines(&runs_path) {
        let id = v["id"].clone();
        all_runs.retain(|x| x["id"] != id);
        all_runs.push(v);
    }
    let results = json!({
        "bench_format": 1,
        "host": host,
        "written_at": cf::local_stamp(),
        "plan_source": plan_label,
        "plan": plan,
        "args": std::env::args().skip(1).collect::<Vec<_>>(),
        "env": env,
        "probe": probe,
        "runs": all_runs,
    });
    let results_path = out.join(format!("results-{host}.json"));
    if let Err(e) = write_json(&results_path, &results, false) {
        eprintln!("error: writing results: {e}");
        return 1;
    }
    let summary = super::summary::render(&results);
    let _ = fs::write(out.join("SUMMARY.txt"), &summary);
    print!("{summary}");
    println!("{}", results_path.display());
    0
}
