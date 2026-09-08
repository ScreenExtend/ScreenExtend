# L — App shell, CLI, logging, single-instance, Linux scaffold, build & packaging

Territory read in full: `src-tauri/src/{lib.rs, cli.rs, main.rs, logbus.rs, single_instance.rs}`,
all of `src-tauri/src/linux_utils/`, `src-tauri/{Cargo.toml, build.rs, tauri.conf.json,
tauri.macos.conf.json}`, `src-tauri/capabilities/{default.json, desktop.json}`,
`src-tauri/windows/{hooks.nsh, fragment.wxs}`, `.github/workflows/build-release.yml` (plus the
deleted `ci.yml` recovered from `e1d5d28^`), `src-tauri/{resources,binaries}/{PROVENANCE.md,
SHA256SUMS}`, `package.json`, `pnpm-workspace.yaml`, `README.md`, `.gitignore`,
`.gitattributes`, `LICENSE`.

`LICENSE` is confirmed **GNU AGPL-3.0, 661 lines**, matching `Cargo.toml:9`
(`license-file = "../LICENSE"`) and `README.md:139`.

---

## 1. Files and purpose

**`src-tauri/src/main.rs` (6 lines).** The entire binary entry point:
`#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]` plus
`fn main() { screenextend_lib::run() }`. The `windows_subsystem` attribute is what makes the
release build GUI-subsystem on Windows, and therefore what forces `cli::attach_console()` to
exist at all.

**`src-tauri/src/lib.rs` (680 lines).** The Tauri application: the shared event/DTO types
(`Device`, `DeviceJoin/Modify/Remove(+Action)`, `NetworkChange`, `HostedNetworkNoPassword`,
`CloudStatusChange`, `SessionIdChange`, `JoinAttemptsPaused`, `PermissionStatus`,
`UnsupportedApi`, `CompatibilityReport`, `DisplayCapacity`, `DeviceAudioOutputs`), a handful of
OS-independent commands (`exit_app`, `set_window_zoom`, `get_username`, avatar get/set/remove),
the macOS app menu, and `run()` — which is the whole lifecycle: `cli::fast_path()`, the specta
`Builder` with `collect_commands!`/`collect_events!`, the debug-only `bindings.ts` export, the
plugin stack, and a `setup` closure carrying three legacy CLI branches plus the normal
single-instance/window path. It also glob-imports whichever `*_utils` module matches the target,
which is how the command list stays identical across OSes.

**`src-tauri/src/cli.rs` (1557 lines).** The shipped command-line interface. Top-level: `exit()`
(flush + `process::exit`), `attach_console()`, session-id/OTP generators, the Wi-Fi QR payload
builder, `print_help`/`print_version`, `user_args()`, `launched_without_terminal()`,
`fast_path()` and `dispatch()`. Everything else lives in the `#[cfg(any(windows, target_os =
"macos"))] mod desktop` submodule: matches helpers over `tauri_plugin_cli::Matches`, a
dotted-key accessor layer over the `tauri-plugin-store` `config.json`, `host_running()`, the
join-URL/QR helpers, `route()`, and one `run_*` function per subcommand. Every `run_*` returns
`!` and calls `exit()`.

**`src-tauri/src/logbus.rs` (96 lines).** A 2000-line ring backlog behind a global mutex, a
`VERBOSE` atomic, an `AppHandle` in a `OnceLock`, the `LogLine` specta event, the
`get_log_backlog` command, and the `tprintln!`/`teprintln!` macros that CLAUDE.md mandates over
`println!`/`eprintln!`.

**`src-tauri/src/single_instance.rs` (119 lines).** The instance token: an exclusive `fs4` lock
on `screenextend.lock`, plus a loopback TCP control server whose `port\ntoken\n` is written to
`screenextend.ctrl` so another process can send `FOCUS` or `QUIT`.

**`src-tauri/src/linux_utils/` (215 lines across 8 files).** The Linux scaffold. `mod.rs` (85)
holds `AppState {}` and a dozen stub commands; `compatibility.rs` (38) parses
`/etc/os-release`; `virtual_display.rs` (28) always errors; `hosted_network.rs` (20) returns
`true` for everything; `streamer.rs` (19) `bail!`s from a `VaapiBackend`; `permissions.rs` (17)
returns empty/true; `audio.rs` (7) `bail!`s; `networking.rs` is **one blank line**.

**`src-tauri/build.rs` (3 lines).** `tauri_build::build()`.

**`src-tauri/Cargo.toml` (211 lines).** Package metadata, `[lib] crate-type = ["staticlib",
"cdylib", "rlib"]`, `[lints.rust] dead_code = "allow"`, and the dependency set: Tauri + specta,
ten Tauri plugins, serde, tokio (`features = ["full"]`), axum/axum-server/rustls/rcgen,
`webrtc 0.17` + `turn 0.17`, `tokio-tungstenite` + `futures-util`, a utilities block, then
`cfg(target_os = "macos")` (objc2 family, libc, block2, dispatch2, mach2, arc-swap, plist,
httparse) and `cfg(windows)` (winreg, windows-future, windows-core, parking_lot, rayon, clap, and
a 60-feature `windows` 0.62). The only profile section is `[profile.dev.package.objc2]`.

**`src-tauri/tauri.conf.json` (266 lines).** Version 0.6.3, identifier
`app.screenextend.desktop`, the webview CSP, the bundle block (six Windows resources, the
`nefconc` `externalBin`, NSIS/WiX hooks, `createUpdaterArtifacts: true`), the entire
`plugins.cli` subcommand tree, and the updater pubkey/endpoint.

**`src-tauri/tauri.macos.conf.json` (41 lines).** The macOS overlay passed with `--config`:
same version/identifier, `csp: null`, `resources: ["resources/libopus.dylib"]`,
`externalBin: []`, `minimumSystemVersion: "10.15"`.

**`src-tauri/capabilities/default.json` (123 lines).** Permissions for the `main` window: the
shell allowlist (nefconc x3 shapes, certutil x2, self-invocation, netsh x2, `control desk.cpl`,
`open x-apple.systempreferences:…`), plus `shell:allow-open`,
`clipboard-manager:allow-write-text`, `core:window:allow-destroy`,
`core:webview:allow-set-webview-zoom`, `store/os/dialog/notification:default`.

**`src-tauri/capabilities/desktop.json` (17 lines).** `updater:default`, `process:default`,
`autostart:allow-{enable,disable,is-enabled}` for macOS/windows/linux.

**`src-tauri/windows/hooks.nsh` (6 lines).** `NSIS_HOOK_PREUNINSTALL` → elevated
`ScreenExtend.exe removedrivers` when not upgrading.

**`src-tauri/windows/fragment.wxs` (28 lines).** A deferred, non-impersonated WiX custom action
running `removedrivers` on full removal, plus a marker registry component group.

**`.github/workflows/build-release.yml` (119 lines).** Tag-triggered (`app-v*`) build of Windows
x64 and macOS universal via `tauri-action`, then a `finalize-release` job that strips the version
out of asset filenames, patches `latest.json` to match, publishes as latest, and force-moves the
`latest` git tag.

