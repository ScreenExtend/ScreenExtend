use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use rand::Rng;
use subtle::ConstantTimeEq;
use tauri::{AppHandle, Manager};

pub const LOCK_FILE: &str = "screenextend.lock";
pub const CTRL_FILE: &str = "screenextend.ctrl";

pub enum Command {
    Focus,
    Quit,
    /// Ask the running host for its live telemetry as one line of JSON. `ScreenExtend status`
    /// runs in a separate process from the host, so this is the only way it can report
    /// per-session counters that exist only in the host's memory.
    Stats,
    /// "Are you there?" — answered with `OK <pid>`. This is what lets another process test for a
    /// running host without taking the lock, which is inherently racy: the GUI is not holding
    /// it between `acquire_lock` and `std::mem::forget`, so a concurrent lock test can win it
    /// and make the starting instance believe it is the second one.
    ///
    /// The pid is a suffix rather than a second line, so a reader that only knows about the
    /// bare `OK` still reads the answer it expects — the prefix-stable rule the client's
    /// telemetry frames follow. It is what lets the display-driver probe say *which* process is
    /// holding the driver's pipe instead of calling a busy driver a broken one.
    Ping,
    /// Re-read `config.json` and replay it into the running host's `AppState`, so a change
    /// made by `devices set` / `turn set` / `config set` applies live instead of at the next
    /// start. Answered with `OK` once the replay has run.
    Reload,
    /// Send back the running host's log backlog, newest last, one line per line, terminated by
    /// a blank line. `ScreenExtend logs` runs in a fresh process whose own ring is empty, so
    /// this is the only way for it to print anything.
    Logs,
}

impl Command {
    fn verb(&self) -> &'static str {
        match self {
            Command::Focus => "FOCUS",
            Command::Quit => "QUIT",
            Command::Stats => "STATS",
            Command::Ping => "PING",
            Command::Reload => "RELOAD",
            Command::Logs => "LOGS",
        }
    }
}

/// What happened when we asked for the single-instance lock.
///
/// The three outcomes used to collapse into `Option::None`, so a permissions error, a read-only
/// profile and a genuinely running second instance were indistinguishable — and every caller
/// read them all as "already running", which for the GUI meant offering to show an instance
/// that does not exist.
pub enum LockOutcome {
    Acquired(File),
    HeldByOther,
    Unavailable(std::io::Error),
}

pub fn acquire_lock(path: &Path) -> LockOutcome {
    let file = match File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        Ok(file) => file,
        Err(e) => return LockOutcome::Unavailable(e),
    };
    match file.try_lock_exclusive() {
        Ok(true) => LockOutcome::Acquired(file),
        Ok(false) => LockOutcome::HeldByOther,
        Err(e) => LockOutcome::Unavailable(e),
    }
}

/// Poll for the lock until `timeout`. Only `HeldByOther` is worth waiting on; an I/O error is
/// returned at once, since retrying an unwritable path just burns the timeout.
pub fn wait_for_lock(path: &Path, timeout: Duration) -> LockOutcome {
    let start = Instant::now();
    loop {
        match acquire_lock(path) {
            LockOutcome::HeldByOther => {}
            outcome => return outcome,
        }
        if start.elapsed() >= timeout {
            return LockOutcome::HeldByOther;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// Close the main window, which runs the app's own shutdown path.
///
/// The headless `serve` host has no window, so it passes its own quit closure instead —
/// `app.exit(0)` there would skip the BYE broadcast and leave every virtual display attached.
pub fn quit_main_window(app: &AppHandle) {
    match app.get_webview_window("main") {
        Some(window) => {
            let _ = app.run_on_main_thread(move || {
                let _ = window.close();
            });
        }
        None => app.exit(0),
    }
}

/// How long a connection may take to send its one line.
///
/// Connections are handled one at a time on the accept thread, so this is also how long a
/// client that connects and says nothing can delay a real `stop`.
const CTRL_READ_TIMEOUT: Duration = Duration::from_millis(500);

/// Write the port and token, readable only by this user.
///
/// Whoever can read this file can send `QUIT` to the running host. On unix the app's local
/// data directory is not necessarily private, so the mode is set explicitly and also applied
/// to a file that already exists with wider permissions. On Windows the directory under
/// `%LOCALAPPDATA%` already carries a user-only ACL that the file inherits.
fn write_ctrl_file(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

pub fn start_control_server<Q, R>(
    app: AppHandle,
    ctrl_path: PathBuf,
    focus: fn(&tauri::WebviewWindow),
    quit: Q,
    reload: R,
) -> std::io::Result<()>
where
    Q: Fn(&AppHandle) + Send + 'static,
    R: Fn(&AppHandle) -> Result<(), String> + Send + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let token = format!("{:032x}", rand::rng().random::<u128>());
    write_ctrl_file(&ctrl_path, &format!("{port}\n{token}\n"))?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(CTRL_READ_TIMEOUT));
            handle_connection(stream, &app, &token, focus, &quit, &reload);
        }
    });
    Ok(())
}

