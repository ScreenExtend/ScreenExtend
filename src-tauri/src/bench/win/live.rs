//! `live` on Windows: a real virtual display, real capture, real WHEP, a decoding receiver.
//!
//! STUB. Every run returns `result: "skipped"`. The child already runs where the real one must
//! (inside Tauri's `setup`, because a virtual display needs `platform::setup`), so only `run`
//! needs replacing.

use serde_json::Value as Json;

use super::super::plan::RunSpec;

/// The macOS Screen Recording test hook. Named here because the shared runner clears and sets it;
/// it means nothing on Windows.
pub const DENY_ENV: &str = "SE_BENCH_DENY_SCREEN_RECORDING";

/// `bench --child live <spec>`: called from `cli::dispatch`, on the main thread inside `setup`.
pub fn child_main(app: &tauri::AppHandle) -> ! {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(arg) = args.get(3) else {
        eprintln!("error: bench --child live <spec.json>");
        super::super::exit(2)
    };
    crate::logbus::set_verbose(true);
    let text = match std::fs::read_to_string(arg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: read {arg}: {e}");
            super::super::exit(2)
        }
    };
    let spec: RunSpec = match serde_json::from_str(&text) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: live spec: {e}");
            super::super::exit(2)
        }
    };
    let result = run(app, &spec);
    match &spec.result_path {
        Some(p) => {
            let tmp = format!("{p}.tmp");
            let text = serde_json::to_string(&result).unwrap_or_default();
            if std::fs::write(&tmp, text)
                .and_then(|_| std::fs::rename(&tmp, p))
                .is_err()
            {
                eprintln!("error: cannot write {p}");
                super::super::exit(1)
            }
        }
        None => println!("{result}"),
    }
    super::super::exit(0)
}

fn run(_app: &tauri::AppHandle, spec: &RunSpec) -> Json {
    super::not_implemented(spec, "live")
}
