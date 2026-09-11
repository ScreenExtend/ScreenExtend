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
  (`index.html`, `at-capacity.html`, `input.js`, `audio.js`, `audio-worker.js`,
  `audio-worklet.js`, `transform-worker.js`, `nosleep.js`, `styles.css`, `logo.svg`). This is
  plain HTML/JS/CSS served by the Rust HTTP server, `include_str!`'d into the binary, NOT part of
  the Vite/React build. Edit it directly; there is no bundler step for it and it must stay
  dependency-free.

## Repo map

```
src/                        desktop control UI (React + Vite)
src-tauri/                  Rust core (crate `screenextend`, lib `screenextend_lib`, bin `ScreenExtend`)
  src/cli.rs                the shipped command-line interface + the `probe` harness
  src/lib.rs                Tauri app: command/event registry, setup, lifecycle
  src/logbus.rs             tprintln!/teprintln! log bus (writer thread + backlog + LogLine event)
  src/single_instance.rs    lockfile + loopback control server
                            (focus/quit/stats/ping/reload/logs/session)
  src/test_support.rs       cfg(test) only: the skip convention, percentiles, measurement output
  src/streamer/             OS-independent streaming core + the client web page
  src/windows_utils/        Windows backend
  src/macos_utils/          macOS backend
  src/linux_utils/          Linux scaffold — does NOT compile today (see below)
  macos/ScreenExtendAudio/  the macOS 10.15–12.x virtual audio device (AudioServerPlugIn, C++)
  resources/                bundled DLLs/driver/cert + vendored AMF & NVENC headers
                            + PROVENANCE.md + SHA256SUMS
  binaries/                 nefconc sidecar (externalBin) + provenance
  capabilities/             Tauri permission capabilities (shell allowlist lives here)
  windows/                  NSIS hook + WiX fragment for the installer
  tools/                    build-time-only C layout generators for the AMF and NVENC ABIs
  examples/audio_spike.rs   throwaway WASAPI/Opus measurement spike
scripts/                    the repo's checkers (see "Checkers" below) + client test harnesses
ci/perf-baseline.json       committed encoder timings the CI perf gate compares against
docs/                       the engineering docs and ledgers (see "Docs" below)
guides/                     standalone verification directives for hardware neither dev box has
.github/workflows/          ci.yml (gates, on every push) + build-release.yml (tag `app-v*`)
```

`linux-vdd/` and `.agents/` are gitignored scratch directories; they may be present in a working
tree and are not part of the build.

## Commands

```sh
pnpm install                     # native build scripts are pre-approved via allowBuilds in pnpm-workspace.yaml

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
(rules-of-hooks as an error), `react-refresh` and `typescript-eslint`, plus a Node-globals
block for `scripts/**/*.mjs`. Type-aware linting is on (`parserOptions.project`), which is what
`@typescript-eslint/no-floating-promises` needs. It passes with 0 errors and ~30 non-blocking
warnings. TypeScript strictness is additionally enforced by `tsc` in `pnpm build`.

### Tests

Rust tests are inline `#[cfg(test)]` modules, several of them collected in per-module `test/`
subdirectories (`streamer/test/`, `windows_utils/streamer/test/`, `windows_utils/audio/test/`,
`macos_utils/audio/test/`, `macos_utils/audio/legacy/test/`). Many are hardware- or OS-gated.

**A test that verified nothing must say so.** That is the one rule `src-tauri/src/test_support.rs`
exists to enforce, after an audit found six hardware-gated tests reporting `ok` having returned at
their first line — on exactly the machine where a green tick is most misleading. So:

- A runtime-gated early return goes through `crate::test_support::skip(what, reason)` (or
  `skip_because` for a plain sentence), which prints `SKIP <what>: <reason>` on **stderr** — not
  `teprintln!`, which nothing under `cargo test` makes visible.
- `SE_REQUIRE_HW=1` turns every such skip into a panic, so a machine that is supposed to have the
  capability can be made strict without a second suite.
- A test that cannot run unattended is `#[ignore = "<reason>"]`, so the harness prints the reason
  rather than a bare `ignored`. **Do not exclude a test by name in CI** (`--skip`): that hides it
  from the report, which is the failure mode the convention exists to prevent.
- `test_support::show_measurements()` routes the log bus to stderr for tests that report timings
  with `tprintln!`, and `percentiles()` prints them in the shape `scripts/perf-gate.mjs` parses.

The client page has its own suite, `pnpm test:client` (`scripts/test-client.mjs`), which runs
seven dependency-free Node harnesses over the shipped `.js` files with `vm` and minimal browser
stubs: the input wire format and telemetry frame, golden input vectors the Rust parser reads back,
the Opus decode worker, the jitter buffer and the planar ring ABI, the WebCodecs video path and
the RTP→host-clock inversion, ICE gathering, and the `/reconfig` epochs. No runner and no
devDependency, and `src-tauri/src/streamer/static/` stays byte-identical.

The `#[ignore]`d latency harnesses are documented in `docs/manual-tests/latency-harnesses.md` —
`cargo test` never runs them, so without that note the numbers are never produced. The perf gate
(`ci/perf-baseline.json` + `scripts/perf-gate.mjs`) keys baselines by machine class, takes the
**lowest** of several runs per metric, and lets a key carry its own `tolerance` overriding the
20 % default; the dev box's key is at 10 % because an idle machine is a tighter instrument than a
shared runner. An unseeded key is not a failure — the gate reports it, uploads the observed
numbers and passes, which is how a new runner tells you what to paste back.

### Checkers

Five static checks, all runnable locally in seconds, all run by CI:

- `node scripts/check-lockfile.mjs` (`pnpm test:lockfile`) — asserts that every installed package
  declaring `optionalDependencies` has that list recorded in `pnpm-lock.yaml`. A package whose
  payload is a native binary (`@tauri-apps/cli`, `esbuild`, `rollup`) ships one npm package per
  platform and reaches it that way, so a lockfile missing the list installs a JS shim with no
  binary behind it — and only the machine that wrote the lockfile keeps working, off its warm
  store. Runs after `pnpm install --frozen-lockfile`, which is what it reads the manifests from.
- `node scripts/check-bindings.mjs` — cross-checks the `collect_commands!` / `collect_events!`
  lists in `lib.rs` against what `src/lib/bindings.ts` exports. **Needs no build**, which is the
  point: the generated file carries `// @ts-nocheck`, so a stale binary can revert the whole TS
  surface with nothing else failing.
- `node scripts/check-versions.mjs` — asserts the app version is identical in all four files.
- `node scripts/check-legacy-webkit.mjs` — re-derives the post-floor API list from `dist/` (the
  desktop UI, Safari 13.1 floor) *and* from `src-tauri/src/streamer/static/` (the client page,
  under a stricter contract), and fails on anything not covered by a shim or an argument.
- `bash scripts/check-macos-dyld.sh <binary> <sdk> [--floor 10.15]` — fails on any link-time
  symbol newer than the floor. `scripts/macos-dyld-allowlist.txt` holds the reviewed exceptions.

`scripts/perf-gate.mjs` and the `scripts/test-client-*.mjs` harnesses are covered under "Tests".

### Docs

`docs/` is committed and is the engineering record:

- `INVENTORY.md` — the component inventory, and a "Corrections to CLAUDE.md" section. **Where
  that file and this one disagree, check the code.**
- `CAPABILITY-TIERS.md` — every tiered subsystem × tier, with its probe and verification status.
- `TELEMETRY.md` — the host/client measurement contract; `INPUT-PROTOCOL.md` — the input,
  `PING`/`PONG` and `STATS` wire formats; `DATAFLOW.md`, `LATENCY-MAP.md`, `BASELINE.md`.
- `OPTIMIZATIONS.md` — every change attempted, with before/after and a `VERIFIED` /
  `COMPILE-CHECKED` / `UNVERIFIED` status, including the ones that were rejected.
- `RISK-REGISTER.md`, `dev-workflow.md` (the two-machine protocol), `manual-tests/` (scripts for
  what the development machines cannot execute), `airplay-fallback-design.md`.
- `discovery/`, `research/`, `reviews/` — the audit reports the current work is built on.
- `recovered/` — design notes that were deleted from the tree and restored, each with a header
  recording the commit it came from. Read `docs/recovered/README.md` before citing one.

### `guides/`

Five **standalone directives**, each written for one fresh session on hardware or an OS that
neither development machine can exercise — a Windows 11 26200 / RTX 3080 Ti hybrid laptop and a
macOS 10.15.8 Intel MacBook. A guide's reader has no access to the other guides or to either dev
box; everything outside its stated territory is already audited and must not be touched. Pick the
one whose row your machine satisfies, and run two in separate sessions rather than merging them —
each owns a disjoint set of ledger rows.

| Guide | Territory | Floor |
|---|---|---|
| `modern-mac.md` | everything macOS above the 10.15 dev floor: SCK video and audio, the Process Tap, VideoToolbox low-latency and LTR, Sequoia re-consent, `CGVirtualDisplay` on a modern OS, the hosted network end to end; Part B adds Developer ID signing, notarization, TCC identity, translocation and the universal binary | macOS 14.2, console session |
| `intel-mac.md` | the band nothing else reaches: SCK's first release and the first-frame commit gate, the audio driver as the *selected* tier with its `.pkg` flow, and everything needing **two GPUs** | macOS 12.3–12.7.6, Intel, dual-GPU |
| `windows-amd.md` | the entire AMF backend — audit, fix, then optimise to NVENC parity; `amd_layout` against a live runtime, the usage tiers, NV12 vs BGRA, the CPU bridge, LTR repair, the async ring, the SPS VUI rewrite | Windows 10 2004, a VCN/VCE AVC part |
| `windows-legacy.md` | everything below Windows 11 24H2: the WGC property tiering, DXGI duplication end to end, hybrid-adapter selection, the DPR ladder on an older WDDM, `netsh` hosted network, the WASAPI engine period and the audio conversion tiers | Windows 10 19041 – build 22631, hybrid graphics |
| `clients-network-and-glass-to-glass.md` | everything the host cannot prove alone: all fourteen client-browser tiers, the whole client page, trickle ICE and profile negotiation against real engines, DSCP/WMM on a real AP, TURN and the cloud relay, repair under induced loss, and the camera glass-to-glass measurement | a client matrix, two APs, a 240 fps camera |

