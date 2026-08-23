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

Linting is wired up: `pnpm lint` runs ESLint 9 (flat config in `eslint.config.js`, scoped to
`src/`) with `react-hooks` (rules-of-hooks as an error), `react-refresh`, and
`typescript-eslint`. It currently passes with a handful of non-blocking warnings. TypeScript
strictness is additionally enforced via `tsc` in `pnpm build`. CI runs both.

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
`hosted_network`, `virtual_display`, `streamer`, `compatibility`, `device_reporter`, `audio`).
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
  **Device trust** (auto-join approval + bans) is keyed on a per-device **token**
  (`mint_device_token`, `Shared{Approved,Banned}Devices`, `is_device_*`), *not* the IP —
  the host mints the token on a successful OTP join and returns it via the `X-Device-Token`
  header; the client stores it and presents it on rejoin. The IP is only a display hint. The
  `OtpLimiter` also has a global cross-key brute-force guard (defeats the cloud relay's
  rotating-`client_id` bypass).
- `webrtc_session.rs` — WebRTC/WHEP negotiation, ICE servers. The video track is a raw
  `TrackLocalStaticRTP` (not `TrackLocalStaticSample`): we packetize H.264 ourselves so each
  frame's RTP timestamp carries the shared host clock (`host_ns_to_rtp90k`), which the client
  inverts for A/V sync (§6.5). NACK/RTX (interceptors) and loss-based BWE (`getStats`) are
  unaffected.
- `pipeline.rs` — capture → encode → RTP feed.
- `tls.rs` — self-signed cert generation at runtime (`rcgen`).
- `cloud.rs` — cross-network relay control channel (WebSocket via `tokio-tungstenite`) +
  TURN. Cross-network joins require a configured TURN server (see the `turn-required-cross-network` memory).
- `input/` — remote keyboard/mouse injection, with per-OS impls + a shared protocol.
- `audio/` — cross-platform system-audio transport glue: `AudioPacket`, host-side
  `AudioDiagnostics`, the shared host timebase (`host_now_ns` / `host_instant_to_ns` /
  `host_ns_to_rtp90k` — **the one clock both audio `capture_ns` and the video RTP timestamp ride
  for A/V sync**), the reference-counted `AudioHub` (one host-wide capture fanned out to N
  sessions via `tokio::sync::broadcast`, started on the first audio-enabled subscriber and stopped
  when the last leaves), and `protocol.rs` (the 13-byte DataChannel header: seq u32, capture ns
  u64, flags u8 + raw Opus).
- `static/` — the client browser page (see top of this file). Adds `audio.js` (WebCodecs
  `AudioDecoder` → ring → worklet, plus the NetEQ track fallback) and `audio-worklet.js` (our
  jitter buffer). A/V sync (§6.5) lives here: `transform-worker.js` recovers each video frame's
  host-capture time from its RTP timestamp and reports the display lag; `audio.js` commands the
  worklet a buffer depth so audio plays in step with the picture, and the worklet corrects drift
  only at silence boundaries. Measured offset via `SEAudio.getSyncInfo()` (no on-screen HUD).

### System audio (`windows_utils/audio/`, Windows only)

Per-device system-audio capture + Opus encode. Design decisions are measured, not assumed —
see `AUDIO_NOTES.md` (from the `examples/audio_spike.rs` throwaway spike):

- `loopback.rs` — WASAPI **legacy `IAudioClient::Initialize` loopback** path (the IAudioClient3
  low-latency path rejects the loopback flag). Runs on a **dedicated OS thread** with COM MTA +
  MMCSS "Pro Audio", event-driven, re-acquiring on default-device change / `AUDCLNT_E_DEVICE_INVALIDATED`.
- `silence.rs` — a **silent render companion** is mandatory: it is the clock source that makes
  the loopback event fire and keeps packets flowing while the host is idle (measured: 0/20 event
  signals without it, 20/20 with it). Started lazily, stopped with the capture.
- `format.rs` — mix-format negotiation; 48 kHz float32 stereo is the zero-copy fast path,
  everything else is `AUTOCONVERTPCM`'d / downmixed (ITU BS.775) / int→float.
- `opus_sys.rs` + `encoder.rs` — hand-written libopus FFI (mirrors `x264_sys.rs`), `libopus.dll`
  loaded via `libloading` and bundled in `resources/` (provenance in `resources/PROVENANCE.md`);
  `RESTRICTED_LOWDELAY` (CELT-only), 5 ms frames.
- `device.rs` — `IMMNotificationClient`; its callback runs on a COM thread we don't own, so it
  only posts to the capture thread over `crossbeam-channel` (never blocks, never takes the
  capture lock).

**Capture-thread convention:** the audio capture thread is a real-time OS thread (MMCSS), never
a tokio task. It never allocates or locks in the drain/encode path beyond the reused
accumulator + the per-packet `Bytes` copy. It talks to the async world over `crossbeam-channel`
(→ the `AudioHub` bridge → broadcast), the way the video pipeline does. macOS/Linux provide
compiling stubs (`{macos,linux}_utils/audio.rs`) that return "unsupported".

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
- **CI** (`.github/workflows/ci.yml`) runs on every pull request and push to `main`:
  frontend build + `pnpm lint`, a Windows + macOS Rust matrix (`cargo fmt --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo test`), and `pnpm audit`/`cargo audit`
  plus a vendored-binary checksum check.
- **Release CI** (`.github/workflows/build-release.yml`) triggers on pushing a tag matching
  `app-v*` (or manual `workflow_dispatch`) — **not** on push to `main` (the old "commit
  footer ends with `rebuild`" gate was replaced so a typo can't silently skip a release). It
  builds Windows (x64) + macOS (Intel + Apple Silicon) via `tauri-action`, drafts a GitHub
  Release, strips the version from asset filenames, then marks it latest.
- Auto-update: `tauri-plugin-updater` reads `latest.json` from GitHub Releases (pubkey in
  `tauri.conf.json`).

## Native resources & drivers

- Bundled resources (`tauri.conf.json` → `bundle.resources`): the signed Windows Virtual
  Display Driver (`.dll`/`.cat`/`.inf`), its cert (`ScreenExtend.cer`), `libx264-164.dll`
  (software-encode fallback), and `libopus.dll` (system-audio Opus encode, loaded via
  `libloading`; provenance + SHA-256 in `resources/PROVENANCE.md` / `SHA256SUMS`). `binaries/nefconc` is an `externalBin` used with `certutil`
  to install the driver (requires Administrator, elevated via the `elevated-command` crate).
- macOS uses a `tauri.macos.conf.json` overlay config (passed with `--config` in CI) and
  `Entitlements.plist`.
