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
    logs                  Print the running host's log backlog
    update check | install
    display-settings      Open the OS display settings (arrange displays)
    stop                  Ask a running instance to quit

SERVE OPTIONS:
    --http-port <n>       Override the HTTP port
    --https-port <n>      Override the HTTPS port
    --session-id <id>     Use a specific session id (default: random). `-` reads stdin;
                          SCREENEXTEND_SESSION_ID is read when the flag is absent.
    --otp <code>          Use a specific OTP (default: random). `-` reads stdin.
    --otp-file <path>     Read the OTP from a file. Prefer this or SCREENEXTEND_OTP:
                          a command line is readable by every process on the machine.
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

fn user_args() -> Vec<std::ffi::OsString> {
    std::env::args_os()
        .skip(1)
        .filter(|a| !a.to_string_lossy().starts_with("-psn_"))
        .collect()
}

#[cfg(target_os = "macos")]
fn launched_without_terminal() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 0 && libc::isatty(libc::STDERR_FILENO) == 0 }
}

#[cfg(not(target_os = "macos"))]
fn launched_without_terminal() -> bool {
    false
}

pub fn fast_path() {
    let raw: Vec<String> = user_args()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let Some(first) = raw.first().map(String::as_str) else {
        return;
    };
    // Only the first token counts. Scanning the whole vector meant `config set key -h` printed
    // the global help and exited 0 instead of storing `-h`, which a script cannot tell from
    // success — and every value-taking flag had the same hole. Per-subcommand help still works:
    // `tauri-plugin-cli` injects `help`/`version` into each declared subcommand, and `probe`
    // parses its own.
    if first == "help" || first == "-h" || first == "--help" {
        attach_console();
        print_help();
        exit(0);
    }
    if first == "version" || first == "-V" || first == "--version" {
        attach_console();
        print_version();
        exit(0);
    }
    #[cfg(target_os = "macos")]
    if first == "audio-recover" {
        crate::macos_utils::audio::legacy::routing::watchdog_recover();
        exit(0);
    }
}

pub fn dispatch(app: &tauri::AppHandle) -> Outcome {
    let args = user_args();
    if args.is_empty() {
        return Outcome::LaunchGui;
    }
    attach_console();
    // `probe` is intercepted here rather than declared in `tauri.conf.json` under
    // `plugins.cli`. The harness has ~25 tuning flags parsed by hand in
    // `streamer::config::Config::from_args`, and `tauri-plugin-cli` rejects any argument it has
    // not been told about — declaring them twice, in two different syntaxes, to have them
    // parsed twice would be the worse trade. It stays undocumented in `--help` because it is a
    // developer tool, not a shipped command.
    if args[0] == "probe" {
        return probe::run(app);
    }
    route(app)
}

#[cfg(not(any(windows, target_os = "macos")))]
mod probe {
    pub fn run(_app: &tauri::AppHandle) -> super::Outcome {
        eprintln!("error: the probe harness is not built for this platform");
        super::exit(2)
    }
}

/// The developer probe harness, reachable as `ScreenExtend probe`.
///
/// It runs inside the Tauri app on purpose: `--live` and `--whep-selftest` need a real virtual
/// display, which means `platform::setup` and the `AppState` it builds. The standalone
/// `streamer::cli::run` entry point could never do that, which is why it was never wired into
/// `main.rs` and had bit-rotted into unreachable code.
#[cfg(any(windows, target_os = "macos"))]
mod probe {
    use super::exit;
    use super::Outcome;

    #[cfg(target_os = "windows")]
    use crate::windows_utils as platform;

    #[cfg(target_os = "macos")]
    use crate::macos_utils as platform;

    use crate::streamer::config::Config;
    use crate::streamer::Streamer;

    fn print_probe_help() {
        println!(
            r#"ScreenExtend probe — developer capture/encode/transport harness (not a shipped command)

USAGE:
    ScreenExtend probe <MODE> [TUNING FLAGS]

MODES:
    --capture [path]      one captured frame -> PNG           (default capture_probe.png)
    --dxgi [path]         one DXGI-duplication frame -> BMP   (default dxgi_probe.bmp)
    --encode [path]       300 synthetic frames -> Annex-B     (default out.h264)
    --live [path]         150 live frames -> Annex-B          (default live.h264)
    --bitrate             exercise the adaptive-bitrate reconfigure path
    --audio               subscribe to the real system-audio hub and report the capture
                          period as delivered, packet age at the subscriber, and the
                          capture thread's own histograms (see --duration)
    --process-loopback    with --audio, take the Windows process-loopback tier instead of
                          endpoint loopback, which is otherwise only reached when the
                          endpoint tier fails and so cannot be measured
    --whep-selftest       in-process WHEP client against a real virtual display;
                          asserts the answer SDP and that RTP actually flows
                          (requires a build with --features probe)
    --duration <secs>     how long a probe runs. The self-test holds the session open this
                          long after RTP is proven; --live captures for this long instead of
                          stopping at 150 frames; --audio samples for this long (default 10).
                          Without it a run is a liveness check whose histograms describe
                          startup transients.

TUNING FLAGS are the streamer's own and are parsed straight off the command line:
    --encoder <auto|nvidia|intel|software>   --scale <10..100>   --fps <n>   --max-fps <n>
    --qp <1..51>   --h264-profile <baseline|main|high>   --intra-refresh <on|off>
    --monitor <n>   --max-bitrate-kbps <n>   --disable-gpu-encode
    --capture-backend <wgc|dxgi|auto>        Windows capture backend. DXGI is otherwise
                          only reached by WGC failing, so it cannot be measured without this.
                          Spelled out in full because `--capture` above already selects the
                          single-frame PNG probe.
    --synthetic-pattern                      encode a generated pattern, not the desktop
    --workload <mixed|static|scroll|motion|drag>   scripted synthetic workload (implies
                          --synthetic-pattern): a still desktop, scrolling text, full-frame
                          60 fps motion, or a rigid block translating like a dragged window
    --counter             overlay a large frame number + elapsed ms clock, so a 240 fps
                          camera shot of host and client can be differenced (docs/BASELINE.md)
    --turn-url/--turn-username/--turn-credential/--turn-urls/--turn-secret/--turn-ttl
    --stun <a,b,..>   --port <n>   --https-port <n>   --bind-ip <ip>   --lan-ip <ip>

The self-test prints its per-stage telemetry summary on exit; `--whep-selftest` plus
`ScreenExtend status --json` in another shell is the fastest way to see the whole matrix."#
        );
    }