`guides/MASTER_GUIDE.md` is the index and `guides/PLAN.md` the 106-item inventory they are
written from. Three things are covered by no guide and say so there: the pre-10.14.4 AirPlay
tier (a scope decision, not a hardware problem — it can only be selected below the app's own
`minimumSystemVersion`), the first run of the GitHub Actions workflows (needs a push), and Linux.

## The shipped CLI (`src-tauri/src/cli.rs`)

The single binary is both the GUI app and a full CLI; with no arguments it opens the desktop app.
There are two entry points, and both matter:

1. **`cli::fast_path()`** — called at the very top of `lib.rs::run`, *before* the Tauri builder.
   Handles `--help`/`-h`/`help`, `--version`/`-V`/`version`, the hidden macOS `audio-recover`
   watchdog (restores the saved default audio output after a crash), and the `probe` harness.
   These must work without a Tauri app, hence the separate path.
2. **`cli::dispatch(app.handle())`** — called from `setup`, routes everything else through
   `desktop::route` (compiled only on Windows/macOS). Every branch is `-> !` and calls `exit()`;
   returning `Outcome::LaunchGui` is what falls through to the window.

Subcommands are declared in `tauri.conf.json` under `plugins.cli` (the `tauri-plugin-cli`
matches drive the router) and implemented in `cli.rs`:

```
serve [--http-port N] [--https-port N] [--session-id ID] [--otp CODE] [--otp-file PATH]
      [--no-cloud] [--no-qr] [--encoder auto|nvidia|intel|amd|software]
      [--software-encode] [-v|--verbose]
status [--session-id ID] [--json]
qr [-t lan|cloud|all] [--session-id ID] [--no-render] [--json]
session new [--no-render] [--json]
devices list | set <ip> [--scale --orientation --refresh-rate --video-scale
                         --video-quality --control on|off --audio on|off
                         --pointer-scope display|all --auto-framerate on|off] | reset <ip>
network start <ssid> <password> | stop | status | wifi-on | wifi-qr
config list | get <key> | set <key> <value> | path      # dotted keys, e.g. serverPorts.http
stun show | set <urls> | clear
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
- `serve` is the headless host: it takes the single-instance lock, **starts the single-instance
  control server** (so `stop`, `status`, `logs` and `config set` can reach it), runs
  `platform::setup`, applies saved config (ports, STUN, TURN, disconnect grace, device trust,
  per-device overrides), mints or takes a session id + OTP, optionally registers the cloud
  session, prints join URLs/QRs, and blocks on Ctrl+C (`ctrlc`), removing all virtual displays on
  the way out.
- **Secrets do not go on argv**, which every process on the machine can read: the OTP and the
  session id are read from `SCREENEXTEND_OTP` / `SCREENEXTEND_SESSION_ID`, from `--otp-file`, or
  from stdin with `-`. `--otp` still works and still says so.
- **A session id lives in the host's memory, so the CLI asks the host for it.** It is minted at
  bootstrap (the desktop app) or at startup (`serve`) and never written to `config.json` — it is
  a per-run credential, not a setting. `qr` and `status` read the live id over the control
  server's `SESSION` verb and only mint one when nothing answered, saying so in the output
  (`sessionIdSource` is `flag` | `host` | `generated`, and `hostRunning` and
  `hostReportedSession` are separate questions: a host still starting, or one predating the verb,
  is running but reports nothing). `session new` mints a pair, **applies it to the running host**
  with `SESSION <id> <otp>`, waits up to 8 s for the relay to leave `connecting`, reads the
  credentials back — the desktop UI mirrors both values and can push its own copy back — and only
  then prints URLs. With no host up it prints the `SCREENEXTEND_SESSION_ID` /
  `SCREENEXTEND_OTP` pair for `serve` to consume; with a host that cannot take them it fails and
  changes nothing rather than printing a credential nobody is serving.
- Most read commands accept `--json`. `doctor` uses its exit code as the verdict (0 fine,
  1 unsupported OS, 3 a required permission is missing), which is what CI asserts against.
- `network start/stop/status` are Windows-only; on macOS they print an explanatory error because
  the hosted network is owned by the running host process.
- `stop`, `status`, `qr`, `session new`, `logs` and the config replay all talk to the running
  instance over `single_instance`'s loopback control server (`STATS` / `PING` / `RELOAD` /
  `LOGS` / `FOCUS` / `QUIT` / `SESSION`, each authenticated with the token in
  `screenextend.ctrl`).
- On Windows the binary is GUI-subsystem in release (`windows_subsystem = "windows"` in
  `main.rs`), so `attach_console()` (`AttachConsole(ATTACH_PARENT_PROCESS)`) is what makes CLI
  output visible in the parent shell.
- `installdrivers`, `removedrivers` and `hostednetwork <ssid> <password>` are **legacy**
  subcommands still handled inline in `lib.rs::run`'s `setup` (they shell out to
  `certutil`/`nefconc`/`netsh` and exit). `route` deliberately returns `LaunchGui` for them so
  `lib.rs` keeps ownership. Prefer `drivers install|remove` and `network start` for new work.

### Developer probe harness (`ScreenExtend probe`, not shipped-facing)

`cli.rs`'s `probe` module plus `streamer/config.rs::Config::from_args` and `Streamer::probe_*`
exercise capture→encode→WebRTC in isolation. It is intercepted in `fast_path` rather than
declared in `tauri.conf.json`, because its flags are the streamer's own and `tauri-plugin-cli`
would reject them; and it runs **inside the Tauri app**, because `--live` and `--whep-selftest`
need a real virtual display and therefore `platform::setup` and the `AppState` it builds.

`ScreenExtend probe --help` is authoritative. Modes: `--capture`, `--dxgi`, `--encode`, `--live`,
`--bitrate`, `--audio` (with `--process-loopback`), `--whep-selftest` (needs `--features probe`,
which is off by default because the self-test carries a TLS verifier that accepts any
certificate). Tuning flags include `--encoder`, `--capture-backend wgc|dxgi|auto`, `--qp`,
`--h264-profile`, `--intra-refresh`, `--scale`, `--synthetic-pattern`, `--workload`, `--counter`
and the `--stun`/`--turn-*` family. Several of these exist because a tier is otherwise only
reachable by the tier above it failing, and so cannot be measured.

## The Rust ↔ TypeScript bridge (important)

Commands and events are defined in Rust and consumed type-safely in TS via
[`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta):

- All commands/events are registered in `collect_commands!` / `collect_events!` in
  `src-tauri/src/lib.rs::run`.
- The bridge is exported to **`src/lib/bindings.ts`** (prefixed with `// @ts-nocheck`) only on a
  **debug build** *and* only when the process was started **with no CLI arguments** — see the
  `#[cfg(debug_assertions)] if std::env::args_os().nth(1).is_none()` guard. Running
  `cargo run -- serve` in debug will not regenerate bindings; `pnpm tauri dev` will. The path is
  resolved from `CARGO_MANIFEST_DIR`, so the working directory does not decide which file is
  written.
- "No arguments" is only a proxy for "this is the current build", so a stale debug binary can
  silently revert the whole surface with `tsc` none the wiser. **Diff `bindings.ts` before
  committing**, and run `node scripts/check-bindings.mjs`, which catches it without a build.
- **`bindings.ts` is generated — never hand-edit it.** To change the API, edit the Rust
  command/event + its `#[specta]` types and re-run `pnpm tauri dev`.
- Frontend calls go through `commands.*` and `events.*` imported from `@/lib/bindings`.

Registered commands (the per-OS ones exist in all three `*_utils` modules):
`check_system_requirements`, `check_permissions`, `request_permission`,
`open_permission_settings`, `relaunch_app`, `setup`, `set_session_credentials`,
`register_cloud_session`, `unregister_cloud_session`, `get_cloud_status`, `exit_app`,
`set_window_zoom`, `get_username`, `set_avatar`, `get_avatar`, `remove_avatar`,
`get_network_adapters`, `watch_for_network_changes`, `start_hosted_network`,
`stop_hosted_network`, `is_hosted_network`, `get_hosted_network_status`, `is_wifi_on`,
`turn_on_wifi`, `install_drivers`,
`remove_drivers`, `quit_driver_holder`, `install_audio_driver`, `uninstall_audio_driver`, `audio_driver_status`,
`set_legacy_volume_key_proxy`, `set_device_override`, `set_device_pointer_scope`,
`set_device_auto_frame_rate`, `refresh_device_stream`, `disconnect_session`,
`set_device_audio_output`, `get_device_audio_outputs`, `remove_device_override`,
`set_device_banned`, `set_device_approved`, `set_disconnect_grace`, `get_disconnect_grace`,
`get_display_capacity`, `set_turn_config`, `get_turn_config`, `set_server_ports`,
`get_server_ports`, `set_disable_gpu_encode`, `get_disable_gpu_encode`, `get_log_backlog`,
`set_log_streaming`, `log_frontend_error`.

Registered events: `DeviceJoin`, `DeviceAudioOutputs`, `DeviceModify`, `DeviceRemove`,
`NetworkChange`, `HostedNetworkNoPassword`, `ScreenRecordingRevoked`, `CloudStatusChange`,
`SessionIdChange`, `JoinAttemptsPaused`, `LogLine`.

Shared types worth knowing: `CompatibilityReport` (os name/version/floor/supported, the
`unsupported_apis` list, and the six probed backend names — audio, display, capture, encoder,
hosted network, input) and `DisplayCapacity` (`{ max, in_use, full, backend }`, which gates the
whole dashboard render).

## Architecture

### App lifecycle (`lib.rs::run`)

`main.rs` is three lines into `screenextend_lib::run()`. `run` then:
1. `cli::fast_path()` (help/version/audio-recover/probe, may exit).
2. Builds the specta `Builder` with the command/event lists; exports `bindings.ts` under the
   debug + no-args guard.