**Provenance files.** `resources/PROVENANCE.md` (82 lines) documents the IDD driver, its cert,
`libopus.dll`, `libopus.dylib` (with an explicit x86_64-only warning) and `libx264-164.dll`.
`binaries/PROVENANCE.md` (31 lines) documents `nefconc` and the two macOS placeholder stubs. Both
`SHA256SUMS` files are checksum lists.

**Root files.** `package.json` (83 lines, version 0.6.3, `browserslist: safari >= 13.1, chrome >=
87`); `pnpm-workspace.yaml` (19 lines: `allowBuilds` for core-js-pure/esbuild/sharp, seven
transitive `overrides`, three ignored GHSAs); `README.md` (143 lines); `.gitignore` (48 lines);
`.gitattributes` (**one line**: `*.html text eol=lf`).

---

## 2. Function catalog

### `main.rs`
- `main()` — calls `screenextend_lib::run()`.

### `lib.rs`
- `Device::defaults(DeviceInfo) -> Device` (147-182) — clamps refresh rate to
  `MIN/MAX_REFRESH_RATE` (0 → 60), snaps DPR with `platform::snap_display_dpr`, computes
  `max_dpr` against a `SLIDER_DPR_CEILING` of 3.0, and defaults scale 100 / video_scale 100 /
  video_quality 15 / control on / **audio off**.
- `exit_app(AppHandle)` (209-221) — if `AppState` exists, `broadcast_bye(HostExit,
  BYE_FLUSH_TIMEOUT=250 ms)` then `remove_all_displays`, then `app.exit(0)`.
- `set_window_zoom(WebviewWindow, f64)` (223-231) — on macOS refuses with a message when
  `webview_zoom_supported()` is false; otherwise `window.set_zoom`.
- `webview_zoom_supported()` (233-240, macOS) — `OnceLock` + `AnyClass::get(c"WKWebView")` +
  `responds_to(sel!(setPageZoom:))`. **A correct capability probe**, not a version check.
- `get_username()` (242-246) — `whoami::username()`, `""` on error.
- `avatar_path(&AppHandle)` (248-252) — `app_data_dir()/avatar.png`, `create_dir_all` first.
- `set_avatar` / `get_avatar` / `remove_avatar` (254-300) — write/read/delete; all use
  `log::error!` (not `teprintln!`) on failure; `get_avatar` maps `NotFound` to `None` and
  `remove_avatar` maps `NotFound` to `true`.
- `build_menu(&AppHandle)` (302-326) — real About/Quit submenu on macOS, empty menu elsewhere.
- `run()` (328-680) — see §"Lifecycle" below.

### `cli.rs` — top level
- `exit(i32) -> !` (7-11) — flush stdout+stderr, `process::exit`.
- `attach_console()` (13-22) — `AttachConsole(ATTACH_PARENT_PROCESS)` on Windows, no-op
  elsewhere. Return value discarded.
- `generate_session_id()` (24-31) — 12 chars from a 32-symbol confusable-free alphabet
  (`23456789ABCDEFGHJKLMNPQRSTUVWXYZ`).
- `generate_otp()` (33-39) — 6 decimal digits.
- `is_ipv4(&str)` (41-43) — `Ipv4Addr` parse test.
- `wifi_qr_payload(ssid, password)` (45-59) — `WIFI:T:WPA;S:…;P:…;;` with `\ ; , : "` escaped;
  `T:nopass` when the password is empty.
- `print_version()` (61-63), `print_help()` (65-123) — hand-written help listing every
  subcommand and every `serve` flag.
- `user_args()` (125-130) — argv minus argv[0] minus macOS `-psn_*` launcher args.
- `launched_without_terminal()` (132-140) — macOS only: both stdout and stderr non-tty.
- `fast_path()` (142-165) — help / version / macOS `audio-recover`; runs before the Tauri
  builder.
- `dispatch(&AppHandle) -> Outcome` (167-173) — `LaunchGui` when argv is empty, else
  `attach_console()` + `route()`.
- `route` (175-181) — a no-op `LaunchGui` on non-Windows/macOS; otherwise re-exported from
  `desktop`.

### `cli.rs::desktop`
- `arg_str` / `arg_flag` / `arg_u16` / `wants_json` / `sub` (210-235) — thin accessors over
  `Matches`. `arg_flag` falls back to `occurrences > 0` when the value is not a bool.
- `open_store(&AppHandle) -> Arc<Store>` (241-249) — opens `config.json`; **exits 1** on failure.
- `get_nested` / `set_nested` / `set_path` (251-295) — dotted-key read/write. `set_path`
  `.expect("object node")` twice (280, 291).
- `cfg_str` / `cfg_u64` / `cfg_bool` (297-307).
- `host_running(&AppHandle)` (311-323) — takes the exclusive lock and drops it; `true` means
  "someone else holds it" **or** "the file could not be opened".
- `cloud_url(session_id)` (327-329) — `https://session.screenextend.app/?id=…`.
- `lan_entries(adapters, port, session_id)` (331-346) — first IPv4 per adapter →
  `http://ip:port/?id=…`. Never emits an HTTPS URL.
- `render_qr` / `print_join` (348-359) — `qr2term::print_qr`; errors go to stderr, not the logbus.
- `route(&AppHandle) -> Outcome` (363-404) — the 16-arm dispatcher. On a `matches()` error it
  returns `LaunchGui` if argv is empty or there is no terminal (macOS Finder launch), else
  prints and exits 2. `installdrivers|removedrivers|hostednetwork` deliberately return
  `LaunchGui` so `lib.rs` keeps ownership.
- `run_serve(&AppHandle, &Matches) -> !` (408-522) — see §"`serve` startup sequence".
- `apply_saved_devices(&AppHandle, &Store)` (524-569) — for each entry of `devices[]`, one
  `platform::set_device_override` call. Reads scale/orientation/refreshRate/videoScale/
  videoQuality/remoteControl/dpr/systemAudio, defaulting 100/Landscape/60/100/15/true/1.0/false.
- `install_device_listeners(&AppHandle)` (571-586) — `app.listen("device-join" | "device-remove")`
  and prints a one-line `+`/`-` notice. The kebab-case names are correct (confirmed against
  `src/lib/bindings.ts:170,173`).
- `print_serve_banner(http_port, session_id, otp, cloud, qr)` (588-605).
- `run_status` (609-695) — host-running flag, `name`, `publicSessionsEnabled`, ports, TURN,
  hosted-network name, adapter list. `--json` variant. **Prints no driver state** despite its
  own description.
- `run_qr` (699-738) — `-t lan|cloud|all`, `--session-id` (random if omitted), `--no-render`,
  `--json`.
- `run_session` (740-788) — only `new`: mints an id + OTP, prints join targets, and prints a
  `serve --session-id … --otp …` hint.
- `run_devices` (792-931) — `list` (fixed-width table or JSON), `set` (clamps scale 25-200,
  refresh 15-500, video-scale 10-100, video-quality 1-51; `remoteControl`/`systemAudio` from
  `on`/`off`; `store.save()`), `reset`.
