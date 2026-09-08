use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt;
use rand::Rng;
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
}

impl Command {
    fn verb(&self) -> &'static str {
        match self {
            Command::Focus => "FOCUS",
            Command::Quit => "QUIT",
            Command::Stats => "STATS",
        }
    }
}

pub fn acquire_lock(path: &Path) -> Option<File> {
    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    match file.try_lock_exclusive() {
        Ok(true) => Some(file),
        _ => None,
    }
}

pub fn wait_for_lock(path: &Path, timeout: Duration) -> Option<File> {
    let start = Instant::now();
    loop {
        if let Some(file) = acquire_lock(path) {
            return Some(file);
        }
        if start.elapsed() >= timeout {
            return None;
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

pub fn start_control_server<Q>(
    app: AppHandle,
    ctrl_path: PathBuf,
    focus: fn(&tauri::WebviewWindow),
    quit: Q,
) -> std::io::Result<()>
where
    Q: Fn(&AppHandle) + Send + 'static,
{
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let token = format!("{:032x}", rand::rng().random::<u128>());
    std::fs::write(&ctrl_path, format!("{port}\n{token}\n"))?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            handle_connection(stream, &app, &token, focus, &quit);
        }
    });
    Ok(())
}

/// Remove the control file so a later `stop` reports "no running instance" instead of dialling
/// a port the OS has since handed to somebody else.
pub fn clear_control_file(ctrl_path: &Path) {
    let _ = std::fs::remove_file(ctrl_path);
}

fn handle_connection<Q>(
    stream: TcpStream,
    app: &AppHandle,
    token: &str,
    focus: fn(&tauri::WebviewWindow),
    quit: &Q,
) where
    Q: Fn(&AppHandle),
{
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut line = String::new();
    if BufReader::new(read_half).read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.trim().splitn(2, ' ');
    if parts.next() != Some(token) {
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
            let json = crate::streamer::telemetry::snapshot_json().to_string();
            let _ = stream.write_all(json.as_bytes());
            let _ = stream.write_all(b"\n");
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
