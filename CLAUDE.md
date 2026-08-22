# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

ScreenExtend is a Tauri 2 desktop app that turns any device with a web browser into a
wireless second monitor. It runs on the **host** (the PC being extended). A **client**
(phone/tablet/laptop) joins by opening a URL or scanning a QR code — no client install. The
host creates a real virtual display per client, GPU-encodes it, and streams it over WebRTC
(WHEP signaling, H.264). See `README.md` for the user-facing feature overview.

Two distinct frontends live in this repo:
- `src/` — the **desktop control UI** (React, runs in the Tauri webview on the host).
- `src-tauri/src/streamer/static/` — the **client web page** served to joining devices
  (`index.html`, `input.js`, `transform-worker.js`, `styles.css`). This is plain
  HTML/JS/CSS served by the Rust HTTP server, NOT part of the Vite/React build. Edit it
  directly; there is no bundler step for it.

## Commands

```sh
pnpm install
pnpm approve-builds --all        # approve native build scripts (esbuild, sharp, core-js-pure)

pnpm tauri dev                   # run the full desktop app (host). Also regenerates src/lib/bindings.ts
pnpm tauri build                 # production build -> src-tauri/target/release/bundle/

pnpm dev                         # frontend only, Vite dev server on fixed port 1420
pnpm build                       # tsc + vite build (type-check + web bundle only, no Tauri)
```

Rust code lives in `src-tauri/`; run `cargo` commands from there (`cargo check`, `cargo build`, `cargo test`).

There is **no lint script and no ESLint config file** despite ESLint devDependencies being
installed — do not assume `pnpm lint` exists. TypeScript strictness is enforced via `tsc`
in `pnpm build`.

### Host CLI subcommands (shipped binary)

Defined in `tauri.conf.json` under `plugins.cli` and handled in `src-tauri/src/lib.rs::run` `setup`:

```sh
ScreenExtend.exe installdrivers          # install signed virtual display driver + cert (admin)
ScreenExtend.exe removedrivers           # uninstall driver + cert (admin)
ScreenExtend.exe hostednetwork <ssid> <password>   # start an ad-hoc Wi-Fi hosted network (Windows)
```

`src-tauri/src/streamer/cli.rs` + `Streamer::probe_*` are a **separate developer probe
harness** (probe_capture / probe_dxgi / probe_encode / probe_live / probe_bitrate /
whep_selftest) for testing the capture→encode→WebRTC pipeline in isolation. They are not
wired into the shipping `main.rs` entry point.

## The Rust ↔ TypeScript bridge (important)