- `run_network` (935-989) — `wifi-on`, `wifi-qr` (from `hostedNetworkCredentials`),
  `start`/`stop`/`status`.
- `network_started` (991-1002, Windows) — parses `netsh wlan show hostednetwork` for
  `Status: Started`, matching by splitting on the literal `"Status"` (locale-fragile).
- `network_start` (1004-1022, Windows) — re-invokes **itself** with `hostednetwork <ssid>
  <password>` through `elevated_command`, then re-checks `network_started()`.
- `network_stop` / `network_status` (1024-1051, Windows).
- `network_start` / `network_stop` / `network_status` (1053-1071, non-Windows) — explanatory
  error, exit 1.
- `run_config` (1075-1148) — `path`, `list`, `get`, `set` (value parsed as JSON, falling back to
  a string).
- `run_turn` (1152-1221) — `show` (masks the credential as `(set)`), `set`, `clear`. Both writers
  say the change applies "the next time the host starts".
- `run_account` (1225-1263) — `whoami`, `name [value]`, `avatar`.
- `run_avatar` (1265-1310) — `set <path>` / `remove` / `show`, delegating to the `lib.rs`
  commands.
- `run_autostart` (1314-1355) — `enable` / `disable` / `status` via `ManagerExt`.
- `run_drivers` (1359-1382) — `install` / `remove`; both print "requested" unconditionally
  because the platform functions return `()`.
- `run_doctor` (1386-1434) — `check_system_requirements()` + `check_permissions()`; human and
  `--json` output; exit code is `0`/`1` on `os_supported`.
- `run_logs` (1438-1451) — `--lines/-n` tail of `get_log_backlog()`; in a CLI process the backlog
  is always empty.
- `run_update` (1455-1519) — `check` / `install` via `tauri_plugin_updater`, both
  `block_on(updater.check())`; install ends in `app.restart()`.
- `run_stop` (1523-1535) — `signal_running_instance(ctrl, Quit)`.
- `run_display_settings` (1539-1556) — `control desk.cpl` on Windows,
  `open x-apple.systempreferences:com.apple.preference.displays` on macOS. Note it spawns via
  `std::process::Command`, **not** through `tauri_plugin_shell`, so the
  `capabilities/default.json` entries for those two commands (lines 100-111) are only exercised
  by the frontend path.

### `logbus.rs`
- `set_verbose(bool)` / `verbose()` (14-21) — relaxed atomic.
- `bus()` (33-38) — `OnceLock` init of the ring + `AppHandle` slot.
- `push_line(String)` (40-52) — lock, evict at 2000, `push_back(line.clone())`, unlock, then
  `LogLine(line).emit(app)` if attached.
- `attach(AppHandle)` (54-56) — `OnceLock::set`, result discarded (a second call is silently a
  no-op).
- `get_log_backlog()` (58-62) — clones the whole ring.
- `tprintln!` / `teprintln!` (64-96) — `format!`, echo to stdout/stderr only when verbose, then
  `push_line`.

### `single_instance.rs`
- `acquire_lock(&Path) -> Option<File>` (19-30) — open + `try_lock_exclusive`.
- `wait_for_lock(&Path, Duration)` (32-43) — 150 ms poll loop.
- `start_control_server(AppHandle, PathBuf, fn(&WebviewWindow))` (45-63) — bind loopback:0,
  write `port\ntoken\n`, spawn the accept thread.
- `handle_connection(TcpStream, &AppHandle, &str, fn)` (65-95) — read one line, compare the
  token, dispatch `FOCUS` (`run_on_main_thread(focus)`) or `QUIT` (close the main window, else
  `app.exit(0)`).
- `signal_running_instance(&Path, Command) -> bool` (97-119) — read the ctrl file, connect with a
  2 s timeout, write `"<token> <VERB>\n"`.

### `linux_utils`
- `command_exists(&str)` (mod.rs:14-20) — shells out to `which`.
- `focus_main_window` (22-24) — `window.set_focus()`.
- `setup(AppHandle) -> bool` (26-30) — **returns `command_exists("nmcli")`** and never
  `manage`s an `AppState`.
- `install_audio_driver` / `uninstall_audio_driver` / `audio_driver_status` (32-48) — `"unsupported"`.
- `set_legacy_volume_key_proxy(bool)` (50-52) — empty.
- `refresh_device_stream` (54-58) — `false`.
- `set_device_audio_output` (60-67) — empty.
- `get_device_audio_outputs` (69-76) — `AudioOutputsReport::default()`.
- `get_display_capacity` (78-85) — `backend: "unsupported"`, rest default.
- `compatibility::os_version_string` (3-24) — `/etc/os-release` `PRETTY_NAME`, then `uname -sr`,
  then `"Linux"`.
- `compatibility::check_system_requirements` (26-38) — `os_supported: true`, both backends
  `"unsupported"`, `unsupported_apis` empty.
- `hosted_network::{start,stop,is}_hosted_network` — all `true`.
- `permissions::{check_permissions, request_permission, open_permission_settings}` — empty vec,
  `true`, no-op.
- `streamer::VaapiBackend::{new, encode, set_bitrate}` — all `bail!`.
- `virtual_display::LinuxVirtualDisplay::{new_shared, create_display, remove_display,
  remove_all_displays}` — `Some(Arc::new(Self))`, then `Err("not yet implemented")` and two
  no-ops.

---

## 3. Threads

| Where | Kind | Priority/QoS | Lifetime | Owns |
| --- | --- | --- | --- | --- |
| `single_instance.rs:55` | `std::thread::spawn`, control-server accept loop | default | process lifetime; never joined, never signalled to stop | the `TcpListener`, the token `String`, a cloned `AppHandle`. Handles connections **serially**, 2 s read timeout each. |
| `lib.rs:82` (`run_on_main_thread`) | not a thread — a main-thread hop | — | per FOCUS/QUIT | window focus / close |
| `cli.rs:509` (`ctrlc::set_handler`) | the `ctrlc` crate's own signal thread | default | serve lifetime | sends `()` on an `mpsc` channel |
| `windows_utils/mod.rs:673`, `macos_utils/mod.rs:510` (`remove_all_displays`) | `std::thread::spawn(...).join()` | default | one call | runs display removal off the caller's thread and **blocks the caller** until done — on the exit path this blocks `exit_app` / `run_serve`'s shutdown |
| `windows_utils/mod.rs:804`, `macos_utils/mod.rs:661` | `std::thread::spawn` **per adapter IP** | each builds a full multi-thread tokio runtime with `on_thread_start(tune_transport_thread)` | until the adapter goes away | one axum server + one entire tokio runtime, `worker_threads` unset → `available_parallelism()` workers each |
| `tauri::async_runtime::block_on` at `lib.rs:458/482/558`, `cli.rs:431/469/488/491` | borrows the ambient runtime | — | per call | blocks the caller |

The single most consequential item is the last row of the streamer table: **one tokio runtime
per network adapter**, each sized to the full core count. See the finding
`major-streamer-tokio-runtime-per-adapter.md`.

