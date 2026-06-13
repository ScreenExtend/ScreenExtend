<div align="center">

# ScreenExtend

**Extend your screen. Extend your possibilities. Unlock ultimate productivity.**

A free desktop‑extension solution that turns any device with a web browser into a wireless second monitorm without any app to install on the client.

</div>

> [!WARNING]
> Current builds support **Windows hosts with an NVIDIA or Intel GPU** (NVENC / QSV), and are not yet widely tested. Use at your own risk. macOS and Linux host support is scaffolded but not yet functional, and the AMD encoder is stubbed out (see [Platform support](#platform-support)).

---

## Overview

ScreenExtend runs as a desktop app on the **host** machine (the computer whose screen you want to extend). The host advertises a session over your local network. Any **client** (phone, tablet, laptop, spare PC, etc) joins simply by opening a URL or scanning a QR code in its browser. The host then spins up a real virtual display for that client and streams it over WebRTC, so the client behaves like a genuine extended monitor: drag windows onto it, move your cursor across it, and work on the extra space.

Each client gets its own dedicated virtual display and video pipeline, so multiple devices can join the same host and each acts as an independent monitor.

## Features

- **Hardware‑accelerated streaming.** Desktop capture is encoded with the GPU and delivered over WebRTC for low latency.
- **Per‑device settings.** Adjust resolution scale, orientation, refresh rate, and video scale/quality independently for each connected device.
- **Password‑protected sessions.** A session ID plus a one‑time password (OTP) restrict any new join requests.
- **Offline / no‑internet mode.** The host can host its own Ad-hoc Wifi hosted network so devices can connect with no central router.
- **Auto network discovery.** The host listens on every active network adapter and rebuilds join URLs/QR codes as network changes occur.
- **Encrypted transport.** Streaming and signaling run over HTTPS/WebRTC with a self‑signed certificate generated at runtime.

## How it works

```
   Client browser                    Host (ScreenExtend desktop app)
 ┌─────────────────┐    WHEP/HTTPS   ┌───────────────────────────────────┐
 │  open URL /     │                 │  axum server (per network IP)     │
 │  scan QR + OTP  │                 │   • validates session ID + OTP    │
 │                 │                 │   • creates a virtual display     │
 │  <video> via    │     WebRTC      │   • captures + NVENC‑encodes it   │
 │  WebCodecs      │     (H.264)     │   • streams via WebRTC            │
 └─────────────────┘                 └───────────────────────────────────┘
```

1. On launch the host generates a session ID and an OTP, and starts a small HTTPS server bound to each network adapter.
2. The desktop UI shows a QR code / URL per network address. The client opens `http(s)://<host-ip>:<port>/?id=<sessionId>` and submits the OTP plus its own screen metrics. (the host serves both HTTP and HTTPS, with the secure endpoint supporting faster decoding using WebCodecs)
3. The host validates the credentials, creates a **virtual display** sized to the client via a signed Windows display driver, captures that display with Windows Graphics Capture (older Windows builds that don't support WGC use DXGI Desktop Duplication), encodes it with **NVENC/QSV**, and negotiates a **WebRTC** connection using **WHEP**.
4. The client decodes the H.264 stream (via WebCodecs, with a fallback transform worker) and renders it fullscreen, acting as an extended monitor.
5. Editing a device's settings results in automatic changes and re-negotiation, without destroying and recreating the display.

## Technologies & architecture

| Layer | Stack |
| --- | --- |
| **Desktop shell** | [Tauri 2](https://tauri.app) (Rust core + system webview) |
| **Frontend** | React 18, TypeScript, Vite, Tailwind CSS, shadcn/ui + Radix UI, React Router |
| **Rust + TS bridge** | [`tauri-specta`](https://github.com/oscartbeaumont/tauri-specta) - typed commands/events, generated into `src/lib/bindings.ts` |
| **Web/signaling server** | [`axum`](https://github.com/tokio-rs/axum) + `axum-server` over TLS (`rustls`, self‑signed via `rcgen`) |
| **Streaming** | [`webrtc`](https://github.com/webrtc-rs/webrtc) with WHEP signaling; H.264 |
| **Capture** | Windows Graphics Capture ([`windows-capture`](https://github.com/NiiightmareXD/windows-capture)), with a custom DXGI Desktop Duplication engine (GPU cursor compositing) as fallback on Windows builds where WGC cannot open virtual displays |
| **Encoding** | NVIDIA NVENC or Intel QSV FFI bindings (AMD scaffolded) |
| **Virtual displays** | Bundled signed Windows Virtual Display Driver (IDD), driven over IPC ([`driver_ipc`](https://github.com/MolotovCherry/virtual-display-rs)) and installed with `nefconc` + `certutil` |
| **Networking** | Windows hosted network (`netsh wlan`) for offline mode, live network‑adapter watching |

### Repository layout

```
.
├── src/                      # React + TypeScript frontend (the host's control UI)
│   ├── pages/                #   dashboard, devices, settings, bootstrap
│   ├── components/           #   shadcn/ui components, providers, device details
│   └── lib/bindings.ts       #   auto-generated Tauri command/event bindings
├── src-tauri/                # Rust core
│   ├── src/streamer/         #   axum server, WHEP/WebRTC sessions, pipeline, TLS, config
│   ├── src/{windows,macos,linux}_utils/  # platform-specific implementations (WIP)
│   ├── src/streamer/static/  #   browser client served to joining devices (HTML/CSS/JS)
│   ├── resources/            #   signed virtual display driver + certificate
│   └── binaries/             #   nefconc sidecar (device-node/driver installer)
└── .github/workflows/        # Windows release build (CI)
```

## Platform support

The client is just a web page, so anything with a reasonably modern browser (WebRTC + WebCodecs) can be a second monitor. The host is currently Windows + NVIDIA/Intel only.

**Minimum host OS:** Windows 10 version 2004 (build 19041) or later, including Windows 11. Only 64‑bit (x86‑64) machines are supported.

### Hardware encoder support

ScreenExtend encodes captured displays with the host GPU. The matrix below lists the common hardware video‑encoding APIs and reflects the **current** state of each path in ScreenExtend:

| Encoding API | GPU Vendor | Linux | macOS | Windows |
| --- | --- | :---: | :---: | :---: |
| AMF | AMD | ➖ |   | 🟡 |
| NVENC | NVIDIA | ➖ |   | ✅ |
| Quick Sync | Intel | ➖ |   | ✅ |
| Media Foundation | Qualcomm |   |   | ➖ |
| Video Toolbox | Apple |   | ➖ |   |
| | Intel |   | ➖ |   |
| Software | Any | ➖ | ➖ | ➖ |

✅ Supported &nbsp;·&nbsp; 🟡 In progress &nbsp;·&nbsp; ➖ Not supported

## Building from source

### Prerequisites

- **[Rust](https://rustup.rs/)** (stable toolchain)
- **[Node.js](https://nodejs.org/)** (LTS) and **[pnpm](https://pnpm.io/)**
- **Tauri 2 system dependencies** — see the [Tauri prerequisites guide](https://tauri.app/start/prerequisites/). On Windows this means the MSVC C++ build tools and WebView2.

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

Pushes to the `release` branch trigger `.github/workflows/build-windows.yml`, which builds the 64‑bit Windows target via `tauri-action` and publishes a GitHub Release. Prebuilt installers are available on the [Releases page](https://github.com/ScreenExtend/ScreenExtend/releases).

## Contributing

Contributions are welcome!

- **Bugs & feature requests:** open an [issue](https://github.com/ScreenExtend/ScreenExtend/issues) with as much detail as you can (host OS/GPU, client device/browser, and steps to reproduce).
- **Code:** open a pull request against `main`. Please keep changes focused and match the style of the surrounding code.
- **Unsupported platforms:** ScreenExtend currently runs on a limited set of hosts. If yours is unsupported and you'd like to help test, email [support@screenextend.app](mailto:support@screenextend.app) with your device information.

Pull requests and issues are reviewed on a roughly biweekly basis.

## License

ScreenExtend is licensed under the **GNU Affero General Public License v3 (AGPL‑3.0)**. Any code from ScreenExtend incorporated into other projects must include the original copyright notice and license text, all source must remain public and accessible to users, and any changes must be clearly indicated. See [LICENSE](LICENSE) for the full text.

## Contact
General inquiries: [hi@screenextend.app](mailto:hi@screenextend.app)  
Website: [screenextend.app](https://screenextend.app/)