    /// Which probe to run. Taken from the `probe` subcommand's own flags so the CLI reads
    /// `probe --live` rather than `--probe-live`; the streamer's long spellings are still
    /// accepted because `Config::from_args` scans the whole command line either way.
    struct Mode {
        capture: Option<String>,
        dxgi: Option<String>,
        encode: Option<String>,
        live: Option<String>,
        bitrate: bool,
        selftest: bool,
        audio: bool,
        process_loopback: bool,
    }

    fn parse_mode(args: &[String]) -> Mode {
        let mut m = Mode {
            capture: None,
            dxgi: None,
            encode: None,
            live: None,
            bitrate: false,
            selftest: false,
            audio: false,
            process_loopback: false,
        };
        let path_after = |i: usize, default: &str| -> String {
            match args.get(i + 1) {
                Some(v) if !v.starts_with('-') => v.clone(),
                _ => default.to_string(),
            }
        };
        for (i, a) in args.iter().enumerate() {
            match a.as_str() {
                "--capture" | "--probe-capture" => {
                    m.capture = Some(path_after(i, "capture_probe.png"))
                }
                "--dxgi" | "--probe-dxgi" => m.dxgi = Some(path_after(i, "dxgi_probe.bmp")),
                "--encode" | "--probe-encode" => m.encode = Some(path_after(i, "out.h264")),
                "--live" | "--probe-live" => m.live = Some(path_after(i, "live.h264")),
                "--bitrate" | "--probe-bitrate" => m.bitrate = true,
                "--whep-selftest" | "--selftest" => m.selftest = true,
                "--audio" => m.audio = true,
                "--process-loopback" => m.process_loopback = true,
                _ => {}
            }
        }
        m
    }

    /// Config wired to the real `AppState`, the way the shipping server builds it: same virtual
    /// display, same session map, same audio hub. Bound to loopback on high ports so it cannot
    /// collide with a host already serving on 8080/8443.
    #[cfg(any(test, feature = "probe"))]
    fn live_config(app: &tauri::AppHandle, base: Config) -> Config {
        use tauri::Manager;
        let state = app.state::<platform::AppState>();
        *state.session_auth.session_id.lock().unwrap() =
            crate::streamer::test::whep_selftest::SELFTEST_SESSION_ID.to_string();
        *state.session_auth.otp.lock().unwrap() =
            crate::streamer::test::whep_selftest::SELFTEST_OTP.to_string();
        Config {
            bind_ip: std::net::Ipv4Addr::LOCALHOST,
            lan_ip: Some("127.0.0.1".to_string()),
            local_ips: Some(state.local_ips.clone()),
            virtual_display: Some(state.virtual_display.clone()),
            session_auth: Some(state.session_auth.clone()),
            device_overrides: Some(state.device_overrides.clone()),
            sessions: Some(state.sessions.clone()),
            disconnect_grace: Some(state.disconnect_grace.clone()),
            user_turn: Some(state.user_turn.clone()),
            banned_devices: Some(state.banned_devices.clone()),
            approved_devices: Some(state.approved_devices.clone()),
            otp_limiter: Some(state.otp_limiter.clone()),
            audio_hub: Some(state.audio_hub.clone()),
            ..base
        }
    }

