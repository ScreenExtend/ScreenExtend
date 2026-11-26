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
    /// Ask the running host for its live telemetry as one line of JSON: per-session counters
    /// exist only in the host's memory, and `ScreenExtend status` is a separate process.
    Stats,
    /// Liveness, answered with `OK <pid>`. Testing for a host by taking the lock is racy: the
    /// GUI does not hold it between `acquire_lock` and `std::mem::forget`, so a concurrent lock
    /// test can win it and make the starting instance believe it is the second one.
    ///
    /// The pid is a suffix, not a second line, so a reader that only knows the bare `OK` still
    /// reads it. The display-driver probe uses it to name the process holding the driver's pipe.
    Ping,
    /// Re-read `config.json` and replay it into the running host's `AppState`, so a change
    /// made by `devices set` / `turn set` / `config set` applies live instead of at the next
    /// start. Answered with `OK` once the replay has run.
    Reload,
    /// Send back the running host's log backlog, newest last, one line per line, terminated by
    /// a blank line. `ScreenExtend logs` runs in a fresh process whose own ring is empty.
    Logs,
    /// Ask the running host which credentials it is actually serving, as one line of JSON.
    /// A session id lives only in the host's memory (minted at bootstrap or at `serve` startup,
    /// never written to `config.json`), and `qr`, `status` and `session new` each run in their
    /// own process, so this is the only place they can read it.
    ///
    /// **The reply carries the OTP, deliberately.** The socket is loopback-only and every request
    /// carries the 128-bit token from the user-only `screenextend.ctrl`; whoever can read that
    /// file can already `QUIT` the host or set its credentials with [`Command::SetSession`], so
    /// the OTP is inside the trust boundary. It is there so a setter can read back what it
    /// applied (the desktop UI mirrors both values and can push its own copy back). No shipped
    /// command prints it.
    Session,
    /// Apply new join credentials to the running host, and re-register the relay under the new
    /// id when the host has a public session. Answered with `OK` or `ERR <reason>`.
    SetSession {
        session_id: String,
        otp: String,
    },
}

impl Command {
    /// The request line this command sends, after the token and the separating space.
    fn wire(&self) -> String {
        match self {
            Command::Focus => "FOCUS".to_string(),
            Command::Quit => "QUIT".to_string(),
            Command::Stats => "STATS".to_string(),
            Command::Ping => "PING".to_string(),
            Command::Reload => "RELOAD".to_string(),
            Command::Logs => "LOGS".to_string(),
            Command::Session => "SESSION".to_string(),
            Command::SetSession { session_id, otp } => format!("SESSION {session_id} {otp}"),
        }
    }
}

/// What the running host is serving right now, carried by [`Command::Session`] in both
/// directions: the host builds one, the reader parses one back.
///
/// An empty `session_id` means the host is up but has not minted credentials yet (the desktop
/// app does that from its bootstrap, some time after launch).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub session_id: String,
    pub otp: String,
    /// Whether the host currently holds a cloud relay client. The persisted
    /// `publicSessionsEnabled` is only the intent.
    pub public_sessions: bool,
    pub cloud_state: String,
    pub cloud_detail: String,
}

impl SessionInfo {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "sessionId": self.session_id,
            "otp": self.otp,
            "publicSessions": self.public_sessions,
            "cloudState": self.cloud_state,
            "cloudDetail": self.cloud_detail,
        })
    }

    /// Parse a `SESSION` reply. `None` for anything that is not this object, which includes a
    /// host too old to know the verb (it closes the connection saying nothing).
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let text = |key: &str| {
            v.get(key)
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string()
        };
        v.get("sessionId")?.as_str()?;
        Some(SessionInfo {
            session_id: text("sessionId"),
            otp: text("otp"),
            public_sessions: v
                .get("publicSessions")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            cloud_state: text("cloudState"),
            cloud_detail: text("cloudDetail"),
        })
    }
}

