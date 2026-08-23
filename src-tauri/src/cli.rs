use std::io::Write;

pub enum Outcome {
    LaunchGui,
}

fn exit(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code)
}

#[cfg(windows)]
pub fn attach_console() {
    use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    unsafe {
        let _ = AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
pub fn attach_console() {}

fn generate_session_id() -> String {
    use rand::Rng;
    const ALPHA: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";
    let mut rng = rand::rng();
    (0..12)
        .map(|_| ALPHA[rng.random_range(0..ALPHA.len())] as char)
        .collect()
}

fn generate_otp() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    (0..6)
        .map(|_| char::from(b'0' + rng.random_range(0..10u8)))
        .collect()
}

fn is_ipv4(ip: &str) -> bool {
    ip.parse::<std::net::Ipv4Addr>().is_ok()
}

fn wifi_qr_payload(ssid: &str, password: &str) -> String {
    let escape = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace(';', "\\;")
            .replace(',', "\\,")
            .replace(':', "\\:")
            .replace('"', "\\\"")
    };
    if password.is_empty() {
        format!("WIFI:T:nopass;S:{};;", escape(ssid))
    } else {
        format!("WIFI:T:WPA;S:{};P:{};;", escape(ssid), escape(password))
    }
}

fn print_version() {
    println!("ScreenExtend {}", env!("CARGO_PKG_VERSION"));
}

fn print_help() {
    print!(
        r#"ScreenExtend {version} — turn any device into a wireless second monitor.

USAGE:
    ScreenExtend [SUBCOMMAND]

Run with no arguments to open the desktop app.

SUBCOMMANDS:
    serve                 Run the streaming host headlessly (no window) until Ctrl+C
    status                Show session, network, driver and config state
    qr                    Print join QR codes / URLs for a session
    session new           Generate a fresh session id + OTP and print join info
    devices list          List saved per-device display overrides
    devices set <ip>      Save display overrides for a device
    devices reset <ip>    Clear saved overrides for a device
    network start <ssid> <password>   Start a hosted Wi-Fi network
    network stop          Stop the hosted network
    network status        Show hosted-network state
    network wifi-on       Turn the Wi-Fi radio on
    network wifi-qr       Print a Wi-Fi join QR for the saved hosted network
    config list           Print all persisted settings
    config get <key>      Read a setting (dotted keys, e.g. serverPorts.http)
    config set <key> <v>  Write a setting (value parsed as JSON, else string)
    config path           Print the config.json path
    turn show             Show the TURN relay configuration
    turn set <urls> [--username U] [--credential C]   Configure the TURN relay
    turn clear            Remove the TURN relay configuration
    account name [value]  Show or set the host display name
    account whoami        Print the OS username
    account avatar set <path> | remove | show
    autostart enable | disable | status
    drivers install | remove          Install/remove the virtual display driver
    doctor                Check system requirements and permissions
    logs                  Print the in-process log backlog
    update check | install
    display-settings      Open the OS display settings (arrange displays)
    stop                  Ask a running instance to quit

SERVE OPTIONS:
    --http-port <n>       Override the HTTP port
    --https-port <n>      Override the HTTPS port
    --session-id <id>     Use a specific session id (default: random)
    --otp <code>          Use a specific OTP (default: random)
    --no-cloud            Do not register a public "Anywhere (Internet)" session
    --no-qr               Do not render QR codes in the terminal
    --software-encode     Force CPU (software) video encoding
    -v, --verbose         Print internal diagnostic logs to the terminal

GLOBAL:
    -h, --help            Show this help
    -V, --version         Show the version

Most commands accept --json for machine-readable output.
"#,
        version = env!("CARGO_PKG_VERSION"),
    );
}

pub fn fast_path() {
    if std::env::args_os().nth(1).is_none() {
        return;
    }
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let first = raw[0].as_str();
    if first == "help" || raw.iter().any(|a| a == "-h" || a == "--help") {
        attach_console();
        print_help();
        exit(0);
    }
    if first == "version" || raw.iter().any(|a| a == "-V" || a == "--version") {
        attach_console();
        print_version();
        exit(0);
    }
}

pub fn dispatch(app: &tauri::AppHandle) -> Outcome {
    if std::env::args_os().nth(1).is_none() {
        return Outcome::LaunchGui;
    }
    attach_console();
    route(app)
}

#[cfg(not(any(windows, target_os = "macos")))]
fn route(_app: &tauri::AppHandle) -> Outcome {
    Outcome::LaunchGui
}

#[cfg(any(windows, target_os = "macos"))]
pub use desktop::route;

#[cfg(any(windows, target_os = "macos"))]
mod desktop {
    use super::*;

    use std::sync::Arc;

    use serde_json::Value;
    use tauri::{Listener, Manager};
    use tauri_plugin_cli::{CliExt, Matches};
    use tauri_plugin_store::{Store, StoreExt};

    use crate::single_instance;
    use crate::streamer::session::{
        DEFAULT_DISCONNECT_GRACE_SECS, DEFAULT_HTTPS_PORT, DEFAULT_HTTP_PORT,
    };

    #[cfg(target_os = "windows")]
    use crate::windows_utils as platform;

    #[cfg(target_os = "macos")]
    use crate::macos_utils as platform;

    const CLOUD_SESSION_DOMAIN: &str = "session.screenextend.app";
    const CONFIG_STORE: &str = "config.json";

    // small helpers over the plugin's matches

    fn arg_str(m: &Matches, key: &str) -> Option<String> {
        m.args
            .get(key)
            .and_then(|d| d.value.as_str().map(str::to_string))
    }

