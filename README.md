<div align="center">

# ScreenExtend

**Extend your screen. Extend your possibilities. Unlock ultimate productivity.**

A free desktop‑extension solution that turns any device with a web browser into a wireless second monitorm without any app to install on the client.

</div>

> [!WARNING]
> Current builds support **Windows hosts with an NVIDIA or Intel GPU and Mac hosts** (NVENC / QSV), and are not yet widely tested. Use at your own risk. Linux host support is scaffolded but not yet functional, and the AMD encoder is stubbed out (see [Platform support](#platform-support)).

---

## How to use

Run ScreenExtend on the **host** (the PC you want to extend). It launches straight into the app, installing its virtual display driver on first run if needed. From there, everything happens across three screens:

- **Add Device** (the home screen). Shows a QR code and URL for each network the host is on, plus an "Anywhere (Internet)" tile for joining across different networks. On your client device (phone, tablet, laptop, etc), scan the QR or open the URL in a browser, enter the 6-digit session OTP, and submit. The host spins up a virtual display for it and the client becomes a fullscreen extended monitor.
- **Edit Device.** Lists every connected device with its live settings. Open a device to adjust its resolution scale, orientation, refresh rate, and video scale/quality, or to remove it. The **Display Settings** button opens your OS display settings so you can rearrange where each extended screen sits.
- **Settings.** View and regenerate the **session OTP**, start an offline **hosted network** so devices can join with no router, set the **disconnect timeout** (how long a display is kept when a device drops), configure a **TURN server** for cross-network connections, change your display name, and read the live logs.

## Overview

ScreenExtend runs as a desktop app on the **host** machine (the computer whose screen you want to extend). The host advertises a session over your local network. Any **client** (phone, tablet, laptop, spare PC, etc) joins by opening a URL or scanning a QR code in its browser. The host spins up a real virtual display for that client and streams it over WebRTC, so it acts like an actual extended monitor: drag windows onto it, move your cursor across it, work on the extra space.

Each client gets its own dedicated virtual display and video pipeline, so multiple devices can join the same host and each acts as an independent monitor.

## Features

- **Hardware-accelerated streaming.** Desktop capture is encoded with the GPU and delivered over WebRTC for low latency.
- **Per-device settings.** Adjust resolution scale, orientation, refresh rate, and video scale/quality independently for each connected device.
- **Password-protected sessions.** A session ID plus a one-time password (OTP) gate new join requests, with per-device and global rate limiting on wrong guesses. **Exception:** once a device joins successfully it is issued a trust token and remembered, so known devices auto-rejoin **without** re-entering the code. Revoke or ban a device on the Devices screen.
- **Full remote control of the host.** Joining a session grants that device **keyboard, mouse, and clipboard control** of the host — it is a second monitor you can also drive. Turn this off per device with the remote-control toggle (Devices screen, or `ScreenExtend devices set <ip> --control off`).
- **System audio streaming (Windows).** Optionally stream the host's system audio to a device alongside the video, captured with low-latency WASAPI loopback, encoded as low-delay Opus, and delivered over the same DTLS transport with a custom client-side jitter buffer (bypassing the browser's NetEQ, matching the video path). Off by default; enable it per device (Devices screen, or `ScreenExtend devices set <ip> --audio on`). **Privacy:** this captures *everything the host is playing* — including other apps, calls, and notifications — for that device, which is exactly why it is opt-in per device and defaults to off. macOS/Linux hosts show the toggle disabled (not yet supported).
- **Offline / no-internet mode.** The host can run its own ad-hoc Wi-Fi hosted network so devices can connect with no central router.
- **Auto network discovery.** The host listens on every active network adapter and rebuilds join URLs/QR codes as network changes occur.
- **Encrypted media; opt-in encrypted signaling.** The audio/video stream is always encrypted (WebRTC DTLS-SRTP). The LAN join page is served over plaintext **HTTP** by default so the QR scan just works (no certificate warning); on load it offers to switch to the **HTTPS** endpoint — backed by a self-signed certificate generated at runtime — so the join code is sent encrypted. Cross-network ("Anywhere") joins go through the cloud relay over HTTPS.

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

1. On launch the host generates a session ID and an OTP, and starts a small HTTPS server bound to each network adapter.
2. The desktop UI shows a QR code / URL per network address. The client opens `http(s)://<host-ip>:<port>/?id=<sessionId>` and submits the OTP plus its own screen metrics. (The host serves both HTTP and HTTPS, with the secure endpoint supporting faster decoding via WebCodecs.)
3. The host validates the credentials, creates a **virtual display** sized to the client via a signed Windows display driver, captures that display with Windows Graphics Capture (older Windows builds that don't support WGC fall back to DXGI Desktop Duplication), encodes it with **NVENC/QSV**, and negotiates a **WebRTC** connection using **WHEP**.
4. The client decodes the H.264 stream (via WebCodecs, with a fallback transform worker) and renders it fullscreen, acting as an extended monitor.
5. Editing a device's settings triggers automatic changes and renegotiation, without destroying and recreating the display.

## Technologies & architecture

| Layer | Stack |
| --- | --- |
| **Desktop shell** | [Tauri 2](https://tauri.app) (Rust core + system webview) |
| **Frontend** | React 18, TypeScript, Vite, Tailwind CSS, shadcn/ui + Radix UI, React Router |
| **Rust + TS bridge** | [`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta) - typed commands/events, generated into `src/lib/bindings.ts` |
| **Web/signaling server** | [`axum`](https://github.com/tokio-rs/axum) + `axum-server` over TLS (`rustls`, self-signed via `rcgen`) |
| **Streaming** | [`webrtc`](https://github.com/webrtc-rs/webrtc) with WHEP signaling; H.264 |
| **Capture** | Windows Graphics Capture ([`windows-capture`](https://github.com/NiiightmareXD/windows-capture)), with a custom DXGI Desktop Duplication engine (GPU cursor compositing) as fallback on Windows builds where WGC can't open virtual displays |
| **Encoding** | NVIDIA NVENC or Intel QSV FFI bindings (AMD scaffolded) |
| **Virtual displays** | Bundled signed Windows Virtual Display Driver (IDD), driven over IPC ([`driver_ipc`](https://github.com/MolotovCherry/virtual-display-rs)) and installed with `nefconc` + `certutil` |
| **Networking** | Windows hosted network (`netsh wlan`) for offline mode, live network-adapter watching |

## Platform support

The client is just a web page, so anything with a reasonably modern browser (WebRTC + WebCodecs) can be a second monitor. The host runs on **Windows** (NVIDIA NVENC / Intel Quick Sync, with a libx264 software fallback) and **macOS** (VideoToolbox). Linux is scaffolded but not yet functional.

**Minimum host OS:** Windows 10 version 2004 (build 19041) or later, including Windows 11. Only 64-bit (x86-64) machines are supported. macOS Catalina 10.15+.

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

### Installing the virtual display driver

Creating extended displays requires the bundled signed virtual display driver. The app installs it for you on first use, but it can also be triggered from the CLI (this trusts the bundled certificate and creates the display device node, and requires Administrator):

```sh
ScreenExtend.exe installdrivers   # install driver + certificate
ScreenExtend.exe removedrivers    # uninstall driver + certificate
```

### Releases

Pushing a version tag matching `app-v*` (or running the workflow manually via **Run workflow**) triggers `.github/workflows/build-release.yml`, which builds Windows (x64) and macOS (Intel + Apple Silicon) via `tauri-action` and drafts a GitHub Release. Every pull request and push to `main` is checked by `.github/workflows/ci.yml` (build, lint, `cargo fmt`/`clippy`/`test`, and dependency audits). Prebuilt installers are available on the [Releases page](https://github.com/ScreenExtend/ScreenExtend/releases).

## Contributing

Contributions are welcome.

- **Bugs & feature requests:** open an [issue](https://github.com/ScreenExtend/ScreenExtend/issues) with as much detail as you can (host OS/GPU, client device/browser, and steps to reproduce).
- **Code:** open a pull request against `main`. Please keep changes focused and match the style of the surrounding code.
- **Unsupported platforms:** ScreenExtend currently runs on a limited set of hosts. If yours isn't supported and you'd like to help test, email [support@screenextend.app](mailto:support@screenextend.app) with your device info.

Pull requests and issues are reviewed roughly every two weeks.

## License

ScreenExtend is licensed under the **GNU Affero General Public License v3 (AGPL-3.0)**. Any code from ScreenExtend incorporated into other projects must include the original copyright notice and license text, all source must remain public and accessible to users, and any changes must be clearly indicated. See [LICENSE](LICENSE) for the full text.

## Contact
General inquiries: [hi@screenextend.app](mailto:hi@screenextend.app)
Website: [screenextend.app](https://screenextend.app/)