3. Builds the Tauri app with plugins: updater (custom comparator — *any* version difference
   counts as an update), process, autostart (LaunchAgent on macOS), dialog, os,
   clipboard-manager, shell, notification, cli, store. macOS gets a real app menu
   (`build_menu`), and `macos_utils::objc_exception::install_logger()` runs before the builder so
   an uncaught Objective-C exception is logged rather than an opaque abort.
4. In `setup`: `mount_events`, `cli::dispatch`, `streamer::input::prime()`,
   `permissions::prime()`, then the legacy CLI subcommands, then the default branch — acquire the
   single-instance lock (offering "Quit running instance" / "Show running instance" on conflict),
   `logbus::attach`, build the main window (1200×675, min 1050×650, maximized, title
   "ScreenExtend"), focus it, and start the single-instance control server.

The per-OS `setup` **command** (invoked from the frontend, or by `serve`) is what initializes the
virtual display and populates `AppState`.

### Single instance (`single_instance.rs`)

An exclusive `fs4` lock on `screenextend.lock` in `app_local_data_dir` is the instance token; the
running instance also listens on an ephemeral loopback TCP port whose address **and a random
token** are written to `screenextend.ctrl`, so another process can send `FOCUS`, `QUIT`, `STATS`
(one line of telemetry JSON), `PING` (liveness without taking the lock, which is racy), `RELOAD`
(re-read `config.json` into the running `AppState`), `LOGS` (the backlog) or `SESSION`. The token
is compared in constant time, the same as the join OTP.

`SESSION` goes both ways. Bare, it answers one line of JSON — the live session id, the OTP,
whether the host holds a relay client, and that relay's state — because a session id exists only
in the host's memory and every reporting command runs in its own process. `SESSION <id> <otp>`
applies new credentials to the running `AppState` (`set_session_credentials`), re-registers the
cloud session under the new id **only when the host already holds a relay client** (registering
one here would publish a session nobody asked to publish), and emits `SessionIdChange` — which
carries the OTP too, because the desktop UI mirrors both values and pushes its own copy back
whenever either changes. Credentials are validated (non-empty, ≤64 chars, `[A-Za-z0-9_-]`) before
they can replace ones the host can serve. The two `AppState` halves live in `cli.rs`
(`live_session_info` / `apply_session_credentials`, with Linux stubs) so `single_instance.rs`
stays OS-independent, the same way `RELOAD` is a callback.

**The OTP is on that wire deliberately.** The socket is loopback-only and every request carries
the 128-bit token from a user-only file, and whoever can read that file can already `QUIT` the
host or *set* its credentials — so the OTP is inside the trust boundary, not outside it. It is
there so a setter can read back what it applied; no shipped command prints it (`session new`
prints the OTP it minted itself, and `status` deliberately does not).

### Logging (`logbus.rs`)

Use **`tprintln!` / `teprintln!`** (exported by `#[macro_use] mod logbus`) rather than
`println!`/`eprintln!` in the Rust core. A call site pays for exactly one bounded-channel send of
the already-formatted `String`: no lock a capture, encode or audio thread can contend on, and no
JSON serialization, because Tauri serializes an event payload *before* it checks whether anyone
is listening. A dedicated `logbus` writer thread owns the 2000-line ring backlog, coalesces up to
64 lines per 50 ms into one `LogLine` event, and emits only while the Settings log panel says it
is listening (`set_log_streaming`). Lines are echoed to stdout/stderr only in verbose mode
(`serve -v`). Overflow drops the line and reports the drop in-band. `get_log_backlog` and
`ScreenExtend logs` read the backlog.

### Platform abstraction

`src-tauri/src/` has three `cfg`-gated sibling modules — `windows_utils/`, `macos_utils/`,
`linux_utils/` — each meant to expose the **same surface**: `AppState`, `setup`, `networking`,
`hosted_network`, `virtual_display`, `streamer`, `compatibility`, `permissions`,
`device_reporter`, `audio`, plus the shared command set. Windows additionally has `driver_ipc/`
and the vendored `windows_capture/`; macOS additionally has `airplay/` and `objc_exception.rs`.
`lib.rs` glob-imports the active one (`use windows_utils::*` etc.), so the Tauri command list is
identical across OSes and dispatch happens at compile time. `streamer/platform.rs` is the runtime
dispatcher for capture/encode/tuning/audio-start that calls into the per-OS backend.

`AppState` (identical on Windows and macOS) holds: `virtual_display`, `stop_hosted_network`,
`hosted_network_running`, `network_adapters`, `local_ips`, `streamers`, `session_auth`,
`device_reporter`, `device_overrides`, `sessions`, `disconnect_grace`, `user_turn`, `stun_urls`,
`banned_devices`, `approved_devices`, `otp_limiter`, `server_ports`, `disable_gpu_encode`,
`encoder_vendor`, `cloud`, `cloud_status`, `audio_hub`.

**Current support:** Windows (NVENC / Quick Sync / AMF / x264) and macOS (VideoToolbox) are
functional.

**Linux does not currently compile**, and the repo map above says so on purpose. `linux_utils`
implements roughly two thirds of the commands `collect_commands!` names, `linux_utils/networking.rs`
is a single blank line, `AppState` is `{}`, and `streamer/pipeline.rs` has `cfg` arms for windows
and macos only with no `not(any(...))` fallback while `lib.rs` declares `mod streamer;`
unconditionally. Fixing it is real work, not a flag. When adding a capability, still add it to
all three modules to keep the surface consistent (Linux paths may `bail!`).

The **AMD AMF backend** (`windows_utils/streamer/amd/` + `amd_sys/`, design in
`windows_utils/streamer/amd.md`) is complete and wired into the `Auto` chain, but **`UNVERIFIED`**
— no machine on this project has an AMD GPU, so its FFI is asserted by `test/amd_layout.rs`
(numbers measured by a C compiler reading the vendored headers, not counted by hand) and nothing
in it has ever encoded a frame. Eight defects found by reading the shipped code (D-01…D-08) have
since been fixed and are **`COMPILE-CHECKED`**, not measured: probe by PCI id rather than by
description string, LTR on the ring, the `AsyncEncodeRing` AMF arm, and the `amd/sps.rs` VUI
rewrite. `guides/windows-amd.md` is the standalone directive that verifies the whole backend on
real hardware; `docs/manual-tests/amd-amf.md` is the shorter script inside it.

### Concurrency & locking

Shared mutable state (`AppState` fields, the `streamer` session/override/ban maps, cloud status,
etc.) is guarded with **`std::sync::Mutex` — this is the one blocking mutex type for
first-party code.** Do not introduce `parking_lot::Mutex` outside
`windows_utils/windows_capture/` (that module is a vendored fork of the `windows-capture` crate
and keeps its own `parking_lot` locks; it is a fork under active maintenance, not a frozen blob,
but keep its style).

**Take a streamer-core lock with `lock_recover()`, not `.unwrap()`.** `streamer/lock.rs` adds
`LockExt::lock_recover`, which recovers a poisoned guard, counts the recovery and logs the first
one. The `Arc<Mutex<…>>` values here are shared across every adapter's server and the cloud
thread, so one panic while any guard is live would otherwise poison the lock for the whole host:
axum catches the panic per task, and the visible symptom is every request returning 500 with
nothing in the log to explain it. The guarded data is maps, counters and epochs — a torn update
to one of those is far cheaper than a host that must be restarted.

**The rule: never hold a `std::sync::MutexGuard` across an `.await`.** Keep critical sections
short — lock, read/copy/clone what you need, drop the guard, *then* await. This is also
compiler-enforced in `Send` contexts (axum handlers, `async` Tauri commands) because
`MutexGuard` is `!Send`, so a violation there fails to build; but it is *not* caught in
non-`Send` closures/callbacks, so keep the discipline everywhere.

Watch the temporary-lifetime footgun: a guard produced in an `if let` / `match` scrutinee
(e.g. `if let Some(x) = map.lock_recover().get(k) { … }`) lives until the **end of the
block**, not the end of the line — never put an `.await` inside such a block.

Use an async-aware lock (`tokio::sync::Mutex`/`RwLock`) **only** when a lock genuinely must be
held across an `.await`. There are exactly two today, both intentional:
`DISPLAY_CORRELATION_LOCK` in `streamer/server.rs` (it serializes virtual-display creation, and
it is held **process-wide across a ≤5 s attach poll** — plus, on macOS, up to 3 s of mode-activate
polling — so a second join can wait behind the first; the before/after device-name diff is what
needs the exclusivity) and `receive_error: RwLock<…>` in `windows_utils/driver_ipc/client.rs`.
Prefer the sync mutex + short critical section for anything else.

Real-time threads (audio capture, the video capture callbacks) take none of these locks — see
the capture-thread conventions below.

### The `streamer` module (`src-tauri/src/streamer/`)

Cross-platform core, OS-independent:

- `mod.rs` — the `Streamer` type and **one shared tokio runtime for the whole host**
  (`transport_runtime()`): multi-thread, at most `MAX_TRANSPORT_WORKERS` = 4 workers,
  `on_thread_start(platform::tune_transport_thread)`, built once and kept for the process
  lifetime. Every network adapter's server runs on it. Before this each adapter built its own
  runtime sized to `available_parallelism()`, so three adapters on a 16-thread host meant 48
  priority-boosted, mostly idle workers competing with the encoder; and an adapter change (a
  Wi-Fi roam) tore a whole runtime down and rebuilt it. `prepare()` sets DPI awareness + process
  tuning. Also exposes the `probe_*` entry points.
- `config.rs` — `Config` (the whole streaming configuration, including the `Shared*` handles
  threaded in from `AppState`), `ScalePercent` (10–100), `EncoderVendor`
  (`auto|nvidia|intel|amd|software`), `H264Profile`, the defaults (ports 8080/8443, monitor 1,
  `max_fps` 500, Google STUN), and `from_args()`/`print_help()` for the probe harness.
