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
    /// Ask the running host which credentials it is *actually* serving, as one line of JSON.
    ///
    /// `qr`, `status` and `session new` all run in their own process, and a session id lives
    /// only in the host's memory — it is minted at bootstrap (the GUI) or at startup (`serve`)
    /// and never written to `config.json`. Without this verb those commands had nothing to read,
    /// so they each minted a fresh random id and printed join URLs for a session that does not
    /// exist; a client that scanned one got `503 host offline` from the relay.
    ///
    /// **The reply carries the OTP.** That is a deliberate decision, not an oversight: the
    /// socket is loopback-only and every request is authenticated with the 128-bit token in
    /// `screenextend.ctrl`, which is written user-only — and whoever can read that file can
    /// already `QUIT` the host or *set* its credentials with [`Command::SetSession`], so the
    /// OTP is inside the trust boundary, not outside it. It is on the wire so that a setter can
    /// read back what it applied (the desktop UI mirrors these two values and can push its own
    /// copy back over them), which is a check no shipped command could otherwise make. No
    /// shipped command prints it: `session new` prints the OTP it minted itself.
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
/// A `session_id` that is empty means the host is up but has not minted credentials yet — the
/// desktop app does that from its bootstrap, so there is a window after launch where the answer
/// is honestly "not yet" rather than an id.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionInfo {
    pub session_id: String,
    pub otp: String,
    /// Whether the host currently holds a cloud relay client, which is what "public sessions"
    /// means to a *running* host — the persisted `publicSessionsEnabled` is only the intent.
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

    /// Parse a `SESSION` reply. `None` for anything that is not this object — which is also what
    /// a host too old to know the verb answers, since it closes the connection saying nothing.
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

/// One authenticated control request, parsed off the wire.
///
/// Split out from [`handle_connection`] so the wire format has a test that needs no Tauri app:
/// the verbs that touch `AppState` cannot be exercised in a unit test, but every byte of the
/// framing around them can.
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
///
/// The token is compared in constant time, the same as the join OTP in `streamer/session.rs`,
/// so the loopback port cannot be used to guess it a byte at a time.
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
/// `SessionAuth::validate` refuses an empty id or OTP outright, so applying an empty pair would
/// silently take the host off the air; and the request line is space-separated, so a value
/// carrying a space could not have survived the parse anyway. Everything else is a length and
/// charset clamp — the same alphabet the minters use, plus `-` and `_` so an operator-chosen
/// `--session-id` still passes.
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
            let mut out = String::new();
            for line in crate::logbus::get_log_backlog() {
                out.push_str(&line);
                out.push('\n');
            }
            let _ = stream.write_all(out.as_bytes());
            let _ = stream.flush();
        }
        // The two `AppState` verbs are handled by `cli`, which is where the per-OS `platform`
        // alias and the Tauri commands live; this module stays OS-independent, the same way
        // `RELOAD` is a callback rather than a config reader.
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
        .write_all(format!("{token} {}\n", command.wire()).as_bytes())
        .is_ok()
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

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

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
        // The verbs that predate it are unchanged, which is what lets a new CLI drive an older
        // host for everything but SESSION.
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
        // A wrong, short or absent token is indistinguishable from any other: nothing at all.
        assert_eq!(
            parse_request("wrong SESSION ABCDEFGHJKLM 123456", TOKEN),
            None
        );
        assert_eq!(parse_request("SESSION", TOKEN), None);
        assert_eq!(parse_request("", TOKEN), None);
    }

    /// A `SESSION` set with one argument is a set with an empty OTP, not a query — otherwise a
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
        // What a host too old for the verb answers: it closes the connection saying nothing.
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
    ///
    /// This is the round trip the defect needed and did not have — `qr` and `session new` minted
    /// an id per invocation because there was no verb to ask the running host for one.
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
