//! `ScreenExtend bench`: a self-contained encoder benchmark (built with `--features bench`).
//!
//! Intercepted in `cli::fast_path()` before the Tauri builder, so nothing here needs a Tauri
//! runtime, a window or AppKit. Stages: `env` (machine facts), `probe` (what every encoder
//! session is and which properties it takes), `lab` (the plan's runs, each a real-time replay of
//! a clip through the production `Encoder`). Every unit of work runs in a child process of this
//! same executable (`--child <kind> <spec.json>`) under a watchdog, so a crash or a hang is a
//! recorded result and the suite carries on. One file comes back: `results-<host>.json`.

#[cfg(target_os = "macos")]
mod cf;
#[cfg(target_os = "macos")]
mod churn;
#[cfg(target_os = "macos")]
mod clip;
#[cfg(target_os = "macos")]
mod env;
#[cfg(target_os = "macos")]
mod live;
#[cfg(target_os = "macos")]
mod live_rx;
#[cfg(target_os = "macos")]
mod live_window;
#[cfg(target_os = "macos")]
mod metrics;
#[cfg(target_os = "macos")]
mod pipe;
#[cfg(target_os = "macos")]
mod plan;
#[cfg(target_os = "macos")]
mod probe;
#[cfg(target_os = "macos")]
mod psnr;
#[cfg(target_os = "macos")]
mod replay;
#[cfg(target_os = "macos")]
mod runner;
#[cfg(target_os = "macos")]
mod series;
#[cfg(target_os = "macos")]
mod summary;
#[cfg(target_os = "macos")]
pub(crate) mod synth_source;

/// Whether these `bench` arguments are a unit of work that must run inside the Tauri runtime
/// (the `live` child, which needs `platform::setup` and the `AppState` it builds). `fast_path`
/// lets those through to the builder instead of handling them before it.
pub fn wants_tauri(args: &[String]) -> bool {
    cfg!(target_os = "macos")
        && args.first().map(String::as_str) == Some("--child")
        && args.get(1).map(String::as_str) == Some("live")
}

/// The `live` child, reached from `cli::dispatch` inside Tauri's `setup`.
#[cfg(target_os = "macos")]
pub fn live_child(app: &tauri::AppHandle) -> ! {
    live::child_main(app)
}

fn exit(code: i32) -> ! {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}

const HELP: &str = "\
ScreenExtend bench — VideoToolbox encoder benchmark

USAGE
  ScreenExtend bench [options]

  (no options)           run the embedded default plan: env, probe, lab
  --quick                the embedded quick plan (about 12 minutes)
  --plan <file|name>     a plan JSON file, or a builtin: quick | default | anchors | full
  --recording <file>     enable workloads of kind \"recording\" (a SE_RAW_RECORD take)
  --out <dir>            output directory (default ./screenextend-bench-<host>-<stamp>/)
  --resume <dir>         continue a run directory, skipping what runs.jsonl already holds
  --stages env,probe,lab which stages to run (groups with \"stage\": \"pipe\" or \"live\" run in
                         the lab stage; a live run creates a virtual display and needs Screen
                         Recording already granted — it is skipped, never prompted, otherwise)
  --only <substring>     only runs whose id contains this
  --list                 print the expanded run list and the estimated duration; run nothing
  --per-frame            write a per-frame CSV for every run into <out>/logs/
  --keep-clips           keep rendered synthetic clips in <out>/cache/

The last line printed is the absolute path of the results file to send back.
";

pub fn main(args: &[String]) -> ! {
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        eprintln!("error: the bench harness is only implemented for macOS");
        exit(2)
    }
    #[cfg(target_os = "macos")]
    {
        exit(mac_main(args))
    }
}