- `server.rs` — axum HTTP(S) server per network adapter; validates session ID + OTP on join,
  creates the virtual display sized to the client, starts the pipeline. Holds the clamp
  constants: `MIN/MAX_REFRESH_RATE` 15/500, `MIN/MAX_DISPLAY_SCALE` 25/200,
  `MAX_EFFECTIVE_SCALE` 500, the input caps (`MAX_SDP_LEN` 64 KiB, `MAX_DEVICE_NAME_CHARS` 64, …),
  `DISPLAY_ATTACH_TIMEOUT` 5 s and `LEAVE_SETTLE` 1.5 s. Routes: `/`, `/health`, `POST /whep`,
  `PATCH /whep/{id}` (trickle ICE, `application/trickle-ice-sdpfrag`), `DELETE /whep/{id}`,
  `/leave`, `/reconfig`, `/ice-config`, `/net-config`, `/audio-outputs`, plus the static assets.
  Every session-scoped route (`/reconfig`, `/leave`, `/audio-outputs`) names its session with
  `?join=<id>`; `PATCH`/`DELETE /whep/{id}` take it in the path. None of them falls back to the
  peer's address — see `session.rs` below. `remove_display_async` takes `DISPLAY_CORRELATION_LOCK`
  for the removal too, because Windows reuses a display device name the moment one is freed and a
  removal overlapping a create's before/after diff makes the create find nothing new.
  `index` serves `static/index.html` with `__SAME_DEVICE_FLAG__` substituted and the
  cross-origin isolation headers (COOP `same-origin` + COEP `require-corp`) that make
  `crossOriginIsolated` true on a secure context, which is what lets the audio ring be a real
  `SharedArrayBuffer`; over plain HTTP they are inert and the client falls back. `at_capacity_page`
  serves `static/at-capacity.html` when the display backend is at its cap — which, per
  `session::concurrent_display_cap`, is only ever the macOS AirPlay tier.
  `server.rs` also owns the DPR ladder: `dpr_mode_ladder` builds one resolution per ratio in
  `platform::display_dpr_ladder()`, sorted and deduped, and hands it to
  `create_display_with_modes`; **each backend adds the transposes** (Windows in
  `virtual_display.rs`, macOS in `mode_list`) so an orientation flip is a pure mode switch. The
  effective ratio is snapped down by `platform::snap_display_dpr`.
- `session.rs` — **two identities, and neither is an address.**

  A **session is one join** — one browser tab. The host mints a 128-bit `join_id`
  (`mint_join_id`) at `POST /whep` and hands it back as the WHEP resource, in `X-Join-Id` and in
  an `se_join` cookie. `SharedSessions` is keyed by it, and so is everything a session owns:
  `DeviceSessionState`'s live display, capture stopper, sequence numbers, both epochs, leave
  signal, control channel, disconnect grace and the telemetry registry. Two tabs of one browser —
  same machine, same address, same device — are two sessions with two displays and neither can
  reach the other's. Simultaneous displays are uncapped; `session::concurrent_display_cap` is
  still the only refusal and still only the macOS AirPlay tier. `remove_session` is called once,
  on the final teardown, because keyed by join the map would otherwise gain an entry for every
  tab ever opened.

  A **device is its token** (`mint_device_token`, minted on the first successful OTP join,
  returned via `X-Device-Token`, stored by the client in `localStorage` and presented on every
  join after that). Per-device settings (`SharedDeviceOverrides`), the audio sink
  (`SharedAudioSinks`), auto-join approval and bans (`Shared{Approved,Banned}Devices`,
  `is_device_banned` — **token only**) and the `config.json` `devices[]`/`knownDevices[]` rows
  are all keyed by it. Two tabs share a token and therefore share every setting.

  **The IP is a label.** It is stored on the session (`note_session_ip`), refreshed from whichever
  request arrives last, and shown in the UI and the log. Nothing is ever addressed by it: an
  address moves under a live session (a Wi-Fi roam, a DHCP renewal, a NAT rebind) and is recycled
  between devices, so resolving by it is how a device that leaves hands its session, its settings
  and its ban to whatever picks up the lease next. `token_for_ip` exists only as a CLI
  convenience and refuses to answer when two devices share an address. The one thing still keyed
  by address is the OTP rate-limit bucket, which is a bucket and not an identity.

  `/reconfig`, `/leave` and `/audio-outputs` carry `?join=`; the `se_join` cookie is a bridge for
  a page cached from before the id existed, and because a cookie is per-origin the explicit
  parameter always wins. `PATCH`/`DELETE /whep/{id}` take the join id in the path and require the
  device token as a second factor. There is deliberately **no address fallback** on any request
  path.

  **What a settings edit costs the stream is a question with five answers, and
  `DeviceOverride::reconfig_action` is where it is decided** — one comparison of the saved
  override against the submitted one, with the field → action table beside it as a test. The
  desktop UI writes every field on every save, so "a save happened" is never the question.

  | class | fields | what happens |
  |---|---|---|
  | (a) cosmetic | `name`, labels, the device token, `knownDevices` | nothing reaches the host at all |
  | (b) input-only | `remoteControl`, `pointerScope` | written into the running injector through `apply_live_input_for_device` → `input::LiveInput` (two relaxed atomics read per event). No epoch |
  | (c) audio | `systemAudio`, the output device | `audio_enabled` is polled per packet by `audio_enabled_now`; the sink rides in *every* `/reconfig` body and the client applies it on the next poll. No epoch |
  | (d)+(e) stream | `videoQuality`, `videoScale`, `autoFrameRate`, `orientation`, `refreshRate`, `dpr` | `bump_reconfig_epoch_for_device`, which moves every join that device holds |
  | (e) scale | `scale` alone | `apply_scale_live_for_device`: the extended desktop's DPI changes under a stream whose geometry is already right. No epoch |

  An epoch bump is a **renegotiation in place, not a rejoin.** The client answers it by offering
  again and presenting the `joinId` it already holds; `continuing_join` honours that id for a
  session that is still open and still belongs to the same device token, so `start_session` finds
  an `existing` live display, changes its mode (or leaves it alone for an encoder-only edit) and
  keeps the join. Same join id, same display id, no `DeviceRemove`, no second display, and the
  windows the user arranged stay where they are. The outgoing session's teardown task finds its
  `session_seq` superseded and stands down. **Never make an epoch bump a leave-and-rejoin**, and
  never widen a class without the table above and its test.

  `OtpLimiter` allows `MAX_OTP_ATTEMPTS` 5 per key with a 60 s lockout, plus a global cross-key
  guard (`MAX_GLOBAL_OTP_ATTEMPTS` 20 per 60 s window → 60 s pause) that defeats the cloud
  relay's rotating-`client_id` bypass; the pause surfaces as the `JoinAttemptsPaused` event.
- `webrtc_session.rs` — WebRTC/WHEP negotiation, ICE servers, locality detection
  (`SessionLocality`), SDP candidate summarizing, the audio DataChannel + Opus-track fallback,
  and the bitrate driver. The video track is a raw `TrackLocalStaticRTP` (not
  `TrackLocalStaticSample`): we packetize H.264 ourselves so each frame's RTP timestamp carries
  the shared host clock (`host_ns_to_rtp90k`), which the client inverts for A/V sync.
- `packetizer.rs` — our RFC 6184 packetizer, replacing `rtp::codecs::h264::H264Payloader`, which
  could silently drop SPS/PPS when the STAP-A aggregate did not fit the MTU and then clear the
  stashed copies — with an infinite GOP that is a permanently black client with every host-side
  signal reporting success.
- `negotiate.rs` — reads the offer's `profile-level-id` and decides the H.264 profile and the
  level the answer advertises, instead of sending Baseline to everything (Baseline costs CAVLC
  and no 8×8 transform: 5–15 % of the bitrate at equal quality, and bytes are queueing delay).
- `congestion.rs` — send-side congestion control from **transport-wide feedback**. The writer
  stamps the transport-wide sequence number itself, so the host knows which packet got which
  sequence and can read a delay signal at the browser's TWCC cadence instead of driving the
  bitrate off RTCP `fraction_lost` at ~1 Hz. `bitrate.rs` holds `BitrateController` (smoothing,
  cut thresholds, `DEFAULT_MIN_BITRATE_BPS` 1 Mbps) and `estimate_from_loss`.
- `pacer.rs` — a send pacer for the per-viewer RTP writer, because webrtc-rs has none: a 40 KB
  IDR is ~35 back-to-back datagrams landing in one 802.11 TXOP.
- `repair.rs` — loss recovery without a keyframe. The transport writes per-frame receipts derived
  from transport-wide feedback; the encode thread reads them at its next submit and turns them
  into `invalidate_reference` / `use_ltr` / `mark_ltr`. `keyframe.rs` is the gate that stops the
  four separate paths reaching for an IDR (RTCP PLI, RTCP FIR, payloader error, lagged viewer)
  from turning a transient into a keyframe storm; its window is derived from the measured RTT.
- `sockets.rs` — the UDP socket options webrtc-rs never sets: DSCP/`IP_TOS` so video is not in
  the AP's best-effort queue with every background download, `SO_SNDBUF`, and Windows'
  `SIO_UDP_CONNRESET`.
- `telemetry.rs` — per-stage frame timing and degradation counters, all on the one host clock, so
  a host number is directly comparable with what the client reports back over `OP::STATS`.
  Nothing in `record` allocates, locks or logs — three relaxed `fetch_add`s and a `fetch_max`.
  Read it with `ScreenExtend status --json`, which goes through the control server's `STATS` verb.
- `lock.rs` — `LockExt::lock_recover`, above.
- `pipeline.rs` — thin re-export layer pointing at the active OS pipeline.
- `platform.rs` — the runtime dispatcher: DPI awareness, process/thread tuning,
  `max_display_dpr()` (2.0 on macOS, 4.0 elsewhere), the probe entry points,
  `start_audio_capture()`, and the `EncoderBackend`/`BackendConfig` trait shared by encoders.
- `synthetic.rs` — the scripted workloads (`static`, `scroll`, `motion`, `drag`, `mixed`) the
  probe harness encodes, so an encoder number can name the content that produced it.
- `tls.rs` — self-signed cert generation at runtime (`rcgen`), cached as
  `self-signed-cert.pem`/`self-signed-key.pem` (both gitignored).
