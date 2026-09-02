# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ScreenExtend is a Tauri 2 desktop app that turns any device with a web browser into a
wireless second monitor. It runs on the **host** (the PC being extended). A **client**
(phone/tablet/laptop) joins by opening a URL or scanning a QR code — no client install. The
host creates a real virtual display per client, GPU-encodes it, and streams it over WebRTC
(WHEP signaling, H.264). `README.md` has the user-facing feature overview.

Two distinct frontends live in this repo:
- `src/` — the **desktop control UI** (React, runs in the Tauri webview on the host).
- `src-tauri/src/streamer/static/` — the **client web page** served to joining devices
  (`index.html`, `input.js`, `audio.js`, `audio-worklet.js`, `transform-worker.js`,
  `nosleep.js`, `styles.css`, `logo.svg`). This is plain HTML/JS/CSS served by the Rust HTTP
  server, `include_str!`'d into the binary, NOT part of the Vite/React build. Edit it
  directly; there is no bundler step for it and it must stay dependency-free.

## Repo map

```
src/                        desktop control UI (React + Vite)
src-tauri/                  Rust core (crate `screenextend`, lib `screenextend_lib`, bin `ScreenExtend`)
  src/cli.rs                the shipped command-line interface
  src/lib.rs                Tauri app: command/event registry, setup, lifecycle
  src/logbus.rs             tprintln!/teprintln! log bus (backlog + LogLine event)
  src/single_instance.rs    lockfile + loopback control server (focus / quit)
  src/streamer/             OS-independent streaming core + the client web page
  src/windows_utils/        Windows backend
  src/macos_utils/          macOS backend
  src/linux_utils/          Linux scaffold (compiles, mostly bail!s)
  resources/                bundled DLLs/driver/cert + PROVENANCE.md + SHA256SUMS
  binaries/                 nefconc sidecar (externalBin) + provenance
  capabilities/             Tauri permission capabilities (shell allowlist lives here)
  windows/                  NSIS hook + WiX fragment for the installer
  examples/audio_spike.rs   throwaway WASAPI/Opus measurement spike
macos/ScreenExtendAudio/    the macOS 10.15–12.x virtual audio device (AudioServerPlugIn, C++)
.github/workflows/          build-release.yml only (CI is currently disabled — see below)
```

## Commands

```sh
pnpm install
pnpm approve-builds --all        # approve native build scripts (esbuild, sharp, core-js-pure)

pnpm tauri dev                   # run the full desktop app (host). Also regenerates src/lib/bindings.ts
pnpm tauri build                 # production build -> src-tauri/target/release/bundle/

pnpm dev                         # frontend only, Vite dev server on fixed port 1420
pnpm build                       # tsc + vite build (type-check + web bundle only, no Tauri)
pnpm lint                        # ESLint 9
```

macOS builds take an overlay config: `pnpm tauri build --config src-tauri/tauri.macos.conf.json`
(that is what release CI passes, along with `--target universal-apple-darwin`).

Rust code lives in `src-tauri/`; run `cargo` commands from there (`cargo check`, `cargo build`,
`cargo test`, `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`).

Linting: ESLint 9 flat config in `eslint.config.js`, scoped to `src/` (`src-tauri/`,
`src/lib/bindings.ts` and `src/lib/next-navigation-stub.ts` are ignored) with `react-hooks`
(rules-of-hooks as an error), `react-refresh` and `typescript-eslint`. It passes with 0 errors
and ~10 non-blocking warnings. TypeScript strictness is additionally enforced by `tsc` in
`pnpm build`.

Tests are inline `#[cfg(test)]` modules, several of them collected in per-module `test/`
subdirectories (`streamer/test/`, `windows_utils/streamer/test/`, `windows_utils/audio/test/`,
`macos_utils/audio/test/`, `macos_utils/audio/legacy/test/`). Many are hardware- or OS-gated and
no-op off their platform.

## The shipped CLI (`src-tauri/src/cli.rs`)

The single binary is both the GUI app and a full CLI; with no arguments it opens the desktop app.
There are two entry points, and both matter:

1. **`cli::fast_path()`** — called at the very top of `lib.rs::run`, *before* the Tauri builder.
   Handles `--help`/`-h`/`help`, `--version`/`-V`/`version`, and the hidden macOS
   `audio-recover` watchdog (restores the saved default audio output after a crash). These must
   work without a Tauri app, hence the separate path.
2. **`cli::dispatch(app.handle())`** — called from `setup`, routes everything else through
   `desktop::route` (compiled only on Windows/macOS). Every branch is `-> !` and calls `exit()`;
   returning `Outcome::LaunchGui` is what falls through to the window.

Subcommands are declared in `tauri.conf.json` under `plugins.cli` (the `tauri-plugin-cli`
matches drive the router) and implemented in `cli.rs`:

