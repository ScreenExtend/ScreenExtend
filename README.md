<div align="center">

# ScreenExtend

**Extend your screen. Extend your possibilities. Unlock ultimate productivity.**

A free desktop‑extension solution that turns any device with a web browser into a wireless second monitor without any app to install on the client.

</div>

> [!WARNING]
> Early software, not widely tested yet. Hosts run on Windows and macOS. Linux is scaffolded but doesn't work, and the AMD AMF encoder is written and wired up but has never encoded a frame — nobody on the project has an AMD GPU, so an AMD host may still fall back to CPU encoding. See [Platform support](#platform-support).

---

## Getting started

Install ScreenExtend on the **host** (the computer that needs more screen). On Windows it sets up its virtual display driver the first time it runs; macOS needs no driver.

1. **Add Device** shows a QR code and URL for every network the host is on, plus an *Anywhere (Internet)* tile for devices somewhere else entirely.
2. Scan or type the URL on the other device, enter the 6-digit code, submit.
3. The host builds a virtual display sized to that device and streams it. Drag a window over.

Every client gets its own display and its own encoder, so several devices can hang off one host and be arranged like any other monitors (the **Display Settings** button opens your OS arrangement pane).

**Edit Device** allows customization of comprehensive settings: scale, pixel ratio, orientation, refresh rate, video scale and quality, remote control, audio, audio output. **Settings** holds the session code, the offline hosted network, disconnect grace period, STUN and TURN servers, ports, software-encode override, the log terminal, and a `config.json` editor for everything the panels do not surface.

## What it does

- **GPU capture and encode.** Desktop capture straight into NVENC / Quick Sync / VideoToolbox, out over WebRTC. A settings change keeps the virtual display — the desktop windows stay where they are — and renegotiates the stream around it.
- **Remote control.** A joined device drives the host's keyboard, mouse, and clipboard. This setting can be turned off.
- **OTP-based security.** A session ID plus a one-time code gate devices from joining, rate-limited per device and globally. Devices that make it in get a trust token and rejoin without retyping. Revoke or ban them from Edit Device.
- **System audio (Windows / macOS).** Off by default, opt-in per device. Low-delay Opus over the same encrypted transport as the video, with a custom-coded jitter buffer. Windows uses WASAPI loopback; macOS uses Core Audio process tap on 14.2+ and ScreenCaptureKit on 13+. Below 13 macOS has no system-audio API at all, so ScreenExtend carries its own virtual output device for 10.15–12.x. That tier works end to end — it has been measured capturing the system mix on 10.15 — but it has never been signed or notarized, so today it only loads on a Mac with SIP disabled.
- **No router needed.** The host can bring up its own Wi-Fi access point to work offline (Windows hosted network; CoreWLAN HostAP mode on macOS). It also watches network adapters and rebuilds join URLs as required.
- **Encrypted media, opt-in encrypted signaling.** Audio and video always ride DTLS-SRTP. The LAN join page is plain HTTP by default so a QR scan doesn't hit a certificate warning; the page then offers the HTTPS endpoint (self-signed, generated at runtime), which is both private and faster.

## How it works

```
   Client browser                       Host (ScreenExtend desktop app)
 ┌─────────────────┐    WHEP/HTTPS   ┌───────────────────────────────────┐
 │  open URL /     │                 │  axum server (per network IP)     │
 │  scan QR + OTP  │                 │   • validates session ID + OTP    │
 │                 │                 │   • creates a virtual display     │
 │  <video> via    │     WebRTC      │   • captures + GPU-encodes it     │
 │  WebCodecs      │     (H.264)     │   • streams via WebRTC            │
 └─────────────────┘                 └───────────────────────────────────┘
```

### Joining over a cable

A wired client is not a separate mode — it is just a different network. An Android phone with
**Settings → Hotspot & tethering → USB tethering** turned on presents an ordinary IP interface to
the host, so ScreenExtend serves it on that subnet like any other adapter: open the join URL
printed for the tether address and it works with no extra setup. That removes Wi-Fi contention
from the path entirely, which is the single biggest source of jitter on a busy network. iOS has no
USB tethering, so this is Android-only.

The host log names what the connection is actually riding
(`session link: local candidate … is on a wired adapter`), so it is possible to tell a tethered
join from a Wi-Fi one at a glance.

## Command line

The shipped binary works headlessly:

```sh
ScreenExtend serve --no-cloud -v   # run the host with no window until Ctrl+C
ScreenExtend qr   # print join URLs and QR codes
ScreenExtend devices set 192.168.1.42 --refresh-rate 120 --audio on
ScreenExtend network start MyHotspot hunter2
ScreenExtend doctor   # check system requirements and permissions
ScreenExtend drivers install   # virtual display driver (elevated)
```

`ScreenExtend --help` lists the rest: `status`, `session`, `config`, `turn`, `stun`, `account`, `autostart`, `logs`, `update`, `display-settings`, `stop`.

## Built with