- `cloud.rs` — cross-network relay control channel (WebSocket via `tokio-tungstenite`) to
  `wss://session.screenextend.app/host/v1/connect`, protocol version 1, 45 s heartbeat timeout,
  exponential backoff to 30 s, `CloudState` surfaced to the UI via `CloudStatusChange`.
  A cross-network join does **not** require a configured TURN server, and saying so was wrong:
  measured against the live relay, `/ice-config` there serves Google STUN and no TURN, and a
  relayed session connects on server-reflexive candidates from both ends. TURN is what rescues
  the one topology reflexive candidates cannot cross (carrier-grade NAT on the client with a
  symmetric NAT on the host). Note also that the relay serves its **own** older copy of the
  client page, so nothing in `streamer/static/` reaches a relayed client, and it tunnels only
  `POST /whep`, `GET /reconfig` and `POST /leave` — `PATCH /whep/{id}` has nowhere to land, which
  is why the trickle decision is a strict `trickleIce === true`. See `docs/OPTIMIZATIONS.md`
  UX-XNET.
- `input/` — remote keyboard/mouse/clipboard injection: `protocol.rs` (wire format, including the
  `PING`/`PONG` pair, `STATS`, `KEYFRAME_REQUEST` and the batched pointer frames),
  `scancode.rs`, and a per-OS backend selected with `#[path]` (`windows.rs`, `macos.rs`,
  `linux.rs`, `generic.rs`). `prime()` warms the macOS keyboard path at startup.
- `audio/` — cross-platform system-audio transport glue: `AudioPacket`, `AudioDiagnostics`,
  the shared host timebase (`host_now_ns` / `host_instant_to_ns` / `host_ns_to_rtp90k` — **the
  one clock both audio `capture_ns` and the video RTP timestamp ride for A/V sync**), the
  reference-counted `AudioHub` (one host-wide capture fanned out to N sessions via
  `tokio::sync::broadcast`, started on the first audio-enabled subscriber and stopped when the
  last leaves — the hub's mutex guards only the refcount and the handle, never a device open or
  close, which happen with the lock released behind a `busy` flag and a condvar), the
  `AudioSink` the capture backends publish through, and `protocol.rs` (the 13-byte DataChannel
  header: seq u32, capture ns u64, flags u8, then raw Opus — or, behind `FLAG_REDUNDANT`, a u16
  length plus the current *and* previous frames, which is the only loss-robustness tool
  available because in-band FEC is LPC-layer-only and inert under `RESTRICTED_LOWDELAY`).
  It also hosts the **OS-independent Opus encoder** — the hand-written
  libopus FFI (`opus_sys.rs`) and the encoder wrapper (`encoder.rs`) — shared by every capture
  backend (libopus is cross-platform C; only the bundled library name differs: `libopus.dll` vs
  `libopus.dylib`). `macos_utils/audio/opus_encoder.rs` is only a re-export of these.
- `static/` — the client browser page (see below).
- `test/` — `whep_selftest.rs` (in-process WHEP client that asserts RTP flows), `bitrate.rs`, and
  the trickle-ICE golden vector (`.sdpfrag`, pinned `binary` in `.gitattributes` because it is
  the exact CRLF body the client writes).

### Video capture & encode — Windows (`windows_utils/streamer/`)

- `capture.rs` / `windows_capture/` — Windows.Graphics.Capture path; `windows_capture/` is a
  **vendored fork** of the `windows-capture` crate (2.0.0), already modified, so treat it as
  first-party code that happens to use `parking_lot`.
  `windows_capture/FORK.md` records the upstream pin and checksum, every divergence and every
  deleted file — read it before changing anything in there, and update it in the same commit.
- `dxgi/` — custom DXGI Desktop Duplication path including cursor compositing. It is the fallback
  and is otherwise only reached by WGC failing, which is why the probe harness has
  `--capture-backend dxgi` to force it.
- `scaler.rs`, `tuning.rs`, `workload_window.rs` — GPU scaling, process/thread priority tuning,
  and a borderless window that paints a scripted workload onto a real display so `--workload` can
  exercise capture as well as encode.
- Encoders: `nvidia/` (hand-written NVENC FFI in `nvenc_sys/`), `intel/` (oneVPL / Quick Sync FFI
  in `intel_sys.rs`), `amd/` (AMF, with the hand-written C-vtable FFI in `amd_sys/`), `x264/`
  (libx264 FFI, the CPU fallback). `intel.md` and `amd.md` sit beside them and are the reference
  for why each backend is configured the way it is.
- `pipeline.rs` picks the backend: `EncoderVendor::Auto` tries NVENC → Intel → AMF → x264;
  `Intel` tries the same-adapter path, then an own-device "CPU bridge" (`Backend::IntelCpu`),
  then x264; `Amd` likewise (`Backend::Amf` → `Backend::AmfCpu` → x264); `Software` goes straight
  to x264. Vendor detection is by DXGI **`VendorId`** (`Vendor::from_pci_id`), falling back to
  the description string (`select_vendor`) only where no id is available. The tier that actually
  started is reported through `CompatibilityReport.encoder_backend`, not only to the log.
- NVENC runs **asynchronous mode** when the caps report it and `nvEncRegisterAsyncEvent` exists:
  a per-picture completion event with a bounded `WaitForSingleObject` turns a wedged encoder into
  a named error instead of a thread blocked inside the driver, and it allows several output
  buffers instead of one. `SE_NVENC_NO_ASYNC=1` goes back to the synchronous path. It also runs
  up to `MAX_LTR_SLOTS` = 2 long-term reference slots, cap-gated on
  `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION`, which is what lets `repair.rs` answer a loss with a
  P-frame predicted from a reference the client is known to hold rather than an IDR.
- **The FFI layout tests are generated from the real headers, by a C compiler.** `test/nvenc_layout.rs`
  and `test/amd_layout.rs` assert struct sizes, field offsets, vtable slot indices, GUIDs and
  struct revision words measured by `tools/nvenc_layout_gen.c` / `tools/amf_layout_gen.c` reading
  the vendored headers in `resources/{nvenc,amf}-headers/` — not by a bindgen run checking itself.
  A wrong revision word or vtable slot is not a compile error and not a crash; it is a silent
  degrade to the next vendor on a machine with working hardware. **Never hand-edit the generated
  `abi_*` bodies**; regenerate them. `src-tauri/tools/README.md` has the procedure.
- **`AsyncEncodeRing`** is the decoupling ring both hardware backends use: the capture callback
  only blits into a free slot and sends a message, and a dedicated encode thread submits, drains
  and broadcasts. It is `RingEncoder::{Nvenc, Amf}` over `RingTextures::{Shared, Local}` —
  NVENC's slots are cross-adapter shared textures behind a keyed mutex, AMF's are the encoder's
  own textures on the capture device, so nothing is shared and slot ownership is the free/ready
  channels'. The AMF ring is chosen for `--encoder amd`, and for `auto` only when the capture
  device is itself AMD; anything else falls back to the inline `build_backend` path.