---

## 4. Locks

| Lock | Guards | Taken by | Hot path? | Guard across `.await` / callback? |
| --- | --- | --- | --- | --- |
| `logbus::LogBus.backlog: Mutex<VecDeque<String>>` (`logbus.rs:27`) | the 2000-line ring | every `tprintln!`/`teprintln!` (429 call sites) and `get_log_backlog` | **yes** — reached from capture, encode and tokio-worker threads | No. `push_line` scopes the guard in an explicit `{ … }` block (lines 42-48) and drops it before `emit`. Correct as written. |
| `logbus::VERBOSE: AtomicBool` (`logbus.rs:12`) | verbose echo | every macro expansion | yes | n/a — relaxed load |
| `logbus::BUS: OnceLock<LogBus>` / `LogBus.app: OnceLock<AppHandle>` | init + the handle | `bus()`, `attach` | yes (a `OnceLock::get` per line) | n/a |
| Tauri-internal `js_event_listeners: Mutex` + `webviews_lock()` (`tauri-2.11.5/src/event/listener.rs:281`, `manager/mod.rs:548`) | the listener registry | **implicitly, once per `tprintln!`**, inside `LogLine::emit` | yes, and invisible from this codebase | No, but it means each log line takes two *foreign* mutexes on top of ours. |
| `fs4` file lock on `screenextend.lock` | the instance token | `acquire_lock` from `lib.rs:614`, `cli.rs:420`, `cli.rs:316` | startup only | n/a. `host_running` takes and immediately drops it — a race against GUI startup. |
| `AppState.*` mutexes (`windows_utils/mod.rs:261-282`) | shared session/override/cloud state | commands invoked by `run_serve` (`set_turn_config`, `set_disconnect_grace`, `set_device_override`, `set_session_credentials`, `register_cloud_session`) | no (startup) | Not in this territory's code. Each of those commands is a synchronous `#[tauri::command]` taking `State<'_, AppState>`; `run_serve` calls them from a plain fn, so the `!Send` compile-time guard CLAUDE.md relies on does **not** apply here — the discipline is manual on this path. |
| `AppState.disable_gpu_encode: Arc<AtomicBool>` | software-encode override | `cli.rs:465-467` (store), `windows_utils/mod.rs:797` (load) | no | The load is a **snapshot into `Config`**, so later stores are invisible to running streamers. Separate finding. |

**No `if let`/`match` scrutinee temporary-lifetime hazards were found in this territory.** The
two `if let Some(app) = bus.app.get()` (`logbus.rs:49`) and
`if let Some(state) = app.try_state::<AppState>()` (`lib.rs:212`) scrutinees produce
`Option<&T>`, not guards, so nothing is held. `cli.rs` uses `store.as_deref().and_then(...)`
throughout rather than holding anything across an await.

---

## 5. Buffers and copies

This territory carries no media buffers. The allocations that matter are per-log-line and
per-config-read:

- **Per `tprintln!`:** (1) `format!` allocates a `String`; (2) `push_line` does
  `line.clone()` (`logbus.rs:47`) — a **second full allocation of the same bytes**; (3)
  `LogLine(line).emit(app)` hands the original to serde, which allocates a **third** buffer for
  the JSON payload inside `EmitArgs::new`, unconditionally, before any listener check. Three
  allocations and one JSON serialization per line, whether or not anything is listening.
- **Ring eviction:** `backlog.pop_front()` at 2000 lines drops one `String` per line thereafter —
  a steady-state alloc/free pair per log line.
- **`get_log_backlog`:** clones every `String` in the ring (up to 2000) into a fresh `Vec`
  (`logbus.rs:61`), then Tauri serializes that to JSON for the IPC reply. Click-time only.
- **Config reads:** `get_nested` (`cli.rs:251-259`) does `cur.get(part)?.clone()` per path
  segment — a full `serde_json::Value` deep clone per dotted level. `run_devices`'s table loop
  clones every field (`cli.rs:822`, `d.get(k).cloned()`). All startup/one-shot, irrelevant to
  latency.
- **`Device` / event payloads:** every `DeviceJoin`/`DeviceModify`/… carries owned `String`s and
  is serialized per emission. These fire on join/leave/settings-change, not per frame.

No zero-copy, GPU→GPU, or readback paths exist here.

---

## 6. Queues

- **`logbus` ring** — `VecDeque<String>`, capacity hint 2000, hard cap `MAX_BACKLOG = 2000`
  (`logbus.rs:10`). Bounded, non-blocking, **drop-oldest** back-pressure. Under high log volume
  the oldest lines are silently lost, which is the right behaviour but means the backlog can
  cover a very short window when a hot path is logging.
- **`run_serve`'s Ctrl+C channel** — `std::sync::mpsc::channel()` (`cli.rs:508`), unbounded, one
  message ever. The main thread blocks in `rx.recv()` (line 512). The `set_handler` result is
  discarded, so if the handler cannot be installed `serve` blocks forever on a channel nobody
  will ever send to — the process would then only die on a hard kill, skipping `broadcast_bye`
  and `remove_all_displays` and leaving virtual displays behind.
- **`single_instance` control server** — no queue; the OS accept backlog is the only buffer, and
  connections are handled one at a time with a 2 s read timeout.
- **Tauri event bus** — `LogLine` and the device events go through Tauri's listener registry.
  There is no batching and no coalescing anywhere in this territory.

---

## 7. Syscall / FFI boundaries

- **`AttachConsole(ATTACH_PARENT_PROCESS)`** — `cli.rs:15-18`, `windows::Win32::System::Console`.
  The `windows` 0.62 feature `Win32_System_Console` (`Cargo.toml:150`) exists solely for this.
  Return value discarded; failure is invisible, which is correct (no parent console is normal).
- **`libc::isatty(STDOUT_FILENO/STDERR_FILENO)`** — `cli.rs:134`, macOS only, inside `unsafe`.
  Floor-present, no dyld exposure.
- **`objc2::runtime::AnyClass::get(c"WKWebView")` + `responds_to(sel!(setPageZoom:))`** —
  `lib.rs:237-238`. Pure runtime interop; **no link-time reference**, so it is dyld-safe on the
  10.15 floor. This is the model the rest of the macOS code is supposed to follow.
- **`fs4::fs_std::FileExt::try_lock_exclusive`** — `single_instance.rs:26`. `LockFileEx` on
  Windows, `flock` on unix.
- **`TcpListener`/`TcpStream` on `127.0.0.1`** — `single_instance.rs:50, 108`. Loopback only.
- **`std::process::Command`** — `cli.rs:993` (`netsh wlan show hostednetwork`), `1026`
  (`netsh wlan stop hostednetwork`), `1011` (self re-invocation under `elevated_command`), `1541`
  (`control desk.cpl`), `1546` (`open x-apple.systempreferences:…`), and
  `linux_utils/{mod.rs:15 (which), compatibility.rs:15 (uname)}`. **None of these go through
  `tauri_plugin_shell`**, so the `capabilities/default.json` allowlist does not gate them — the
  allowlist only constrains the frontend-initiated path in `lib.rs`.
- **`tauri_plugin_shell`** — `lib.rs:459/472/491/505/516/528/543` (netsh, certutil x2, nefconc x3)
  and `lib.rs:567/580/592/598`. These *are* ACL-gated by `capabilities/default.json:11-112`.
- **`elevated_command::Command`** — `cli.rs:1014`. UAC elevation on Windows.
- **`qr2term::print_qr`** — `cli.rs:349`, writes ANSI to stdout.
- **`ctrlc::set_handler`** — `cli.rs:509`, installs a `SIGINT`/console-ctrl handler.
- **`whoami::username()`** — `lib.rs:245`.
- **`macos_utils::objc_exception::install_logger()`** — `lib.rs:332`, called unconditionally at
  the very start of `run()` on macOS, before the Tauri builder.
- **`crate::macos_utils::audio::legacy::routing::watchdog_recover()`** — `cli.rs:162`, the hidden
  `audio-recover` subcommand, reached from `fast_path` before any Tauri machinery exists.

No versioning structs, no non-Rust calling conventions, and no `extern static` references in this
territory. The one macOS API used (`WKWebView.setPageZoom:`, Safari 14+) is probed by selector.

---

## 8. Panics

Every one of these is reachable from a runtime condition, not a logic invariant.

**`lib.rs`, normal startup path (affects every user):**
- `app.path().app_local_data_dir().unwrap()` (609). A resolution failure panics before any window
  is drawn, with no dialog.
- `WebviewWindowBuilder::…build()?` (653-663) — this one is a `?` into `setup`'s error, which
  Tauri then turns into `.expect("error while running tauri application")` at line 678.
- `.expect("error while running tauri application")` (679) — the final catch-all.

**`lib.rs`, debug-only bindings export (398-408):** three `.expect`s — the specta export, the
read-back, and the write-back — all on a relative path resolved against the CWD.

**`lib.rs`, legacy subcommand branches (446-604):** `hostednetwork` — `.expect("no ssid")` (446),
`.expect("no password")` (452), two `.output().await.unwrap()` (471, 477). `installdrivers` —
`resolve(...).unwrap()` + `into_string().unwrap()` per `resource_path` call (485-489),
`resource_dir().unwrap()` at 499/511/524/536/550, and five `.output().await.unwrap()` at
502/514/527/542/553. `removedrivers` — the same shape, 559-604. These run during **uninstall**
(`windows/hooks.nsh:4`, `windows/fragment.wxs:6`), where a panic is the worst possible outcome.

**`cli.rs`:**
- `set_path`'s two `.expect("object node")` (280, 291) — reachable from
  `ScreenExtend config set a.b.c value` when an intermediate node exists but is not an object.
  `set_nested` guards the *root* (263-269) but `set_path` guards only by replacing non-objects
  *before* recursing, so the `expect` is defensive rather than provably unreachable.
- `serde_json::to_string_pretty(&out).unwrap()` — 638, 725, 769, 809, 1098, 1400.
- `device.as_object_mut().unwrap()` (864) — the value comes from either a `json!` literal or a
  previously stored entry; a hand-edited `config.json` with a non-object in `devices[]` panics.
- `std::env::current_exe().unwrap_or_default()` (1010) — degrades to an empty path, which then
  fails to spawn rather than panicking. Fine.

**`logbus.rs`:** `backlog.lock().unwrap()` at 43 and 61 — panics only if a previous holder
panicked while holding it.

**`linux_utils`:** none, but only because most of the module does not exist.

---

## 9. Silent fallbacks and swallowed errors

- `logbus.rs:50` — `let _ = LogLine(line).emit(app);` an IPC failure is invisible.
- `logbus.rs:55` — `let _ = bus().app.set(app);` a **second** `attach` silently does nothing.
- `lib.rs:433` — `let _ = crate::cli::dispatch(app.handle());` the `Outcome` is discarded (safe
  today, since every CLI branch is `-> !`, but it would silently swallow a future `LaunchGui`
  distinction).
- `lib.rs:610` — `let _ = std::fs::create_dir_all(&lock_dir);` failure surfaces later as a
  bogus "already running".
- `lib.rs:628`, `641` — `single_instance::signal_running_instance(...)` return value ignored in
  both the Quit and Focus paths; if the signal fails, the Focus path still `exit(0)`s, so the
  user clicks "Show running instance" and simply nothing happens.
- `lib.rs:649` — `std::mem::forget(file)` — deliberate (holds the lock for the process
  lifetime) and worth keeping, but it also means the lock file is never explicitly released.
- `single_instance.rs:57` — `let Ok(stream) = stream else { continue };` accept errors dropped.
- `single_instance.rs:58` — `let _ = stream.set_read_timeout(...)`.
- `single_instance.rs:72` — a read error returns silently.
- `single_instance.rs:77` — a **token mismatch returns silently**; there is no log line, so a
  wrong-token probe is undetectable.
- `single_instance.rs:82, 87, 88` — `let _ = app.run_on_main_thread(...)`, `let _ =
  window.close()`.
- `single_instance.rs:111` — `let _ = stream.set_write_timeout(...)`.
- `cli.rs:17` — `let _ = AttachConsole(...)`.
- `cli.rs:418` — `let _ = std::fs::create_dir_all(&lock_dir);`.
- `cli.rs:440` — `let store = app.store(CONFIG_STORE).ok();` — a store failure makes `serve`
  silently fall back to **every default** (ports 8080/8443, no TURN, no saved device overrides,
  cloud on) with no warning. Contrast `open_store` (241-249), which exits 1. The two behaviours
  should not differ.
- `cli.rs:506` — `let _ = std::io::stdout().flush();`.
- `cli.rs:509` — `let _ = ctrlc::set_handler(...)` — see the queue note; a failure here makes
  `serve` unstoppable except by hard kill.
- `cli.rs:702, 749` — `app.store(...).ok()` again, same silent-default behaviour in `qr` and
  `session new`.
- `cli.rs:350` — `qr2term` errors go to stderr, not the logbus.
- `cli.rs:1017` — `let _ = app;` and `cli.rs:1401` — `let _ = app;` (unused-parameter silencing).
- `lib.rs:262, 276, 288, 296` — avatar errors go to `log::error!`, **not** `teprintln!`, so they
  never reach the backlog or the Settings log terminal. This is the one place in the territory
  that violates CLAUDE.md's logging rule.
- `linux_utils/mod.rs:17-19`, `compatibility.rs:15-22` — `which`/`uname` failures collapse to
  `false` / `"Linux"`.

---

## 10. TODO / FIXME / HACK / XXX

**None.** A grep for `TODO|FIXME|HACK|XXX` across the entire territory (`lib.rs`, `cli.rs`,
`main.rs`, `logbus.rs`, `single_instance.rs`, `linux_utils/`, `Cargo.toml`, `build.rs`,
`tauri.conf.json`, `capabilities/`, `windows/`, `.github/workflows/`, `README.md`,
`package.json`) returns zero matches.

The nearest equivalents are prose markers rather than tags:
- `resources/PROVENANCE.md:5-6` — 'Versions/tags marked "confirm" should be verified against the
  upstream release', with `confirm` appearing at lines 14, 16, 78-79.
- `binaries/PROVENANCE.md:6-7` — the same "confirm" caveat for the nefcon version and license.
- `resources/PROVENANCE.md:63-67` — the `⚠️ Architecture` note that the shipped
  `libopus.dylib` is x86_64-only and must be replaced with a universal build.
- `pnpm-workspace.yaml:15-19` — three `auditConfig.ignoreGhsas` entries
  (`GHSA-r5fr-rjxr-66jc`, `GHSA-f23m-r3pf-42rh`, `GHSA-xxjr-mmjv-4gpg`) with no comment saying
  what they are or when they can be dropped. Nothing runs `pnpm audit` today anyway.

---

## 11. Latency path

**None of this territory sits on the glass-to-glass path — with one exception, and it is a real
one.**

| Stage | On the path? | Cost |
| --- | --- | --- |
| `main.rs`, `lib.rs::run` setup, window build | no | startup only |
| `cli.rs` subcommand handling | no | one-shot; every branch exits |
| `cli.rs::run_serve` startup | no, but it **configures** the path | ports, TURN, grace and device overrides are all applied before `watch_for_network_changes`, so ordering is correct today (see the ordering note below) |
| `single_instance.rs` | no | startup + the occasional FOCUS/QUIT |
| **`logbus::push_line`** | **yes, indirectly** | 3 allocations + 1 JSON serialization + 3 mutexes per line, from 429 call sites including 50 in `windows_utils/streamer/pipeline.rs`, 41 in `streamer/server.rs`, 30 in `streamer/webrtc_session.rs`, 12 in `windows_utils/audio/loopback.rs`. The ring mutex is **shared between the real-time capture/audio threads and every tokio worker**, which is exactly the kind of cross-thread lock CLAUDE.md forbids on the capture path. |
| `Cargo.toml` release profile | **yes** | there isn't one. Every shipped build runs `lto = false`, `codegen-units = 16`, which suppresses cross-crate inlining through `webrtc`/`bytes`/the encoder FFI wrappers. |
| One tokio runtime per adapter | **yes** | N_adapters × N_cpus priority-tuned worker threads oversubscribing the machine the encoder is trying to run on. |

**Serialization points found in this territory:**
1. `remove_all_displays` (`windows_utils/mod.rs:673`) spawns a thread and immediately `.join()`s
   it, so `exit_app` and `run_serve`'s shutdown block on display teardown on the main thread.
2. `run_serve` is strictly sequential: `platform::setup` (`cli.rs:431`, itself a
   `block_on` over a `spawn_blocking` display creation) → ports → software-encode → TURN → grace
   → `apply_saved_devices` → credentials → `watch_for_network_changes` → cloud registration →
   banner. Nothing overlaps. Startup, not steady state, but it is why `serve` takes a while to
   print its banner.
3. `logbus`'s single global mutex serializes every logging thread against every other one.

**Ordering fragility worth recording:** `run_serve` sets `state.server_ports` (`cli.rs:457`) and
`disable_gpu_encode` (`cli.rs:464-468`) **before**
`platform::networking::watch_for_network_changes` (`cli.rs:490`), which is what actually spawns
the streamers. That ordering is load-bearing and undocumented — both values are read by value
into each streamer's `Config` at creation time (`windows_utils/mod.rs:783, 797`), so moving
either line below 490 would silently disable `--http-port` and `--software-encode`.

---

## 12. Stale references

**Documents cited that do not exist:**
- `README.md:127` cites `.github/workflows/ci.yml`. Deleted in `e1d5d28` (2026-08-22, "fix:
  temporarily disable ci till further notice", -92 lines). `find .github -type f` returns only
  `build-release.yml`.
- `src-tauri/resources/PROVENANCE.md:3-4` and `src-tauri/binaries/PROVENANCE.md:3-4` both say
  checksums are "verified in CI". The `blob-checksums` job that did that was in the deleted
  `ci.yml` (lines 84-92 of `e1d5d28^:.github/workflows/ci.yml`).
- `binaries/PROVENANCE.md:14` cites "`src-tauri/src/lib.rs` `installdrivers`/`removedrivers`" —
  still accurate (`lib.rs:481`, `557`), but CLAUDE.md calls those legacy and directs new work to
  `drivers install|remove`, so the provenance file points at the deprecated path.
- `binaries/PROVENANCE.md:26-31` says macOS builds ship the two placeholder stubs.
  `tauri.macos.conf.json:25` sets `"externalBin": []`, so macOS builds ship no `nefconc` at all.
- `resources/PROVENANCE.md:39` cites `windows_utils/audio/opus_sys.rs`; per CLAUDE.md the Opus
  FFI moved to `streamer/audio/opus_sys.rs` (which line 52 of the same file correctly cites) —
  the two references in one document disagree.

**References to AI tooling:**
- `src-tauri/.gitignore` — `# AI documentation` → `**/ai-docs/`, and `# AI tooling` →
  `**/samples/`, `**/vddrs/`. None of those directories exist anywhere in the tree.