/// Remove the control file so a later `stop` reports "no running instance" instead of dialling
/// a port the OS has since handed to somebody else.
pub fn clear_control_file(ctrl_path: &Path) {
    let _ = std::fs::remove_file(ctrl_path);
}

fn handle_connection<Q, R>(
    stream: TcpStream,
    app: &AppHandle,
    token: &str,
    focus: fn(&tauri::WebviewWindow),
    quit: &Q,
    reload: &R,
) where
    Q: Fn(&AppHandle),
    R: Fn(&AppHandle) -> Result<(), String>,
{
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut line = String::new();
    if BufReader::new(read_half).read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.trim().splitn(2, ' ');
    let presented = parts.next().unwrap_or("");
    // Same treatment the join OTP gets in `streamer/session.rs`: compare in constant time so
    // the loopback port cannot be used to guess the token a byte at a time.
    let matches =
        presented.len() == token.len() && bool::from(presented.as_bytes().ct_eq(token.as_bytes()));
    if !matches {
        teprintln!("[single-instance] control connection presented a bad token; ignoring");
        return;
    }
    let mut stream = stream;
    match parts.next() {
        Some("FOCUS") => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = app.run_on_main_thread(move || focus(&window));
            }
        }
        Some("QUIT") => quit(app),
        Some("STATS") => {
            // The telemetry registry is keyed by join id and carries no labels, so the session
            // map supplies the names, addresses and geometry `devices list` prints beside them.
            let mut snapshot = crate::streamer::telemetry::snapshot_json();
            if let Some(state) = app.try_state::<crate::AppState>() {
                if let Some(obj) = snapshot.as_object_mut() {
                    obj.insert("joins".into(), join_rows(&state.sessions));
                }
            }
            let json = snapshot.to_string();
            let _ = stream.write_all(json.as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
        }
        Some("PING") => {
            let _ = stream.write_all(format!("OK {}\n", std::process::id()).as_bytes());
            let _ = stream.flush();
        }
        Some("RELOAD") => {
            let reply = match reload(app) {
                Ok(()) => "OK\n".to_string(),
                Err(e) => format!("ERR {e}\n"),
            };
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
        Some("LOGS") => {
            let mut out = String::new();
            for line in crate::logbus::get_log_backlog() {
                out.push_str(&line);
                out.push('\n');
            }
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
        }
        _ => {}
    }
}

/// Send `command` and read the running instance's reply.
///
/// `None` means nothing is listening. The reply body is empty for commands that do not answer.
pub fn query_running_instance(ctrl_path: &Path, command: Command) -> Option<String> {
    let contents = std::fs::read_to_string(ctrl_path).ok()?;
    let mut lines = contents.lines();
    let port = lines.next().and_then(|s| s.trim().parse::<u16>().ok())?;
    let token = lines.next().unwrap_or("").trim();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    stream
        .write_all(format!("{token} {}\n", command.verb()).as_bytes())
        .ok()?;
    let _ = stream.flush();
    let mut reply = String::new();
    let mut buf = [0u8; 4096];
    loop {
        match std::io::Read::read(&mut stream, &mut buf) {
            Ok(0) => break,
            Ok(n) => reply.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => break,
        }
    }
    Some(reply)
}