| Layer | Stack |
| --- | --- |
| Shell | [Tauri 2](https://tauri.app) — Rust core, system webview |
| UI | React 18, TypeScript, Vite, Tailwind, shadcn/ui + Radix |
| Rust ↔ TS | [`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta), generated into `src/lib/bindings.ts` |
| Server | [`axum`](https://github.com/tokio-rs/axum) + `rustls`, certs minted at runtime with `rcgen` |
| Streaming | [`webrtc`](https://github.com/webrtc-rs/webrtc), WHEP signaling, H.264 |
| Capture | [`windows-capture`](https://github.com/NiiightmareXD/windows-capture), custom DXGI duplication, ScreenCaptureKit, CGDisplayStream below macOS 12.3 |
| Virtual displays | Signed Windows IDD driver over IPC ([`driver_ipc`](https://github.com/MolotovCherry/virtual-display-rs)); private CoreGraphics APIs on macOS |

## Platform support

The client is a web page, so anything with a reasonably modern browser can be the second monitor. Hosts need **Windows 10 20H1 (build 19041) or later, x86-64 only**, or **macOS 10.15 Catalina or later**.

Those are the floors the app is built and tested against. Individual pieces have their own, higher ones — system audio needs macOS 13 for a supported path, ScreenCaptureKit capture needs 12.3 — and ScreenExtend picks what a machine actually has by probing for it rather than by reading a version number, so a host below one of those floors loses that feature rather than failing to start.

### Hardware encoder support

ScreenExtend encodes captured displays with the host GPU. The matrix below lists the common hardware video-encoding APIs and reflects the **current** state of each path in ScreenExtend:

| Encoding API | GPU Vendor | Windows | macOS | Linux |
| --- | --- | :---: | :---: | :---: |
| AMF | AMD | 🟡 | n/a | ➖ |
| NVENC | NVIDIA | ✅ | n/a | ➖ |
| Quick Sync | Intel | ✅ | n/a | ➖ |
| Media Foundation | Qualcomm | ➖ | n/a | n/a |
| Video Toolbox | Apple | n/a | ✅ | n/a |
| Video Toolbox | Intel | n/a | ✅ | n/a |
| Software | Any | ✅ | ✅ | ➖ |

✅ Supported &nbsp;·&nbsp; 🟡 Written and wired up, never run on real hardware &nbsp;·&nbsp; ➖ Not supported &nbsp;·&nbsp; n/a That API does not exist on that OS

Software encode is libx264 on Windows and VideoToolbox's own software encoder on macOS. It is what turning off **GPU Video Encoding** in Settings selects, and it is also the automatic fallback when no hardware encoder can be opened.

## Building from source

### Prerequisites

- **[Rust](https://rustup.rs/)** (stable toolchain)
- **[Node.js](https://nodejs.org/)** (LTS) and **[pnpm](https://pnpm.io/)**
- **Tauri 2 system dependencies** (see the [Tauri prerequisites guide](https://tauri.app/start/prerequisites/))

### Setup & run (development)

```sh
# Install frontend dependencies
pnpm install

# Run the app in dev mode
pnpm tauri dev
```

Running in dev mode also regenerates the typed TS bindings (`src/lib/bindings.ts`) from the Rust command/event definitions.

### Production build

```sh
pnpm tauri build
```

Installers and the executable are emitted under `target/release/bundle/` inside your cargo target directory — `src-tauri/target/release/bundle/` unless `CARGO_TARGET_DIR` says otherwise.

### Development

`docs/dev-workflow.md` has the working conventions: the gates to run before a commit (`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `pnpm lint`/`pnpm build`/`node scripts/check-legacy-webkit.mjs` when the frontend changed), and the invariants that are easy to break silently — the macOS 10.15 link-time floor, the Safari 13.1 floor in the desktop UI, and the rule that the client page has no build step and no dependencies.

`CLAUDE.md` is the map of the codebase. The rest of `docs/` is the engineering record: `INVENTORY.md` (what every component is), `CAPABILITY-TIERS.md` (every runtime-probed tier and how it is probed), `TELEMETRY.md` and `INPUT-PROTOCOL.md` (the host↔client wire formats), `OPTIMIZATIONS.md` (every change attempted, kept or reverted, with its measurements), and `manual-tests/` (the checks neither development machine can run).

### Releases

Every push and pull request runs `.github/workflows/ci.yml`: eslint, the frontend build and four static checks on Linux (the Safari 13.1 floor of the built UI, the generated Rust↔TS bindings against the macro lists that produce them, the four copies of the version string, and the client page's wire-format helpers); `cargo fmt`/`clippy -D warnings`/`test` on Windows and on macOS 13, 14 and 15; a runtime smoke on those macOS runners for the capture and audio tiers the dev machines are too old to reach; a 10.15 dyld-floor audit of the release binary; verification of the vendored blob checksums; and a Windows encoder perf gate against `ci/perf-baseline.json`. Pushing an `app-v*` tag builds Windows x64 and macOS Intel + Apple Silicon and drafts a release. Prebuilt installers live on the [Releases page](https://github.com/ScreenExtend/ScreenExtend/releases).

## Contributing

Issues and pull requests are welcome. This is a small project, so there is no promised response time.

- **Bugs:** open an [issue](https://github.com/ScreenExtend/ScreenExtend/issues) with host OS/GPU, client device and browser, and how to reproduce it.
- **Code:** PR against `main`. Keep it focused and match the surrounding style.
- **Unsupported hardware:** if your host isn't covered and you'd like to help test, mail [support@screenextend.app](mailto:support@screenextend.app) with your device details.

## License

[AGPL-3.0](LICENSE). TLDR: Reuse it, but keep the copyright notice and license text, keep the source public and available to users, and mark what you changed.

## Contact
General inquiries: [hi@screenextend.app](mailto:hi@screenextend.app)
Website: [screenextend.app](https://screenextend.app/)