- `.gitignore:43-48` — `# Temporary agent files` → `docs/`, `.agents/`, `skills-lock.json`,
  `PLAN.md`, `PROMPT.md`. `docs/` being ignored means **this report is untracked**.
- `.gitignore:26-28` — `# Testing files` → `testwaior - Copy/` (does not exist), `.claude/`
  (not a testing file).

**Comments that no longer describe the code:**
- `lib.rs:1` — `//use rand::Rng;`, `lib.rs:7` — `//use tauri::Emitter;`: commented-out imports.
- `lib.rs:184-207` — a 24-line commented-out `get_devices` mock command, with a matching
  commented-out entry in `collect_commands!` at line 341. It uses `rand::thread_rng()` and
  `gen_range(1, 256)`, i.e. the `rand` 0.8 API; the crate is now `rand 0.9.1`
  (`Cargo.toml:78`), so the block would not even compile if uncommented.
- `cli.rs:1017` — `let _ = app;` inside `network_start`, and `cli.rs:1401` — `let _ = app;`
  inside `run_doctor`: both are unused-parameter silencers that read like intent markers.
- `main.rs:1` — "Prevents additional console window on Windows in release, DO NOT REMOVE!!" is
  accurate, but it is worth noting that this is precisely what makes `attach_console()`
  necessary, and neither comment points at the other.