```
serve [--http-port N] [--https-port N] [--session-id ID] [--otp CODE]
      [--no-cloud] [--no-qr] [--software-encode] [-v|--verbose]
status [--session-id ID] [--json]
qr [-t lan|cloud|all] [--session-id ID] [--no-render] [--json]
session new [--no-render] [--json]
devices list | set <ip> [--scale --orientation --refresh-rate --video-scale
                         --video-quality --control on|off --audio on|off] | reset <ip>
network start <ssid> <password> | stop | status | wifi-on | wifi-qr
config list | get <key> | set <key> <value> | path      # dotted keys, e.g. serverPorts.http
turn show | set <urls> [--username U] [--credential C] | clear
account name [value] | whoami | avatar set <path>|remove|show
autostart enable | disable | status
drivers install | remove
doctor [--json]
logs [--lines N]
update check | install
display-settings
stop
```

Notes:
- `serve` is the headless host: it takes the single-instance lock, runs `platform::setup`,
  applies saved config (ports, TURN, disconnect grace, per-device overrides), mints or takes a
  session id + OTP, optionally registers the cloud session, prints join URLs/QRs, and blocks on
  Ctrl+C (`ctrlc`), removing all virtual displays on the way out.
- Most read commands accept `--json`.
- `network start/stop/status` are Windows-only; on macOS they print an explanatory error because
  the hosted network is owned by the running host process.
- `stop` and the "already running" dialog talk to the running instance over
  `single_instance`'s loopback control server.
- On Windows the binary is GUI-subsystem in release (`windows_subsystem = "windows"` in
  `main.rs`), so `attach_console()` (`AttachConsole(ATTACH_PARENT_PROCESS)`) is what makes CLI
  output visible in the parent shell.
- `installdrivers`, `removedrivers` and `hostednetwork <ssid> <password>` are **legacy**
  subcommands still handled inline in `lib.rs::run`'s `setup` (they shell out to
  `certutil`/`nefconc`/`netsh` and exit). `route` deliberately returns `LaunchGui` for them so
  `lib.rs` keeps ownership. Prefer `drivers install|remove` and `network start` for new work.

### Developer probe harness (separate, not shipped-facing)

`src-tauri/src/streamer/cli.rs` + `streamer/config.rs::Config::from_args` + `Streamer::probe_*`
are a **standalone developer harness** for exercising capture→encode→WebRTC in isolation
(`--probe-capture`, `--probe-dxgi`, `--probe-encode`, `--probe-live`, `--probe-bitrate`,
`--whep-selftest`, plus tuning flags like `--encoder`, `--qp`, `--h264-profile`,
`--intra-refresh`, `--scale`, `--turn-*`). It parses `std::env::args` by hand and is **not**
wired into `main.rs`; `Config::from_args`'s help text still names an old crate.

## The Rust ↔ TypeScript bridge (important)