/// One authenticated control request, parsed off the wire. Split out from [`handle_connection`]
/// so the wire format has a test that needs no Tauri app.
#[derive(Debug, PartialEq)]
pub(crate) enum Request<'a> {
    Focus,
    Quit,
    Stats,
    Ping,
    Reload,
    Logs,
    Session,
    SetSession { session_id: &'a str, otp: &'a str },
    Unknown,
}

/// Parse one request line. `None` means the token did not match and nothing should be done.
/// The token is compared in constant time, so the loopback port cannot be used to guess it a
/// byte at a time.
pub(crate) fn parse_request<'a>(line: &'a str, token: &str) -> Option<Request<'a>> {
    let mut parts = line.trim().splitn(2, ' ');
    let presented = parts.next().unwrap_or("");
    let matches =
        presented.len() == token.len() && bool::from(presented.as_bytes().ct_eq(token.as_bytes()));
    if !matches {
        return None;
    }
    let rest = parts.next().unwrap_or("").trim();
    let (verb, args) = match rest.split_once(' ') {
        Some((verb, args)) => (verb, args.trim()),
        None => (rest, ""),
    };
    Some(match verb {
        "FOCUS" => Request::Focus,
        "QUIT" => Request::Quit,
        "STATS" => Request::Stats,
        "PING" => Request::Ping,
        "RELOAD" => Request::Reload,
        "LOGS" => Request::Logs,
        "SESSION" if args.is_empty() => Request::Session,
        "SESSION" => match args.split_once(' ') {
            Some((session_id, otp)) => Request::SetSession {
                session_id: session_id.trim(),
                otp: otp.trim(),
            },
            None => Request::SetSession {
                session_id: args,
                otp: "",
            },
        },
        _ => Request::Unknown,
    })
}

/// Reject credentials the host could not serve before they replace ones it can.
///
/// `SessionAuth::validate` refuses an empty id or OTP, so applying an empty pair would silently
/// take the host off the air. The rest is a length and charset clamp: the minters' alphabet plus
/// `-` and `_`, so an operator-chosen `--session-id` still passes.
pub(crate) fn validate_credentials(session_id: &str, otp: &str) -> Result<(), String> {
    let ok = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    if session_id.is_empty() || otp.is_empty() {
        return Err("a session id and an OTP are both required".to_string());
    }
    if session_id.len() > 64 || otp.len() > 64 {
        return Err("the session id and OTP must be 64 characters or fewer".to_string());
    }
    if !ok(session_id) || !ok(otp) {
        return Err(
            "the session id and OTP may only carry letters, digits, '-' and '_'".to_string(),
        );
    }
    Ok(())
}