**CLAUDE.md disagreements with the code** (the brief says the code wins):
- CLAUDE.md: "Current version: **0.5.4**". Actual: **0.6.3** in all four required files
  (`package.json:4`, `Cargo.toml:3`, `tauri.conf.json:5`, `tauri.macos.conf.json:5`) — those four
  agree with each other; only CLAUDE.md is stale.
- CLAUDE.md: "`linux_utils/` Linux scaffold (compiles, mostly `bail!`s)". It does not compile.
- CLAUDE.md's registered-command list omits `set_window_zoom` (`lib.rs:347`) and
  `get_display_capacity` (`lib.rs:374`), both of which are registered.
- CLAUDE.md's macOS module list omits `macos_utils/objc_exception.rs`, which `lib.rs:332` calls
  on every macOS launch.
- CLAUDE.md's repo map omits `linux-vdd/` (present, gitignored) and `scripts/`.

---

## 13. Dead code candidates

| Candidate | Evidence nothing selects it |
| --- | --- |
| **`clap` dependency** (`Cargo.toml:141`) | `grep -rn "clap" src-tauri/src` → **zero matches**. The shipped CLI uses `tauri-plugin-cli` matches; the probe harness parses `std::env::args` by hand. |
| **`tauri-plugin-http`** (`Cargo.toml:40`, `lib.rs:427`) | No `http:*` permission in `capabilities/default.json`, so the frontend cannot invoke it; no `@tauri-apps/plugin-http` in `package.json`; no Rust caller. |
| **`lib.rs:184-207` commented `get_devices`** | Commented out, along with its `collect_commands!` entry (341). Uses a `rand` 0.8 API the crate no longer provides. |
| **`Outcome::LaunchGui`'s return value at `lib.rs:433`** | `let _ = …`; every `route` arm except the three legacy names is `-> !`. |
| **`ScreenExtend logs` + its `--lines` flag** | The backlog is per-process; a CLI process always has an empty one. `cli.rs:1444-1446` says so itself. |
| **`crossbeam-channel` duplicate declaration** (`Cargo.toml:89`) | Already declared at `Cargo.toml:71` for all targets. |
| **`wmi` in the shared `[dependencies]`** (`Cargo.toml:80`) | Used only at `windows_utils/compatibility.rs:7` and `windows_utils/networking.rs:8`. |
| **`[lib] crate-type = ["staticlib", "cdylib"]`** (`Cargo.toml:17`) | The binary needs only `rlib`; there is no mobile target configured despite `#[cfg_attr(mobile, …)]` at `lib.rs:328`. |
| **`capabilities/default.json:100-111`** (`control` / `open`) | `run_display_settings` (`cli.rs:1539-1556`) spawns both with `std::process::Command`, bypassing the plugin. Only a frontend caller would exercise these entries — worth confirming one exists before removing them. |
| **`linux_utils/streamer.rs::VaapiBackend`** | Never named outside its own file; the Linux target does not build. |
| **`[lints.rust] dead_code = "allow"`** (`Cargo.toml:23-24`) | Not dead itself, but it is why the above accumulates undetected. |