Commands and events are defined in Rust and consumed type-safely in TS via
[`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta):

- All commands/events are registered in `collect_commands!` / `collect_events!` in
  `src-tauri/src/lib.rs::run`.
- The bridge is exported to **`src/lib/bindings.ts`** (prefixed with `// @ts-nocheck`) only on a
  **debug build** *and* only when the process was started **with no CLI arguments** — see the
  `#[cfg(debug_assertions)] if std::env::args_os().nth(1).is_none()` guard. Running
  `cargo run -- serve` in debug will not regenerate bindings; `pnpm tauri dev` will.
- **`bindings.ts` is generated — never hand-edit it.** To change the API, edit the Rust
  command/event + its `#[specta]` types and re-run `pnpm tauri dev`.
- Frontend calls go through `commands.*` and `events.*` imported from `@/lib/bindings`.

Registered commands (the per-OS ones exist in all three `*_utils` modules):
`check_system_requirements`, `check_permissions`, `request_permission`,
`open_permission_settings`, `setup`, `set_session_credentials`, `register_cloud_session`,
`unregister_cloud_session`, `get_cloud_status`, `exit_app`, `get_username`, `set_avatar`,
`get_avatar`, `remove_avatar`, `get_network_adapters`, `watch_for_network_changes`,
`start_hosted_network`, `stop_hosted_network`, `is_hosted_network`, `is_wifi_on`,
`turn_on_wifi`, `install_drivers`, `remove_drivers`, `install_audio_driver`,
`uninstall_audio_driver`, `audio_driver_status`, `set_legacy_volume_key_proxy`,
`set_device_override`, `set_device_audio_output`, `get_device_audio_outputs`,
`remove_device_override`, `set_device_banned`, `set_device_approved`, `set_disconnect_grace`,
`get_disconnect_grace`, `set_turn_config`, `get_turn_config`, `set_server_ports`,
`get_server_ports`, `set_disable_gpu_encode`, `get_disable_gpu_encode`, `get_log_backlog`.

Registered events: `DeviceJoin`, `DeviceAudioOutputs`, `DeviceModify`, `DeviceModifyAction`,
`DeviceRemove`, `DeviceRemoveAction`, `NetworkChange`, `HostedNetworkNoPassword`,
`CloudStatusChange`, `SessionIdChange`, `JoinAttemptsPaused`, `LogLine`.

## Architecture

### App lifecycle (`lib.rs::run`)

`main.rs` is three lines into `screenextend_lib::run()`. `run` then:
1. `cli::fast_path()` (help/version/audio-recover, may exit).
2. Builds the specta `Builder` with the command/event lists; exports `bindings.ts` under the
   debug + no-args guard.
3. Builds the Tauri app with plugins: updater (custom comparator — *any* version difference
   counts as an update), process, autostart (LaunchAgent on macOS), dialog, os,
   clipboard-manager, shell, notification, cli, http, store. macOS gets a real app menu
   (`build_menu`).
4. In `setup`: `mount_events`, `cli::dispatch`, `streamer::input::prime()`, then the legacy CLI
   subcommands, then the default branch — acquire the single-instance lock (offering "Quit
   running instance" / "Show running instance" on conflict), `logbus::attach`, build the main
   window (1200×675, min 1050×650, maximized, title "ScreenExtend"), focus it, and start the
   single-instance control server.

The per-OS `setup` **command** (invoked from the frontend, or by `serve`) is what initializes the
virtual display and populates `AppState`.

### Single instance (`single_instance.rs`)

An exclusive `fs4` lock on `screenextend.lock` in `app_local_data_dir` is the instance token; the
running instance also listens on an ephemeral loopback TCP port whose address is written to
`screenextend.ctrl`, so another process can send `Focus` or `Quit`. `ScreenExtend stop` and
`cli::host_running()` both go through this.

### Logging (`logbus.rs`)

Use **`tprintln!` / `teprintln!`** (exported by `#[macro_use] mod logbus`) rather than
`println!`/`eprintln!` in the Rust core. They push into a 2000-line ring backlog, emit a
`LogLine` event to the UI (the Settings log terminal), and only echo to stdout/stderr when
verbose mode is on (`serve -v`). `get_log_backlog` and `ScreenExtend logs` read the backlog.

### Platform abstraction

`src-tauri/src/` has three `cfg`-gated sibling modules — `windows_utils/`, `macos_utils/`,
`linux_utils/` — each exposing the **same surface**: `AppState`, `setup`, `networking`,
`hosted_network`, `virtual_display`, `streamer`, `compatibility`, `permissions`,
`device_reporter`, `audio`, plus the shared command set. Windows additionally has `driver_ipc/`
and the vendored `windows_capture/`. `lib.rs` glob-imports the active one
(`use windows_utils::*` etc.), so the Tauri command list is identical across OSes and dispatch
happens at compile time. `streamer/platform.rs` is the runtime dispatcher for
capture/encode/tuning/audio-start that calls into the per-OS backend.

`AppState` (identical on Windows and macOS) holds: `virtual_display`, `stop_hosted_network`,
`hosted_network_running`, `network_adapters`, `local_ips`, `streamers`, `session_auth`,
`device_reporter`, `device_overrides`, `sessions`, `disconnect_grace`, `user_turn`,
`banned_devices`, `approved_devices`, `otp_limiter`, `server_ports`, `disable_gpu_encode`,
`cloud`, `cloud_status`, `audio_hub`.

**Current support:** Windows (NVENC/QSV/x264) and macOS (VideoToolbox) are functional. Linux is
scaffolded but non-functional; AMD encode is a stub (`windows_utils/streamer/amd.rs`, notes in
`amd.md`). When adding a capability, add it to all three modules to keep the surface consistent
(Linux/AMD paths may `bail!`).

### Concurrency & locking

Shared mutable state (`AppState` fields, the `streamer` session/override/ban maps, cloud status,
etc.) is guarded with **`std::sync::Mutex` — this is the one blocking mutex type for
first-party code.** Do not introduce `parking_lot::Mutex` outside
`windows_utils/windows_capture/` (that module is a vendored fork of the `windows-capture` crate
and keeps its own `parking_lot` locks — leave it as-is).

**The rule: never hold a `std::sync::MutexGuard` across an `.await`.** Keep critical sections
short — lock, read/copy/clone what you need, drop the guard, *then* await. This is also
compiler-enforced in `Send` contexts (axum handlers, `async` Tauri commands) because
`MutexGuard` is `!Send`, so a violation there fails to build; but it is *not* caught in
non-`Send` closures/callbacks, so keep the discipline everywhere.

Watch the temporary-lifetime footgun: a guard produced in an `if let` / `match` scrutinee
(e.g. `if let Some(x) = map.lock().unwrap().get(k) { … }`) lives until the **end of the
block**, not the end of the line — never put an `.await` inside such a block.

Use an async-aware lock (`tokio::sync::Mutex`/`RwLock`) **only** when a lock genuinely must be
held across an `.await`. There are exactly two today, both intentional:
`DISPLAY_CORRELATION_LOCK` in `streamer/server.rs:94` (serializes virtual-display creation
across its `spawn_blocking`/settle awaits) and `receive_error: RwLock<…>` in
`windows_utils/driver_ipc/client.rs`. Prefer the sync mutex + short critical section for
anything else.

Real-time threads (audio capture, the video capture callbacks) take none of these locks — see
the capture-thread conventions below.

### The `streamer` module (`src-tauri/src/streamer/`)

Cross-platform core, OS-independent:

- `mod.rs` — the `Streamer` type: `run`/`run_with_handle`/`serve` build a multi-thread tokio
  runtime with `on_thread_start(platform::tune_transport_thread)` and call `server::run`;
  `prepare()` sets DPI awareness + process tuning. Also exposes the `probe_*` entry points.
- `config.rs` — `Config` (the whole streaming configuration, including the `Shared*` handles
  threaded in from `AppState`), `ScalePercent` (10–100), `EncoderVendor`
  (`auto|nvidia|intel|software`), `H264Profile`, the defaults (ports 8080/8443, monitor 1,
  `max_fps` 500, Google STUN), and `from_args()`/`print_help()` for the probe harness.
- `server.rs` — axum HTTP(S) server per network adapter; validates session ID + OTP on join,
  creates the virtual display sized to the client, starts the pipeline. Holds the clamp
  constants: `MIN/MAX_REFRESH_RATE` 15/500, `MIN/MAX_DISPLAY_SCALE` 25/200,
  `MAX_EFFECTIVE_SCALE` 500, the input caps (`MAX_SDP_LEN` 64 KiB, `MAX_DEVICE_NAME_CHARS` 64, …),
  `DISPLAY_ATTACH_TIMEOUT` 5 s and `LEAVE_SETTLE` 1.5 s. Routes: `/`, `/health`, `/whep`,
  `/leave`, `/reconfig`, `/ice-config`, `/net-config`, `/audio-outputs`, plus the static assets
  (`/input.js`, `/audio.js`, `/audio-worklet.js`, `/transform-worker.js`, `/nosleep.js`,
  `/styles.css`, `/logo.svg`). `index` serves `static/index.html` with `__SAME_DEVICE_FLAG__`
  substituted and the cross-origin isolation headers (COOP `same-origin` + COEP `require-corp`)
  that let the client use a SharedArrayBuffer ring on a secure context; over plain HTTP they are
  inert and the client falls back to a postMessage ring.
- `session.rs` — per-client session state keyed by client IP (`DeviceSessionState`, live
  display, capture stopper, sequence numbers), device overrides, the OTP limiter, disconnect
  grace (default 10 s, range 0–600), server ports, user TURN config, per-session audio-output
  lists. Settings changes apply live via **epoch bumps** (`bump_reconfig_epoch`,
  `bump_kick_epoch`) rather than tearing down the display; the client polls `/reconfig`.
  **Device trust** (auto-join approval + bans) is keyed on a per-device **token**
  (`mint_device_token`, `Shared{Approved,Banned}Devices`, `is_device_*`), *not* the IP — the
  host mints the token on a successful OTP join and returns it via the `X-Device-Token` header;
  the client stores it and presents it on rejoin. The IP is only a display hint.
  `OtpLimiter` allows `MAX_OTP_ATTEMPTS` 5 per key with a 60 s lockout, plus a global cross-key
  guard (`MAX_GLOBAL_OTP_ATTEMPTS` 20 per 60 s window → 60 s pause) that defeats the cloud
  relay's rotating-`client_id` bypass; the pause surfaces as the `JoinAttemptsPaused` event.
- `webrtc_session.rs` — WebRTC/WHEP negotiation, ICE servers, locality detection
  (`SessionLocality`), SDP candidate summarizing, the audio DataChannel + Opus-track fallback,
  and the loss-based bitrate driver (`getStats` poll every 120 ms). The video track is a raw
  `TrackLocalStaticRTP` (not `TrackLocalStaticSample`): we packetize H.264 ourselves so each
  frame's RTP timestamp carries the shared host clock (`host_ns_to_rtp90k`), which the client
  inverts for A/V sync. NACK/RTX interceptors and loss-based BWE are unaffected.
- `bitrate.rs` — `BitrateController` (smoothing, cut thresholds, `DEFAULT_MIN_BITRATE_BPS`
  1 Mbps) and `estimate_from_loss`.
- `pipeline.rs` — thin re-export layer pointing at the active OS pipeline
  (`windows_utils::streamer::pipeline` / `macos_utils::streamer::pipeline`).
- `platform.rs` — the runtime dispatcher: DPI awareness, process/thread tuning,
  `max_display_dpr()` (2.0 on macOS, 4.0 elsewhere), the probe entry points,
  `start_audio_capture()`, and the `EncoderBackend`/`BackendConfig` trait shared by encoders.
- `tls.rs` — self-signed cert generation at runtime (`rcgen`), cached as
  `self-signed-cert.pem`/`self-signed-key.pem` (both gitignored).
- `cloud.rs` — cross-network relay control channel (WebSocket via `tokio-tungstenite`) to
  `wss://session.screenextend.app/host/v1/connect`, protocol version 1, 45 s heartbeat timeout,
  exponential backoff to 30 s, `CloudState` surfaced to the UI via `CloudStatusChange`.
  Cross-network joins require a configured TURN server.
- `input/` — remote keyboard/mouse/clipboard injection: `protocol.rs` (wire format),
  `scancode.rs`, and a per-OS backend selected with `#[path]` (`windows.rs`, `macos.rs`,
  `linux.rs`, `generic.rs`). `prime()` warms the macOS keyboard path at startup.
- `audio/` — cross-platform system-audio transport glue: `AudioPacket`, `AudioDiagnostics`,
  the shared host timebase (`host_now_ns` / `host_instant_to_ns` / `host_ns_to_rtp90k` — **the
  one clock both audio `capture_ns` and the video RTP timestamp ride for A/V sync**), the
  reference-counted `AudioHub` (one host-wide capture fanned out to N sessions via
  `tokio::sync::broadcast`, started on the first audio-enabled subscriber and stopped when the
  last leaves), and `protocol.rs` (the 13-byte DataChannel header: seq u32, capture ns u64,
  flags u8 + raw Opus). It also hosts the **OS-independent Opus encoder** — the hand-written
  libopus FFI (`opus_sys.rs`) and the encoder wrapper (`encoder.rs`) — shared by every capture
  backend (libopus is cross-platform C; only the bundled library name differs: `libopus.dll` vs
  `libopus.dylib`). `macos_utils/audio/opus_encoder.rs` is only a re-export of these.
- `static/` — the client browser page (see below).
- `test/` — `whep_selftest.rs` (in-process WHEP client that asserts RTP flows) and `bitrate.rs`.

### Video capture & encode — Windows (`windows_utils/streamer/`)

- `capture.rs` / `windows_capture/` — Windows.Graphics.Capture path; `windows_capture/` is a
  **vendored fork** of the `windows-capture` crate (keep its `parking_lot` locks and style).
- `dxgi/` — custom DXGI Desktop Duplication path including cursor compositing.
- `scaler.rs`, `tuning.rs` — GPU scaling and process/thread priority tuning.
- Encoders: `nvidia/` (hand-written NVENC FFI in `nvenc_sys/`), `intel/` (oneVPL / Quick Sync FFI
  in `intel_sys.rs`), `x264/` (libx264 FFI, the CPU fallback), `amd.rs` (stub).
- `pipeline.rs` picks the backend: `EncoderVendor::Auto` tries NVENC → Intel → x264; `Intel`
  tries the same-adapter path, then an own-device "CPU bridge" (`Backend::IntelCpu`), then x264;
  `Software` goes straight to x264. Vendor detection is by DXGI adapter description
  (`select_vendor`). NVENC uses a texture ring to decouple capture from encode.

### Video capture & encode — macOS (`macos_utils/streamer/`)

- `mod.rs` — the `CaptureBackend` trait, `CaptureError`, pixel formats, and `start_capture()`,
  which tries **ScreenCaptureKit** when `screencapturekit_available()` (12.3+) and falls back to
  **CGDisplayStream** (`cgds.rs`), reporting `FallbackFailed` only when both fail.
- `sck.rs` — the SCK backend. **Dyld-safety is load-bearing**: SCK only exists on 12.3+, so a
  link-time reference (`objc2-screen-capture-kit`, an `extern_class!`, any 12.3+ `extern static`)
  would add an undefined dyld symbol absent on the 10.15 floor and break loading of the *whole*
  binary. It therefore uses runtime interop only — `AnyClass::get`, `msg_send!`, and a
  `ClassBuilder`-constructed `SCStreamOutput` delegate.
- `cgds.rs` — the CGDisplayStream fallback (deprecated API, `#![allow(deprecated)]`),
  IOSurface → CVPixelBuffer, ITU-R 709 YCbCr.
- `encoder.rs` — VideoToolbox H.264 compression session; `gpu.rs` — Metal device +
  `CVMetalTextureCache`; `frame.rs` — `Frame`/`FrameSink`; `display.rs`, `config.rs` — display
  enumeration and modes; `mach.rs` — `mach_absolute_time`; `qos.rs`, `tuning.rs`, `power.rs`,
  `activity.rs` — thread QoS, process tuning, power/idle assertions.
- `virtual_display.rs` — the private CoreGraphics `CGVirtualDisplay*` classes via
  `extern_class!` / `extern_methods!` (they exist on 10.15, so link-time binding is fine here).
- `hosted_network.rs` + `hostap.m` — CoreWLAN ad-hoc network (prefers 5 GHz channels).

### System audio — Windows (`windows_utils/audio/`)

Per-device system-audio capture + Opus encode:

- `loopback.rs` — WASAPI **legacy `IAudioClient::Initialize` loopback** path (the IAudioClient3
  low-latency path rejects the loopback flag). Runs on a **dedicated OS thread** with COM MTA +
  MMCSS "Pro Audio", event-driven, re-acquiring on default-device change /
  `AUDCLNT_E_DEVICE_INVALIDATED`.
- `silence.rs` — a **silent render companion** is mandatory: it is the clock source that makes
  the loopback event fire and keeps packets flowing while the host is idle (measured: 0/20 event
  signals without it, 20/20 with it). Started lazily, stopped with the capture.
- `format.rs` — mix-format negotiation; 48 kHz float32 stereo is the zero-copy fast path,
  everything else is `AUTOCONVERTPCM`'d / downmixed (ITU BS.775) / int→float.
- `device.rs` — `IMMNotificationClient`; its callback runs on a COM thread we don't own, so it
  only posts to the capture thread over `crossbeam-channel` (never blocks, never takes the
  capture lock). `guards.rs` holds the RAII COM/handle wrappers.
- Opus encode uses the **shared** wrapper in `streamer/audio/`: `libopus.dll` loaded via
  `libloading` and bundled in `resources/`; `RESTRICTED_LOWDELAY` (CELT-only), 5 ms frames.

**Capture-thread convention:** the audio capture thread is a real-time OS thread (MMCSS), never
a tokio task. It never allocates or locks in the drain/encode path beyond the reused accumulator
plus the per-packet `Bytes` copy. It talks to the async world over `crossbeam-channel` (→ the
`AudioHub` bridge → broadcast), the way the video pipeline does. Linux provides a compiling stub
(`linux_utils/audio.rs`) that returns "unsupported".

### System audio — macOS (`macos_utils/audio/`, tiered)

macOS has **no single audio-capture API across 10.15–current**, so `probe_audio_backend()`
probes at runtime and uses the first available tier (native results are cached in a `OnceLock`;
the legacy tier is recomputed because install state flips at runtime):

`ProcessTap` (14.2+) → `ScreenCaptureKitAudio` (13.0+) → `VirtualDevice` (10.15–12.x, driver
installed and healthy) → `NeedsDriverInstall` → `Unsupported`. The chosen tier is reported to
the UI through `CompatibilityReport.audio_backend`.

- `process_tap.rs` — **Core Audio Process Tap** (`CATapDescription` + aggregate device + IOProc),
  preferred (audio-only, no screen-recording indicator). The aggregate device's tap-list UID must
  match the `CATapDescription` UUID or the tap returns `noErr` but pure silence (the documented
  failure mode). Teardown is RAII in the order stop→destroyIOProc→destroyAggregate→destroyTap so
  a leaked tap can't persist a hidden capture.
- `sck_audio.rs` — **ScreenCaptureKit `capturesAudio`** fallback. SCK has no audio-only mode, so
  it rides a minimal 2×2 / 1 fps dummy video stream and uses the existing Screen Recording
  permission.
- `format.rs` — parses the delivered `AudioStreamBasicDescription` (handles **planar** float,
  which Windows never produces) → interleaved-stereo f32; BS.775 downmix duplicated from the
  Windows sibling (the byte layouts diverge too much to share cleanly).
- `ring.rs` — the lock-free SPSC ring between the capture callback and the encode worker.
- `mod.rs` — the `AudioSource` trait over all backends, the tiering, and the encoder worker.
- `legacy/` — the **10.15–12.x virtual-device tier**. Below 13.0 there is no native system-audio
  API at all, so this ships ScreenExtend's own **AudioServerPlugIn** virtual device (built on
  libASPL/MIT, source in `macos/ScreenExtendAudio/`), sets it as the default output, and reads
  the captured mix back over a POSIX shared-memory ring (`shm_reader.rs`, with a HAL-input
  fallback in `hal.rs`) into the same encoder ring — zero new Opus/client code. `routing.rs`
  saves/switches/restores the default output (plus crash recovery via the `audio-recover` fast
  path); `playthrough.rs` plays the capture back to the real device with a gain stage;
  `volume_proxy.rs` and the driver's Volume/Mute controls keep the macOS volume keys working
  while the virtual device is default (the classic UX regression this tier must repair), with
  `volume_keys.rs` as a `CGEventTap` backstop that intercepts F10/F11/F12 if the OS doesn't
  re-enable them. `installer.rs` runs a signed/notarized `.pkg` behind one admin prompt;
  `probe.rs` decides eligibility/health; `branding.rs` holds the device name/UID strings that
  must stay in sync with the C++ driver's `branding.hpp`. **This tier is never selected on
  13.0+.** **Privacy:** it makes ScreenExtend the system output device while streaming — an
  extra reason it is opt-in per device.

**Dyld-safety (load-bearing, same rule as the video SCK backend):** the 14.2 tap functions are
`dlsym`'d, `CATapDescription`/SCK classes come from `AnyClass::get`, SCK audio uses runtime
`msg_send!` interop (**no `objc2-screen-capture-kit` dependency**), and only floor-present
HAL/CoreMedia functions are linked.

**Capture-callback convention (macOS):** the real-time callback (the Process Tap
`AudioDeviceIOProc` or the SCK sample handler) only converts to interleaved-stereo-f32 in
preallocated scratch and pushes into `ring.rs`; the worker thread drains it and Opus-encodes. The
A/V clock is the shared `streamer::audio::host_now_ns()` (a monotonic `Instant` epoch, itself
backed by `mach_absolute_time` on macOS), *not* raw `mach_now()` ticks, so audio `capture_ns` and
the video RTP stamps ride one epoch.

### The macOS audio driver project (`macos/ScreenExtendAudio/`)

A standalone C++ `AudioServerPlugIn` (not DriverKit — Apple does not grant DriverKit
entitlements for virtual audio devices), built on libASPL. Compiled with `clang++` directly via
`scripts/build.sh` (no CMake/Xcode needed), packaged/signed/notarized via `scripts/package.sh`
and `scripts/sign_and_notarize.sh`, installed to `/Library/Audio/Plug-Ins/HAL/`. The
shared-memory byte layout in `macos/ScreenExtendAudio/src/shm_ring.hpp` is a **hard ABI
contract** with `macos_utils/audio/legacy/shm_reader.rs` — change both together. All branded
strings live in `macos/ScreenExtendAudio/src/branding.hpp`. Its README records current status: it compiles and links against the 10.15
SDK, but has never been signed, installed or loaded, so runtime behaviour is unverified.

### The client web page (`streamer/static/`)

- `index.html` — the join form (session id, device name, 6-digit OTP, fullscreen toggle), the
  video stage (`<video>` / `<canvas>` swap), and four modals: HTTPS upsell,
  limited-browser-support warning, same-device warning (driven by the `__SAME_DEVICE_FLAG__`
  substitution) and remote-control permission. It also drives `/reconfig` polling,
  viewport/orientation/DPR reporting, and kick handling.
- `transform-worker.js` — WebCodecs/insertable-streams video path; recovers each frame's
  host-capture time from its RTP timestamp and reports the display lag.
- `audio.js` — WebCodecs `AudioDecoder` → ring → worklet, plus the NetEQ track fallback;
  commands the worklet a buffer depth so audio plays in step with the picture. Measured offset
  via `SEAudio.getSyncInfo()` (no on-screen HUD).
- `audio-worklet.js` — our jitter buffer; corrects drift only at silence boundaries.
- `input.js` — pointer/keyboard/clipboard capture and the input wire protocol.
- `nosleep.js` — keeps the client screen awake.

### Desktop UI (`src/`)

- Entry: `src/main.tsx` → `src/App.tsx`. Routing uses **`createMemoryRouter`** (in-memory, not
  URL-based) with routes `/` (`pages/bootstrap.tsx`), `/dashboard`, `/devices`, `/settings`.
  `src/layout/` holds the shell + sidebar; note that `src/components/pages/device-details.tsx`
  lives under `components/`, not `pages/`.
- **Global state** lives in `App.tsx` via `GlobalProviderContext`
  (`src/components/global-provider.tsx`) — OTP, sessionId, QR values, devices, avatar, zoom,
  per-IP audio outputs, hosted-network state. `App.tsx` also subscribes to backend events
  (`deviceJoin`, `deviceModify`, `deviceAudioOutputs`, `deviceRemove`, `networkChange`,
  `sessionIdChange`, `hostedNetworkNoPassword`, `joinAttemptsPaused`), mirrors them into that
  state, and raises OS notifications via `@tauri-apps/plugin-notification`.
- **Onboarding walkthrough** uses `nextstepjs` (`src/components/walkthrough.tsx`), which expects
  `next/navigation`; Vite aliases that to `src/lib/next-navigation-stub.ts` — keep the alias if
  you touch the resolver config in `vite.config.ts`.
- `src/lib/`: `bindings.ts` (generated), `zoom.ts` (webview zoom + persistence), `avatar.ts`,
  `utils.ts` (`buildQrValues`, `generatePassword`, `cn`), `next-navigation-stub.ts`.
- **i18n** is a custom lightweight implementation in `src/i18n/` (`useSyncExternalStore` +
  `import.meta.glob` over `locales/*.json`, locale persisted in `localStorage`) — NOT
  react-i18next. Use the `useTranslation()` hook and add keys to `src/i18n/locales/en.json`
  (currently the only locale).
- UI is shadcn/ui + Radix primitives under `src/components/ui/`; path alias `@/` → `src/`;
  fonts via `non.geist`; icons via `lucide-react` / `@radix-ui/react-icons`.

### Config persistence (both sides — wire both when adding a setting)

User config is stored **frontend-side** in `tauri-plugin-store`'s `config.json` via
`src/components/config-provider.tsx` (`getConfig`/`updateConfig`/`flushConfig`/`createConfig`,
`defaultConfig`, the `Config`/`Device`/`KnownDevice` types). The shape:

```
name, theme, devices[], knownDevices[], sessionPassword, publicSessionsEnabled, zoomFactor,
disableGpuEncode, legacyVolumeKeyProxy, walkthroughCompleted,
serverPorts{http,https}, hostedNetworkCredentials{name,password},
turnConfig{urls,username,credential}, dontShowAgain{editDevice,editNetwork,compatibility}
```

`Device` = `{ ip, token, name, scale, orientation, refreshRate, videoScale, videoQuality,
remoteControl, systemAudio, audioOutputDeviceId, audioOutputDeviceLabel, os, screenSize, dpr,
maxDpr }`.

Relevant values are pushed to the Rust `AppState` through commands (`setSessionCredentials`,
`setDeviceOverride`, `setDeviceAudioOutput`, `setDisconnectGrace`, `setTurnConfig`,
`setServerPorts`, `setDisableGpuEncode`, `setDeviceBanned`, `setDeviceApproved`,
`setLegacyVolumeKeyProxy`). The **CLI reads and writes the same store directly** (`cli.rs`'s
`open_store` / `get_nested` / `set_nested`, dotted keys), and `serve` replays it into `AppState`
at startup (`apply_saved_devices`). So a new setting usually needs three touches: the TS config
type + default, the Rust command, and the `serve`/`config` CLI path.

## Versioning & releases

- The version string is duplicated in **four** places and must be kept in sync:
  `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, and
  `src-tauri/tauri.macos.conf.json` (`src-tauri/Cargo.lock` then updates automatically).
  The Rust side reads the version from `CARGO_PKG_VERSION` / Tauri package info at runtime —
  there is no hardcoded version literal in `src-tauri/src/`. Current version: **0.5.3**.
- **CI is currently disabled.** `.github/workflows/ci.yml` was removed in commit `e1d5d28`
  ("temporarily disable ci till further notice"); only `build-release.yml` and `dependabot.yml`
  remain, so **nothing runs `cargo fmt`/`clippy`/`test` or `pnpm lint` automatically — run them
  locally before pushing.** (`README.md` still describes the old always-on CI; that paragraph is
  stale.)
- **Release CI** (`.github/workflows/build-release.yml`) triggers on pushing a tag matching
  `app-v*` (or a manual `workflow_dispatch`) — **not** on push to `main`. It builds Windows x64
  and macOS universal (`--target universal-apple-darwin --config src-tauri/tauri.macos.conf.json`)
  via `tauri-action` and drafts a GitHub Release; a second `finalize-release` job strips the
  version out of the asset filenames, patches `latest.json` to match, publishes the release as
  latest, and force-moves the `latest` git tag onto the released commit.
- Auto-update: `tauri-plugin-updater` reads `latest.json` from the GitHub "latest" release
  (pubkey + endpoint in `tauri.conf.json`; `installMode: passive` on Windows).
- Dependabot covers cargo, npm and GitHub Actions; expect a steady stream of `dependabot/*`
  branches.

## Native resources, drivers & permissions

- **Windows bundle resources** (`tauri.conf.json` → `bundle.resources`): the signed Virtual
  Display Driver (`VirtualDisplayDriver.dll`/`.cat`/`.inf`), its cert (`ScreenExtend.cer`),
  `libx264-164.dll` (software-encode fallback), and `libopus.dll` (system-audio Opus encode,
  loaded via `libloading`). Provenance + SHA-256 in `resources/PROVENANCE.md` / `SHA256SUMS`.
  `binaries/nefconc` is an `externalBin` sidecar driven with `certutil` to install the driver
  (requires Administrator; elevation via the `elevated-command` crate). Installer integration
  lives in `windows/hooks.nsh` (NSIS) and `windows/fragment.wxs` (WiX driver cleanup).
- **macOS** uses the `tauri.macos.conf.json` overlay (passed with `--config`), which bundles
  `resources/libopus.dylib` as its only resource and sets `minimumSystemVersion` 10.15. The
  shipped dylib should be a universal (x86_64 + arm64) build — see `resources/PROVENANCE.md`.
  `src-tauri/Entitlements.plist` grants `com.apple.security.device.audio-input`, disables the
  sandbox, and adds the `com.apple.CG.virtual-display` mach-lookup temporary exception (needed by
  the private `CGVirtualDisplay` API). `src-tauri/Info.plist` supplies
  `NSAudioCaptureUsageDescription` and the local-networking ATS exceptions.
- **Tauri capabilities** (`src-tauri/capabilities/`): `default.json` carries the shell
  allowlist — every external command the app may spawn (`nefconc`, `certutil`, `netsh`,
  `control desk.cpl`, `open x-apple.systempreferences:…`, and the self-invocation used by the
  legacy subcommands) is enumerated there with argument validators. **Adding a new shell
  invocation means adding it here**, or it fails at runtime. `desktop.json` covers
  updater/process/autostart permissions. The webview CSP is in `tauri.conf.json` →
  `app.security.csp`.
- **Permissions** are a real command surface on macOS (`macos_utils/permissions.rs`:
  Accessibility for remote input, Screen Recording for capture, with check / request / open
  settings); the Windows and Linux implementations return an empty list.

## Conventions & gotchas

- Never hand-edit `src/lib/bindings.ts`; regenerate with `pnpm tauri dev` (debug + no CLI args).
- A new Tauri command must be added to **all three** `*_utils` modules and to
  `collect_commands!`.
- Use `tprintln!`/`teprintln!`, not `println!`/`eprintln!`, in the Rust core.
- Keep `std::sync::Mutex` critical sections short and never hold one across `.await`.
- No link-time references to macOS APIs newer than 10.15 — use `dlsym` / `AnyClass::get` /
  `msg_send!`.
- The client page has no build step and no dependencies; it must keep working in older mobile
  browsers (that constraint is why, for instance, flex `gap` was replaced with margins).
- Bump the version in all four files at once.
- New shell command → `capabilities/default.json`.
- New user setting → TS config type/default + Rust command + the `serve`/`config` CLI path.

## Dev-box notes

The usual dev box for this repo is a **macOS 10.15.8 Catalina, Intel** MacBook with Command Line
Tools only (no Xcode, no Homebrew) and git 2.24. Consequences: ScreenCaptureKit (12.3+) and the
Core Audio Process Tap (14.2+) are unavailable there, VideoToolbox low-latency (11.0+) is off,
`probe_audio_backend()` returns `Unsupported`/`NeedsDriverInstall`, and the 10.15 SDK cannot
target arm64 (universal artifacts need CI or a newer Mac). Only `cargo check`/`clippy`/`test`,
`pnpm lint`/`build`, and the CGDisplayStream video path can be exercised locally.

Design notes referenced by older commits and by `macos/ScreenExtendAudio/README.md`
(`AUDIO_NOTES.md`, `AUDIO_NOTES_MACOS.md`, `AUDIO_NOTES_MACOS_LEGACY.md`,
`PRD-macos-legacy-audio.md`) are **not in the tree on any branch** — they survive only in
history, on the `sys-audio-winmac` branch (commit `420004a`). The measurement spike they came
from is still checked in as `src-tauri/examples/audio_spike.rs`.