/// What happened when we asked for the single-instance lock. A permissions error or a read-only
/// profile (`Unavailable`) must not read as "already running" (`HeldByOther`).
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
/// The headless `serve` host has no window and passes its own quit closure instead: `app.exit(0)`
/// there would skip the BYE broadcast and leave every virtual display attached.
pub fn quit_main_window(app: &AppHandle) {
    crate::exit_reason::record(
        "control server: another process sent QUIT (`ScreenExtend stop`, a second instance, or \
         the driver-holder prompt)",
    );
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

    // Windows: `gate` hands over a connection only once its whole request line has arrived, and
    // a panic in one request does not end the server
    #[cfg(windows)]
    std::thread::spawn(move || {
        gate::serve(listener, gate::Limits::default(), move |stream| {
            let _ = stream.set_read_timeout(Some(CTRL_READ_TIMEOUT));
            handle_connection(stream, &app, &token, focus, &quit, &reload);
        });
    });
    #[cfg(not(windows))]
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

/// The control server's accept loop on Windows, hardened against the two ways one connection
/// could disable every verb for the life of the process.
///
/// * **A slow client.** Connecting takes no token, and a read timeout applies per read, not per
///   request. So each connection waits for its line on a short-lived thread of its own, `peek`ing
///   so the handler still reads the line itself, and is dropped when the line is longer than
///   [`Limits::max_line`] or has not arrived by [`Limits::deadline`]. The handler never waits on
///   a client to speak, and replies carry a write timeout.
/// * **A panic.** The handler runs each request under `catch_unwind`.
///
/// Requests still execute one at a time, in the order their lines completed, on one handler
/// thread: the verbs were written for that and the quit and reload callbacks are not `Sync`.
#[cfg(windows)]
mod gate {
    use std::io::ErrorKind;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy, Debug)]
    pub(super) struct Limits {
        /// Longest request line accepted. The longest real one is a `SESSION <id> <otp>` of well
        /// under 200 bytes.
        pub max_line: usize,
        /// How long a connection has to deliver its whole line.
        pub deadline: Duration,
        /// Connections waiting for their line at once; more are closed on arrival.
        pub max_pending: usize,
        /// How long writing a reply may block on a client that does not read.
        pub reply_timeout: Duration,
    }

    impl Default for Limits {
        fn default() -> Self {
            Self {
                max_line: 4096,
                deadline: Duration::from_secs(2),
                max_pending: 32,
                reply_timeout: Duration::from_secs(2),
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(super) enum Arrival {
        Ready,
        TooLong,
        Closed,
        TimedOut,
    }

    const POLL: Duration = Duration::from_millis(20);

    /// Wait until `stream` has a whole line buffered, without consuming any of it.
    pub(super) fn wait_for_line(stream: &TcpStream, limits: &Limits) -> Arrival {
        let start = Instant::now();
        let mut buf = vec![0u8; limits.max_line];
        let _ = stream.set_read_timeout(Some(POLL));
        let mut seen = 0usize;
        loop {
            match stream.peek(&mut buf) {
                Ok(0) => return Arrival::Closed,
                Ok(n) => {
                    if buf[..n].contains(&b'\n') {
                        return Arrival::Ready;
                    }
                    if n >= limits.max_line {
                        return Arrival::TooLong;
                    }
                    // `peek` returns at once while unread bytes are waiting, so nothing new
                    // having arrived is the case that must sleep rather than spin.
                    if n == seen {
                        std::thread::sleep(POLL);
                    }
                    seen = n;
                }
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
                Err(_) => return Arrival::Closed,
            }
            if start.elapsed() >= limits.deadline {
                return Arrival::TimedOut;
            }
        }
    }

    /// Accept on `listener` for the life of the process, handing each connection whose request
    /// line has arrived to `handler`, one at a time.
    pub(super) fn serve<H>(listener: TcpListener, limits: Limits, handler: H)
    where
        H: FnMut(TcpStream) + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::sync_channel::<TcpStream>(limits.max_pending);
        let spawned = std::thread::Builder::new()
            .name("control-handler".into())
            .spawn(move || {
                let mut handler = handler;
                for stream in rx {
                    let _ = stream.set_write_timeout(Some(limits.reply_timeout));
                    let outcome =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(stream)));
                    if outcome.is_err() {
                        teprintln!(
                            "[single-instance] a control request panicked; the control server \
                             carries on"
                        );
                    }
                }
            });
        if let Err(e) = spawned {
            teprintln!("[single-instance] could not start the control handler: {e}");
            return;
        }
        let pending = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if pending.fetch_add(1, Ordering::AcqRel) >= limits.max_pending {
                pending.fetch_sub(1, Ordering::AcqRel);
                continue; // dropping it closes it
            }
            let (tx, waiting) = (tx.clone(), Arc::clone(&pending));
            let spawned = std::thread::Builder::new()
                .name("control-conn".into())
                .spawn(move || {
                    if wait_for_line(&stream, &limits) == Arrival::Ready {
                        let _ = tx.send(stream);
                    }
                    waiting.fetch_sub(1, Ordering::AcqRel);
                });
            if spawned.is_err() {
                pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::{Ipv4Addr, SocketAddr};
        use std::sync::atomic::AtomicBool;

        /// A server whose handler echoes the request line, or panics on `boom`.
        fn echo_server(limits: Limits, invoked: Arc<AtomicBool>) -> SocketAddr {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                serve(listener, limits, move |mut stream| {
                    invoked.store(true, Ordering::SeqCst);
                    let mut line = String::new();
                    BufReader::new(stream.try_clone().unwrap())
                        .read_line(&mut line)
                        .unwrap();
                    if line.trim() == "boom" {
                        panic!("a verb panicked");
                    }
                    let _ = stream.write_all(format!("echo {line}").as_bytes());
                });
            });
            addr
        }

        fn ask(addr: SocketAddr, line: &str) -> String {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            s.write_all(line.as_bytes()).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            out
        }

        #[test]
        fn a_client_that_dribbles_does_not_hold_up_the_next_request() {
            let limits = Limits {
                deadline: Duration::from_secs(5),
                ..Limits::default()
            };
            let addr = echo_server(limits, Arc::new(AtomicBool::new(false)));
            let mut slow = TcpStream::connect(addr).unwrap();
            let dribbler = std::thread::spawn(move || {
                for _ in 0..20 {
                    if slow.write_all(b"x").is_err() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            });
            std::thread::sleep(Duration::from_millis(150));
            let t0 = Instant::now();
            assert_eq!(ask(addr, "status\n"), "echo status\n");
            assert!(
                t0.elapsed() < Duration::from_secs(1),
                "the request waited behind the dribbling client ({:?})",
                t0.elapsed()
            );
            dribbler.join().unwrap();
        }

        #[test]
        fn a_panicking_request_does_not_end_the_server() {
            let addr = echo_server(Limits::default(), Arc::new(AtomicBool::new(false)));
            assert_eq!(ask(addr, "boom\n"), "");
            assert_eq!(ask(addr, "again\n"), "echo again\n");
        }

        #[test]
        fn an_oversized_or_unfinished_line_never_reaches_the_handler() {
            let limits = Limits {
                deadline: Duration::from_millis(300),
                ..Limits::default()
            };
            let invoked = Arc::new(AtomicBool::new(false));
            let addr = echo_server(limits, invoked.clone());

            let t0 = Instant::now();
            assert_eq!(ask(addr, &"x".repeat(5000)), "");
            assert_eq!(ask(addr, "no newline"), "");
            assert!(t0.elapsed() < Duration::from_secs(3));
            assert!(!invoked.load(Ordering::SeqCst));
            // and the server is still answering
            assert_eq!(ask(addr, "ok\n"), "echo ok\n");
        }
    }
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
    let Some(request) = parse_request(&line, token) else {
        teprintln!("[single-instance] control connection presented a bad token; ignoring");
        return;
    };
    let mut stream = stream;
    match request {
        Request::Focus => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = app.run_on_main_thread(move || focus(&window));
            }
        }
        Request::Quit => quit(app),
        Request::Stats => {
            // the telemetry registry is keyed by join id and carries no labels: the session map
            // supplies the names, addresses and geometry `devices list` prints beside them
            let mut snapshot = crate::streamer::telemetry::snapshot_json();
            if let Some(state) = app.try_state::<crate::AppState>() {
                if let Some(obj) = snapshot.as_object_mut() {
                    obj.insert("joins".into(), join_rows(&state.sessions));
                    let tier = state.audio_hub.diagnostics().map(|d| d.tier());
                    if let Some(tier) = tier.filter(|t| !t.is_empty()) {
                        obj.insert("audio_tier".into(), tier.into());
                    }
                }
            }
            #[cfg(windows)]
            if let Some(obj) = snapshot.as_object_mut() {
                obj.insert("tune".into(), tune_json());
            }
            let json = snapshot.to_string();
            let _ = stream.write_all(json.as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
        }
        Request::Ping => {
            let _ = stream.write_all(format!("OK {}\n", std::process::id()).as_bytes());
            let _ = stream.flush();
        }
        Request::Reload => {
            let reply = match reload(app) {
                Ok(()) => "OK\n".to_string(),
                Err(e) => format!("ERR {e}\n"),
            };
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
        Request::Logs => {
            let out = logs_reply();
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
        }
        // the two `AppState` verbs live in `cli`, beside the per-OS `platform` alias, so this
        // module stays OS-independent (the same reason `RELOAD` is a callback)
        Request::Session => {
            let reply = crate::cli::live_session_info(app).to_json().to_string();
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.write_all(b"\n");
            let _ = stream.flush();
        }
        Request::SetSession { session_id, otp } => {
            let reply = match validate_credentials(session_id, otp)
                .and_then(|()| crate::cli::apply_session_credentials(app, session_id, otp))
            {
                Ok(()) => "OK\n".to_string(),
                Err(e) => format!("ERR {e}\n"),
            };
            let _ = stream.write_all(reply.as_bytes());
            let _ = stream.flush();
        }
        Request::Unknown => {}
    }
}

