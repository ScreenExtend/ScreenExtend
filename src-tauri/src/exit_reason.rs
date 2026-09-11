//! Why the host process ended, written down before it does.
//!
//! A host that is gone leaves almost nothing behind. The log bus is a 2000-line ring in memory,
//! so it dies with the process; the lock file is `mem::forget`ed at startup and is byte-identical
//! whether the process quit deliberately or was terminated; and a clean `exit(0)` produces no
//! Application-log event and no crash dump. The observable evidence of a host that "vanished
//! during a settings change" and of one the user quit from the menu is therefore *the same
//! evidence*, which is why a report of the first could not be told from the second.
//!
//! So every deliberate exit route records itself here first. [`record`] stores the reason in the
//! process and appends it to a one-line breadcrumb file beside the lock, and the next launch
//! reads it back into the log ([`report_previous`]). A process that is gone with **no** breadcrumb
//! did not leave through any route this crate owns — which is the distinction the silent-exit
//! report needed and could not make.
//!
//! The file is a diagnostic, never a decision: nothing reads it to choose behaviour, a missing or
//! corrupt one is not an error, and it is rewritten (not appended to) so it cannot grow.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The breadcrumb's file name, in `app_local_data_dir` beside `screenextend.lock`.
pub const EXIT_FILE: &str = "screenextend.exit";

/// Where to write the breadcrumb, remembered once so an exit route with no `AppHandle` in reach
/// — the Ctrl+C handler, a `RunEvent` callback — can still record one.
static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The reason this process recorded, if any. Read by the `RunEvent` handler to tell an exit
/// somebody asked for from one nobody did.
static REASON: Mutex<Option<String>> = Mutex::new(None);

/// Remember where the breadcrumb goes. Called once per process, from startup.
pub fn set_dir(dir: PathBuf) {
    *DIR.lock().unwrap_or_else(|e| e.into_inner()) = Some(dir);
}

/// Whether an exit route has already named itself.
pub fn recorded() -> Option<String> {
    REASON.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Record why this process is about to end.
///
/// The first reason wins: the routes chain (`exit_app` runs, then the window closes, then the
/// run loop reports `ExitRequested`) and the one that *started* it is the one worth keeping.
pub fn record(reason: impl Into<String>) {
    let reason = reason.into();
    {
        let mut slot = REASON.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_some() {
            return;
        }
        *slot = Some(reason.clone());
    }
    tprintln!("[exit] {reason}");
    let dir = DIR.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(dir) = dir {
        write_breadcrumb(&dir.join(EXIT_FILE), &reason);
    }
}

/// The line format, split out so the parser and the writer cannot drift apart:
/// `<unix seconds> <pid> <reason>`. The reason is the rest of the line, newlines stripped.
fn breadcrumb_line(now_secs: u64, pid: u32, reason: &str) -> String {
    let one_line: String = reason
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    format!("{now_secs} {pid} {one_line}\n")
}

fn write_breadcrumb(path: &Path, reason: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let _ = std::fs::write(path, breadcrumb_line(now, std::process::id(), reason));
}

/// What a breadcrumb says, once parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Breadcrumb {
    pub unix_secs: u64,
    pub pid: u32,
    pub reason: String,
}

/// Parse one breadcrumb line. `None` for anything that is not one — an empty file, a truncated
/// write, a file some other tool put there.
pub fn parse(contents: &str) -> Option<Breadcrumb> {
    let line = contents.lines().next()?.trim();
    let mut parts = line.splitn(3, ' ');
    let unix_secs = parts.next()?.parse::<u64>().ok()?;
    let pid = parts.next()?.parse::<u32>().ok()?;
    let reason = parts.next()?.trim();
    if reason.is_empty() {
        return None;
    }
    Some(Breadcrumb {
        unix_secs,
        pid,
        reason: reason.to_string(),
    })
}

/// Read the breadcrumb the *previous* instance left, if there is one.
pub fn previous(dir: &Path) -> Option<Breadcrumb> {
    let contents = std::fs::read_to_string(dir.join(EXIT_FILE)).ok()?;
    parse(&contents).filter(|b| b.pid != std::process::id())
}

/// Put the previous instance's exit reason into this instance's log, then clear it.
///
/// Clearing is what makes the absence meaningful: after this, a breadcrumb on disk describes the
/// currently running process, so a host that disappears without writing one can be said to have
/// left through no route this crate owns.
pub fn report_previous(dir: &Path) {
    if let Some(b) = previous(dir) {
        tprintln!(
            "[exit] the previous host (pid {}) ended deliberately: {}",
            b.pid,
            b.reason
        );
    }
    let _ = std::fs::remove_file(dir.join(EXIT_FILE));
}

/// One sentence for a human asking why nothing is running, or `None` when there is no answer.
pub fn previous_summary(dir: &Path) -> Option<String> {
    let b = previous(dir)?;
    Some(format!(
        "The last host (pid {}) ended deliberately: {}",
        b.pid, b.reason
    ))
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_breadcrumb_round_trips() {
        let line = breadcrumb_line(
            1_700_000_000,
            4242,
            "exit_app: the desktop UI asked to quit",
        );
        let b = parse(&line).expect("the line this module writes must parse");
        assert_eq!(b.unix_secs, 1_700_000_000);
        assert_eq!(b.pid, 4242);
        assert_eq!(b.reason, "exit_app: the desktop UI asked to quit");
    }

    /// A reason with a newline in it would otherwise write a second line the parser would never
    /// see, and a reader taking `lines().next()` would report a truncated sentence.
    #[test]
    fn a_multi_line_reason_is_flattened_into_one() {
        let line = breadcrumb_line(1, 2, "first\nsecond\r\nthird");
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert_eq!(parse(&line).expect("parses").reason, "first second  third");
    }

    #[test]
    fn junk_is_not_a_breadcrumb() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("\n"), None);
        assert_eq!(parse("not-a-number 7 reason"), None);
        assert_eq!(parse("17 notapid reason"), None);
        assert_eq!(parse("17 7"), None, "a line with no reason says nothing");
        assert_eq!(parse("17 7    "), None, "nor does one with only spaces");
    }

    /// The first route to speak owns the reason: `exit_app` runs, which closes the window, which
    /// raises `ExitRequested`. Keeping the last would report every deliberate quit as "the run
    /// loop was asked to exit", which is the one sentence that says nothing.
    #[test]
    fn the_first_reason_wins() {
        // The statics are process-global, so this test owns them: it is the only one that writes.
        assert_eq!(recorded(), None, "another test recorded a reason first");
        record("exit_app: the desktop UI asked to quit");
        record("run loop: ExitRequested");
        assert_eq!(
            recorded().as_deref(),
            Some("exit_app: the desktop UI asked to quit")
        );
    }
}