    fn arg_flag(m: &Matches, key: &str) -> bool {
        m.args
            .get(key)
            .map(|d| d.value.as_bool().unwrap_or(d.occurrences > 0))
            .unwrap_or(false)
    }

    fn arg_u16(m: &Matches, key: &str) -> Option<u16> {
        arg_str(m, key).and_then(|s| s.trim().parse::<u16>().ok())
    }

    fn wants_json(m: &Matches) -> bool {
        arg_flag(m, "json")
    }

    fn sub(m: &Matches) -> Option<(&str, &Matches)> {
        m.subcommand
            .as_deref()
            .map(|s| (s.name.as_str(), &s.matches))
    }

    // config store access

    type Cfg = Store<tauri::Wry>;

    fn open_store(app: &tauri::AppHandle) -> Arc<Cfg> {
        match app.store(CONFIG_STORE) {
            Ok(store) => store,
            Err(e) => {
                eprintln!("error: cannot open config store: {e}");
                exit(1);
            }
        }
    }

    fn get_nested(store: &Cfg, key: &str) -> Option<Value> {
        let mut parts = key.split('.');
        let head = parts.next()?;
        let mut cur = store.get(head)?;
        for part in parts {
            cur = cur.get(part)?.clone();
        }
        Some(cur)
    }

    fn set_nested(store: &Cfg, key: &str, value: Value) {
        match key.split_once('.') {
            Some((head, rest)) => {
                let mut root = store
                    .get(head)
                    .unwrap_or_else(|| Value::Object(Default::default()));
                if !root.is_object() {
                    root = Value::Object(Default::default());
                }
                set_path(&mut root, rest, value);
                store.set(head, root);
            }
            None => store.set(key, value),
        }
    }

    fn set_path(node: &mut Value, path: &str, value: Value) {
        match path.split_once('.') {
            Some((head, rest)) => {
                let obj = node.as_object_mut().expect("object node");
                let child = obj
                    .entry(head.to_string())
                    .or_insert_with(|| Value::Object(Default::default()));
                if !child.is_object() {
                    *child = Value::Object(Default::default());
                }
                set_path(child, rest, value);
            }
            None => {
                node.as_object_mut()
                    .expect("object node")
                    .insert(path.to_string(), value);
            }
        }
    }

    fn cfg_str(store: &Cfg, key: &str) -> Option<String> {
        get_nested(store, key).and_then(|v| v.as_str().map(str::to_string))
    }

    fn cfg_u64(store: &Cfg, key: &str) -> Option<u64> {
        get_nested(store, key).and_then(|v| v.as_u64())
    }

    fn cfg_bool(store: &Cfg, key: &str) -> Option<bool> {
        get_nested(store, key).and_then(|v| v.as_bool())
    }

    // host / instance detection

    fn host_running(app: &tauri::AppHandle) -> bool {
        let Ok(dir) = app.path().app_local_data_dir() else {
            return false;
        };
        let lock_path = dir.join(single_instance::LOCK_FILE);
        match single_instance::acquire_lock(&lock_path) {
            Some(file) => {
                drop(file);
                false
            }
            None => true,
        }
    }

    // QR / join URLs

    fn cloud_url(session_id: &str) -> String {
        format!("https://{CLOUD_SESSION_DOMAIN}/?id={session_id}")
    }

    fn lan_entries(
        adapters: &[platform::networking::NetworkInfo],
        port: u16,
        session_id: &str,
    ) -> Vec<(String, String)> {
        adapters
            .iter()
            .filter_map(|adapter| {
                let ipv4 = adapter.ip_addresses.iter().find(|ip| is_ipv4(ip))?;
                Some((
                    adapter.network_name.clone(),
                    format!("http://{ipv4}:{port}/?id={session_id}"),
                ))
            })
            .collect()
    }

    fn render_qr(url: &str) {
        if let Err(e) = qr2term::print_qr(url) {
            eprintln!("(could not render QR: {e:?})");
        }
    }

    fn print_join(title: &str, url: &str, qr: bool) {
        println!("\n{title}\n  {url}");
        if qr {
            render_qr(url);
        }
    }

    // dispatch

    pub fn route(app: &tauri::AppHandle) -> Outcome {
        let matches = match app.cli().matches() {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                eprintln!("Run `ScreenExtend --help` for usage.");
                exit(2);
            }
        };

        let Some((name, m)) = sub(&matches) else {
            return Outcome::LaunchGui;
        };