/// Where this process's control file lives, remembered once so code with no `AppHandle` in
/// reach can still ask whether a host is up and which pid it is.
///
/// The display-driver probe is the caller that needs it: it runs inside
/// `check_system_requirements`, a plain Tauri command, and the answer it wants — "is the pipe
/// held by one of ours, and which?" — is exactly what the control socket knows.
static CTRL_PATH: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn remember_ctrl_path(path: PathBuf) {
    let _ = CTRL_PATH.set(path);
}

/// Whether a `PING` reply says a host is up. `OK` and `OK <pid>` both do.
pub fn is_ok_reply(reply: &str) -> bool {
    let reply = reply.trim();
    reply == "OK" || reply.starts_with("OK ")
}

/// The pid in a `PING` reply, or `None` if the reply carried no pid or was not an `OK`.
fn ping_pid(reply: &str) -> Option<u32> {
    reply.trim().strip_prefix("OK")?.trim().parse().ok()
}

/// The pid of the instance answering on the control socket named by `ctrl_path`.
pub fn running_instance_pid_at(ctrl_path: &Path) -> Option<u32> {
    ping_pid(&query_running_instance(ctrl_path, Command::Ping)?)
}

/// The pid of the instance answering on *this* process's control socket, if one is.
///
/// That may be this process itself, once its own control server is up. Callers deciding whether
/// somebody else is in the way must compare against `std::process::id()`.
pub fn running_instance_pid() -> Option<u32> {
    running_instance_pid_at(CTRL_PATH.get()?)
}

pub fn signal_running_instance(ctrl_path: &Path, command: Command) -> bool {
    let Ok(contents) = std::fs::read_to_string(ctrl_path) else {
        return false;
    };
    let mut lines = contents.lines();
    let Some(port) = lines.next().and_then(|s| s.trim().parse::<u16>().ok()) else {
        return false;
    };
    let token = lines.next().unwrap_or("").trim();

    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(2)) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    stream
        .write_all(format!("{token} {}\n", command.verb()).as_bytes())
        .is_ok()
}

#[cfg(test)]
mod test {
    use super::*;

    /// `PING` grew a pid on the end of its reply. A reader that predates it must still see an
    /// `OK`, and a reader that wants the pid must not be confused by one that has none.
    #[test]
    fn the_ping_reply_is_prefix_stable() {
        assert!(is_ok_reply("OK"));
        assert!(is_ok_reply("OK\n"));
        assert!(is_ok_reply("OK 71848\n"));
        assert!(!is_ok_reply("OKAY"));
        assert!(!is_ok_reply(""));
        assert!(!is_ok_reply("ERR nope"));

        assert_eq!(ping_pid("OK 71848\n"), Some(71848));
        assert_eq!(ping_pid("OK"), None);
        assert_eq!(ping_pid("OK not-a-pid"), None);
        assert_eq!(ping_pid("ERR 71848"), None);
    }
}

/// One JSON object per live join, for `ScreenExtend devices list` and `status --json`.
///
/// Keyed by the join id, which is the session's identity. The address is a label beside it: it
/// moves under a live session and it is recycled between devices, so it never names one.
fn join_rows(sessions: &crate::streamer::session::SharedSessions) -> serde_json::Value {
    let rows: Vec<serde_json::Value> = crate::streamer::session::join_summaries(sessions)
        .into_iter()
        .map(|r| {
            serde_json::json!({
                "joinId": r.join_id,
                "token": r.device_token,
                "ip": r.ip,
                "name": r.name,
                "screenSize": r.screen_size,
                "display": r.display.map(|d| serde_json::json!({
                    "id": d.display_id,
                    "device": d.device_name,
                    "width": d.width,
                    "height": d.height,
                    "refresh": d.refresh,
                    "scale": d.scale,
                    "portrait": d.portrait,
                })),
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}