- **The AMF backend is `UNVERIFIED`** — see the "Current support" note above. Its design note is
  `windows_utils/streamer/amd.md`, and `amd/sps.rs` rewrites the SPS VUI on the way out because
  AMF writes neither colour signalling nor `bitstream_restriction_flag` into an AVC bitstream
  (AMF issues #398, #277, #540) — gated on a read-back, so it stands down if a driver fixes it.

### Video capture & encode — macOS (`macos_utils/streamer/`)

- `mod.rs` — the `CaptureBackend` trait, `CaptureError`, pixel formats, and `start_capture()`,
  which tries **ScreenCaptureKit** when `screencapturekit_available()` — an `AnyClass::get`
  probe for `SCStream` and `SCShareableContent`, never a version number — and falls back to
  **CGDisplayStream** (`cgds.rs`), reporting `FallbackFailed` only when both fail. SCK is not
  committed to until it has actually delivered a frame (1500 ms), because a stream that starts and
  delivers nothing is indistinguishable from a slow one.
- `sck.rs` — the SCK backend. **Dyld-safety is load-bearing**: SCK only exists on 12.3+, so a
  link-time reference (`objc2-screen-capture-kit`, an `extern_class!`, any 12.3+ `extern static`)
  would add an undefined dyld symbol absent on the 10.15 floor and break loading of the *whole*
  binary. It therefore uses runtime interop only — `AnyClass::get`, `msg_send!`, `dlsym` for the
  string constants, and a `ClassBuilder`-constructed `SCStreamOutput` delegate. Its module doc
  numbers the eight steps of the flow and the body comments refer back to those numbers.
- `cgds.rs` — the CGDisplayStream fallback, IOSurface → CVPixelBuffer, ITU-R 709 YCbCr. Every
  function and constant is `dlsym`'d: the API is *obsoleted* in the macOS 15 SDK, and a link-time
  reference to a symbol Apple has announced the end of would make the whole binary unloadable the
  day it goes. Surfaces held past the callback carry an `IOSurfaceIncrementUseCount`, which the
  header requires and without which WindowServer overwrites frames mid-encode.
- `encoder.rs` — VideoToolbox H.264 compression session; `clock.rs` — the capture-time gate (an
  OS frame stamp is used only when it is in the past, because CGDisplayStream's `displayTime` is
  a *scheduled presentation* time, ahead of delivery on every frame); `gpu.rs` — an empty
  placeholder (the macOS path needs no GPU convert or scale stage: WindowServer delivers the frame
  already scaled and already in `420f`, and the IOSurface reaches VideoToolbox without a copy);
  `frame.rs` — `Frame`/`FrameSink`; `display.rs`, `config.rs` — display enumeration and modes;
  `mach.rs` — `mach_absolute_time`; `qos.rs`, `tuning.rs`, `power.rs`, `activity.rs` — thread QoS,
  process tuning, power/idle assertions.
- VideoToolbox **does** have long-term references from macOS 12.0, but token-based: the encoder
  picks its own and hands back an opaque `RequireLTRAcknowledgementToken`. There is no slot to
  name, so `RepairAction`'s `mark_ltr`/`use_ltr_bitmap` have nothing to map onto and the macOS
  pipeline declares zero slots — every repair is still a keyframe. `Encoder::supports_ltr()`
  reports per session whether this Mac advertises the property. See `docs/OPTIMIZATIONS.md`.
- `virtual_display.rs` — the private CoreGraphics `CGVirtualDisplay*` classes via
  `extern_class!` / `extern_methods!` (objc2 resolves these at runtime, so a Mac without them
  still loads the binary — but instantiating a missing class panics, hence the probe below).
  It also owns the **display backend tiering**, mirroring the audio subsystem's:
  `probe_display_backend()` → `CgVirtualDisplay` when `AnyClass::get("CGVirtualDisplay")`
  succeeds (10.14.4+ — **not** 10.15, the classes predate Sidecar by a release), else
  `AirPlayFallback`. `new_shared_controller()` is what `setup` calls; the chosen tier is
  reported to the UI as `CompatibilityReport.display_backend`. **Probe by class lookup, never by
  version number.**
- `airplay/` — the **pre-10.14.4 display tier**, below the supported floor now and kept
  only as a safety net. A minimal fake AirPlay receiver
  (Bonjour via a hand-written `dns_sd.h` FFI, a hand-rolled RTSP/1.0 + HTTP/1.1 socket because
  hyper can parse neither, static `/fp-setup` tables, accept-and-discard for the H.264 stream)
  plus an Accessibility driver for the Displays menu extra. macOS creates the display; we only
  make it exist and then capture it with the unchanged CGDisplayStream pipeline. **No decode,
  no decrypt, no VideoToolbox.** Design + measurements: `docs/airplay-fallback-design.md`.
  Live harness: `cargo test --lib macos_utils::airplay::tests::live_end_to_end -- --ignored
  --nocapture`.
- `hosted_network.rs` — CoreWLAN **HostAP** (infrastructure AP) mode via the private
  `startHostAPModeWithSSID:securityType:channel:password:error:`, behind a `respondsToSelector:`
  probe, with a deprecated-IBSS second tier and an explicit "use your own router" third tier
  (`probe_hosted_network_backend`). Prefers 5 GHz channels 36/40/48/153/157/161 and 2.4 GHz
  channel 11, all chosen to avoid AWDL's social channels 6/44/149. A start is only reported as
  working once `interfaceMode()` reads HostAP *and* the interface holds a routable IPv4.
  Read-only surface report: `cargo test --lib
  macos_utils::hosted_network::tests::report_hostap_surface -- --ignored --nocapture`.

### System audio — Windows (`windows_utils/audio/`, tiered)

Per-device system-audio capture + Opus encode. Two capture tiers, selected at runtime:

**Tier 1, endpoint loopback** (`loopback.rs::acquire_endpoint`) — WASAPI **legacy
`IAudioClient::Initialize` loopback** on the `eRender`/`eConsole` endpoint (the IAudioClient3
low-latency path rejects the loopback flag). Runs on a **dedicated OS thread** with COM MTA +
MMCSS "Pro Audio", event-driven, re-acquiring on default-device change,
`AUDCLNT_E_DEVICE_INVALIDATED`, a dead companion, or 2 s of an unsignalled event.

**Tier 2, process loopback** (`process_loopback.rs`) —
`ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK)` with
`AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` and
`PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` on our own pid, i.e. "the whole system
except us". **Probed by attempting the activation, never by a build number.** It is the
fallback rather than the default because measurement says it is not faster (both ride the same
shared engine); what it buys is capture that survives an exclusive-mode endpoint and device
churn, plus a format we choose instead of the endpoint's. It needs no silent companion —
measured, not assumed. `ScreenExtend probe --audio --process-loopback` is the only way to reach
it on a machine where tier 1 works.

- `silence.rs` — a **silent render companion** is mandatory for tier 1: it is the clock source
  that makes the loopback event fire and keeps packets flowing while the host is idle (measured:
  0/20 event signals without it, 20/20 with it — `docs/recovered/AUDIO_NOTES.md` §3.2). It is
  also the only stream we own that may ask the engine for a smaller period (`engine.rs`,
  `IAudioClient3::InitializeSharedAudioStream`), which the capture stream then inherits — a
  probed tier that is a correctly-detected no-op on hardware exposing a single period. Started
  lazily, stopped with the capture, and its death is detected, logged, counted and recovered from.
- `engine.rs` — `GetSharedModeEnginePeriod` / `GetDevicePeriod`. The engine *period* and the
  engine *buffer* are different numbers and are reported separately.
- `format.rs` — mix-format negotiation; 48 kHz float32 stereo is the fast path, a non-matching
  endpoint is `AUTOCONVERTPCM`'d by the engine, and if the engine refuses, capture falls back to
  the endpoint's own format and downmixes (ITU BS.775) / converts int→float in userspace. Note
  that the fast path is not zero-copy: it is a load/clamp/push loop over the wake's frames.
- `device.rs` — `IMMNotificationClient`; its callback runs on a COM thread we don't own, so it
  only posts to the capture thread over `crossbeam-channel` (never blocks, never takes the
  capture lock). `guards.rs` holds the RAII COM/handle/mix-format wrappers.
- Opus encode uses the **shared** wrapper in `streamer/audio/`: `libopus.dll` loaded via
  `libloading` and bundled in `resources/`; `RESTRICTED_LOWDELAY` (CELT-only), 5 ms frames.

**Capture-thread convention:** the audio capture thread is a real-time OS thread (MMCSS), never
a tokio task. It never allocates or locks in the drain/encode path beyond the reused accumulator
plus the per-packet `Bytes` copy — which rules out percentile sorting and `tprintln!` on the 2 s
diagnostic path too; those live on a separate reporting thread. It publishes straight into the
`AudioHub`'s `tokio::sync::broadcast` through an `AudioSink`, because `broadcast::Sender::send`
is synchronous and non-blocking; there is no intermediate channel and no copy thread. Linux
provides a compiling stub (`linux_utils/audio.rs`) that returns "unsupported".

### System audio — macOS (`macos_utils/audio/`, tiered)

macOS has **no single audio-capture API across 10.15–current**, so `probe_audio_backend()`
probes at runtime and uses the first available tier (native results are cached in a `OnceLock`;
the legacy tier is recomputed because install state flips at runtime):

`ProcessTap` (14.2+) → `ScreenCaptureKitAudio` (13.0+) → `VirtualDevice` (10.15–12.x, driver
installed and healthy) → `NeedsDriverInstall` → `Unsupported`. The chosen tier is reported to
the UI through `CompatibilityReport.audio_backend`.

- `process_tap.rs` — **Core Audio Process Tap** (`CATapDescription` + aggregate device + IOProc),
  macOS **14.2+**, preferred (audio-only, no screen-recording indicator). The aggregate device's
  tap-list UID must match the `CATapDescription` UUID or the tap returns `noErr` but pure silence
  (the documented failure mode) — built exactly like the `AudioCap` reference, from the
  description's own `UUIDString`. Teardown is RAII in order
  stop→destroyIOProc→destroyAggregate→destroyTap so a leaked tap can't persist a hidden capture.
- `sck_audio.rs` — **ScreenCaptureKit `capturesAudio`**, macOS **13.0+**, fallback. SCK has no
  audio-only mode, so it rides a minimal 2×2/1 fps dummy video stream; uses the existing Screen
  Recording permission.
- `format.rs` — parses the delivered `AudioStreamBasicDescription` (handles **planar** float, which
  Windows never produces) → interleaved-stereo f32; BS.775 downmix duplicated from the Windows
  sibling (the byte layouts diverge too much to share cleanly).
- `mod.rs` — the shared `AudioSource` trait over all backends, the `probe_audio_backend()` tiering
  above, and the encoder worker.
- `legacy/` — the **macOS 10.15–12.x virtual-device tier**. Below 13.0 there is no native
  system-audio API at all, so this ships ScreenExtend's own **AudioServerPlugIn** virtual device
  (built on libASPL/MIT, in `src-tauri/macos/ScreenExtendAudio/`), sets it as the default output,
  and reads the captured mix back over a POSIX shared-memory ring (`shm_reader.rs`, with a
  HAL-input fallback) into the same encoder ring — zero new Opus/client code. `routing.rs`
  saves/switches/restores the default output (+ crash recovery on launch); `playthrough.rs` plays
  the capture to the real device with a gain stage; `volume_proxy.rs` + the driver's Volume/Mute
  controls keep the macOS volume keys working while the virtual device is default (the classic UX
  regression this tier must repair), with `volume_keys.rs` as a `CGEventTap` backstop that
  intercepts F10/F11/F12 directly if the OS doesn't re-enable them. `installer.rs` runs a
  signed/notarized `.pkg` behind one admin prompt. **This tier is never selected on 13.0+**
  (native wins). **Privacy:** it makes ScreenExtend the system output device while streaming — an
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

### The macOS audio driver project (`src-tauri/macos/ScreenExtendAudio/`)

A standalone C++ `AudioServerPlugIn` (not DriverKit — Apple does not grant DriverKit
entitlements for virtual audio devices), built on libASPL. Compiled with `clang++` directly via
`scripts/build.sh` (no CMake/Xcode needed) and packaged by `scripts/package.sh`;
`scripts/release.sh` is the one command that does build → sign → package → notarize → staple,
skipping cleanly whichever steps have no identity configured, so a developer with no Apple
credentials still gets an unsigned `.pkg`. Installed to `/Library/Audio/Plug-Ins/HAL/`. The
shared-memory byte layout in `src/shm_ring.hpp` is a **hard ABI contract** with
`macos_utils/audio/legacy/shm_reader.rs` — change both together. All branded strings live in
`src/branding.hpp`.

**Status: verified end to end on the 10.15.8 development Mac** (SIP disabled) — it builds from a
clean tree, the `.pkg` installs and `coreaudiod` picks the device up without a reboot, the
plug-in loads and creates its shared-memory segment, and 2,083,840 samples have gone through
`OnProcessMixedOutput` into the shipped `start_capture()` path with 0 overruns and every
`capture_ns` delta exactly 5.000 ms. What is **not** verified: signing and notarization (no Apple
Developer identity on either machine, so it has only ever loaded with SIP disabled), arm64 (the
10.15 SDK cannot target it), and the non-stereo downmix branch. Its README and
`docs/manual-tests/macos-audio-driver-signing.md` carry the detail.

### The client web page (`streamer/static/`)

It is one shared scope with no build step, so everything about it is a compatibility question
first. Each subsystem degrades through explicit **tiers**, and every tier is a value the client
reports back to the host in the `OP::STATS` frame — a device on the slow path must not look like
one on the fast path (`docs/TELEMETRY.md`).

- `index.html` — the join form (session id, device name, 6-digit OTP, fullscreen toggle), the
  video stage (`<video>` / `<canvas>` swap), and the four modals: HTTPS upsell,
  limited-browser-support warning, same-device warning (driven by the `__SAME_DEVICE_FLAG__`
  substitution) and the microphone card that reveals audio-output labels. Every one of them —
  and the browser's own output picker — goes through `beginModal`/`endModal`, which leaves
  fullscreen, drops the pointer and keyboard locks and stops forwarding input, because a dialog
  raised over a locked stage cannot be clicked at all. It also drives `/reconfig` polling,
  trickle ICE, viewport /
  orientation / DPR reporting, and kick handling. It owns the two tier enums:
  `VIDEO_PATH = { webcodecsCanvas, webcodecsVideo, videoElement }` and
  `AUDIO_PATH = { workletSab, workletPort, workletPost, opusTrack }`.
- `at-capacity.html` — served instead of the join form when the display backend is at its cap.
  Only the macOS AirPlay tier has one; every other backend is uncapped and never serves it.
- `transform-worker.js` — the WebCodecs video path, over `RTCRtpScriptTransform` where it exists
  and the legacy `createEncodedStreams` where it does not (`TRANSPORT` tiers), rendering either
  into a canvas or through a `VideoTrackGenerator` (`renderTier`). It recovers each frame's
  host-capture time from its RTP timestamp. **It never schedules; it resynchronises** — frames are
  painted inline in the decoder's output callback, and when the decoder falls behind the answer is
  to drop to the freshest frame and ask for a keyframe. The out-of-band keyframe request is only
  claimed as a tier once a `PONG` says the host parses the opcode.
- `audio.js` — owns the `AudioContext`, the worklet node and the decoder worker, plus the NetEQ
  track fallback. On the fast path it does nothing per packet but transfer the DataChannel buffer
  to the worker. It commands the worklet a buffer depth so audio plays in step with the picture;
  measured offset via `SEAudio.getSyncInfo()` (no on-screen HUD). It also owns the ring's
  allocation size, which makes the ring layout a **three-file** contract.
- `audio-worker.js` — Opus decode off the main thread: header parse, dedup, WebCodecs
  `AudioDecoder`, deinterleave, ring write. An `RTCDataChannel` cannot be handed to a worker, so
  the page still receives the packet — but nothing else happens there.
- `audio-worklet.js` — our jitter buffer; corrects drift only at silence boundaries. The ring is
  **planar** (separate L/R regions) so both sides are `set()` memcpys, and its byte layout is a
  hard contract with `audio-worker.js` and `audio.js` — change the three together. Cross-origin
  isolated, it is a `SharedArrayBuffer` the worker writes and the worklet reads with `Atomics`, so
  a decoded packet reaches the speakers without touching a message queue; otherwise the identical
  ring is fed over a `MessageChannel`, or by `postMessage` via the page. Positions are absolute
  uint32 frame counters, which is why the capacity must stay a power of two.
- `input.js` — pointer/keyboard/clipboard capture, the input wire protocol, coalesced pointer
  batching, and `noteHostCaps` (the host capability bits in the `PONG` tail). Nothing on the page
  may assume an optional mechanism works before the first `PONG` carrying its bit lands.
- `nosleep.js` — keeps the client screen awake.

The page has been through seven cold review passes (`docs/reviews/`), covering correctness,
fallbacks, old browsers and regressions; read the relevant one before reworking a subsystem.

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
  state, and raises OS notifications via `@tauri-apps/plugin-notification`. `dashboard.tsx`
  additionally subscribes to `cloudStatusChange` and gates its render on `get_display_capacity`.
- **Onboarding walkthrough** uses `nextstepjs` (`src/components/walkthrough.tsx`), which expects
  `next/navigation`; Vite aliases that to `src/lib/next-navigation-stub.ts` — keep the alias if
  you touch the resolver config in `vite.config.ts`.
- The Settings page carries a monaco-based `config.json` editor (`config-json-editor.tsx`,
  `json-editor.tsx`, `lib/monaco-*.ts`, `lib/config-schema.ts`, `lib/apply-config.ts`) and the log
  terminal (`log-terminal.tsx`, which buffers to a `requestAnimationFrame` and keys rows by a
  monotonic sequence so a burst is one commit, not 2000 repainted rows).
- `src/lib/`: `bindings.ts` (generated), `zoom.ts` (webview zoom + persistence), `avatar.ts`,
  `utils.ts` (`buildQrValues`, `generatePassword`, `cn`), `error-reporting.ts` (routes
  `console.error` to the log bus), `route-chunks.ts` / `preload-editor.ts` (chunking and the
  editor preload), `legacy-webview-polyfills.ts`, `next-navigation-stub.ts`.
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
disableGpuEncode, legacyVolumeKeyProxy, walkthroughCompleted, disconnectGraceSecs,
serverPorts{http,https}, hostedNetworkCredentials{name,password},
turnConfig{urls,username,credential}, stunUrls,
dontShowAgain{editDevice,editNetwork,compatibility,configEditor},
driverRepair{boot,install,reinstall,relaunch}
```

`Device` = `{ ip, token, name, scale, orientation, refreshRate, videoScale, videoQuality,
remoteControl, pointerScope, autoFrameRate, systemAudio, audioOutputDeviceId,
audioOutputDeviceLabel, os, screenSize, dpr, maxDpr }`; `KnownDevice` = `{ token, ip, name, os, screenSize, lastSeen, banned }`.
**Both are keyed by `token`**, not by `ip` — the address is a display field that changes under a
live session and is recycled between devices. A row written before tokens existed matches by
address, which is all it names, until that device joins once more and is minted one; until then
it is not replayed onto anything. `ConnectedDevice = Device & { joinId }` is the runtime-only
shape the dashboard's device list carries — one entry per live join, never persisted.

Relevant values are pushed to the Rust `AppState` through commands (`setSessionCredentials`,
`setDeviceOverride`, `setDeviceAudioOutput`, `setDisconnectGrace`, `setTurnConfig`,
`setServerPorts`, `setDisableGpuEncode`, `setDeviceBanned`, `setDeviceApproved`,
`setLegacyVolumeKeyProxy`); `stunUrls` is read on the Rust side at `setup`. The **CLI reads and
writes the same store directly** (`cli.rs`'s `open_store` / `get_nested` / `set_nested`, dotted
keys), `serve` replays it into `AppState` at startup (`apply_saved_config`), and the control
server's `RELOAD` verb replays it into a *running* host. So a new setting usually needs three
touches: the TS config type + default, the Rust command, and the `serve`/`config` CLI path.

## Versioning & releases

- The version string is duplicated in **four** files and must be kept in sync: `package.json`,
  `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, `src-tauri/tauri.macos.conf.json`
  (`src-tauri/Cargo.lock` then updates automatically). `node scripts/check-versions.mjs` asserts
  it, in CI and locally. The Rust side reads the version from `CARGO_PKG_VERSION` / Tauri package
  info at runtime — there is no hardcoded version literal in `src-tauri/src/`, and there is
  deliberately none in this file either.
- **CI** (`.github/workflows/ci.yml`) runs on **every push to any branch** and on pull requests,
  so it runs whenever the repository owner pushes. Jobs:
  - `frontend` (ubuntu) — `pnpm test:lockfile`, `pnpm lint`, `pnpm build`,
    `check-legacy-webkit.mjs`, `pnpm test:client`, `check-bindings.mjs`, `check-versions.mjs`.
  - `rust` (windows-latest, macos-13, macos-14, macos-15) — `pnpm build` first because
    `tauri_build` embeds `../dist` at compile time, then `cargo fmt --check`,
    `cargo clippy --all-targets -- -D warnings`, and a plain `cargo test` with **no `--skip`**:
    a test that cannot run on the runner gates itself through `test_support::skip`.
  - `macos-smoke` (macos-13/14/15) — the runners are the only machines on this project where
    12.3+/13+/14.2+ code can execute at all. Builds the host binary, asserts `doctor --json`
    reports the probed backends and that the **audio tier matches the runner's macOS**
    (`screencapturekit` on macos-13, `process_tap` on 14/15), then runs the `#[ignore]`d
    VideoToolbox encode tests.
  - `dyld-floor` (macos-15) — release build, `otool -L` for any framework absent on 10.15,
    `LC_BUILD_VERSION` against `minimumSystemVersion`, then `check-macos-dyld.sh --floor 10.15`.
    The runner SDK is not the 10.15 SDK, so the strong verdict still comes from the dev Mac.
  - `blob-checksums` (ubuntu) — `sha256sum -c` over both `SHA256SUMS` files. This is what both
    `PROVENANCE.md` files mean by "verified in CI".
  - `perf-gate` (windows) — three release runs of the x264 encoder tests, lowest taken, compared
    against `ci/perf-baseline.json`.

  A predecessor `ci.yml` existed for 4 minutes in 2026-08 and never ran; the current file is new
  configuration, not a restoration.
- **Release CI** (`.github/workflows/build-release.yml`) triggers on pushing a tag matching
  `app-v*` (or a manual `workflow_dispatch`) — **not** on push to `main`. It builds Windows x64
  and macOS universal (`--target universal-apple-darwin --config src-tauri/tauri.macos.conf.json`)
  via `tauri-action` and drafts a GitHub Release; a second `finalize-release` job strips the
  version out of the asset filenames, patches `latest.json` to match, publishes the release as
  latest, and force-moves the `latest` git tag onto the released commit.
- Auto-update: `tauri-plugin-updater` reads `latest.json` from the GitHub "latest" release
  (pubkey + endpoint in `tauri.conf.json`; `installMode: passive` on Windows).

## Native resources, drivers & permissions

- **Windows bundle resources** (`tauri.conf.json` → `bundle.resources`): the signed Virtual
  Display Driver (`VirtualDisplayDriver.dll`/`.cat`/`.inf`), its cert (`ScreenExtend.cer`),
  `libx264-164.dll` (software-encode fallback), and `libopus.dll` (system-audio Opus encode,
  loaded via `libloading`). Provenance + SHA-256 in `resources/PROVENANCE.md` / `SHA256SUMS`,
  checked by the `blob-checksums` CI job. The vendored AMF and NVENC headers live under
  `resources/{amf,nvenc}-headers/` and are pinned `text eol=lf` in `.gitattributes` — without
  that, a Windows checkout changes their bytes and the checksum job disagrees with every local
  run. **Any new vendored blob directory needs a `.gitattributes` line at the same time as its
  checksum.**
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
  settings / relaunch); the Windows and Linux implementations return an empty list.
  **Location Services is deliberately never requested.** macOS treats a Wi-Fi SSID as location
  data, so `CWInterface.ssid` can return nil without it and the dashboard then labels the adapter
  "Wi-Fi" — on purpose, not by accident. A Location prompt is one of the heaviest consents macOS
  shows and the only thing it would buy here is a cosmetic label on a row that already carries the
  adapter's addresses; do not add it. `macos_utils/networking.rs::wifi_ssid` says so once in the
  log.