        match name {
            "serve" => run_serve(app, m),
            "status" => run_status(app, m),
            "qr" => run_qr(app, m),
            "session" => run_session(app, m),
            "devices" => run_devices(app, m),
            "network" => run_network(app, m),
            "config" => run_config(app, m),
            "turn" => run_turn(app, m),
            "account" => run_account(app, m),
            "autostart" => run_autostart(app, m),
            "drivers" => run_drivers(app, m),
            "doctor" => run_doctor(app, m),
            "logs" => run_logs(m),
            "update" => run_update(app, m),
            "stop" => run_stop(app),
            "display-settings" => run_display_settings(),
            "installdrivers" | "removedrivers" | "hostednetwork" => Outcome::LaunchGui,
            other => {
                eprintln!("error: unknown subcommand `{other}`");
                eprintln!("Run `ScreenExtend --help` for usage.");
                exit(2);
            }
        }
    }

    // serve (headless host)

    fn run_serve(app: &tauri::AppHandle, m: &Matches) -> ! {
        crate::logbus::set_verbose(arg_flag(m, "verbose"));

        let lock_dir = match app.path().app_local_data_dir() {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("error: {e}");
                exit(1);
            }
        };
        let _ = std::fs::create_dir_all(&lock_dir);
        let lock_path = lock_dir.join(single_instance::LOCK_FILE);
        match single_instance::acquire_lock(&lock_path) {
            Some(file) => std::mem::forget(file), // hold for the process lifetime
            None => {
                eprintln!(
                    "ScreenExtend is already running. Quit it first, or run `ScreenExtend stop`."
                );
                exit(1);
            }
        }

        println!("Starting the ScreenExtend host…");
        let ready = tauri::async_runtime::block_on(platform::setup(app.clone()));
        if !ready {
            eprintln!("error: could not initialize the virtual display driver.");
            eprintln!("Install it first (Administrator): ScreenExtend drivers install");
            exit(1);
        }
        let state = app.state::<platform::AppState>();
        crate::streamer::input::prime();

        let store = app.store(CONFIG_STORE).ok();
        let http = arg_u16(m, "http-port")
            .or_else(|| {
                store
                    .as_deref()
                    .and_then(|s| cfg_u64(s, "serverPorts.http"))
                    .map(|n| n as u16)
            })
            .unwrap_or(DEFAULT_HTTP_PORT);
        let https = arg_u16(m, "https-port")
            .or_else(|| {
                store
                    .as_deref()
                    .and_then(|s| cfg_u64(s, "serverPorts.https"))
                    .map(|n| n as u16)
            })
            .unwrap_or(DEFAULT_HTTPS_PORT);
        state.server_ports.set(http, https);

        let software = arg_flag(m, "software-encode")
            || store
                .as_deref()
                .and_then(|s| cfg_bool(s, "disableGpuEncode"))
                .unwrap_or(false);
        if software {
            state
                .disable_gpu_encode
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if let Some(store) = store.as_deref() {
            let urls = cfg_str(store, "turnConfig.urls").unwrap_or_default();
            if !urls.is_empty() {
                platform::set_turn_config(
                    app.state(),
                    urls,
                    cfg_str(store, "turnConfig.username").unwrap_or_default(),
                    cfg_str(store, "turnConfig.credential").unwrap_or_default(),
                );
            }
            let grace =
                cfg_u64(store, "disconnectGraceSecs").unwrap_or(DEFAULT_DISCONNECT_GRACE_SECS);
            platform::set_disconnect_grace(app.state(), grace as u32);
            apply_saved_devices(app, store);
        }

        let session_id = arg_str(m, "session-id").unwrap_or_else(generate_session_id);
        let otp = arg_str(m, "otp").unwrap_or_else(generate_otp);
        platform::set_session_credentials(app.state(), session_id.clone(), otp.clone());

        platform::networking::watch_for_network_changes(app.clone());

        let cloud = !arg_flag(m, "no-cloud")
            && store
                .as_deref()
                .and_then(|s| cfg_bool(s, "publicSessionsEnabled"))
                .unwrap_or(true);
        if cloud {
            platform::register_cloud_session(app.clone(), app.state(), session_id.clone());
        }

        install_device_listeners(app);

        print_serve_banner(http, &session_id, &otp, cloud, !arg_flag(m, "no-qr"));

        println!("\nHost is running. Press Ctrl+C to stop.");
        let _ = std::io::stdout().flush();

        let (tx, rx) = std::sync::mpsc::channel();
        let _ = ctrlc::set_handler(move || {
            let _ = tx.send(());
        });
        let _ = rx.recv();

        println!("\nStopping…");
        platform::remove_all_displays(&state.virtual_display);
        exit(0);
    }

    fn apply_saved_devices(app: &tauri::AppHandle, store: &Cfg) {
        let Some(devices) = get_nested(store, "devices").and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        }) else {
            return;
        };
        for device in devices {
            let Some(ip) = device.get("ip").and_then(|v| v.as_str()) else {
                continue;
            };
            let num = |key: &str, default: u32| {
                device
                    .get(key)
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32)
                    .unwrap_or(default)
            };
            let orientation = device
                .get("orientation")
                .and_then(|v| v.as_str())
                .unwrap_or("Landscape")
                .to_string();
            let control = device
                .get("remoteControl")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            platform::set_device_override(
                app.state(),
                ip.to_string(),
                num("scale", 100),
                orientation,
                num("refreshRate", 60),
                num("videoScale", 100),
                num("videoQuality", 15),
                control,
            );
        }
    }

    fn install_device_listeners(app: &tauri::AppHandle) {
        app.listen("device-join", |event| {
            if let Ok(v) = serde_json::from_str::<Value>(event.payload()) {
                let ip = v.get("ip").and_then(|x| x.as_str()).unwrap_or("?");
                let os = v.get("os").and_then(|x| x.as_str()).unwrap_or("");
                let size = v.get("screenSize").and_then(|x| x.as_str()).unwrap_or("");
                println!("+ device connected: {ip}  {os} {size}");
            }
        });
        app.listen("device-remove", |event| {
            if let Ok(v) = serde_json::from_str::<Value>(event.payload()) {
                let ip = v.get("ip").and_then(|x| x.as_str()).unwrap_or("?");
                println!("- device disconnected: {ip}");
            }
        });
    }

    fn print_serve_banner(http_port: u16, session_id: &str, otp: &str, cloud: bool, qr: bool) {
        println!("\n──────────────────────────────────────────");
        println!("Session:  {session_id}");
        println!("OTP:      {otp}");
        println!("──────────────────────────────────────────");

        let adapters = platform::networking::cli_adapters();
        let lan = lan_entries(&adapters, http_port, session_id);
        if lan.is_empty() {
            println!("\nNo local networks found yet — connect to Wi-Fi/Ethernet, or start a hosted network.");
        }
        for (title, url) in lan {
            print_join(&title, &url, qr);
        }
        if cloud {
            print_join("Anywhere (Internet)", &cloud_url(session_id), qr);
        }
    }

    // status

    fn run_status(app: &tauri::AppHandle, m: &Matches) -> ! {
        let store = open_store(app);
        let running = host_running(app);
        let session_id = arg_str(m, "session-id");
        let http = cfg_u64(&store, "serverPorts.http").unwrap_or(DEFAULT_HTTP_PORT as u64) as u16;
        let adapters = platform::networking::cli_adapters();

        if wants_json(m) {
            let networks: Vec<Value> = adapters
                .iter()
                .map(|a| {
                    serde_json::json!({
                        "name": a.network_name,
                        "ips": a.ip_addresses,
                    })
                })
                .collect();
            let out = serde_json::json!({
                "hostRunning": running,
                "name": cfg_str(&store, "name"),
                "publicSessionsEnabled": cfg_bool(&store, "publicSessionsEnabled").unwrap_or(true),
                "serverPorts": {
                    "http": http,
                    "https": cfg_u64(&store, "serverPorts.https").unwrap_or(DEFAULT_HTTPS_PORT as u64),
                },
                "turnConfigured": !cfg_str(&store, "turnConfig.urls").unwrap_or_default().is_empty(),
                "hostedNetworkName": cfg_str(&store, "hostedNetworkCredentials.name"),
                "networks": networks,
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            exit(0);
        }

        println!("ScreenExtend {}", env!("CARGO_PKG_VERSION"));
        println!(
            "  Host running:      {}",
            if running { "yes" } else { "no" }
        );
        if let Some(name) = cfg_str(&store, "name") {
            println!("  Account name:      {name}");
        }
        println!(
            "  Public sessions:   {}",
            if cfg_bool(&store, "publicSessionsEnabled").unwrap_or(true) {
                "enabled"
            } else {
                "disabled"
            }
        );
        println!(
            "  Server ports:      http {http}, https {}",
            cfg_u64(&store, "serverPorts.https").unwrap_or(DEFAULT_HTTPS_PORT as u64)
        );
        let turn = cfg_str(&store, "turnConfig.urls").unwrap_or_default();
        println!(
            "  TURN relay:        {}",
            if turn.is_empty() {
                "not configured".to_string()
            } else {
                turn
            }
        );
        if let Some(hn) = cfg_str(&store, "hostedNetworkCredentials.name") {
            println!("  Hosted network:    {hn}");
        }

        println!("\nNetworks:");
        if adapters.is_empty() {
            println!("  (none active)");
        }
        for adapter in &adapters {
            let ipv4 = adapter.ip_addresses.iter().find(|ip| is_ipv4(ip));
            match (ipv4, &session_id) {
                (Some(ip), Some(id)) => {
                    println!("  {}: http://{ip}:{http}/?id={id}", adapter.network_name)
                }
                (Some(ip), None) => println!("  {}: {ip}", adapter.network_name),
                (None, _) => println!("  {}: (no IPv4)", adapter.network_name),
            }
        }
        if session_id.is_none() {
            println!(
                "\nA session id is issued at runtime. Run `ScreenExtend serve`, or pass --session-id\nto build full join URLs, or `ScreenExtend qr` to generate one."
            );
        }
        exit(0);
    }

    // qr / session

    fn run_qr(app: &tauri::AppHandle, m: &Matches) -> ! {
        let session_id = arg_str(m, "session-id").unwrap_or_else(generate_session_id);
        let target = arg_str(m, "target").unwrap_or_else(|| "all".to_string());
        let store = app.store(CONFIG_STORE).ok();
        let http = store
            .as_deref()
            .and_then(|s| cfg_u64(s, "serverPorts.http"))
            .unwrap_or(DEFAULT_HTTP_PORT as u64) as u16;

        let mut entries: Vec<(String, String)> = Vec::new();
        if target == "lan" || target == "all" {
            entries.extend(lan_entries(
                &platform::networking::cli_adapters(),
                http,
                &session_id,
            ));
        }
        if target == "cloud" || target == "all" {
            entries.push(("Anywhere (Internet)".to_string(), cloud_url(&session_id)));
        }

        if wants_json(m) {
            let out = serde_json::json!({
                "sessionId": session_id,
                "targets": entries.iter().map(|(t, u)| serde_json::json!({"title": t, "url": u})).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            exit(0);
        }

        println!("Session: {session_id}");
        if entries.is_empty() {
            println!("No join targets (no local networks found).");
        }
        let render = !arg_flag(m, "no-render");
        for (title, url) in entries {
            print_join(&title, &url, render);
        }
        exit(0);
    }

    fn run_session(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend session new [--json]");
            exit(2);
        };
        match name {
            "new" => {
                let session_id = generate_session_id();
                let otp = generate_otp();
                let store = app.store(CONFIG_STORE).ok();
                let http = store
                    .as_deref()
                    .and_then(|s| cfg_u64(s, "serverPorts.http"))
                    .unwrap_or(DEFAULT_HTTP_PORT as u64) as u16;
                let cloud = store
                    .as_deref()
                    .and_then(|s| cfg_bool(s, "publicSessionsEnabled"))
                    .unwrap_or(true);
                let mut entries =
                    lan_entries(&platform::networking::cli_adapters(), http, &session_id);
                if cloud {
                    entries.push(("Anywhere (Internet)".to_string(), cloud_url(&session_id)));
                }
                if wants_json(sm) {
                    let out = serde_json::json!({
                        "sessionId": session_id,
                        "otp": otp,
                        "targets": entries.iter().map(|(t, u)| serde_json::json!({"title": t, "url": u})).collect::<Vec<_>>(),
                    });
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                    exit(0);
                }
                println!("Session: {session_id}");
                println!("OTP:     {otp}");
                let render = !arg_flag(sm, "no-render");
                for (title, url) in entries {
                    print_join(&title, &url, render);
                }
                println!(
                    "\nThis is a one-off credential for display. Start the host with:\n  ScreenExtend serve --session-id {session_id} --otp {otp}"
                );
                exit(0);
            }
            other => {
                eprintln!("error: unknown session subcommand `{other}`");
                exit(2);
            }
        }
    }

    // devices

    fn run_devices(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend devices <list|set|reset>");
            exit(2);
        };
        let store = open_store(app);
        match name {
            "list" => {
                let devices = get_nested(&store, "devices")
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .unwrap_or_default();
                if wants_json(sm) {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&Value::Array(devices)).unwrap()
                    );
                    exit(0);
                }
                if devices.is_empty() {
                    println!("No saved device overrides.");
                    exit(0);
                }
                println!(
                    "{:<16} {:>6} {:<10} {:>8} {:>6} {:>8} {:>8}",
                    "IP", "SCALE", "ORIENT", "REFRESH", "VSCALE", "VQUAL", "CONTROL"
                );
                for d in &devices {
                    let s = |k: &str| d.get(k).cloned().unwrap_or(Value::Null);
                    let ip = s("ip").as_str().unwrap_or("").to_string();
                    let scale = s("scale").as_u64().unwrap_or(100);
                    let orient = s("orientation").as_str().unwrap_or("Landscape").to_string();
                    let refresh = s("refreshRate").as_u64().unwrap_or(60);
                    let vscale = s("videoScale").as_u64().unwrap_or(100);
                    let vqual = s("videoQuality").as_u64().unwrap_or(15);
                    let control = s("remoteControl").as_bool().unwrap_or(true);
                    println!(
                        "{ip:<16} {:>5}% {orient:<10} {:>6}Hz {:>5}% {vqual:>8} {:>8}",
                        scale,
                        refresh,
                        vscale,
                        if control { "on" } else { "off" }
                    );
                }
                exit(0);
            }
            "set" => {
                let Some(ip) = arg_str(sm, "ip") else {
                    eprintln!("usage: ScreenExtend devices set <ip> [--scale N] [--orientation Portrait|Landscape] [--refresh-rate N] [--video-scale N] [--video-quality N] [--control on|off]");
                    exit(2);
                };
                let mut existing = get_nested(&store, "devices")
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .unwrap_or_default();
                let mut device = existing
                    .iter()
                    .find(|d| d.get("ip").and_then(|v| v.as_str()) == Some(ip.as_str()))
                    .cloned()
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "ip": ip, "name": "", "scale": 100, "orientation": "Landscape",
                            "refreshRate": 60, "videoScale": 100, "videoQuality": 15,
                            "remoteControl": true, "os": "", "screenSize": ""
                        })
                    });
                let obj = device.as_object_mut().unwrap();
                obj.insert("ip".into(), Value::String(ip.clone()));
                if let Some(v) = arg_str(sm, "scale").and_then(|s| s.parse::<u32>().ok()) {
                    obj.insert("scale".into(), Value::from(v.clamp(25, 200)));
                }
                if let Some(v) = arg_str(sm, "orientation") {
                    obj.insert("orientation".into(), Value::String(v));
                }
                if let Some(v) = arg_str(sm, "refresh-rate").and_then(|s| s.parse::<u32>().ok()) {
                    obj.insert("refreshRate".into(), Value::from(v.clamp(15, 500)));
                }
                if let Some(v) = arg_str(sm, "video-scale").and_then(|s| s.parse::<u32>().ok()) {
                    obj.insert("videoScale".into(), Value::from(v.clamp(10, 100)));
                }
                if let Some(v) = arg_str(sm, "video-quality").and_then(|s| s.parse::<u32>().ok()) {
                    obj.insert("videoQuality".into(), Value::from(v.clamp(1, 51)));
                }
                if let Some(v) = arg_str(sm, "control") {
                    obj.insert("remoteControl".into(), Value::Bool(v == "on"));
                }

                existing.retain(|d| d.get("ip").and_then(|v| v.as_str()) != Some(ip.as_str()));
                existing.push(device.clone());
                store.set("devices", Value::Array(existing));
                if let Err(e) = store.save() {
                    eprintln!("error: failed to save config: {e}");
                    exit(1);
                }
                println!("Saved override for {ip}: {device}");
                if host_running(app) {
                    println!("(A host is running — restart it or reconnect the device to apply.)");
                }
                exit(0);
            }
            "reset" => {
                let Some(ip) = arg_str(sm, "ip") else {
                    eprintln!("usage: ScreenExtend devices reset <ip>");
                    exit(2);
                };
                let mut existing = get_nested(&store, "devices")
                    .and_then(|v| match v {
                        Value::Array(a) => Some(a),
                        _ => None,
                    })
                    .unwrap_or_default();
                let before = existing.len();
                existing.retain(|d| d.get("ip").and_then(|v| v.as_str()) != Some(ip.as_str()));
                store.set("devices", Value::Array(existing.clone()));
                if let Err(e) = store.save() {
                    eprintln!("error: failed to save config: {e}");
                    exit(1);
                }
                if existing.len() == before {
                    println!("No saved override for {ip}.");
                } else {
                    println!("Cleared saved override for {ip}.");
                }
                exit(0);
            }
            other => {
                eprintln!("error: unknown devices subcommand `{other}`");
                exit(2);
            }
        }
    }

    // network

    fn run_network(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend network <start|stop|status|wifi-on|wifi-qr>");
            exit(2);
        };
        match name {
            "wifi-on" => {
                if platform::hosted_network::turn_on_wifi() {
                    println!("Wi-Fi is on.");
                    exit(0);
                } else {
                    eprintln!("Could not turn on Wi-Fi.");
                    exit(1);
                }
            }
            "wifi-qr" => {
                let store = open_store(app);
                let ssid = cfg_str(&store, "hostedNetworkCredentials.name").unwrap_or_default();
                let password =
                    cfg_str(&store, "hostedNetworkCredentials.password").unwrap_or_default();
                if ssid.is_empty() {
                    eprintln!(
                        "No saved hosted-network name. Set one in the app or with `config set`."
                    );
                    exit(1);
                }
                let payload = wifi_qr_payload(&ssid, &password);
                if wants_json(sm) {
                    println!(
                        "{}",
                        serde_json::json!({"ssid": ssid, "password": password, "qr": payload})
                    );
                    exit(0);
                }
                println!("Network:  {ssid}");
                println!(
                    "Password: {}",
                    if password.is_empty() {
                        "(open)"
                    } else {
                        &password
                    }
                );
                render_qr(&payload);
                exit(0);
            }
            "start" => network_start(app, sm),
            "stop" => network_stop(),
            "status" => network_status(sm),
            other => {
                eprintln!("error: unknown network subcommand `{other}`");
                exit(2);
            }
        }
    }

    #[cfg(windows)]
    fn network_started() -> bool {
        std::process::Command::new("netsh")
            .args(["wlan", "show", "hostednetwork"])
            .output()
            .map(|o| {
                let out = String::from_utf8_lossy(&o.stdout);
                out.split("Status")
                    .any(|s| s.trim().starts_with(": Started"))
            })
            .unwrap_or(false)
    }

    #[cfg(windows)]
    fn network_start(app: &tauri::AppHandle, m: &Matches) -> ! {
        let (Some(ssid), Some(password)) = (arg_str(m, "ssid"), arg_str(m, "password")) else {
            eprintln!("usage: ScreenExtend network start <ssid> <password>");
            exit(2);
        };
        let exe = std::env::current_exe().unwrap_or_default();
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("hostednetwork").arg(&ssid).arg(&password);
        println!("Requesting elevation to start the hosted network…");
        let _ = elevated_command::Command::new(cmd).output();
        if network_started() {
            println!("Hosted network '{ssid}' started.");
            let _ = app;
            exit(0);
        }
        eprintln!("Could not start the hosted network. Your Wi-Fi adapter may not support it.");
        exit(1);
    }

    #[cfg(windows)]
    fn network_stop() -> ! {
        let ok = std::process::Command::new("netsh")
            .args(["wlan", "stop", "hostednetwork"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            println!("Hosted network stopped.");
            exit(0);
        }
        eprintln!("Could not stop the hosted network.");
        exit(1);
    }

    #[cfg(windows)]
    fn network_status(m: &Matches) -> ! {
        let started = network_started();
        if wants_json(m) {
            println!("{}", serde_json::json!({"started": started}));
        } else {
            println!(
                "Hosted network: {}",
                if started { "started" } else { "stopped" }
            );
        }
        exit(0);
    }

    #[cfg(not(windows))]
    fn network_start(_app: &tauri::AppHandle, _m: &Matches) -> ! {
        eprintln!("On macOS the hosted network is tied to the running host — start it from the app or `ScreenExtend serve` (it stops when the host exits).");
        exit(1);
    }

    #[cfg(not(windows))]
    fn network_stop() -> ! {
        eprintln!(
            "On macOS the hosted network stops when the host exits. Stop the app or `serve`."
        );
        exit(1);
    }

    #[cfg(not(windows))]
    fn network_status(_m: &Matches) -> ! {
        eprintln!("On macOS the hosted-network state lives in the running host (app or `serve`).");
        exit(1);
    }

    // config

    fn run_config(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend config <list|get|set|path>");
            exit(2);
        };
        match name {
            "path" => {
                match app.path().app_config_dir() {
                    Ok(dir) => println!("{}", dir.join(CONFIG_STORE).display()),
                    Err(e) => {
                        eprintln!("error: {e}");
                        exit(1);
                    }
                }
                exit(0);
            }
            "list" => {
                let store = open_store(app);
                let entries = store.entries();
                if wants_json(sm) {
                    let map: serde_json::Map<String, Value> = entries.into_iter().collect();
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&Value::Object(map)).unwrap()
                    );
                } else if entries.is_empty() {
                    println!("(config is empty — run the app once to create it)");
                } else {
                    for (k, v) in entries {
                        println!("{k} = {v}");
                    }
                }
                exit(0);
            }
            "get" => {
                let store = open_store(app);
                let Some(key) = arg_str(sm, "key") else {
                    eprintln!("usage: ScreenExtend config get <key>");
                    exit(2);
                };
                match get_nested(&store, &key) {
                    Some(v) => println!("{v}"),
                    None => {
                        eprintln!("error: key `{key}` not found");
                        exit(1);
                    }
                }
                exit(0);
            }
            "set" => {
                let store = open_store(app);
                let (Some(key), Some(value)) = (arg_str(sm, "key"), arg_str(sm, "value")) else {
                    eprintln!("usage: ScreenExtend config set <key> <value>");
                    exit(2);
                };
                let parsed = serde_json::from_str::<Value>(&value)
                    .unwrap_or_else(|_| Value::String(value.clone()));
                set_nested(&store, &key, parsed);
                if let Err(e) = store.save() {
                    eprintln!("error: failed to save config: {e}");
                    exit(1);
                }
                println!(
                    "set {key} = {}",
                    get_nested(&store, &key).unwrap_or(Value::Null)
                );
                exit(0);
            }
            other => {
                eprintln!("error: unknown config subcommand `{other}`");
                exit(2);
            }
        }
    }

    // turn

    fn run_turn(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend turn <show|set|clear>");
            exit(2);
        };
        let store = open_store(app);
        match name {
            "show" => {
                let urls = cfg_str(&store, "turnConfig.urls").unwrap_or_default();
                let username = cfg_str(&store, "turnConfig.username").unwrap_or_default();
                let credential = cfg_str(&store, "turnConfig.credential").unwrap_or_default();
                if wants_json(sm) {
                    println!(
                        "{}",
                        serde_json::json!({"urls": urls, "username": username, "credential": credential})
                    );
                } else if urls.is_empty() {
                    println!("TURN relay: not configured");
                } else {
                    println!("URLs:       {urls}");
                    println!("Username:   {username}");
                    println!(
                        "Credential: {}",
                        if credential.is_empty() {
                            "(none)"
                        } else {
                            "(set)"
                        }
                    );
                }
                exit(0);
            }
            "set" => {
                let Some(urls) = arg_str(sm, "urls") else {
                    eprintln!(
                        "usage: ScreenExtend turn set <urls> [--username U] [--credential C]"
                    );
                    exit(2);
                };
                let obj = serde_json::json!({
                    "urls": urls.trim(),
                    "username": arg_str(sm, "username").unwrap_or_default().trim(),
                    "credential": arg_str(sm, "credential").unwrap_or_default().trim(),
                });
                store.set("turnConfig", obj);
                if let Err(e) = store.save() {
                    eprintln!("error: failed to save config: {e}");
                    exit(1);
                }
                println!("TURN relay saved. It applies the next time the host starts.");
                exit(0);
            }
            "clear" => {
                store.set(
                    "turnConfig",
                    serde_json::json!({"urls": "", "username": "", "credential": ""}),
                );
                if let Err(e) = store.save() {
                    eprintln!("error: failed to save config: {e}");
                    exit(1);
                }
                println!("TURN relay cleared.");
                exit(0);
            }
            other => {
                eprintln!("error: unknown turn subcommand `{other}`");
                exit(2);
            }
        }
    }

    // account

    fn run_account(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend account <name|whoami|avatar>");
            exit(2);
        };
        match name {
            "whoami" => {
                println!("{}", crate::get_username());
                exit(0);
            }
            "name" => {
                let store = open_store(app);
                match arg_str(sm, "value") {
                    Some(value) => {
                        let trimmed = value.trim();
                        if trimmed.is_empty() {
                            eprintln!("error: name must not be empty");
                            exit(2);
                        }
                        store.set("name", Value::String(trimmed.to_string()));
                        if let Err(e) = store.save() {
                            eprintln!("error: failed to save config: {e}");
                            exit(1);
                        }
                        println!("Name set to {trimmed}");
                    }
                    None => {
                        println!("{}", cfg_str(&store, "name").unwrap_or_default());
                    }
                }
                exit(0);
            }
            "avatar" => run_avatar(app, sm),
            other => {
                eprintln!("error: unknown account subcommand `{other}`");
                exit(2);
            }
        }
    }

    fn run_avatar(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, sm)) = sub(m) else {
            eprintln!("usage: ScreenExtend account avatar <set|remove|show>");
            exit(2);
        };
        match name {
            "set" => {
                let Some(path) = arg_str(sm, "path") else {
                    eprintln!("usage: ScreenExtend account avatar set <path>");
                    exit(2);
                };
                let bytes = match std::fs::read(&path) {
                    Ok(b) => b,
                    Err(e) => {
                        eprintln!("error: cannot read {path}: {e}");
                        exit(1);
                    }
                };
                if crate::set_avatar(app.clone(), bytes) {
                    println!("Avatar updated.");
                    exit(0);
                }
                eprintln!("error: failed to save avatar.");
                exit(1);
            }
            "remove" => {
                if crate::remove_avatar(app.clone()) {
                    println!("Avatar removed.");
                    exit(0);
                }
                eprintln!("error: failed to remove avatar.");
                exit(1);
            }
            "show" => {
                match crate::get_avatar(app.clone()) {
                    Some(bytes) => println!("Avatar set ({} bytes).", bytes.len()),
                    None => println!("No avatar set (the default logo is used)."),
                }
                exit(0);
            }
            other => {
                eprintln!("error: unknown avatar subcommand `{other}`");
                exit(2);
            }
        }
    }

    // autostart

    fn run_autostart(app: &tauri::AppHandle, m: &Matches) -> ! {
        use tauri_plugin_autostart::ManagerExt;
        let Some((name, _)) = sub(m) else {
            eprintln!("usage: ScreenExtend autostart <enable|disable|status>");
            exit(2);
        };
        let manager = app.autolaunch();
        match name {
            "enable" => match manager.enable() {
                Ok(()) => {
                    println!("Launch at startup enabled.");
                    exit(0);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit(1);
                }
            },
            "disable" => match manager.disable() {
                Ok(()) => {
                    println!("Launch at startup disabled.");
                    exit(0);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit(1);
                }
            },
            "status" => {
                let enabled = manager.is_enabled().unwrap_or(false);
                println!(
                    "Launch at startup: {}",
                    if enabled { "enabled" } else { "disabled" }
                );
                exit(0);
            }
            other => {
                eprintln!("error: unknown autostart subcommand `{other}`");
                exit(2);
            }
        }
    }

    // drivers

    fn run_drivers(app: &tauri::AppHandle, m: &Matches) -> ! {
        let Some((name, _)) = sub(m) else {
            eprintln!("usage: ScreenExtend drivers <install|remove>");
            exit(2);
        };
        match name {
            "install" => {
                println!("Requesting elevation to install the virtual display driver…");
                platform::install_drivers(app.clone());
                println!("Driver install requested.");
                exit(0);
            }
            "remove" => {
                println!("Requesting elevation to remove the virtual display driver…");
                platform::remove_drivers(app.clone());
                println!("Driver removal requested.");
                exit(0);
            }
            other => {
                eprintln!("error: unknown drivers subcommand `{other}`");
                exit(2);
            }
        }
    }

    // doctor

    fn run_doctor(app: &tauri::AppHandle, m: &Matches) -> ! {
        let report = platform::compatibility::check_system_requirements();
        let permissions = platform::permissions::check_permissions();

        if wants_json(m) {
            let out = serde_json::json!({
                "os": { "name": report.os_name, "version": report.os_version, "supported": report.os_supported, "minimum": report.min_os_version },
                "unsupportedApis": report.unsupported_apis.iter().map(|a| serde_json::json!({
                    "name": a.name, "description": a.description, "requiredVersion": a.required_version, "severity": a.severity,
                })).collect::<Vec<_>>(),
                "permissions": permissions.iter().map(|p| serde_json::json!({
                    "key": p.key, "name": p.name, "granted": p.granted, "required": p.required,
                })).collect::<Vec<_>>(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            let _ = app;
            exit(0);
        }

        println!(
            "Operating system: {} — {}",
            report.os_name, report.os_version
        );
        println!("Minimum required: {}", report.min_os_version);
        println!(
            "Supported:        {}",
            if report.os_supported { "yes" } else { "NO" }
        );
        if report.unsupported_apis.is_empty() {
            println!("\nAll capabilities available.");
        } else {
            println!("\nCapability notes:");
            for api in &report.unsupported_apis {
                println!("  [{}] {} — {}", api.severity, api.name, api.description);
            }
        }
        if !permissions.is_empty() {
            println!("\nPermissions:");
            for p in &permissions {
                println!(
                    "  [{}] {} ({})",
                    if p.granted { "granted" } else { "MISSING" },
                    p.name,
                    if p.required { "required" } else { "optional" }
                );
            }
        }
        exit(if report.os_supported { 0 } else { 1 });
    }

    // logs

    fn run_logs(m: &Matches) -> ! {
        let lines = crate::logbus::get_log_backlog();
        let take = arg_str(m, "lines")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        let start = lines.len().saturating_sub(take);
        if lines.is_empty() {
            println!("(no logs in this process — the backlog lives in the running host; try `ScreenExtend serve`)");
        }
        for line in &lines[start..] {
            println!("{line}");
        }
        exit(0);
    }

    // update

    fn run_update(app: &tauri::AppHandle, m: &Matches) -> ! {
        use tauri_plugin_updater::UpdaterExt;
        let Some((name, _)) = sub(m) else {
            eprintln!("usage: ScreenExtend update <check|install>");
            exit(2);
        };
        let updater = match app.updater() {
            Ok(u) => u,
            Err(e) => {
                eprintln!("error: updater unavailable: {e}");
                exit(1);
            }
        };
        match name {
            "check" => match tauri::async_runtime::block_on(updater.check()) {
                Ok(Some(update)) => {
                    println!("Update available: v{}", update.version);
                    if let Some(body) = &update.body {
                        if !body.trim().is_empty() {
                            println!("\n{}", body.trim());
                        }
                    }
                    exit(0);
                }
                Ok(None) => {
                    println!("You're up to date (v{}).", env!("CARGO_PKG_VERSION"));
                    exit(0);
                }
                Err(e) => {
                    eprintln!("error: update check failed: {e}");
                    exit(1);
                }
            },
            "install" => match tauri::async_runtime::block_on(updater.check()) {
                Ok(Some(update)) => {
                    println!("Downloading v{}…", update.version);
                    let result = tauri::async_runtime::block_on(
                        update.download_and_install(|_chunk, _total| {}, || {}),
                    );
                    match result {
                        Ok(()) => {
                            println!("Installed. Restarting…");
                            app.restart();
                        }
                        Err(e) => {
                            eprintln!("error: install failed: {e}");
                            exit(1);
                        }
                    }
                }
                Ok(None) => {
                    println!("Already up to date.");
                    exit(0);
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    exit(1);
                }
            },
            other => {
                eprintln!("error: unknown update subcommand `{other}`");
                exit(2);
            }
        }
    }

    // stop

    fn run_stop(app: &tauri::AppHandle) -> ! {
        let Ok(dir) = app.path().app_local_data_dir() else {
            eprintln!("error: could not resolve app data directory");
            exit(1);
        };
        let ctrl_path = dir.join(single_instance::CTRL_FILE);
        if single_instance::signal_running_instance(&ctrl_path, single_instance::Command::Quit) {
            println!("Asked the running instance to quit.");
            exit(0);
        }
        eprintln!("No running ScreenExtend instance found.");
        exit(1);
    }

    // display settings

    fn run_display_settings() -> ! {
        #[cfg(windows)]
        let spawned = std::process::Command::new("control")
            .arg("desk.cpl")
            .spawn()
            .is_ok();
        #[cfg(target_os = "macos")]
        let spawned = std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.displays")
            .spawn()
            .is_ok();
        if spawned {
            println!("Opened display settings.");
            exit(0);
        }
        eprintln!("Could not open display settings.");
        exit(1);
    }
}