    /// Measure the host-side audio path end to end, the way a joined device sees it.
    ///
    /// It drives an `AudioHub` — the same type `AppState` holds, subscribed the same way a
    /// session's forwarder subscribes — so it exercises the shipping capture backend, the
    /// shipping Opus encoder and the shipping fan-out rather than a parallel harness that
    /// could drift from them. Its own hub rather than `AppState`'s, because audio needs no
    /// virtual display and this must stay runnable on a host with no display driver installed.
    ///
    /// It answers the two questions the audio budget is made of: how often the endpoint
    /// actually delivers (the engine period as realised, not as requested), and how old a
    /// packet is by the time a session could put it on the wire.
    fn audio_probe(secs: u64) -> anyhow::Result<()> {
        use std::time::{Duration, Instant};

        use crate::streamer::audio::host_now_ns;
        use crate::streamer::telemetry::Histogram;

        let secs = if secs == 0 { 10 } else { secs };
        let hub = crate::streamer::audio::AudioHub::new();
        let mut sub = hub.subscribe()?;
        println!(
            "audio probe: {}Hz x{}ch, encoder lookahead {} samples ({:.2} ms); \
             sampling for {secs}s",
            sub.format.sample_rate,
            sub.format.channels,
            sub.lookahead_samples,
            sub.lookahead_samples as f64 * 1000.0 / sub.format.sample_rate.max(1) as f64,
        );

        // Consumer-side view: the gap between packets arriving here, and the age of each packet
        // measured against its own `capture_ns`. The capture thread records its own halves into
        // `AudioDiagnostics`; printing both is how a scheduling hop shows up as a difference.
        let arrival = Histogram::new();
        let age = Histogram::new();
        let mut packets = 0u64;
        let mut bytes = 0u64;
        let mut silent = 0u64;
        let mut discontinuities = 0u64;
        let mut seq_gaps = 0u64;
        let mut last_seq: Option<u32> = None;
        let mut last_arrival: Option<u64> = None;
        let mut size_max = 0usize;
        let mut skew_min_ns = i64::MAX;
        let mut skew_max_ns = i64::MIN;
        let mut skew_sum_ns: i128 = 0;
        let mut ahead_of_clock = 0u64;

        // Bounded, because a tier that delivers nothing is a result the probe has to be able
        // to report — `blocking_recv` on a silent capture would simply never return, and
        // "the probe printed nothing" is the least useful possible answer.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .map_err(|e| anyhow::anyhow!("probe runtime: {e}"))?;
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            let next = rt.block_on(async {
                tokio::time::timeout(Duration::from_millis(500), sub.rx.recv()).await
            });
            let Ok(next) = next else {
                continue; // 500 ms with no audio; keep waiting until the deadline
            };
            match next {
                Ok(pkt) => {
                    let now = host_now_ns();
                    if let Some(prev) = last_arrival {
                        arrival.record_span(prev, now);
                    }
                    last_arrival = Some(now);
                    age.record_span(pkt.capture_ns, now);
                    let skew = now as i64 - pkt.capture_ns as i64;
                    skew_min_ns = skew_min_ns.min(skew);
                    skew_max_ns = skew_max_ns.max(skew);
                    skew_sum_ns += skew as i128;
                    if skew < 0 {
                        ahead_of_clock += 1;
                    }
                    if let Some(prev) = last_seq {
                        if pkt.seq != prev.wrapping_add(1) {
                            seq_gaps += 1;
                        }
                    }
                    last_seq = Some(pkt.seq);
                    packets += 1;
                    bytes += pkt.data.len() as u64;
                    size_max = size_max.max(pkt.data.len());
                    if pkt.flags & crate::streamer::audio::FLAG_SILENT != 0 {
                        silent += 1;
                    }
                    if pkt.flags & crate::streamer::audio::FLAG_DISCONTINUITY != 0 {
                        discontinuities += 1;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    seq_gaps += n;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }

        let diagnostics = hub.diagnostics();
        hub.unsubscribe();

        let a = arrival.snapshot();
        let g = age.snapshot();
        println!("\n---- audio probe ({secs}s) ----");
        println!(
            "packets={packets} ({:.1}/s), payload avg={:.1}B max={size_max}B, \
             silent={silent}, discontinuity={discontinuities}, seq gaps={seq_gaps}",
            packets as f64 / secs as f64,
            if packets == 0 {
                0.0
            } else {
                bytes as f64 / packets as f64
            },
        );
        println!(
            "subscriber inter-arrival p50/p90/p99/max = {} ms",
            a.brief()
        );
        println!("capture -> subscriber   p50/p90/p99/max = {} ms", g.brief());
        if packets > 0 {
            println!(
                "capture_ns vs host clock: min={:.2} mean={:.2} max={:.2} ms; \
                 {ahead_of_clock}/{packets} packets stamped ahead of the clock",
                skew_min_ns as f64 / 1.0e6,
                (skew_sum_ns / packets as i128) as f64 / 1.0e6,
                skew_max_ns as f64 / 1.0e6,
            );
        }
        if let Some(d) = diagnostics {
            println!("{}", d.summary());
        }
        if packets == 0 {
            anyhow::bail!(
                "no audio packets in {secs}s — the endpoint delivered nothing, which is the \
                 silent-companion failure mode (AUDIO_NOTES.md §3.2)"
            );
        }
        Ok(())
    }

    fn finish(result: anyhow::Result<()>) -> ! {
        // `exit()` skips destructors, and a piped stdout is block-buffered, so without this
        // the whole report can be lost — which reads as "the probe printed nothing".
        let flush = || {
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
        };
        for (key, t) in crate::streamer::telemetry::all() {
            if t.frames_encoded.load(std::sync::atomic::Ordering::Relaxed) == 0 {
                continue;
            }
            println!("[{key}] {}", t.summary_line());
        }
        match result {
            Ok(()) => {
                flush();
                exit(0)
            }
            Err(e) => {
                eprintln!("probe failed: {e:?}");
                flush();
                exit(1)
            }
        }
    }

    pub fn run(app: &tauri::AppHandle) -> Outcome {
        let args: Vec<String> = super::user_args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if args.iter().any(|a| a == "-h" || a == "--help") || args.len() == 1 {
            print_probe_help();
            exit(0);
        }

        // The harness is a diagnostic tool: everything it learns should reach the terminal.
        crate::logbus::set_verbose(true);
        let mode = parse_mode(&args);
        let base = Config::from_args();

        if let Some(path) = mode.capture {
            finish(Streamer::new(base).probe_capture(&path));
        }
        if let Some(path) = mode.dxgi {
            finish(Streamer::new(base).probe_dxgi(&path));
        }
        if let Some(path) = mode.encode {
            finish(Streamer::new(base).probe_encode(&path));
        }
        if mode.bitrate {
            finish(Streamer::new(base).probe_bitrate());
        }
        if mode.audio {
            let secs = base.probe_secs;
            #[cfg(target_os = "windows")]
            if mode.process_loopback {
                crate::windows_utils::audio::loopback::PREFER_PROCESS_LOOPBACK
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            finish(audio_probe(secs));
        }

        if (mode.live.is_some() || mode.selftest)
            && !tauri::async_runtime::block_on(platform::setup(app.clone()))
        {
            eprintln!("error: could not initialize the virtual display driver.");
            eprintln!("Install it first (Administrator): ScreenExtend drivers install");
            eprintln!("Check the rest of the environment with: ScreenExtend doctor");
            exit(1);
        }

        if let Some(path) = mode.live {
            finish(Streamer::new(base).probe_live(&path));
        }

        if mode.selftest {
            #[cfg(any(test, feature = "probe"))]
            {
                let mut cfg = live_config(app, base);
                cfg.port = 18080;
                cfg.https_port = 18443;
                let telemetry_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                crate::streamer::telemetry::spawn_summary_printer(std::sync::Arc::clone(
                    &telemetry_stop,
                ));
                let result = Streamer::new(cfg).whep_selftest();
                telemetry_stop.store(true, std::sync::atomic::Ordering::Relaxed);
                finish(result);
            }
            #[cfg(not(any(test, feature = "probe")))]
            {
                eprintln!(
                    "error: --whep-selftest is not in this build. It carries a TLS verifier that \n\
                     accepts any certificate, so it is compiled only with `--features probe`:\n\
                     \n    cargo run --features probe -- probe --whep-selftest\n"
                );
                exit(2);
            }
        }

        print_probe_help();
        exit(2)
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
fn route(_app: &tauri::AppHandle) -> Outcome {
    Outcome::LaunchGui
}

/// The `RELOAD` control verb's handler. There is no config replay on the Linux scaffold, so it
/// reports success and does nothing.
#[cfg(not(any(windows, target_os = "macos")))]
pub fn reload_config_into_state(_app: &tauri::AppHandle) -> Result<(), String> {
    Ok(())
}

#[cfg(any(windows, target_os = "macos"))]
pub use desktop::{reload_config_into_state, route};

#[cfg(any(windows, target_os = "macos"))]
mod desktop {
    use super::*;

    use std::sync::Arc;

    use serde_json::Value;
    use tauri::{Listener, Manager};
    use tauri_plugin_cli::{CliExt, Matches};
    use tauri_plugin_store::{Store, StoreExt};

    #[cfg(target_os = "macos")]
    use tauri::path::BaseDirectory;

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

    /// Is a host process up?
    ///
    /// The control socket is asked first, because the lock test is destructive: taking the
    /// exclusive lock and dropping it races a starting GUI, which is not holding it between
    /// `acquire_lock` and `std::mem::forget` — a concurrent `status` could win the lock there
    /// and make the real instance believe it is the second one. The lock is still the
    /// authority, so it is the fallback when no control file answers.
    fn host_running(app: &tauri::AppHandle) -> bool {
        let Ok(dir) = app.path().app_local_data_dir() else {
            return false;
        };
        let ctrl_path = dir.join(single_instance::CTRL_FILE);
        if let Some(reply) =
            single_instance::query_running_instance(&ctrl_path, single_instance::Command::Ping)
        {
            if reply.trim() == "OK" {
                return true;
            }
        }
        match single_instance::acquire_lock(&dir.join(single_instance::LOCK_FILE)) {
            single_instance::LockOutcome::Acquired(file) => {
                drop(file);
                false
            }
            single_instance::LockOutcome::HeldByOther => true,
            single_instance::LockOutcome::Unavailable(e) => {
                eprintln!("warning: could not test the single-instance lock: {e}");
                false
            }
        }
    }

    // driver state

    /// What `status` and `doctor` report about the drivers this host depends on.
    ///
    /// Read-only and unelevated on both platforms: Windows probes the display driver's named
    /// pipe and asks `certutil` whether the signing certificate is still trusted, macOS reads
    /// the legacy audio driver's health and whether the installer package made it into the
    /// bundle. `status`'s own help text has promised driver state since it was written.
    struct DriverState {
        rows: Vec<(&'static str, String)>,
        json: Value,
    }

    #[cfg(windows)]
    fn windows_certificate_trusted() -> Option<bool> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("certutil")
            .args(["-store", "root", "ScreenExtend"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        Some(out.status.success())
    }

    fn driver_state(_app: &tauri::AppHandle) -> DriverState {
        #[cfg(windows)]
        {
            let backend = platform::virtual_display::probe_display_backend();
            let display = match backend {
                platform::virtual_display::DisplayBackend::IddDriver => {
                    "installed and answering".to_string()
                }
                platform::virtual_display::DisplayBackend::DriverNotInstalled => {
                    "NOT INSTALLED — run `ScreenExtend drivers install` (Administrator)".to_string()
                }
                platform::virtual_display::DisplayBackend::DriverNotResponding => {
                    "installed but not answering its pipe".to_string()
                }
            };
            let cert = windows_certificate_trusted();
            let cert_text = match cert {
                Some(true) => "trusted in the machine root store".to_string(),
                Some(false) => {
                    "NOT in the root store — the driver will fail to load after a reboot"
                        .to_string()
                }
                None => "could not query (certutil unavailable)".to_string(),
            };
            DriverState {
                rows: vec![
                    ("Virtual display driver", display),
                    ("Signing certificate", cert_text),
                ],
                json: serde_json::json!({
                    "virtualDisplay": backend.as_str(),
                    "signingCertificate": match cert {
                        Some(true) => "trusted",
                        Some(false) => "absent",
                        None => "unknown",
                    },
                }),
            }
        }
        #[cfg(target_os = "macos")]
        {
            let status = platform::audio_driver_status();
            let audio = match status.as_str() {
                "ready" => "installed and healthy".to_string(),
                "needs_install" => {
                    "not installed — System Audio on 10.15-12.x needs it".to_string()
                }
                "installed_but_unhealthy" => {
                    "installed but not answering; reinstall it".to_string()
                }
                other => other.to_string(),
            };
            // A build whose bundle is missing the package can only ever fail the install, so
            // say so here rather than letting the user discover it from a generic error.
            let pkg = _app
                .path()
                .resolve("resources/ScreenExtendAudio.pkg", BaseDirectory::Resource)
                .ok();
            let pkg_present = pkg.as_deref().is_some_and(|p| p.exists());
            let pkg_text = match (&pkg, pkg_present) {
                (Some(p), true) => format!("bundled ({})", p.display()),
                (Some(p), false) => {
                    format!("MISSING from this build ({})", p.display())
                }
                (None, _) => "could not resolve the bundle resource path".to_string(),
            };
            DriverState {
                rows: vec![
                    ("System audio driver", audio),
                    ("Audio installer package", pkg_text),
                ],
                json: serde_json::json!({
                    "systemAudio": status,
                    "installerPackageBundled": pkg_present,
                }),
            }
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
                if user_args().is_empty() || launched_without_terminal() {
                    return Outcome::LaunchGui;
                }
                eprintln!("error: {e}");
                eprintln!("Run `ScreenExtend --help` for usage.");
                exit(2);
            }
        };

        // `fast_path` only claims help/version when they are the *first* token, so
        // `serve --help` reaches the plugin. clap reports those as a match with no subcommand
        // and a `help`/`version` arg; without this they would fall through to `LaunchGui` and
        // open a window instead of printing anything.
        if let Some(text) = matches.args.get("help").and_then(|d| d.value.as_str()) {
            println!("{text}");
            exit(0);
        }
        if matches.args.contains_key("version") {
            print_version();
            exit(0);
        }

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
            "logs" => run_logs(app, m),
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
            single_instance::LockOutcome::Acquired(file) => std::mem::forget(file), // process lifetime
            single_instance::LockOutcome::HeldByOther => {
                eprintln!(
                    "ScreenExtend is already running. Quit it first, or run `ScreenExtend stop`."
                );
                exit(1);
            }
            single_instance::LockOutcome::Unavailable(e) => {
                eprintln!(
                    "error: could not take the single-instance lock at {}: {e}",
                    lock_path.display()
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
            apply_saved_config(app, store);
        }

        let session_id = match read_credential(m, "session-id", "SCREENEXTEND_SESSION_ID", None) {
            Ok(Some(id)) => id,
            Ok(None) => generate_session_id(),
            Err(e) => {
                eprintln!("error: {e}");
                exit(2);
            }
        };
        let otp = match read_credential(m, "otp", "SCREENEXTEND_OTP", Some("otp-file")) {
            Ok(Some(code)) => code,
            Ok(None) => generate_otp(),
            Err(e) => {
                eprintln!("error: {e}");
                exit(2);
            }
        };
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

        print_serve_banner(http, https, &session_id, &otp, cloud, !arg_flag(m, "no-qr"));

        // Only in verbose mode: the summary is one logbus line per active session every 5 s, and
        // every logbus line is a JSON IPC event, so it stays off unless somebody is watching.
        let telemetry_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if arg_flag(m, "verbose") {
            crate::streamer::telemetry::spawn_summary_printer(Arc::clone(&telemetry_stop));
        }

        let (tx, rx) = std::sync::mpsc::channel();

        // Without this the host answers no control connection at all, so `ScreenExtend stop`
        // either reports "no running instance" or dials a stale port left by an earlier GUI
        // run — while `serve`'s own "already running" message tells you to use it.
        let ctrl_path = lock_dir.join(single_instance::CTRL_FILE);
        let quit_tx = std::sync::Mutex::new(tx.clone());
        if let Err(e) = single_instance::start_control_server(
            app.clone(),
            ctrl_path.clone(),
            |_| {},
            move |_app: &tauri::AppHandle| {
                if let Ok(tx) = quit_tx.lock() {
                    let _ = tx.send(());
                }
            },
            reload_config_into_state,
        ) {
            eprintln!(
                "warning: control server unavailable, `ScreenExtend stop` will not work: {e}"
            );
        }

        println!("\nHost is running. Press Ctrl+C to stop.");
        let _ = std::io::stdout().flush();

        let _ = ctrlc::set_handler(move || {
            let _ = tx.send(());
        });
        let _ = rx.recv();

        println!("\nStopping…");
        telemetry_stop.store(true, std::sync::atomic::Ordering::Relaxed);
        single_instance::clear_control_file(&ctrl_path);
        crate::streamer::session::broadcast_bye(
            &state.sessions,
            crate::streamer::session::ByeReason::HostExit,
            crate::BYE_FLUSH_TIMEOUT,
        );
        platform::remove_all_displays(&state.virtual_display);
        exit(0);
    }

    /// Read a join credential without requiring it on the command line.
    ///
    /// Precedence: the flag, then the `--*-file` path if one is declared for this credential,
    /// then the environment variable. `-` as the flag's value means "read one line from stdin".
    /// argv is readable by every process on the machine and lands in shell history, and the OTP
    /// is the only gate on a join once the session id is known — so a value that arrives that
    /// way is used but warned about.
    fn read_credential(
        m: &Matches,
        flag: &str,
        env_key: &str,
        file_flag: Option<&str>,
    ) -> Result<Option<String>, String> {
        if let Some(value) = arg_str(m, flag) {
            if value == "-" {
                let mut line = String::new();
                std::io::stdin()
                    .read_line(&mut line)
                    .map_err(|e| format!("could not read --{flag} from stdin: {e}"))?;
                let line = line.trim().to_string();
                if line.is_empty() {
                    return Err(format!("--{flag} - was given but stdin was empty"));
                }
                return Ok(Some(line));
            }
            eprintln!(
                "warning: --{flag} on the command line is visible to every process on this \
                 machine and is written to shell history. Prefer {env_key}, --{flag} - (stdin){}.",
                file_flag.map(|f| format!(", or --{f}")).unwrap_or_default()
            );
            return Ok(Some(value));
        }
        if let Some(path) = file_flag.and_then(|f| arg_str(m, f)) {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("could not read the credential file {path}: {e}"))?;
            let value = text.trim().to_string();
            if value.is_empty() {
                return Err(format!("the credential file {path} is empty"));
            }
            return Ok(Some(value));
        }
        match std::env::var(env_key) {
            Ok(value) if !value.trim().is_empty() => Ok(Some(value.trim().to_string())),
            _ => Ok(None),
        }
    }

    /// Everything in `config.json` that can be pushed into a live `AppState`.
    ///
    /// `serve` runs this at startup and the `RELOAD` control verb runs it again, so a change
    /// made by `devices set` / `turn set` / `config set` from another shell reaches a host that
    /// is already streaming. Server ports and the encoder choice are deliberately absent: they
    /// are read once when the servers and pipelines are built.
    fn apply_saved_config(app: &tauri::AppHandle, store: &Cfg) {
        platform::set_turn_config(
            app.state(),
            cfg_str(store, "turnConfig.urls").unwrap_or_default(),
            cfg_str(store, "turnConfig.username").unwrap_or_default(),
            cfg_str(store, "turnConfig.credential").unwrap_or_default(),
        );
        let grace = cfg_u64(store, "disconnectGraceSecs").unwrap_or(DEFAULT_DISCONNECT_GRACE_SECS);
        platform::set_disconnect_grace(app.state(), grace as u32);
        platform::set_legacy_volume_key_proxy(
            cfg_bool(store, "legacyVolumeKeyProxy").unwrap_or(false),
        );
        apply_saved_devices(app, store);
        apply_saved_trust(app, store);
    }

    /// Replay the ban and auto-join sets from `knownDevices[]`.
    ///
    /// Trust is keyed on the per-device token, not the IP (`streamer/session.rs`). Without this
    /// a headless host started from the same `config.json` as the GUI is *less* restrictive
    /// than the GUI: every previously banned device is un-banned and can rejoin with the OTP,
    /// and every approved device has to retype it.
    fn apply_saved_trust(app: &tauri::AppHandle, store: &Cfg) {
        let Some(known) = get_nested(store, "knownDevices").and_then(|v| match v {
            Value::Array(a) => Some(a),
            _ => None,
        }) else {
            return;
        };
        let mut banned = 0usize;
        let mut approved = 0usize;
        for device in known {
            let token = device
                .get("token")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let ip = device
                .get("ip")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if token.trim().is_empty() && ip.trim().is_empty() {
                continue;
            }
            let is_banned = device
                .get("banned")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            platform::set_device_banned(app.state(), token.clone(), ip, is_banned);
            // A banned device is never also auto-join approved; the GUI models the two as
            // exclusive and the join path checks the ban first either way.
            platform::set_device_approved(app.state(), token, !is_banned);
            if is_banned {
                banned += 1;
            } else {
                approved += 1;
            }
        }
        if banned > 0 || approved > 0 {
            tprintln!("replayed device trust: {approved} approved, {banned} banned");
        }
    }

    /// Re-read `config.json` from disk and replay it. This is what the `RELOAD` control verb
    /// runs inside the host process, on behalf of a CLI process that has just written the file.
    pub fn reload_config_into_state(app: &tauri::AppHandle) -> Result<(), String> {
        let store = app
            .store(CONFIG_STORE)
            .map_err(|e| format!("cannot open config store: {e}"))?;
        store
            .reload()
            .map_err(|e| format!("cannot re-read config store: {e}"))?;
        apply_saved_config(app, &store);
        tprintln!("[control] configuration reloaded from disk");
        Ok(())
    }

    /// Ask a running host to re-read its configuration. `true` when it confirmed.
    fn request_reload(app: &tauri::AppHandle) -> bool {
        let Ok(dir) = app.path().app_local_data_dir() else {
            return false;
        };
        single_instance::query_running_instance(
            &dir.join(single_instance::CTRL_FILE),
            single_instance::Command::Reload,
        )
        .is_some_and(|reply| reply.trim() == "OK")
    }

    /// Print the "applied" / "restart to apply" line after a command that wrote the store.
    fn report_live_apply(app: &tauri::AppHandle) {
        if !host_running(app) {
            return;
        }
        if request_reload(app) {
            println!("(Applied to the running host.)");
        } else {
            println!("(A host is running but did not answer — restart it to apply.)");
        }
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
            let audio = device
                .get("systemAudio")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let dpr = device.get("dpr").and_then(|v| v.as_f64()).unwrap_or(1.0);
            platform::set_device_override(
                app.state(),
                ip.to_string(),
                num("scale", 100),
                orientation,
                num("refreshRate", 60),
                num("videoScale", 100),
                num("videoQuality", 15),
                control,
                dpr,
                audio,
            );
            // Without this a device pinned to a specific host output in the GUI reverts to the
            // default output under `serve`, which is audible rather than theoretical.
            if let Some(output) = device.get("audioOutputDeviceId").and_then(|v| v.as_str()) {
                platform::set_device_audio_output(app.state(), ip.to_string(), output.to_string());
            }
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

    fn print_serve_banner(
        http_port: u16,
        https_port: u16,
        session_id: &str,
        otp: &str,
        cloud: bool,
        qr: bool,
    ) {
        println!("\n──────────────────────────────────────────");
        println!("Session:  {session_id}");
        println!("OTP:      {otp}");
        println!("──────────────────────────────────────────");

        let adapters = platform::networking::cli_adapters();
        let lan = lan_entries(&adapters, http_port, session_id);
        if lan.is_empty() {
            println!("\nNo local networks found yet — connect to Wi-Fi/Ethernet, or start a hosted network.");
        }
        for (title, url) in &lan {
            print_join(title, url, qr);
        }
        // The HTTPS endpoint is served on every adapter too and is the one that gets a secure
        // context (SharedArrayBuffer audio, clipboard), but it carries a self-signed
        // certificate — so it is printed as text and never as a QR, which would put a scanner
        // straight into a certificate warning.
        if !lan.is_empty() {
            println!("\nHTTPS (self-signed certificate; accept the warning once):");
            for (title, url) in &lan {
                let secure = url.replacen("http://", "https://", 1).replacen(
                    &format!(":{http_port}/"),
                    &format!(":{https_port}/"),
                    1,
                );
                println!("  {title}: {secure}");
            }
        }
        if cloud {
            print_join("Anywhere (Internet)", &cloud_url(session_id), qr);
        }
    }

    // status

    /// Live per-session telemetry, asked of the running host over the single-instance control
    /// socket. `status` is a separate process, so this is the only path to counters that exist
    /// only in the host's memory. `None` when no host is up or it is too old to answer.
    fn live_telemetry(app: &tauri::AppHandle) -> Option<Value> {
        let dir = app.path().app_local_data_dir().ok()?;
        let reply = single_instance::query_running_instance(
            &dir.join(single_instance::CTRL_FILE),
            single_instance::Command::Stats,
        )?;
        serde_json::from_str::<Value>(reply.trim()).ok()
    }

    fn print_telemetry(telemetry: &Value) {
        let Some(sessions) = telemetry.get("sessions").and_then(|v| v.as_object()) else {
            return;
        };
        println!("\nStreaming sessions:");
        if sessions.is_empty() {
            println!("  (none connected)");
            return;
        }
        for (ip, s) in sessions {
            println!(
                "  {ip}: capture={} encoder={} frames={}",
                s.get("capture_backend")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                s.get("encoder").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("frames_encoded")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            );
            if let Some(stages) = s.get("stages").and_then(|v| v.as_object()) {
                for (name, h) in stages {
                    let get = |k: &str| h.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
                    println!(
                        "      {name:<20} p50 {:>7.2}  p90 {:>7.2}  p99 {:>7.2}  max {:>8.2} ms  (n={})",
                        get("p50_ms"),
                        get("p90_ms"),
                        get("p99_ms"),
                        get("max_ms"),
                        h.get("count").and_then(|v| v.as_u64()).unwrap_or(0),
                    );
                }
            }
            if let Some(d) = s.get("degradation").and_then(|v| v.as_object()) {
                let hits: Vec<String> = d
                    .iter()
                    .filter(|(_, v)| v.as_u64().unwrap_or(0) != 0)
                    .map(|(k, v)| format!("{k}={}", v.as_u64().unwrap_or(0)))
                    .collect();
                println!(
                    "      degradation: {}",
                    if hits.is_empty() {
                        "none".to_string()
                    } else {
                        hits.join(" ")
                    }
                );
            }
            match s.get("client") {
                Some(c) if !c.is_null() => println!(
                    "      client: video={} audio={} sab={} isolated={} clock={}",
                    c.get("video_path").and_then(|v| v.as_str()).unwrap_or("?"),
                    c.get("audio_path").and_then(|v| v.as_str()).unwrap_or("?"),
                    c.get("sab_available")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    c.get("cross_origin_isolated")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    c.get("clock_offset_valid")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                ),
                _ => println!("      client: has not reported stats"),
            }
            if let Some(c) = s.get("client").filter(|c| !c.is_null()) {
                let tier = |k: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or("?").to_string();
                println!(
                    "      client tiers: transport={} render={} key={} sink={}",
                    tier("transport_tier"),
                    tier("render_tier"),
                    tier("key_tier"),
                    tier("sink_tier"),
                );
                // The one line a support conversation should start from: every bit here was a
                // swallowed error or an untaken branch on the device.
                let degraded: Vec<&str> = c
                    .get("degraded")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();
                println!(
                    "      client degraded: {}",
                    if degraded.is_empty() {
                        "none".to_string()
                    } else {
                        degraded.join(" ")
                    }
                );
            }
        }
    }

    fn run_status(app: &tauri::AppHandle, m: &Matches) -> ! {
        let store = open_store(app);
        let running = host_running(app);
        let telemetry = if running { live_telemetry(app) } else { None };
        let session_id = arg_str(m, "session-id");
        let http = cfg_u64(&store, "serverPorts.http").unwrap_or(DEFAULT_HTTP_PORT as u64) as u16;
        let https =
            cfg_u64(&store, "serverPorts.https").unwrap_or(DEFAULT_HTTPS_PORT as u64) as u16;
        let adapters = platform::networking::cli_adapters();
        let drivers = driver_state(app);

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
                "drivers": drivers.json,
                "telemetry": telemetry,
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

        println!("\nDrivers:");
        for (label, value) in &drivers.rows {
            println!("  {label:<24} {value}");
        }

        println!("\nNetworks:");
        if adapters.is_empty() {
            println!("  (none active)");
        }
        for adapter in &adapters {
            let ipv4 = adapter.ip_addresses.iter().find(|ip| is_ipv4(ip));
            match (ipv4, &session_id) {
                (Some(ip), Some(id)) => {
                    println!("  {}: http://{ip}:{http}/?id={id}", adapter.network_name);
                    println!(
                        "  {:width$}  https://{ip}:{https}/?id={id} (self-signed)",
                        "",
                        width = adapter.network_name.len()
                    );
                }
                (Some(ip), None) => println!("  {}: {ip}", adapter.network_name),
                (None, _) => println!("  {}: (no IPv4)", adapter.network_name),
            }
        }
        if let Some(t) = telemetry.as_ref() {
            print_telemetry(t);
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
                // The env-var form is what is printed, because `--otp` on a command line is
                // readable by every process on the machine and is written to shell history.
                println!(
                    "\nThis is a one-off credential for display. Start the host with:\n  \
                     SCREENEXTEND_SESSION_ID={session_id} SCREENEXTEND_OTP={otp} ScreenExtend serve\n\
                     (PowerShell: $env:SCREENEXTEND_SESSION_ID='{session_id}'; $env:SCREENEXTEND_OTP='{otp}'; ScreenExtend serve)"
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
                    "{:<16} {:>6} {:<10} {:>8} {:>6} {:>8} {:>8} {:>6}",
                    "IP", "SCALE", "ORIENT", "REFRESH", "VSCALE", "VQUAL", "CONTROL", "AUDIO"
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
                    let audio = s("systemAudio").as_bool().unwrap_or(false);
                    println!(
                        "{ip:<16} {:>5}% {orient:<10} {:>6}Hz {:>5}% {vqual:>8} {:>8} {:>6}",
                        scale,
                        refresh,
                        vscale,
                        if control { "on" } else { "off" },
                        if audio { "on" } else { "off" }
                    );
                }
                exit(0);
            }
            "set" => {
                let Some(ip) = arg_str(sm, "ip") else {
                    eprintln!("usage: ScreenExtend devices set <ip> [--scale N] [--orientation Portrait|Landscape] [--refresh-rate N] [--video-scale N] [--video-quality N] [--control on|off] [--audio on|off]");
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
                            "remoteControl": true, "systemAudio": false, "os": "", "screenSize": ""
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
                if let Some(v) = arg_str(sm, "audio") {
                    obj.insert("systemAudio".into(), Value::Bool(v == "on"));
                }

                existing.retain(|d| d.get("ip").and_then(|v| v.as_str()) != Some(ip.as_str()));
                existing.push(device.clone());
                store.set("devices", Value::Array(existing));
                if let Err(e) = store.save() {
                    eprintln!("error: failed to save config: {e}");
                    exit(1);
                }
                println!("Saved override for {ip}: {device}");
                report_live_apply(app);
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
                    report_live_apply(app);
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
                report_live_apply(app);
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
                println!("TURN relay saved.");
                report_live_apply(app);
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
                report_live_apply(app);
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
                if crate::write_avatar(&app.clone(), &bytes) {
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
                match crate::read_avatar(&app.clone()) {
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

    /// `doctor` exits 1 for an unsupported OS and 3 when the OS is fine but a
    /// required permission is denied, so a support script can tell the two apart.
    const DOCTOR_EXIT_PERMISSION: i32 = 3;

    fn tri_state_label(state: &str) -> &'static str {
        match state {
            "granted" => "granted",
            "denied" => "MISSING",
            _ => "unknown",
        }
    }

    #[cfg(target_os = "macos")]
    fn tribool(v: Option<bool>) -> &'static str {
        match v {
            Some(true) => "true",
            Some(false) => "false",
            None => "unknown",
        }
    }

    fn run_doctor(app: &tauri::AppHandle, m: &Matches) -> ! {
        let report = platform::compatibility::check_system_requirements();
        let permissions = platform::permissions::check_permissions();
        let drivers = driver_state(app);
        let permission_missing = permissions
            .iter()
            .any(|p| p.required && p.state == "denied");

        #[cfg(target_os = "macos")]
        let probes = platform::permissions::raw_probes();
        #[cfg(target_os = "macos")]
        let identity = platform::permissions::identity_report();

        if wants_json(m) {
            #[allow(unused_mut)]
            let mut out = serde_json::json!({
                "os": { "name": report.os_name, "version": report.os_version, "supported": report.os_supported, "minimum": report.min_os_version },
                "backends": {
                    "capture": report.capture_backend,
                    "encoder": report.encoder_backend,
                    "display": report.display_backend,
                    "audio": report.audio_backend,
                    "hostedNetwork": report.hosted_network_backend,
                },
                "unsupportedApis": report.unsupported_apis.iter().map(|a| serde_json::json!({
                    "name": a.name, "description": a.description, "requiredVersion": a.required_version, "severity": a.severity,
                })).collect::<Vec<_>>(),
                "permissions": permissions.iter().map(|p| serde_json::json!({
                    "key": p.key, "name": p.name, "state": p.state, "required": p.required,
                    "relaunchRequired": p.relaunch_required, "description": p.description, "detail": p.detail,
                })).collect::<Vec<_>>(),
                "drivers": drivers.json,
            });
            #[cfg(target_os = "macos")]
            {
                out["macos"] = serde_json::json!({
                    "executable": identity.exe_path,
                    "bundle": identity.bundle_path,
                    "translocated": identity.translocated,
                    "designatedRequirement": identity.designated_requirement,
                    "quarantine": identity.quarantine,
                    "sip": identity.sip,
                    "osBuild": identity.os_version,
                    "pid": identity.pid,
                    "parentPid": identity.parent_pid,
                    "parentCommand": identity.parent_command,
                    "probes": {
                        "cgPreflightScreenCaptureAccess": probes.preflight,
                        "cgPreflightScreenCaptureAccessUs": probes.preflight_us,
                        "windowListNameProbe": probes.window_name,
                        "windowListNameProbeUs": probes.window_name_us,
                        "axIsProcessTrusted": probes.ax_trusted,
                        "axSystemWideElement": probes.ax_secondary,
                        "axSystemWideElementUs": probes.ax_secondary_us,
                        "screenCaptureKitAvailable": probes.sck_available,
                        "scShareableContent": probes.sck,
                        "scShareableContentUs": probes.sck_us,
                    },
                    "resetCommands": [
                        "tccutil reset ScreenCapture app.screenextend.desktop",
                        "tccutil reset Accessibility app.screenextend.desktop",
                    ],
                });
            }
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            exit(doctor_code(report.os_supported, permission_missing));
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
        println!("\nBackends:");
        println!("  Capture:        {}", report.capture_backend);
        println!("  Encoder:        {}", report.encoder_backend);
        println!("  Display:        {}", report.display_backend);
        println!("  Audio:          {}", report.audio_backend);
        println!("  Hosted network: {}", report.hosted_network_backend);
        if report.unsupported_apis.is_empty() {
            println!("\nAll capabilities available.");
        } else {
            println!("\nCapability notes:");
            for api in &report.unsupported_apis {
                println!("  [{}] {} — {}", api.severity, api.name, api.description);
            }
        }
        println!("\nDrivers:");
        for (label, value) in &drivers.rows {
            println!("  {label:<24} {value}");
        }
        if permissions.is_empty() {
            println!("\nPermissions: none are required on this platform.");
        } else {
            println!("\nPermissions:");
            for p in &permissions {
                println!(
                    "  [{}] {} ({})",
                    tri_state_label(&p.state),
                    p.name,
                    if p.required { "required" } else { "optional" }
                );
                println!("      {}", p.description);
                if !p.detail.is_empty() && p.detail != p.description {
                    println!("      {}", p.detail);
                }
                if p.relaunch_required {
                    println!("      Quit and reopen ScreenExtend for this to take effect.");
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            println!("\nmacOS TCC identity:");
            println!("  Executable:   {}", identity.exe_path);
            println!(
                "  Bundle:       {}",
                identity.bundle_path.as_deref().unwrap_or("(not a bundle)")
            );
            println!(
                "  Translocated: {}",
                if identity.translocated {
                    "YES — Gatekeeper is running a read-only copy; move the app to /Applications"
                } else {
                    "no"
                }
            );
            println!("  Quarantine:   {}", identity.quarantine);
            println!("  SIP:          {}", identity.sip);
            println!("  OS build:     {}", identity.os_version);
            println!(
                "  Process:      pid {}, parent {} ({})",
                identity.pid, identity.parent_pid, identity.parent_command
            );
            println!("  Designated requirement (this is what TCC matches against):");
            for line in identity.designated_requirement.lines() {
                println!("    {line}");
            }
            println!("\nmacOS raw probes (read them against each other, not on their own):");
            println!(
                "  CGPreflightScreenCaptureAccess: {:<8} ({} us) — cached per process; a false here means nothing on its own",
                tribool(probes.preflight),
                probes.preflight_us
            );
            println!(
                "  window-list name probe:         {:<8} ({} us) — the live read; unknown means no other window was on screen",
                tribool(probes.window_name),
                probes.window_name_us
            );
            println!("  AXIsProcessTrusted:             {}", probes.ax_trusted);
            println!(
                "  AX system-wide element:         {:<8} ({} us)",
                probes.ax_secondary, probes.ax_secondary_us
            );
            println!("  ScreenCaptureKit available:     {}", probes.sck_available);
            println!(
                "  SCShareableContent:             {:<8} ({} us)",
                tribool(probes.sck),
                probes.sck_us
            );
            println!(
                "\nIf System Settings shows the toggle on but a probe above says denied, the grant \
                 is bound to\na different code identity. Reset it and grant again:\n  \
                 tccutil reset ScreenCapture app.screenextend.desktop\n  \
                 tccutil reset Accessibility app.screenextend.desktop"
            );
        }

        exit(doctor_code(report.os_supported, permission_missing));
    }

    fn doctor_code(os_supported: bool, permission_missing: bool) -> i32 {
        if !os_supported {
            1
        } else if permission_missing {
            DOCTOR_EXIT_PERMISSION
        } else {
            0
        }
    }

    // logs

    /// Print the running host's log backlog.
    ///
    /// This process's own ring is empty by construction — it started a moment ago — so the
    /// lines have to come from the host over the single-instance control socket. Without a
    /// host there is nothing to print and that is an error, not an empty success.
    fn run_logs(app: &tauri::AppHandle, m: &Matches) -> ! {
        let take = arg_str(m, "lines")
            .and_then(|s| s.trim().parse::<usize>().ok())
            .unwrap_or(usize::MAX);

        let Ok(dir) = app.path().app_local_data_dir() else {
            eprintln!("error: could not resolve app data directory");
            exit(1);
        };
        let reply = single_instance::query_running_instance(
            &dir.join(single_instance::CTRL_FILE),
            single_instance::Command::Logs,
        );
        let Some(reply) = reply else {
            eprintln!(
                "No running ScreenExtend instance to read logs from.\n\
                 The backlog lives in the host process; start one with `ScreenExtend serve` \
                 or open the app."
            );
            exit(1);
        };

        let lines: Vec<&str> = reply.lines().collect();
        if lines.is_empty() {
            println!("(the running host has not logged anything yet)");
            exit(0);
        }
        let start = lines.len().saturating_sub(take);
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
        // The lock is the authority on whether anything is running. Without this check a
        // control file left behind by a crashed run would have us dial whatever now owns that
        // recycled port and report success.
        if !host_running(app) {
            single_instance::clear_control_file(&ctrl_path);
            eprintln!("No running ScreenExtend instance found.");
            exit(1);
        }
        if single_instance::signal_running_instance(&ctrl_path, single_instance::Command::Quit) {
            println!("Asked the running instance to quit.");
            exit(0);
        }
        eprintln!("A ScreenExtend host is running but is not answering on its control port.");
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
