# ScreenExtend Devlog: Building the Rust Engine Under a Wireless Second Monitor

*Status: v0.2.3 (Windows host (NVIDIA / Intel) and macOS working*

---

## TL;DR

ScreenExtend turns any device with a browser into a real, wireless extended monitor for a Windows host without any client app, no cables. This started as a need back in my middle school years, when my brother would attend speech and debate tournaments. He wanted to use his iPad during rounds to take notes alongside his computer to research, but had troubles sharing documents quickly between rounds. I was determined to program a solution that bridged the gap between devices.

To use ScreenExtend, you open the desktop app, a phone/tablet/laptop scans a QR code, and the host spins up an actual virtual display for that device and streams it over WebRTC with hardware H.264. Each client gets its own display and its own pipeline.

The React/TypeScript control UI existed before this chapter. **Everything described below (the entire Rust core, the Rust to TS bridge, the typed events system, the capture/encode pipeline, the per-vendor GPU paths, the browser-side WebCodecs client, the session model, and the cloud relay) was built from scratch.** The goal was always to **minimize glass-to-glass latency.** Every architectural decision below was made latency-first, quality-second.

---

## 0. The starting point

Before any of this, the repo was a Tauri + React shell: a control panel with pages (dashboard, devices, settings, bootstrap), shadcn/ui + Radix components, a theme provider, and a config provider. I didn't have much experience building in Rust so just built the frontend UI shell, without any working functionality.

---

## 1. Research: "how do you get a desktop into a browser, fast?"

The first week was reading, not writing. I was looking at various options to stream the actual desktop:

| Option | Why it lost |
| --- | --- |
| **MJPEG / screenshot polling** | Trivial, but every frame is a keyframe. Bandwidth explodes, latency is terrible, and there's no motion compression. |
| **HLS / DASH** | Segment-based; even "low-latency HLS" is seconds, not frames. |
| **Raw H.264 over a WebSocket** | Possible, but then I'd have to code my own jitter buffering system, packet loss recovery, congestion control, and NAT traversal by hand. I may consider doing this in a future release. |
| **WebRTC** | Purpose-built for real-time media and runs in most modern browsers. |

There were three more key decisions to be made:

- **Signaling: WHEP.** Rather than invent a signaling protocol or drag in websockets, I used **WHEP** (WebRTC-HTTP Egress Protocol): the client POSTs its SDP offer to `/whep`, the host answers in the HTTP response.

- **Codec: H.264.** Not VP9, not AV1. The client might be a five-year-old phone, and H.264 is the one codec with *universal hardware decode* on the client side and *universal hardware encode* (NVENC/QSV/AMF) on the host side.

- **Transport: Rust, `webrtc-rs`.** Since the shell is Tauri, the media engine lives in the same Rust process as the app.

The other half of the research was the *host* side: **how do you make a fake monitor that Windows treats as real? what about Mac?** The answer is are virtual displays, which are software-only mocked up displays that can be used for various purposes. I used an existing library written in rust for the Windows and Mac clients, installed at runtime with `nefconc` on Windows (device-node/driver install) + `certutil` (trusting the cert).

---

## 2. The spine: Tauri, the Rust to TS bridge, and a typed events system

**Commands (TS to Rust): `tauri-specta`.** Every Rust command is annotated `#[tauri::command] #[specta::specta]`, collected in a `Builder`, and at build time (debug only) it *exports a fully-typed `src/lib/bindings.ts`*. This supports the various commands that the frontend requires: `setup`, `set_session_credentials`, `register_cloud_session`, `get_cloud_status`, etc.

**Events (Rust to TS): the typed event bus.** The UI has to react to things the *host* discovers, such as a device joining, a device leaving, the network changing, the cloud relay's status flipping. These are modeled as `#[derive(..., Type, Event)]` structs (`DeviceJoin`, `DeviceModify`, `DeviceRemove`, `NetworkChange`, `HostedNetworkNoPassword`, `CloudStatusChange`, and `LogLine`) collected via `collect_events!`.

> **Why bother with a code-generated bridge?** Manually maintaining and updating a hand-written bridge between the two would lead to too much overhead in future development. It's better to let the code write itself!

---

## 3. First light: a virtual display and a naive `<video>` pipeline

The first end-to-end milestone (the **"initial release for windows + nvidia"** commit) was deliberately unoptimized. The goal was one moving picture on a phone, however ugly:

1. `setup()` installs the app state: force the desktop into **extend** topology, create the shared virtual-display client, wire up the device reporter, sessions map, overrides, disconnect grace.
2. A client POSTs `/whep` with `{sessionId, otp, deviceName, width, height, sdp}`.
3. The host validates the OTP, **creates a virtual display sized to the client** via the IDD, waits for Windows to actually attach it as a new monitor (polling `EnumDisplayDevices`-style name lists under a correlation lock), forces it to the requested mode, and starts capturing it.
4. Capture to NVENC to an SDP answer, and `webrtc-rs` streams H.264 to the browser's `<video>`.

At this stage the client was a plain `<video>` element with `srcObject` set to the incoming stream. It worked, but was greatly laggy. I was happy that the prototype was working, but the display/capture/encode/transport spine had to have countless speed optimizations.

One key server decision made at this stage was serving **both HTTP and HTTPS, self-signed.** The server binds HTTP (8080) *and* HTTPS (8443) on every adapter, generating a self-signed cert at runtime with `rcgen` (SANs include the LAN IP). HTTP exists because it's the easy first click; HTTPS exists because **WebCodecs requires a secure context** (foreshadowing #6).

---

## 4. The pipeline in anger: capture → encode → broadcast

Once the spine worked, I build the real engine: a Windows capture/encode pipeline designed to keep a frame on the GPU as long as possible and to never let anything queue.

**Capture.** Primary path is **Windows Graphics Capture (WGC)** via the `windows-capture` crate (later becoming a custom fork, see #12). WGC hands back a BGRA `ID3D11Texture2D` already on the display's adapter.

**Two things I learned the hard way and designed around:**

- **Idle desktops still need frames.** A static screen produces no WGC frames, but WebRTC and the decoder need a heartbeat (and any newly-joined viewer needs an IDR *now*). So there's a dedicated **repeater thread**: if no fresh frame arrived within ~2 frame-durations, it re-emits the last encoded picture on a keepalive cadence, and it's the path that services IDR requests when the screen is frozen.
- **Encoders transiently choke.** Under load NVENC/QSV can report "device busy." Rather than tear down the session, the loop tolerates a bounded streak of transient drops (`MAX_TRANSIENT_ENCODE_DROPS`) and keeps going.

Throughout, the pipeline logs rolling encode-path latency (avg/max ms over 60-frame windows). Thread tuning, GPU priority raising, and a "keep-awake" guard round it out, to ensure that the system settings are optimized alongside the pipeline itself.

---

## 5. Going vendor-specific: the part that actually made it fast

This is where "it works" became "it's low-latency," and it's the messiest, most rewarding part of the project. The generic answer (capture texture --> copy to CPU --> hand bytes to an encoder) works everywhere and is *slow* everywhere, because the CPU readback is a latency and bandwidth tax on every single frame. The name of the game is to **keep the frame on the GPU from capture to bitstream.**

The pipeline picks a **`Backend`** by vendor:

### NVENC (NVIDIA)

There is no NVIDIA SDK linked at build time. I **dynamically load** the encoder API and hand-define (from other references) the handful of structs/enums/vtables to ensure no system dependencies are requireds.

The final result is something I'm very proud of: **zero-copy path**. NVENC allocates its input surface as a *shared* D3D11 texture (`SHARED_NTHANDLE | KEYEDMUTEX`). The capture/iGPU side opens that same texture by handle (`OpenSharedResource1`), and each frame:

1. `AcquireSync(WRITER)` on the keyed mutex,
2. either `CopyResource` (no scale) or `scale_into` (VideoProcessor downscale) straight into the shared surface,
3. `Flush`, `ReleaseSync(ENCODER)`,
4. `encode_input()`.

If the zero-copy adapter can't be built (odd adapter topologies or older machines from my own testing), it degrades to an NVENC **CPU-bridge** fallback.

### Intel Quick Sync (QSV / oneVPL)

The Intel backend targets the case where capture *and* encode are the same Intel adapter (a common case with laptops without AMD/NVIDIA dGPUs). One clever bit implemented is that the QSV path **fuses the downscale into its VPP (video pre-processing) pass**, so it wants the *native-size* texture and does BGRA to NV12 + resize in one shot. When same-adapter isn't possible, there's an Intel CPU-bridge last resort as well.

### macOS (VideoToolbox)

**Capture: ScreenCaptureKit, with a CGDisplayStream fallback.** The primary source is **ScreenCaptureKit** (macOS 12.3+). Similar speed optimizations are made to minimize latency: whole-display filter, `420f` biplanar output, `queueDepth = 3`, window-server-composited cursor, no audio, etc. Older systems fall back to **CGDisplayStream**.

**No link-time dependency on newer frameworks.** SCK only exists on 12.3+, and a normal link against `ScreenCaptureKit.framework` would add an undefined dyld symbol that breaks the *entire binary* on older macOS. So the backend touches SCK through **objc2 runtime interop only**. One universal binary loads and runs across all macOS versions, calling functions only when they are defined as available on the device.

**Zero-copy, the Apple way.** A captured frame arrives as an **`IOSurface`**, gets wrapped in a **`CVPixelBuffer`** (no pixel copy), and is handed straight to `VTCompressionSessionEncodeFrame`.

Similar to the previous implementations as well, all parameters are well-document and optimized to minimize latency.

### DXGI Desktop Duplication (the capture fallback)

WGC can't always open *virtual* displays on older Windows builds. So there's a hand-written **DXGI Desktop Duplication** engine as a fallback capture source, including **GPU cursor compositing**.

---

## 6. The client gets serious: WebCodecs over `<video>`

The client (served as static `index.html` + `transform-worker.js` straight from the Rust binary) now has two paths:

- **Fast path (WebCodecs).** An `RTCRtpScriptTransform` (or legacy `createEncodedStreams` where the modern API is missing) pipes encoded frames into a worker. The worker runs a `VideoDecoder` configured `optimizeForLatency: true`, probes a list of `avc1.*` codec strings for one the device actually supports, and draws decoded frames straight onto an **OffscreenCanvas** transferred from the main thread. `playoutDelayHint = 0` and `jitterBufferTarget = 0` tell WebRTC we want frames as soon as they arrive.
- **Fallback path (`<video>`).** If WebCodecs, script transforms, or `transferControlToOffscreen` aren't available, it falls back to a `<video>` element with a small `jitterBufferTarget`. A watchdog also flips to fallback if no WebCodecs frame renders within 8 seconds.

Because WebCodecs needs HTTPS, the client shows a **"switch to HTTPS for low-latency mode"** modal when it detects it's on HTTP without WebCodecs support, linking to the secure port (fetched from `/net-config`).

On top of streaming, the client is built to *feel* like a monitor, not a web page: it acquires fullscreen, pointer lock, keyboard lock (Escape/Meta/Alt/F11), and a screen wake lock; re-acquires them on visibility/fullscreen changes; sizes the canvas to the viewport; and fires a `/leave` beacon on `pagehide` so the host can tear the display down promptly.

---

## 7. Adaptive bitrate and resilience

A fixed bitrate is wrong on a shared Wi-Fi link. So the WebRTC session runs a **BWE (bandwidth estimation) driver**: every 250ms it pulls `getStats`, measures actual send rate and remote fraction-lost, feeds them to `estimate_from_loss`, and a `BitrateController` smooths that into a new target that's pushed live into the encoder (`set_bitrate`) via an atomic + wake channel. When loss goes up, the target bitrate comes down before the picture falls apart.

When you change a device's settings in the UI, the host **does not** destroy and recreate the display (that flashes the whole desktop and reshuffles window positions). Instead:

- `set_device_override` bumps a **reconfig epoch**; removing an override bumps a **kick epoch**.
- The client polls `/reconfig` every second; an epoch change triggers an in-place renegotiation (new resolution/quality) *without* tearing down the virtual display when only the encoder settings changed, and a kick-epoch change cleanly evicts the device.
- **Session sequence numbers** and a disconnect **grace period** prevent races where a quick disconnect/rejoin would otherwise delete a display out from under a reconnecting client.

---

## 8. Sessions, security, and per-device settings

Each host advertises a **session ID + 6-digit OTP**. The `/whep` handler validates both, and an **`OtpLimiter`** enforces lockouts after repeated bad attempts (returning `429` with a retry-after), so the OTP can't be brute-forced over the LAN.

Every connected device is independent: its own virtual display, its own pipeline, and its own **override** record (display scale, orientation, refresh rate, video scale, video quality/QP). The device reporter emits `DeviceJoin`/`DeviceRemove` events so the UI's device list stays live, and overrides are applied both at session start and on in-place reconfigure.

Transport is HTTPS + DTLS/SRTP end to end; the self-signed cert triggers a one-time browser warning on first join (or you can supply a real cert via `--tls-cert/--tls-key`).

---

## 9. Networking: offline mode, adapter watching, and the cloud relay

**Same-network is the happy path.** With host candidates only, two devices on the same Wi-Fi connect directly, lowest possible latency, no servers involved.

**Offline / no-router mode.** The host can stand up its own **ad-hoc Wi-Fi hosted network** so a phone can join with no infrastructure at all, genuinely offline. This is immensely helpful on-the-go when Wi-Fi isn't available. The app also **watches network adapters live** and rebuilds the per-adapter join URLs and QR codes as interfaces come and go.

**Cross-network (cloud relay).** Getting a device on cellular to reach a host behind NAT needs more. Rather than punch holes or expose the host, I built a **cloud relay**: the host opens a persistent **WebSocket** control channel to `session.screenextend.app`, registers its session + capabilities, and the relay tunnels signaling (`/whep`, `/reconfig`, `/leave`) as JSON messages. The media itself still flows peer-to-peer over WebRTC where possible; the relay is a signaling courier, with heartbeats, exponential backoff + jitter, and clean reconnect/drain handling. For the cases where P2P truly can't traverse the NAT, ICE needs a **TURN server** that can be configured on the settings screen.

---

## 10. Observability: the logbus

Debugging a real-time GPU pipeline from `println!` is hopeless. So there's a **logbus**: a macro layer (`tprintln!`/`teprintln!`) feeds a ring-buffered bus that both prints and emits `LogLine` events to a **terminal UI** inside the app, with a `get_log_backlog` command so a freshly-opened log view can backfill history.

---

## 11. Cross-platform reach (scaffolded)

The core is deliberately split so platform specifics live behind a common `pipeline`/`platform` surface (`windows_utils`, `macos_utils`, `linux_utils`). Windows and MacOS are mostly finished, with minor optimizations to be coded and tested. All features for Linux machines are unimplemented.

---

## 12. Where it stands now (v0.2.3)

- **Working:** Windows/MacOS host, platform-specific GPU-based encoding, per-device virtual displays, WebCodecs client with `<video>` fallback, adaptive bitrate, OTP-gated sessions, in-place reconfigure, offline hosted network, live adapter watching, cloud relay + ephemeral/self-hosted TURN, log terminal.
- **Recent hardening:** a batch of various speed fixes and swapping in a custom `windows-capture` fork to get control over capture behavior the upstream crate didn't expose.
- **Not done:** AMD encoder (designed, stubbed), Linux hosts (scaffolded), broad device/GPU test coverage.

---