## Conventions & gotchas

- Never hand-edit `src/lib/bindings.ts`; regenerate with `pnpm tauri dev` (debug + no CLI args),
  diff it, and run `node scripts/check-bindings.mjs`.
- A new Tauri command must be added to **all three** `*_utils` modules and to
  `collect_commands!`.
- Use `tprintln!`/`teprintln!`, not `println!`/`eprintln!`, in the Rust core.
- Take streamer-core mutexes with `lock_recover()`; keep critical sections short and never hold
  one across `.await`.
- **COM is initialized by Tauri.** wry gives the main thread an STA for WebView2, and every sync
  `#[tauri::command]` runs there — so Rust inside Tauri must assume an apartment already exists.
  A second `CoInitializeEx`/`RoInitialize` with a different model returns `RPC_E_CHANGED_MODE`,
  initializes nothing, and leaves WMI/WASAPI/DXGI/WinRT to fail or degrade quietly. Ask with
  `CoGetApartmentType`, never re-initialize on a Tauri thread, and never `CoUninitialize` one.
  Only threads the app spawns itself may initialize, and only through
  `windows_utils::com::ensure_for_this_thread` (`wmi_for_this_thread` for the `wmi` crate —
  `COMLibrary::new()` is banned); the returned `ComScope` releases exactly what it took. The
  measured thread map is in `docs/OPTIMIZATIONS.md` UX-E.
