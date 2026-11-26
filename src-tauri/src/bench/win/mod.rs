//! The Windows half of `ScreenExtend bench`: one module per stage, bound by `bench/mod.rs` to
//! the names the shared runner dispatches on. `live` and `churn` are stubs that report `skipped`.

pub mod avc;
pub mod cache;
pub mod churn;
pub mod clipfmt;
pub mod env;
pub mod feed;
pub mod hooks;
pub mod lab;
pub mod live;
pub mod pipe;
pub mod probe;
pub mod recipe;
pub mod record;
pub mod sys;

use serde_json::{json, Value as Json};

use super::plan::RunSpec;

pub const HELP: &str = "\
ScreenExtend bench: Windows encoder and pipeline benchmark

USAGE
  ScreenExtend bench [options]
  ScreenExtend bench record [record options]

  (no options)           run the embedded default plan: env, probe, lab
  --quick                the embedded quick plan
  --plan <file|name>     a plan JSON file, or a builtin: quick | default | anchors | full
  --recording <file>     enable workloads of kind \"recording\" (a `bench record` take, or a
                         macOS SE_RAW_RECORD take)
  --out <dir>            output directory (default ./screenextend-bench-<host>-<stamp>/)
  --resume <dir>         continue a run directory, skipping what runs.jsonl already holds
  --stages env,probe,lab which stages to run
  --only <substring>     only runs whose id contains this
  --list                 print the expanded run list and the estimated duration; run nothing
  --per-frame            write a per-frame CSV for every run into <out>/logs/
  --keep-clips           keep rendered synthetic clips in <out>/cache/

  Stages: env, probe, lab and pipe; live and churn runs are recorded as skipped.

RECORD (capture of one monitor, BGRA8 exactly as the encoder would receive it)
  --monitor primary|<n>  the monitor to capture (default primary; n is 1-based)
  --seconds N            length of the take (default 10)
  --fps N                frame-rate cap, applied as WGC MinUpdateInterval (default 120)
  --out <file>           output file (default ./recordings/take-<stamp>.seraw)
  --countdown N          seconds of countdown before recording starts (default 5)
  --compress-threads N   LZ4 worker threads (default 4). Every frame is compressed on its own,
                         losslessly, with a CRC-32; the file is container v3 with a frame index
  --raw                  no compression: the v2 fixed-size container
  --synthetic-source     no capture: a paced generator feeds the same slot ring, compressor
                         and unbuffered writer (a disk-path test; the file holds no screen)
  --synthetic-content desktop|noise
                         what the generator paints (default desktop; noise is the worst case)
  --verify <file>        read every frame of a take, check every CRC-32 and the index against
                         a walk of the records; exit 1 on any mismatch

The last line printed by a plan run is the absolute path of the results file.
";

/// The console, the Windows help and `bench record`, ahead of the shared parser. `None` means
/// the shared parser takes over.
pub fn main_prelude(args: &[String]) -> Option<i32> {
    // a child's output is already redirected to its log file
    if args.first().map(String::as_str) == Some("--child") {
        return None;
    }
    crate::cli::attach_console();
    match args.first().map(String::as_str) {
        Some("record") => Some(record::main(&args[1..])),
        Some("-h" | "--help" | "help") => {
            print!(
                "{HELP}
{}
{}",
                lab::HELP,
                pipe::HELP
            );
            Some(0)
        }
        _ => None,
    }
}

/// Checks a group's `deny` items against the macOS `<kind>:<name>` grammar, so a plan is valid
/// or invalid the same way on both systems.
pub fn validate_deny(items: &[String]) -> Result<(), String> {
    for item in items
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (kind, name) = item
            .split_once(':')
            .ok_or_else(|| format!("'{item}': expected <kind>:<name>"))?;
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("'{item}': empty name"));
        }
        match kind.trim() {
            "prop" | "frame" | "spec" | "read" | "vendor" => {}
            "create" if name == "lowlatency" => {}
            "create" => {
                return Err(format!(
                    "'{item}': the only create item is create:lowlatency"
                ))
            }
            other => {
                return Err(format!(
                "'{item}': unknown kind '{other}' (prop | frame | spec | read | create | vendor)"
            ))
            }
        }
    }
    Ok(())
}

/// `ExitStatus::signal()` for the runner's crash test: an exit code with the two top bits set is
/// an NTSTATUS from an unhandled exception, which is Windows' equivalent of a fatal signal.
pub trait ExitStatusSignal {
    fn signal(&self) -> Option<i32>;
}

impl ExitStatusSignal for std::process::ExitStatus {
    fn signal(&self) -> Option<i32> {
        let code = self.code()?;
        ((code as u32) >= 0xC000_0000).then_some(code)
    }
}

/// The result of a stage not implemented on Windows. The runner records it as `skipped` and
/// retries it on `--resume`.
pub fn not_implemented(spec: &RunSpec, stage: &str) -> Json {
    json!({
        "id": spec.id,
        "result": "skipped",
        "skip_reason": format!("the {stage} stage is not implemented on Windows yet"),
        "valid": false,
        "stage": spec.stage,
        "platform": "windows",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_grammar_matches_the_macos_one() {
        assert!(validate_deny(&["create:lowlatency".into()]).is_ok());
        assert!(validate_deny(&["prop:AverageBitRate,frame:ForceKeyFrame".into()]).is_ok());
        assert!(validate_deny(&["prop".into()]).is_err());
        assert!(validate_deny(&["prop:".into()]).is_err());
        assert!(validate_deny(&["create:other".into()]).is_err());
        assert!(validate_deny(&["nope:x".into()]).is_err());
    }

    #[test]
    fn ntstatus_exits_count_as_crashes() {
        use std::os::windows::process::ExitStatusExt;
        let av = std::process::ExitStatus::from_raw(0xC000_0005);
        assert_eq!(av.signal(), Some(0xC000_0005_u32 as i32));
        assert_eq!(std::process::ExitStatus::from_raw(101).signal(), None);
        assert_eq!(std::process::ExitStatus::from_raw(0).signal(), None);
    }
}