/// The `LOGS` reply: the backlog, one line per line. Read with `backlog_snapshot`, not
/// `get_log_backlog`: that command arms the `LogLine` stream as a side effect, which a CLI
/// reader that never listens must not do.
fn logs_reply() -> String {
    let mut out = String::new();
    for line in crate::logbus::backlog_snapshot() {
        out.push_str(&line);
        out.push('\n');
    }
    out
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
        .write_all(format!("{token} {}\n", command.wire()).as_bytes())
        .ok()?;
    let _ = stream.flush();
    Some(read_reply(&mut stream))
}

/// Read a reply to the end and decode it once: decoding each 4 KiB read on its own turns a
/// multi-byte character that straddles a read boundary into two U+FFFD.
fn read_reply(stream: &mut impl std::io::Read) -> String {
    let mut bytes = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Where this process's control file lives, remembered once so code with no `AppHandle` in
/// reach (the display-driver probe) can still ask whether a host is up and which pid it is.
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
        .write_all(format!("{token} {}\n", command.wire()).as_bytes())
        .is_ok()
}

/// The A/B switches this host has asked about, and any name the environment set that none did
/// (a typo in `SE_TUNE_ON` is otherwise silent).
#[cfg(windows)]
fn tune_json() -> serde_json::Value {
    use crate::windows_utils::streamer::tune;
    serde_json::json!({ "switches": tune::report(), "unknown": tune::unknown() })
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
                // What the join's page last posted to `/audio-outputs`: `supported` is `null`
                // until it has said, and `discovery` is each output-discovery API's result.
                "audioOutputs": {
                    "supported": r.audio_outputs_supported,
                    "outputs": r.audio_outputs,
                    "labelled": r.audio_outputs_labelled,
                    "discovery": r.audio_discovery,
                },
            })
        })
        .collect();
    serde_json::Value::Array(rows)
}