- **Probe the capability, never the version number.** Every tiered subsystem in this repo picks
  its tier by attempting the thing — `AnyClass::get`, `dlsym`, `GetProcAddress`, a trial API call,
  a feature-level query — because a version number says nothing about a machine whose hardware,
  drivers or permissions differ inside the same release. `docs/CAPABILITY-TIERS.md` is the table
  of every tier, its probe and its verification status; a new tier belongs in it.
- **No link-time references to macOS APIs newer than 10.15** — use `dlsym` / `AnyClass::get` /
  `msg_send!`. An eagerly-bound symbol dyld cannot resolve kills the whole process before
  `main()`, so this is a load bug, not a missing feature. `extern static` constants count:
  two VideoToolbox property keys (`MaximizePowerEfficiency`, 10.14;
  `PrioritizeEncodingSpeedOverQuality`, 11.0) had to move to `set_optional_bool` for exactly
  this reason. Check with `scripts/check-macos-dyld.sh`, or `nm -u target/release/ScreenExtend`.
- The client page has no build step and no dependencies; it must keep working in older mobile
  browsers (that constraint is why, for instance, flex `gap` was replaced with margins). It is
  one shared scope, so a single unsupported *token* anywhere in `index.html`, `input.js` or
  `audio.js` takes the whole join form down — the same shape of failure as the macOS dyld rule.
  No optional chaining, nullish coalescing, logical assignment, class fields, private `#`
  members, top-level `await`, `BigInt` literals, or the CSS `inset` shorthand without
  long-hands. `audio-worker.js` and `audio-worklet.js` are the exception: they are only ever
  loaded after `AudioDecoder` and `AudioWorklet` have been probed, so they may use newer
  syntax. `node scripts/check-legacy-webkit.mjs` scans `src-tauri/src/streamer/static/` under
  that stricter contract.
- **The client video path never schedules; it resynchronises.** Frames are painted inline in
  the decoder's output callback — no `requestAnimationFrame`, no queue we control. When the
  decoder falls behind, the answer is to drop to the freshest frame and ask for a keyframe,
  never to buffer. Backpressure is measured as decodes that have not yet produced an *output*,
  never as `decodeQueueSize`, which counts acceptance and can read zero with frames in flight.
- Every silent degradation on the client page is a bit in the `OP::STATS` frame
  (`docs/TELEMETRY.md`, `docs/INPUT-PROTOCOL.md`). A new `catch (_) {}` there should call
  `noteSuppressed()`, and a new tier should get a value, or the host cannot tell a device on the
  slow path from one on the fast path. The `PONG` tail carries the host's capability bits the
  same way, and both frames are **prefix-stable**: a reader that sees fewer bytes than it knows
  about reads the prefix and treats the rest as absent.
- DPR is only correct when the resolution multiplier and the OS scale step are the **same
  number**: resolution x `d`, DPI `100*d`%. Snap `d` with `platform::snap_display_dpr` (Windows
  exposes its real DPI table; macOS only has 100/200, because HiDPI there is a binary 2x flag),
  pre-register every ladder mode at create time — a live display cannot be given new ones — and
  re-apply the DPI *after* the resolution change, which resets it.
- **Nothing on a request path resolves a peer by its address.** A session is named by the
  `join_id` the host mints at `POST /whep`; a device is named by its token. An address moves
  under a live session and is handed to the next device that asks for one, so keying by it is
  how one device ends up driving another's display, wearing another's settings, or inheriting
  another's ban. Keep it as a label.
- Simultaneous displays are **uncapped**. `VirtualDisplayController::max_concurrent_displays()`
  returns `None` everywhere except the macOS AirPlay tier, which returns `Some(1)` because macOS
  itself owns that display. Do not cap the other backends, and never evict a connected device to
  make room for a new one; a host that is genuinely full serves `at-capacity.html`. Read the cap
  through **`session::concurrent_display_cap`**, never `max_concurrent_displays()` directly: it
  recognises `Some(1)` and treats any other finite number as uncapped, so `at-capacity.html` and
  `DisplayCapacity.full` cannot be reached on a backend nobody meant to cap.
- **A compatibility row may only be `blocking` if the host cannot create a display, cannot
  capture, or cannot encode.** `UnsupportedApi::blocking` takes a `BareMinimum`, not a string, so
  that is type-enforced; everything else is `optional` (the user gets "Continue anyway") or
  `installable` (the bootstrap fixes it and re-probes before deciding). The decision itself is
  `src/lib/startup-gate.ts`, tested by `pnpm test:gate`.
- The desktop UI must run on **Safari 13.1** (the WKWebView on the 10.15 floor). Anything
  newer that a dependency reaches for is a hard crash, not a missing feature — monaco alone
  needed six shims, and its MediaQueryList call runs at module scope, so it took down the whole
  settings editor chunk. `src/lib/legacy-webview-polyfills.ts` carries the shims (imported first
  from `main.tsx`, and from the two monaco worker wrappers, which never run the entry chunk);
  `node scripts/check-legacy-webkit.mjs` re-derives the list from `dist/` after a build and
  fails on anything unpatched. Keep the two in step, and note that the CSS side of that check is
  weaker than the JS side — a post-13.1 selector can ship silently. On the CSS side, `darkMode` is
  `["variant", ".dark &"]` because tailwind 3.4's default class strategy compiles to
  `:is(.dark *)`, which Safari 13 drops the whole rule over, and **flex** containers may not use
  `gap-*` (13.1 has it for grid only, flex landed in 14.1) — reach for `space-x/y-*`, or a margin
  on the icon when the sibling is a bare text node, which `space-*` does not select.
- Bump the version in all four files at once.
- New shell command → `capabilities/default.json`.
- New user setting → TS config type/default + Rust command + the `serve`/`config` CLI path.
- Record what you changed in `docs/OPTIMIZATIONS.md` with a status
  (`VERIFIED` / `COMPILE-CHECKED` / `UNVERIFIED`), including the things that did not work.

## Dev-box notes

Development happens on two machines and `docs/dev-workflow.md` is the protocol: a Windows box
(the primary tree) and an Intel Mac reachable over SSH, moving commits between them with git
bundles, never through `origin`.

The Mac is **macOS 10.15.8 Catalina, Intel**, with Command Line Tools only (no Xcode, no
Homebrew) and git 2.24. Consequences: ScreenCaptureKit (12.3+) and the Core Audio Process Tap
(14.2+) are unavailable there, VideoToolbox low-latency (11.0+) is off, `probe_audio_backend()`
returns `VirtualDevice` or `NeedsDriverInstall`, and the 10.15 SDK cannot target arm64 (universal
artifacts need CI or a newer Mac). What *can* be exercised locally is `cargo
check`/`clippy`/`test`, `pnpm lint`/`build`, the CGDisplayStream video path, the legacy audio
driver tier, and the 10.15-SDK dyld audit — which is the one check CI cannot do properly, because
no hosted runner has that SDK. Everything above 10.15 is CI's job (`macos-smoke`), and the things
neither can do are written up as scripts in `docs/manual-tests/`.

The Windows box is Windows 11 26200 with an RTX 3080 Ti Laptop (hybrid graphics), so NVENC, Quick
Sync, x264, WGC, DXGI and the WASAPI audio tiers are all exercisable there. **No machine on this
project has an AMD GPU.**
