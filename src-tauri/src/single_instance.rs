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
}

pub fn acquire_lock(path: &Path) -> Option<File> {
    let file = File::options().write(true).create(true).open(path).ok()?;
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

pub fn start_control_server(
    app: AppHandle,
    ctrl_path: PathBuf,
    focus: fn(&tauri::WebviewWindow),
) -> std::io::Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    let token = format!("{:032x}", rand::rng().random::<u128>());
    std::fs::write(&ctrl_path, format!("{port}\n{token}\n"))?;

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            handle_connection(stream, &app, &token, focus);
        }
    });
    Ok(())
}

fn handle_connection(
    stream: TcpStream,
    app: &AppHandle,
    token: &str,
    focus: fn(&tauri::WebviewWindow),
) {
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.trim().splitn(2, ' ');
    if parts.next() != Some(token) {
        return;
    }
    match parts.next() {
        Some("FOCUS") => {
            if let Some(window) = app.get_webview_window("main") {
                let _ = app.run_on_main_thread(move || focus(&window));
            }
        }
        Some("QUIT") => match app.get_webview_window("main") {
            Some(window) => {
                let _ = app.run_on_main_thread(move || {
                    let _ = window.close();
                });
            }
            None => app.exit(0),
        },
        _ => {}
    }
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
    let verb = match command {
        Command::Focus => "FOCUS",
        Command::Quit => "QUIT",
    };
    stream.write_all(format!("{token} {verb}\n").as_bytes()).is_ok()
}