---

## 14. Capability probing

**Done correctly (probe, not version):**
- `webview_zoom_supported()` (`lib.rs:233-240`) — `AnyClass::get(c"WKWebView")` +
  `responds_to(sel!(setPageZoom:))`, cached in a `OnceLock`. Page zoom arrived with Safari 14;
  this probes the selector rather than testing for 10.16/11. Exactly the doctrine.
- `linux_utils::command_exists("nmcli")` (`mod.rs:14-20`) — probes for a binary rather than
  sniffing a distro. The *use* of the result is wrong (see below), but the probe shape is right.

**Version-number branching in this territory: none found.** No `if os_version >= …` anywhere in
`lib.rs`, `cli.rs`, `logbus.rs` or `single_instance.rs`.

**Missing tiers / gaps:**
1. **`doctor` reports no tier at all.** `CompatibilityReport` already carries `audio_backend` and
   `display_backend` (`lib.rs:110-111`), populated by the per-OS
   `probe_audio_backend()`/`probe_display_backend()` — the exact runtime-probe results the tier
   doctrine produces. `run_doctor` (`cli.rs:1386-1434`) prints neither, in either the human or
   the `--json` output. The diagnostic exists and is thrown away at the last step.
2. **No TCC/permission depth.** `run_doctor` prints `PermissionStatus.key/name/granted/required`
   but never `description` (`lib.rs:78`), so "MISSING Screen Recording" comes with no remedy. On
   Windows and Linux `check_permissions()` returns an empty vec, so the section vanishes rather
   than saying "not applicable".
3. **`doctor`'s exit code ignores permissions.** `exit(if report.os_supported {0} else {1})`
   (`cli.rs:1433`) — a supported OS with a missing *required* permission exits 0, so no script or
   CI can detect it.
4. **`status` claims driver state it never shows.** Both `print_help()` (`cli.rs:77`) and
   `tauri.conf.json:79` describe `status` as showing "session, network, **driver** and config
   state"; `run_status` (609-695) prints no driver information.
5. **`linux_utils::setup` uses a probe to answer the wrong question.** It returns
   `command_exists("nmcli")` — the presence of NetworkManager — as its answer to "is the host
   ready to stream", while `LinuxVirtualDisplay::create_display` unconditionally errors and
   `setup` never even `manage`s an `AppState`.
6. **`linux_utils::compatibility` reports `os_supported: true`** with an empty
   `unsupported_apis` vec, i.e. it claims full capability on a target that has none.
7. **`network_started()`** (`cli.rs:991-1002`) parses `netsh` output for the literal English
   strings `"Status"` and `": Started"` — a locale-dependent string probe where a structured
   query (or the `Networking_NetworkOperators` WinRT surface already enabled in `Cargo.toml:182`)
   would be robust.

---

## 15. Questions and cross-cutting concerns for the orchestrator

1. **Is `docs/` meant to be a deliverable?** `.gitignore:44` excludes the whole tree, so every
   discovery report and every synthesized document is untracked. Decide before the synthesis
   phase, not after.
2. **`logbus` is a cross-territory latency question, not a shell one.** 429 call sites, 50 of
   them in `windows_utils/streamer/pipeline.rs` alone. Somebody who owns the pipeline needs to
   answer: **does any `tprintln!` fire per frame?** If yes, the fix is urgent; if no, it is a
   tidy-up. I could not answer it from this territory. The gate itself (skip `emit` when nothing
   is listening) is cheap and worth doing regardless.
3. **One tokio runtime per network adapter** (`windows_utils/mod.rs:804`,
   `macos_utils/mod.rs:661` × `streamer/mod.rs:48-52`, `worker_threads` unset). This spans the
   shell, the streamer and both platform territories. It should be one owner's decision, not
   three.
4. **`disable_gpu_encode` is snapshotted by value into each streamer's `Config`**
   (`windows_utils/mod.rs:797`) while every neighbouring field is a shared `Arc`. The GUI toggle
   therefore does nothing to a running streamer. Whoever owns `Config` should decide whether
   other fields have the same problem.
5. **CI has been off since 2026-08-22.** No `cargo fmt`/`clippy`/`test`, no `pnpm lint`, no
   `cargo audit`, no checksum verification, on any commit for roughly nine months. For a
   *compatibility* engagement targeting two OSes from one dev box, restoring the macOS/Windows
   Rust matrix is arguably the highest-leverage single action available. Note the
   `blob-checksums` job will fail on day one until the darwin stub hashes are re-recorded.