Commands and events are defined in Rust and consumed type-safely in TS via
[`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta):

- All commands/events are registered in `collect_commands!` / `collect_events!` in
  `src-tauri/src/lib.rs::run`.
- On a **debug build** (`pnpm tauri dev`), the bridge is exported to
  **`src/lib/bindings.ts`** (prefixed with `// @ts-nocheck`). **This file is generated —
  never hand-edit it.** To change the API, edit the Rust command/event + its `#[specta]`
  types and re-run `pnpm tauri dev`.
- Frontend calls go through `commands.*` and `events.*` imported from `@/lib/bindings`.

## Architecture

### Platform abstraction

`src-tauri/src/` has three `cfg`-gated sibling modules — `windows_utils/`, `macos_utils/`,
`linux_utils/` — each exposing the **same surface** (`AppState`, `setup`, `networking`,
`hosted_network`, `virtual_display`, `streamer`, `compatibility`, `device_reporter`).
`lib.rs` glob-imports the active one (`use windows_utils::*` etc.), so the Tauri command
list is identical across OSes and dispatch happens at compile time. `streamer/platform.rs`
is the runtime dispatcher for capture/encode/tuning that calls into the per-OS backend.

**Current support:** Windows (NVENC/QSV) and macOS (VideoToolbox) are functional. Linux is
scaffolded but non-functional; AMD encode is stubbed. When adding a capability, add it to
all three modules to keep the surface consistent (Linux/AMD paths may `bail!`).

### Concurrency & locking

Shared mutable state (`AppState` fields, the `streamer` session/override/ban maps, cloud
status, etc.) is guarded with **`std::sync::Mutex` — this is the one blocking mutex type for
first-party code.** Do not introduce `parking_lot::Mutex` outside `windows_utils/windows_capture/`
(that module is a vendored fork of the `windows-capture` crate and keeps its own `parking_lot`
locks — leave it as-is).

**The rule: never hold a `std::sync::MutexGuard` across an `.await`.** Keep critical sections
short — lock, read/copy/clone what you need, drop the guard, *then* await. This is also
compiler-enforced in `Send` contexts (axum handlers, `async` Tauri commands) because
`MutexGuard` is `!Send`, so a violation there fails to build; but it is *not* caught in
non-`Send` closures/callbacks, so keep the discipline everywhere.

Watch the temporary-lifetime footgun: a guard produced in an `if let` / `match` scrutinee
(e.g. `if let Some(x) = map.lock().unwrap().get(k) { … }`) lives until the **end of the
block**, not the end of the line — never put an `.await` inside such a block.

Use an async-aware lock (`tokio::sync::Mutex`/`RwLock`) **only** when a lock genuinely must be
held across an `.await`. There are two such cases today, both intentional:
`DISPLAY_CORRELATION_LOCK` in `streamer/server.rs` (serializes virtual-display creation across
its `spawn_blocking`/settle awaits) and `receive_error: RwLock<…>` in
`windows_utils/driver_ipc/client.rs`. Prefer the sync mutex + short critical section for
anything else.

### The `streamer` module (`src-tauri/src/streamer/`)

Cross-platform core, OS-independent:
- `server.rs` — axum HTTP(S) server per network adapter; validates session ID + OTP on
  join, creates the virtual display sized to the client, starts the pipeline. Holds
  clamp constants (`MIN/MAX_REFRESH_RATE`, `MIN/MAX_DISPLAY_SCALE`).
- `session.rs` — per-client session state (keyed by client IP), device overrides, OTP
  limiter, disconnect grace. Settings changes apply live via **epoch bumps**
  (`bump_reconfig_epoch`, `bump_kick_epoch`) rather than tearing down the display.
- `webrtc_session.rs` — WebRTC/WHEP negotiation, ICE servers.
- `pipeline.rs` — capture → encode → RTP feed.
- `tls.rs` — self-signed cert generation at runtime (`rcgen`).
- `cloud.rs` — cross-network relay control channel (WebSocket via `tokio-tungstenite`) +
  TURN. Cross-network joins require a configured TURN server (see the `turn-required-cross-network` memory).
- `input/` — remote keyboard/mouse injection, with per-OS impls + a shared protocol.
- `static/` — the client browser page (see top of this file).

### Desktop UI (`src/`)

- Entry: `src/main.tsx` → `src/App.tsx`. Routing uses **`createMemoryRouter`** (in-memory,
  not URL-based) with routes `/` (`bootstrap`), `/dashboard`, `/devices`, `/settings`.
- **Global state** lives in `App.tsx` via `GlobalProviderContext`
  (`src/components/global-provider.tsx`) — OTP, sessionId, QR values, devices, avatar,
  zoom, etc. `App.tsx` also subscribes to backend events (`deviceJoin`/`deviceModify`/
  `deviceRemove`, `networkChange`, `sessionIdChange`, `hostedNetworkNoPassword`) and
  mirrors them into that state.
- **Config persistence is split**: user config is stored frontend-side in
  `tauri-plugin-store` `config.json` via `src/components/config-provider.tsx`
  (`getConfig`/`updateConfig`/`createConfig`, `defaultConfig`, `Device`/`Config` types).
  Relevant values are then pushed to the Rust `AppState` through commands
  (`setSessionCredentials`, `setDeviceOverride`, `setDisconnectGrace`, `setTurnConfig`,
  `setServerPorts`, `setDisableGpuEncode`). When adding a setting, wire **both** sides.
- **i18n** is a custom lightweight implementation in `src/i18n/` (`useSyncExternalStore` +
  `import.meta.glob` over `locales/*.json`) — NOT react-i18next. Use the `useTranslation()`
  hook and add keys to `src/i18n/locales/en.json`.
- UI is shadcn/ui + Radix primitives under `src/components/ui/`; path alias `@/` → `src/`.

### App lifecycle

`lib.rs::run` builds the specta bridge, then the Tauri app. In `setup`: parses CLI
subcommands (driver/hostednetwork paths exit early), enforces a **single-instance lock**
(lockfile in `app_local_data_dir`, prompts on conflict), mounts events, attaches the log
bus, and builds the main window. The per-OS `setup` command (invoked from the frontend)
initializes the virtual display and populates `AppState`.

## Versioning & releases

- The version string is duplicated in **four** places and must be kept in sync:
  `package.json`, `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, and the hardcoded
  `current_version()` in `src-tauri/src/lib.rs`.
- **Release CI** (`.github/workflows/build-release.yml`) triggers on push to `main`
  **only when the commit message footer ends with `rebuild`** (or via manual
  `workflow_dispatch`). It builds Windows (x64) + macOS (Intel + Apple Silicon) via
  `tauri-action`, drafts a GitHub Release, strips the version from asset filenames, then
  marks it latest. (The README's mention of a `release` branch / `build-windows.yml` is
  stale — trust the workflow file.)
- Auto-update: `tauri-plugin-updater` reads `latest.json` from GitHub Releases (pubkey in
  `tauri.conf.json`).

## Native resources & drivers

- Bundled resources (`tauri.conf.json` → `bundle.resources`): the signed Windows Virtual
  Display Driver (`.dll`/`.cat`/`.inf`), its cert (`ScreenExtend.cer`), and `libx264-164.dll`
  (software-encode fallback). `binaries/nefconc` is an `externalBin` used with `certutil`
  to install the driver (requires Administrator, elevated via the `elevated-command` crate).
- macOS uses a `tauri.macos.conf.json` overlay config (passed with `--config` in CI) and
  `Entitlements.plist`.
