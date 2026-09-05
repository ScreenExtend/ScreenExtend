<div align="center">

# ScreenExtend

**Extend your screen. Extend your possibilities. Unlock ultimate productivity.**

A free desktop‑extension solution that turns any device with a web browser into a wireless second monitor without any app to install on the client.

</div>

> [!WARNING]
> Early software, not widely tested yet. Hosts run on Windows and macOS. Linux is scaffolded but doesn't work, and the AMD encoder is a stub (AMD hosts utilize CPU encoding). See [Platform support](#platform-support).

---

## Getting started

Install ScreenExtend on the **host** (the computer that needs more screen). On Windows it sets up its virtual display driver the first time it runs; macOS needs no driver.

1. **Add Device** shows a QR code and URL for every network the host is on, plus an *Anywhere (Internet)* tile for devices somewhere else entirely.
2. Scan or type the URL on the other device, enter the 6-digit code, submit.
3. The host builds a virtual display sized to that device and streams it. Drag a window over.

Every client gets its own display and its own encoder, so several devices can hang off one host and be arranged like any other monitors (the **Display Settings** button opens your OS arrangement pane).

**Edit Device** allows customization of comprehensive settings: scale, pixel ratio, orientation, refresh rate, video scale and quality, remote control, audio, audio output. **Settings** holds the session code, the offline hosted network, disconnect grace period, TURN server, ports, software-encode override, and logs.

## What it does

- **GPU capture and encode.** Desktop capture straight into NVENC / Quick Sync / VideoToolbox, out over WebRTC. Settings changes renegotiate in place instead of tearing down the session.
- **Remote control.** A joined device drives the host's keyboard, mouse, and clipboard. This setting can be turned off.
- **OTP-based security.** A session ID plus a one-time code gate devices from joining, rate-limited per device and globally. Devices that make it in get a trust token and rejoin without retyping. Revoke or ban them from Edit Device.
- **System audio (Windows / macOS).** Off by default, opt-in per device. Low-delay Opus over the same encrypted transport as the video, with a custom-coded jitter buffer. Windows uses WASAPI loopback; macOS uses Core Audio process tap on 14.2+, ScreenCaptureKit on 13+, and a bundled signed virtual output device that ScreenExtend installs on 10.15–12.x.
- **No router needed.** The host can create its own ad-hoc Wi-Fi network to work offline. It also watches network adapters and rebuilds join URLs as required.
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

`ScreenExtend --help` lists the rest: `status`, `session`, `config`, `turn`, `account`, `autostart`, `logs`, `update`, `display-settings`, `stop`.

## Built with

| Layer | Stack |
| --- | --- |
| Shell | [Tauri 2](https://tauri.app) — Rust core, system webview |
| UI | React 18, TypeScript, Vite, Tailwind, shadcn/ui + Radix |
| Rust ↔ TS | [`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta), generated into `src/lib/bindings.ts` |
| Server | [`axum`](https://github.com/tokio-rs/axum) + `rustls`, certs minted at runtime with `rcgen` |
| Streaming | [`webrtc`](https://github.com/webrtc-rs/webrtc), WHEP signaling, H.264 |
| Capture | [`windows-capture`](https://github.com/NiiightmareXD/windows-capture), custom DXGI duplication, ScreenCaptureKit |
| Virtual displays | Signed Windows IDD driver over IPC ([`driver_ipc`](https://github.com/MolotovCherry/virtual-display-rs)); private CoreGraphics APIs on macOS |

## Platform support

The client is a web page, so anything with a reasonably modern browser can be the second monitor. Hosts need **Windows 10 20H1 (build 19041) or later, x86-64 only**, or **macOS 10.15 Catalina or later**.

### Hardware encoder support

ScreenExtend encodes captured displays with the host GPU. The matrix below lists the common hardware video-encoding APIs and reflects the **current** state of each path in ScreenExtend:

| Encoding API | GPU Vendor | Windows | macOS | Linux |
| --- | --- | :---: | :---: | :---: |
| AMF | AMD | 🟡 | | ➖ |
| NVENC | NVIDIA | ✅ | | ➖ |
| Quick Sync | Intel | ✅ | | ➖ |
| Media Foundation | Qualcomm | ➖ | | |
| Video Toolbox | Apple | | ✅ | |
| | Intel | | ✅ | |
| Software | Any | ✅ | | ➖ |

✅ Supported &nbsp;·&nbsp; 🟡 In progress &nbsp;·&nbsp; ➖ Not supported

## Building from source

### Prerequisites

- **[Rust](https://rustup.rs/)** (stable toolchain)
- **[Node.js](https://nodejs.org/)** (LTS) and **[pnpm](https://pnpm.io/)**
- **Tauri 2 system dependencies** (see the [Tauri prerequisites guide](https://tauri.app/start/prerequisites/))

### Setup & run (development)

```sh
# Install frontend dependencies
pnpm install

# Approve native build scripts
pnpm approve-builds --all

# Run the app in dev mode
pnpm tauri dev
```

Running in dev mode also regenerates the typed TS bindings (`src/lib/bindings.ts`) from the Rust command/event definitions.

### Production build

```sh
pnpm tauri build
```

Installers and the executable are emitted under `src-tauri/target/release/bundle/`.

### Releases

Every PR and push to `main` runs `.github/workflows/ci.yml` (build, lint, `cargo fmt`/`clippy`/`test`, dependency audits). Pushing an `app-v*` tag builds Windows x64 and macOS Intel + Apple Silicon and drafts a release. Prebuilt installers live on the [Releases page](https://github.com/ScreenExtend/ScreenExtend/releases).

## Contributing

Issues and pull requests are welcome, and get looked at roughly every three days.

- **Bugs:** open an [issue](https://github.com/ScreenExtend/ScreenExtend/issues) with host OS/GPU, client device and browser, and how to reproduce it.
- **Code:** PR against `main`. Keep it focused and match the surrounding style.
- **Unsupported hardware:** if your host isn't covered and you'd like to help test, mail [support@screenextend.app](mailto:support@screenextend.app) with your device details.

## License

[AGPL-3.0](LICENSE). TLDR: Reuse it, but keep the copyright notice and license text, keep the source public and available to users, and mark what you changed.

## Contact
General inquiries: [hi@screenextend.app](mailto:hi@screenextend.app)
Website: [screenextend.app](https://screenextend.app/)
