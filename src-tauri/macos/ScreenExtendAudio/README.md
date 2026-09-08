# ScreenExtend Audio — Core Audio HAL plug-in

The virtual audio device that gives ScreenExtend system-audio capture on **macOS 10.15–12.x**,
where there is no native system-audio API (ScreenCaptureKit audio needs 13.0+, Core Audio Process
Taps need 14.2+).

It is an `AudioServerPlugIn` (userspace, loaded by `coreaudiod`), **not** DriverKit — Apple does not
grant DriverKit entitlements for virtual audio devices. It is built on **libASPL (MIT)**, pinned to
one commit by `scripts/fetch-libaspl.sh`; no other virtual-audio product's name or code appears
here.

## What it does

* Registers a **`ScreenExtend Audio`** output device (bundle id `app.screenextend.desktop.audio`,
  UID `app.screenextend.desktop.audio.device`), 48 kHz stereo, zero declared latency.
* Exposes **Volume + Mute controls** on the output scope so the macOS volume keys and menu-bar
  slider keep working while it is the default output. That is the classic regression a virtual
  output device causes, and repairing it is half of why this tier exists.
* Captures the system mix in `OnProcessMixedOutput` **before** any local gain and writes it into a
  POSIX **shared-memory SPSC ring** (`/ScreenExtendAudio`) that the host app reads with no HAL round
  trip.
* Also mirrors the capture into an **input (loopback) stream**, so the host has a HAL-input fallback
  if shared memory is unavailable.

All the branded strings live in one file: [`src/branding.hpp`](src/branding.hpp).

## The shared-memory contract

The byte layout in [`src/shm_ring.hpp`](src/shm_ring.hpp) is a hard ABI contract with the Rust
reader (`src-tauri/src/macos_utils/audio/legacy/shm_reader.rs`) — **change both together**. That is
enforced, not merely asked for: `legacy/test/ring.rs` `include_str!`s this header and asserts every
constant and every `static_assert(offsetof(...))` against the Rust constants, so `cargo test` fails
if the two drift.

Layout 2 (128-byte header) carries, besides the ring geometry:

| field | written by | why |
|---|---|---|
| `writePos` | driver | monotonic sample index, Release-published |
| `readPos` | host | lets the driver count what it overwrites unread |
| `overruns` | driver | samples lost because the reader fell behind |
| `generation` | driver | bumped per driver start; tells the reader to resync |
| `anchorSeq`/`anchorPos`/`anchorHostTime` | driver | seqlock-guarded `mach_absolute_time` of a known sample, so the host can date audio from when it was captured rather than from its own drain — this is what A/V sync rides |

A segment from an older layout is unlinked and re-created at load: a POSIX shared-memory object can
only be sized once on macOS, so a stale one of the wrong size cannot be grown.

## Logging

The plug-in runs inside `coreaudiod`, so `os_log` is the only window into it:

```sh
log show --last 5m --info --predicate 'subsystem == "app.screenextend.desktop.audio"'
log stream --predicate 'subsystem == "app.screenextend.desktop.audio"'
```

Load, segment creation, geometry, permissions and every failure with its `errno` are reported
there. Nothing logs from an IO callback.

## Build

```sh
./scripts/fetch-libaspl.sh          # clone libASPL at the pinned commit into third_party/
./scripts/build.sh                  # x86_64, unsigned → build/ScreenExtendAudio.driver
SE_ARCHS="x86_64 arm64" ./scripts/build.sh
```

The build uses `clang++` directly (no CMake or Xcode) so it works on a bare Command Line Tools
install. Note that a 10.15 SDK cannot target arm64; `build.sh` checks each requested architecture
up front and says so rather than failing halfway through.

## Release

One command does build → sign → package → notarize → staple → stage:

```sh
./scripts/release.sh                # unsigned local build; every credentialed step is skipped
SE_SIGN_IDENTITY=… SE_INSTALLER_IDENTITY=… SE_NOTARY_PROFILE=… ./scripts/release.sh
```

Every variable, what success looks like, and the `coreaudiod` restart caveat are documented in
[`docs/manual-tests/macos-audio-driver-signing.md`](../../../docs/manual-tests/macos-audio-driver-signing.md).
The finished package is staged at `src-tauri/resources/ScreenExtendAudio.pkg`, which is where
`install_audio_driver` looks for it.

The `.pkg` installs to `/Library/Audio/Plug-Ins/HAL/` (root-owned; one admin prompt) and its
`postinstall` chowns the bundle `root:wheel` and restarts `coreaudiod`. **A HAL plug-in must be
signed and notarized to load unless SIP is disabled.**

## Status

Verified on the development Mac (macOS 10.15.8, Intel, SIP disabled):

* builds from a clean tree with `./scripts/release.sh` alone;
* the unsigned `.pkg` installs, `postinstall` restarts `coreaudiod`, and the device appears in the
  HAL within a second — no reboot;
* the plug-in loads, unlinks a stale layout-1 segment, creates the layout-2 one, and logs its
  geometry; the user can map it `O_RDWR`, so the fast transport is available.

Not verified anywhere yet:

* **signing and notarization** — no Apple Developer identity on either development machine, so the
  driver has only ever been ad-hoc signed and has only ever loaded with SIP disabled;
* **sample flow** through `OnProcessMixedOutput` — macOS grants audio IO only to the console
  session, and both development machines reach this Mac over SSH, where even the built-in output
  device runs zero IO cycles. `tools/hal_render.c` plus `tools/shmcheck.c watch` is the check to
  run from a console session; the procedure is in the manual test above.
