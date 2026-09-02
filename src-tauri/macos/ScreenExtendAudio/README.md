# ScreenExtend Audio — Core Audio HAL plug-in

The virtual audio device that gives ScreenExtend system-audio capture on **macOS 10.15–12.x**,
where there is no native system-audio API (ScreenCaptureKit audio needs 13.0+, Core Audio Process
Taps need 14.2+). See `PRD-macos-legacy-audio.md` and `AUDIO_NOTES_MACOS_LEGACY.md` at the repo
root for the full design and the measured-vs-reasoned findings.

It is an `AudioServerPlugIn` (userspace, loaded by `coreaudiod`), **not** DriverKit — Apple does
not grant DriverKit entitlements for virtual audio devices (PRD §1.1). It is built on
**libASPL (MIT)**; no other virtual-audio product's name or code appears here (PRD §2, §3).

## What it does

* Registers a **`ScreenExtend Audio`** output device (bundle id `app.screenextend.desktop.audio`,
  UID `app.screenextend.desktop.audio.device`).
* Exposes **Volume + Mute controls** on the output scope so the macOS volume keys and menu-bar
  slider keep working while it is the default output (PRD §6.2, layer 1).
* Captures the system mix in `OnProcessMixedOutput` **before** any local gain and writes it into a
  POSIX **shared-memory SPSC ring** (`/ScreenExtendAudio`) that the host app reads with zero HAL
  round-trip (PRD §5.2a). Reports zero device latency (PRD §5.3).
* Also mirrors the capture into an **input (loopback) stream**, so the host has a HAL-input
  fallback if shared memory is unavailable under the `coreaudiod` sandbox (PRD §5.2a, §13.1).

All the branded strings live in one file: [`src/branding.hpp`](src/branding.hpp).
The shared-memory byte layout is a hard ABI contract with the Rust reader
(`src-tauri/src/macos_utils/audio/legacy/shm_reader.rs`) — see [`src/shm_ring.hpp`](src/shm_ring.hpp).

## Build

libASPL is the foundation. For a shipped build, **vendor** it into `third_party/libASPL` (MIT
permits this). For local dev, run `./generate-sources-macos-legacy-audio.sh` at the repo root and
the build falls back to the gitignored checkout automatically.

```sh
./scripts/build.sh                                   # x86_64, unsigned → build/ScreenExtendAudio.driver
ARCHS="x86_64 arm64" ./scripts/build.sh              # universal (needs an SDK that can target arm64)
CODESIGN_ID="Developer ID Application: …" ./scripts/build.sh
```

The build uses `clang++` directly (no CMake/Xcode required) so it works on a bare Command Line
Tools install. Contributors who prefer Xcode can generate a project from `project.yml`
(`xcodegen generate`); the CMake pattern libASPL ships in its `examples/` also works.

## Package, sign, notarize

```sh
INSTALLER_ID="Developer ID Installer: …" ./scripts/package.sh          # → build/ScreenExtendAudio-<v>.pkg
APP_ID=… INSTALLER_ID=… NOTARY_PROFILE=… ./scripts/sign_and_notarize.sh
```

The `.pkg` installs to `/Library/Audio/Plug-Ins/HAL/` (root-owned; one admin prompt) and its
`postinstall` chowns the bundle `root:wheel` and restarts `coreaudiod`. On recent macOS the restart
is often refused, in which case the app tells the user a reboot is needed (PRD §7.5). **A HAL
plug-in must be signed and notarized to load on modern macOS** (PRD §7.7).

## Status on the dev box

This tree **compiles and links** to `ScreenExtendAudio.driver` with Apple clang 12 against the
macOS 10.15 SDK, and the factory symbol `ScreenExtendAudioEntryPoint` is exported. It has **not**
been signed, notarized, installed, or loaded into `coreaudiod` (no Apple Developer identity on the
dev box), so no runtime behaviour has been verified. Every runtime claim is marked accordingly in
`AUDIO_NOTES_MACOS_LEGACY.md`.