#[cfg(test)]
mod test {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn stats_carry_the_switches_this_host_asked_about() {
        assert!(crate::windows_utils::streamer::tune::on(
            "stats_selftest_ships_on"
        ));
        let tune = tune_json();
        assert_eq!(tune["switches"]["stats_selftest_ships_on"]["value"], true);
        assert!(tune["unknown"].is_array());
    }

    /// A reader that predates the pid suffix must still see an `OK`, and one that wants the pid
    /// must cope with a reply that has none.
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

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// A socket hands a reply over in whatever pieces it likes. A multi-byte character cut in
    /// two by a read must come out whole.
    #[test]
    fn a_reply_split_inside_a_character_is_decoded_whole() {
        struct Dribble<'a>(&'a [u8]);
        impl std::io::Read for Dribble<'_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.0.len().min(2).min(buf.len());
                buf[..n].copy_from_slice(&self.0[..n]);
                self.0 = &self.0[n..];
                Ok(n)
            }
        }
        let text = "[display] at startup … ─── → done - ok\n".repeat(50);
        let reply = read_reply(&mut Dribble(text.as_bytes()));
        assert!(
            !reply.contains('\u{FFFD}'),
            "a character was split: {reply:?}"
        );
        assert_eq!(reply, text);
    }

    /// `ScreenExtend logs` never listens to `LogLine`, so it must not arm the event stream: every
    /// later log line would be serialised into an IPC event nobody reads.
    #[test]
    fn the_logs_verb_does_not_arm_the_panel_stream() {
        crate::logbus::set_log_streaming(false);
        let mut reply = String::new();
        for _ in 0..200 {
            // pushed on every turn: under a noisy parallel run one line can be dropped on overflow
            // or pushed out of the backlog before it is read
            crate::logbus::push_line("single-instance-logs-verb-probe".to_string());
            reply = logs_reply();
            if reply.contains("single-instance-logs-verb-probe") {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(reply.contains("single-instance-logs-verb-probe\n"));
        assert!(
            !crate::logbus::streaming_armed(),
            "the LOGS verb armed the LogLine stream"
        );
    }

    #[test]
    fn the_session_verb_is_one_word_to_read_and_three_to_write() {
        assert_eq!(Command::Session.wire(), "SESSION");
        assert_eq!(
            Command::SetSession {
                session_id: "MQZ8VLBXBZPH".into(),
                otp: "418203".into(),
            }
            .wire(),
            "SESSION MQZ8VLBXBZPH 418203"
        );
        // older verbs are unchanged, so a new CLI can drive an older host for everything but
        // SESSION
        assert_eq!(Command::Ping.wire(), "PING");
        assert_eq!(Command::Stats.wire(), "STATS");
    }

    #[test]
    fn a_request_is_only_parsed_for_the_right_token() {
        assert_eq!(
            parse_request(&format!("{TOKEN} SESSION\n"), TOKEN),
            Some(Request::Session)
        );
        assert_eq!(
            parse_request(&format!("{TOKEN} SESSION ABCDEFGHJKLM 123456\n"), TOKEN),
            Some(Request::SetSession {
                session_id: "ABCDEFGHJKLM",
                otp: "123456",
            })
        );
        assert_eq!(
            parse_request(&format!("{TOKEN} PING"), TOKEN),
            Some(Request::Ping)
        );
        assert_eq!(
            parse_request(&format!("{TOKEN} NOPE"), TOKEN),
            Some(Request::Unknown)
        );
        // a wrong, short or absent token is indistinguishable from any other: nothing at all
        assert_eq!(
            parse_request("wrong SESSION ABCDEFGHJKLM 123456", TOKEN),
            None
        );
        assert_eq!(parse_request("SESSION", TOKEN), None);
        assert_eq!(parse_request("", TOKEN), None);
    }

    /// A `SESSION` set with one argument is a set with an empty OTP, not a query: otherwise a
    /// truncated request would read as "tell me the credentials" and answer with them.
    #[test]
    fn a_half_written_set_is_a_set_and_is_refused() {
        assert_eq!(
            parse_request(&format!("{TOKEN} SESSION ABCDEFGHJKLM"), TOKEN),
            Some(Request::SetSession {
                session_id: "ABCDEFGHJKLM",
                otp: "",
            })
        );
        assert!(validate_credentials("ABCDEFGHJKLM", "").is_err());
        assert!(validate_credentials("", "123456").is_err());
        assert!(validate_credentials(&"A".repeat(65), "123456").is_err());
        assert!(validate_credentials("ABCDEFGHJKLM", "12 34").is_err());
        assert!(validate_credentials("has/slash", "123456").is_err());
        assert!(validate_credentials("ABCDEFGHJKLM", "123456").is_ok());
        assert!(validate_credentials("host-1_ci", "abc123").is_ok());
    }

    #[test]
    fn the_session_reply_survives_a_round_trip_through_json() {
        let info = SessionInfo {
            session_id: "MQZ8VLBXBZPH".into(),
            otp: "418203".into(),
            public_sessions: true,
            cloud_state: "registered".into(),
            cloud_detail: "Registered · https://session.screenextend.app/j/MQZ8VLBXBZPH".into(),
        };
        assert_eq!(SessionInfo::from_json(&info.to_json()), Some(info));
        // what a host too old for the verb answers: it closes the connection saying nothing
        assert_eq!(
            serde_json::from_str::<serde_json::Value>("")
                .ok()
                .and_then(|v| SessionInfo::from_json(&v)),
            None
        );
        assert_eq!(
            SessionInfo::from_json(&serde_json::json!({"joins": []})),
            None
        );
    }

    /// The whole `SESSION` exchange over a real loopback socket, with a `Mutex<SessionInfo>`
    /// standing in for the host's `AppState`: the shipped ctrl file, the shipped client
    /// (`query_running_instance`), the shipped parser and the shipped reply shapes.
    #[test]
    fn a_session_query_reads_back_what_a_session_set_wrote() {
        use std::sync::{Arc, Mutex};

        let live = Arc::new(Mutex::new(SessionInfo {
            session_id: "WLZ7JHNBC2RG".into(),
            otp: "111111".into(),
            public_sessions: true,
            cloud_state: "registered".into(),
            cloud_detail: "Registered".into(),
        }));

        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        let dir = std::env::temp_dir().join(format!("se-ctrl-{}-{port}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let ctrl = dir.join(CTRL_FILE);
        write_ctrl_file(&ctrl, &format!("{port}\n{TOKEN}\n")).expect("write ctrl file");

        let served = Arc::clone(&live);
        let host = std::thread::spawn(move || {
            for stream in listener.incoming().take(4) {
                let Ok(stream) = stream else { continue };
                let Ok(read_half) = stream.try_clone() else {
                    continue;
                };
                let mut line = String::new();
                if BufReader::new(read_half).read_line(&mut line).is_err() {
                    continue;
                }
                let mut stream = stream;
                match parse_request(&line, TOKEN) {
                    Some(Request::Session) => {
                        let body = served.lock().unwrap().to_json().to_string();
                        let _ = stream.write_all(body.as_bytes());
                        let _ = stream.write_all(b"\n");
                    }
                    Some(Request::SetSession { session_id, otp }) => {
                        let reply = match validate_credentials(session_id, otp) {
                            Ok(()) => {
                                let mut guard = served.lock().unwrap();
                                guard.session_id = session_id.to_string();
                                guard.otp = otp.to_string();
                                guard.cloud_state = "connecting".to_string();
                                "OK\n".to_string()
                            }
                            Err(e) => format!("ERR {e}\n"),
                        };
                        let _ = stream.write_all(reply.as_bytes());
                    }
                    Some(Request::Ping) => {
                        let _ = stream.write_all(b"OK 4242\n");
                    }
                    _ => {}
                }
                let _ = stream.flush();
            }
        });

        let read = |reply: Option<String>| {
            let reply = reply.expect("the fake host answered");
            let value: serde_json::Value =
                serde_json::from_str(reply.trim()).expect("the reply is JSON");
            SessionInfo::from_json(&value).expect("the reply is a SessionInfo")
        };

        let before = read(query_running_instance(&ctrl, Command::Session));
        assert_eq!(before.session_id, "WLZ7JHNBC2RG");
        assert_eq!(before.otp, "111111");
        assert!(before.public_sessions);

        let refused = query_running_instance(
            &ctrl,
            Command::SetSession {
                session_id: "BOGUS/ID".into(),
                otp: "123456".into(),
            },
        )
        .expect("the fake host answered");
        assert!(
            refused.starts_with("ERR "),
            "a credential the host could not serve must be refused, not applied: {refused:?}"
        );

        let applied = query_running_instance(
            &ctrl,
            Command::SetSession {
                session_id: "MQZ8VLBXBZPH".into(),
                otp: "418203".into(),
            },
        )
        .expect("the fake host answered");
        assert!(is_ok_reply(&applied), "set reply: {applied:?}");

        let after = read(query_running_instance(&ctrl, Command::Session));
        assert_eq!(after.session_id, "MQZ8VLBXBZPH");
        assert_eq!(after.otp, "418203");
        assert_eq!(after.cloud_state, "connecting");

        host.join().expect("the fake host thread");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