6. **`serve` cannot be stopped by `ScreenExtend stop`** and its own error message says otherwise.
   If the engagement plans to use headless `serve` for measurement runs, this needs fixing first
   or every run ends in a hard kill that skips display teardown.
7. **`disconnectGraceSecs` is split-brained** — GUI writes `localStorage`, CLI reads
   `config.json`. Worth a sweep for other settings with the same split; `zoomFactor` is the other
   candidate.
8. **`serve` drops device trust.** Banned devices are un-banned and approved devices must
   re-OTP. If any test plan uses `serve` as the host, its security posture differs from the GUI's.
9. **No `[profile.release]`.** Whoever owns the measurement methodology should decide whether to
   land `lto`/`codegen-units` **before** the baseline numbers are taken, so before/after
   comparisons are not confounded.
10. **`tauri.macos.conf.json` nulls the CSP.** A Windows/macOS behavioural divergence in a file
    the engagement will touch anyway.
11. **`.gitattributes` is one line.** The vendored blob checksums are checkout-dependent for the
    two text-detected files; fixing this properly means a `git add --renormalize` pass, which is
    a repo-wide diff and should be scheduled deliberately rather than snuck into a feature branch.
12. **`plugins.cli` and `cli.rs` are in sync** — I verified every declared subcommand has a
    `route()` arm and every `arg_str`/`arg_flag` key `cli.rs` reads is declared in
    `tauri.conf.json`. That is one thing that has *not* drifted, and it is worth keeping a test
    or a lint on it.

---

## Appendix A — `lib.rs::run` lifecycle, in order

1. `cli::fast_path()` (`330`) — help/version/`audio-recover`. May `exit(0)` before anything else
   exists.
2. `macos_utils::objc_exception::install_logger()` (`331-332`, macOS only).
3. Build the specta `Builder` (`334-396`) — `collect_commands!` (36 commands) and
   `collect_events!` (12 events).
4. **Bindings export** (`398-408`) — `#[cfg(debug_assertions)]` **and**
   `std::env::args_os().nth(1).is_none()`. Writes `../src/lib/bindings.ts` (a **relative** path,
   resolved against the CWD), reads it back, and rewrites it prefixed with `// @ts-nocheck`.
   Three `.expect`s. So `cargo run -- serve` in debug does *not* regenerate; `pnpm tauri dev`
   does.
5. **Plugin stack** (`410-428`), in registration order: `updater` (with a custom comparator —
   `update.version != current`, so **any** difference counts as an update, including a
   downgrade), `process`, `autostart` (`MacosLauncher::LaunchAgent`), `dialog`, `os`,
   `clipboard-manager`, `shell`, `notification`, `cli`, `http` (**orphaned** — no capability, no
   JS package), `store`.
6. `.menu(build_menu)` (`429`), `.invoke_handler(builder.invoke_handler())` (`430`).
7. **`setup` closure** (`431-677`):
   a. `builder.mount_events(app)` (`432`).
   b. `cli::dispatch(app.handle())` (`433`) — never returns for a real subcommand.
   c. `streamer::input::prime()` (`434`) — warms the macOS keyboard path.
   d. `match app.cli().matches().ok().and_then(|m| m.subcommand)` (`435`), with four arms:
      - `"hostednetwork"` (`436-480`) — two `netsh` calls via `tauri_plugin_shell`, then
        `exit(0)`.
      - `"installdrivers"` (`481-556`) — `certutil -addstore root`, `certutil -addstore
        TrustedPublisher`, `nefconc --remove-device-node`, `nefconc --create-device-node`,
        `nefconc --install-driver`, then `exit(0)`.
      - `"removedrivers"` (`557-607`) — `nefconc --remove-device-node`,
        `nefconc --uninstall-driver`, `certutil -delstore root`, `certutil -delstore
        TrustedPublisher`, then `exit(0)`.
      - **default** (`608-674`) — resolve `app_local_data_dir` (`.unwrap()`), `create_dir_all`,
        `acquire_lock`. On conflict, a blocking dialog with "Quit running instance" /
        "Show running instance"; Quit → `signal_running_instance(Quit)` + `wait_for_lock(10 s)`,
        exiting 0 if still locked; Show → `signal_running_instance(Focus)` + `exit(0)`. Then
        `std::mem::forget(lock)`, `logbus::attach`, build the window (`main`,
        `WebviewUrl::App("index.html")`, min 1050×650, inner 1200×675, resizable, **maximized**,
        title "ScreenExtend"), `focus_main_window`, and finally `start_control_server`
        (the only `teprintln!` in the whole file, at `672`).
8. `.run(tauri::generate_context!())` with a trailing `.expect(...)` (`678-679`).

## Appendix B — `serve` startup sequence (`cli.rs:408-522`), in order

1. `logbus::set_verbose(arg_flag("verbose"))` (409).
2. Resolve `app_local_data_dir`, `create_dir_all`, `acquire_lock` → `std::mem::forget`
   (411-428). **No control server is started.**
3. `println!("Starting the ScreenExtend host…")` (430).
4. `block_on(platform::setup(app.clone()))` (431) — creates the virtual display controller and
   `manage`s `AppState`. On `false`: "could not initialize the virtual display driver" + the
   `drivers install` hint, exit 1.
5. `app.state::<AppState>()` (437), `streamer::input::prime()` (438).
6. `app.store(CONFIG_STORE).ok()` (440) — **silently `None` on failure**.
7. Ports: `--http-port` → `serverPorts.http` → `DEFAULT_HTTP_PORT`; same for https (441-456).
   `state.server_ports.set(http, https)` (457).
8. Software encode: `--software-encode` **or** `disableGpuEncode` → store the atomic (459-468).
9. If the store opened (470-484): TURN (`turnConfig.urls/username/credential`, only if urls is
   non-empty) → `platform::set_turn_config`; `disconnectGraceSecs` (**a key the GUI never
   writes**) → `platform::set_disconnect_grace`; **`apply_saved_devices(app, store)`** (483).
10. Session id (`--session-id` or random) + OTP (`--otp` or random) →
    `platform::set_session_credentials` (486-488).
11. `platform::networking::watch_for_network_changes(app.clone())` (490) — **this is what
    actually spawns the per-adapter streamers**, which is why steps 7-9 must precede it.
12. Cloud: `!--no-cloud && publicSessionsEnabled` (default true) →
    `platform::register_cloud_session` (492-499).
13. `install_device_listeners(app)` (501) — `device-join` / `device-remove` console notices.
14. `print_serve_banner(http, session_id, otp, cloud, !--no-qr)` (503) — session/OTP, then one
    HTTP join URL + QR per adapter, then the cloud URL. **HTTPS is never printed.**
15. "Host is running. Press Ctrl+C to stop.", flush, `ctrlc::set_handler` → `mpsc`, block on
    `rx.recv()` (505-512).
16. On Ctrl+C: "Stopping…", `broadcast_bye(HostExit, 250 ms)`,
    `platform::remove_all_displays(&state.virtual_display)` (which spawns and joins a thread),
    `exit(0)` (514-521).