#[cfg(target_os = "macos")]
fn mac_main(args: &[String]) -> i32 {
    use std::path::PathBuf;

    if args.first().map(String::as_str) == Some("--child") {
        return child(&args[1..]);
    }
    let mut o = runner::Opts {
        plan: None,
        quick: false,
        recording: None,
        out: None,
        resume: None,
        stages: Vec::new(),
        only: None,
        list: false,
        per_frame: false,
        keep_clips: false,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |name: &str| -> Result<String, i32> {
            it.next().cloned().ok_or_else(|| {
                eprintln!("error: {name} needs a value");
                2
            })
        };
        let r: Result<(), i32> = (|| {
            match a.as_str() {
                "-h" | "--help" | "help" => {
                    print!("{HELP}");
                    return Err(0);
                }
                "--quick" => o.quick = true,
                "--plan" => o.plan = Some(val("--plan")?),
                "--recording" => o.recording = Some(PathBuf::from(val("--recording")?)),
                "--out" => o.out = Some(PathBuf::from(val("--out")?)),
                "--resume" => o.resume = Some(PathBuf::from(val("--resume")?)),
                "--stages" => {
                    o.stages = val("--stages")?
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                }
                "--only" => o.only = Some(val("--only")?),
                "--list" => o.list = true,
                "--per-frame" => o.per_frame = true,
                "--keep-clips" => o.keep_clips = true,
                other => {
                    eprintln!("error: unknown argument '{other}' (see --help)");
                    return Err(2);
                }
            }
            Ok(())
        })();
        if let Err(code) = r {
            return code;
        }
    }
    for s in &o.stages {
        if !["env", "probe", "lab"].contains(&s.as_str()) {
            eprintln!("error: unknown stage '{s}'");
            return 2;
        }
    }
    runner::run(o)
}

/// One unit of work in a fresh process. The result goes to the spec's `result_path`; stdout and
/// stderr are the runner's log file for this unit.
#[cfg(target_os = "macos")]
fn child(args: &[String]) -> i32 {
    let (Some(kind), Some(arg)) = (args.first(), args.get(1)) else {
        eprintln!("error: --child <kind> <spec.json|inline json>");
        return 2;
    };
    let text = if arg.trim_start().starts_with('{') {
        arg.clone()
    } else {
        match std::fs::read_to_string(arg) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: read {arg}: {e}");
                return 2;
            }
        }
    };
    // The encoder's own log lines go to this child's log file.
    crate::logbus::set_verbose(true);
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: child spec: {e}");
            return 2;
        }
    };
    let result_path = parsed["result_path"].as_str().map(str::to_string);
    let result = match kind.as_str() {
        "env" => {
            let dir = parsed["out_dir"].as_str().unwrap_or(".");
            env::collect(std::path::Path::new(dir), &parsed["parent_env"])
        }
        "probe" => match serde_json::from_value::<probe::ProbeSpec>(parsed) {
            Ok(s) => probe::run(&s),
            Err(e) => {
                eprintln!("error: probe spec: {e}");
                return 2;
            }
        },
        "run" | "pipe" | "churn" => match serde_json::from_value::<plan::RunSpec>(parsed) {
            Ok(s) => match kind.as_str() {
                "pipe" => pipe::run(&s),
                "churn" => churn::run(&s),
                _ => replay::run(&s),
            },
            Err(e) => {
                eprintln!("error: run spec: {e}");
                return 2;
            }
        },
        other => {
            eprintln!("error: unknown child kind '{other}'");
            return 2;
        }
    };
    // Let the log writer echo the last lines before the process goes.
    std::thread::sleep(std::time::Duration::from_millis(100));
    match result_path {
        Some(p) => {
            let tmp = format!("{p}.tmp");
            let text = serde_json::to_string(&result).unwrap_or_default();
            if std::fs::write(&tmp, text)
                .and_then(|_| std::fs::rename(&tmp, &p))
                .is_err()
            {
                eprintln!("error: cannot write {p}");
                return 1;
            }
        }
        None => println!("{result}"),
    }
    0
}
