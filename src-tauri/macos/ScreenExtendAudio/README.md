# ScreenExtend Audio

ScreenExtend Audio is a Core Audio HAL plug-in that gives ScreenExtend system-audio capture on macOS 10.15 to 12.x.

Those versions have no native system-audio API: ScreenCaptureKit audio needs macOS 13.0 and Core Audio Process Taps need 14.2.

The plug-in is an `AudioServerPlugIn` (userspace, loaded by `coreaudiod`), not DriverKit. Apple does not grant DriverKit entitlements for virtual audio devices.

It is built on libASPL (MIT), pinned to one commit by `scripts/fetch-libaspl.sh`. No other virtual-audio product's name or code appears here.

## Requirements

- macOS with the Command Line Tools (`xcrun`, `clang++`, `pkgbuild`, `productbuild`, `codesign`). CMake and Xcode are not needed.
- `git`, which `fetch-libaspl.sh` uses to clone libASPL into `third_party/`.
- For an arm64 or universal build, an SDK that can target arm64. A 10.15 SDK cannot.

## Quick start

From this folder:

```sh
./scripts/fetch-libaspl.sh   # clone libASPL at the pinned commit into third_party/
./scripts/build.sh           # x86_64, unsigned
```

The last line `build.sh` prints is the bundle it wrote:

```text
Built: <this folder>/build/ScreenExtendAudio.driver
```

For a universal build, set the architectures:

```sh
SE_ARCHS="x86_64 arm64" ./scripts/build.sh
```

`build.sh` checks each requested architecture before it compiles anything and stops with an error if the SDK cannot target one.

## What it does

- Registers a `ScreenExtend Audio` output device (bundle id `app.screenextend.desktop.audio`, UID `app.screenextend.desktop.audio.device`), 48 kHz stereo, zero declared latency.
- Exposes Volume and Mute controls on the output scope, so the macOS volume keys and the menu-bar slider keep working while it is the default output. A virtual output device usually breaks those keys, and fixing that is half of why this tier exists.
- Captures the system mix in `OnProcessMixedOutput` **before** any local gain and writes it into a POSIX shared-memory SPSC ring (`/ScreenExtendAudio`). The host app reads the ring with no HAL round trip.
- Mirrors the capture into an input (loopback) stream, so the host has a HAL-input fallback if shared memory is unavailable.

All the branded strings are in one file: [`src/branding.hpp`](src/branding.hpp).

## The shared-memory contract

The byte layout in [`src/shm_ring.hpp`](src/shm_ring.hpp) is a hard ABI contract with the Rust reader (`src-tauri/src/macos_utils/audio/legacy/shm_reader.rs`). **Change both together.** A test enforces it: `legacy/test/ring.rs` `include_str!`s this header and asserts every constant and every `static_assert(offsetof(...))` against the Rust constants, so `cargo test` fails if the two drift.

Layout 2 (128-byte header) carries these fields besides the ring geometry:

| field | written by | why |
|---|---|---|
| `writePos` | driver | monotonic sample index, Release-published |
| `readPos` | host | lets the driver count what it overwrites unread |
| `overruns` | driver | samples lost because the reader fell behind |
| `generation` | driver | bumped per driver start; tells the reader to resync |
| `anchorSeq`/`anchorPos`/`anchorHostTime` | driver | seqlock-guarded `mach_absolute_time` of a known sample, so the host can date audio from when it was captured rather than from its own drain. A/V sync depends on this |

The plug-in unlinks a segment from an older layout and re-creates it at load. On macOS a POSIX shared-memory object can only be sized once, so a stale one of the wrong size cannot be grown.

## Logging

The plug-in runs inside `coreaudiod`, so `os_log` is the only way to see what it does:

```sh
log show --last 5m --info --predicate 'subsystem == "app.screenextend.desktop.audio"'
log stream --predicate 'subsystem == "app.screenextend.desktop.audio"'
```

It logs load, segment creation, geometry, permissions and every failure with its `errno`. Nothing logs from an IO callback.

## Release

`scripts/release.sh` runs build, sign, package, notarize, staple and stage in order:

```sh
./scripts/release.sh                # unsigned local build; every credentialed step is skipped
SE_SIGN_IDENTITY=… SE_INSTALLER_IDENTITY=… SE_NOTARY_PROFILE=… ./scripts/release.sh
```

| Variable | Effect |
|---|---|
| `SE_SIGN_IDENTITY` | Developer ID Application identity. Signs the `.driver` bundle with the hardened runtime. Unset, the bundle stays ad-hoc signed. |
| `SE_INSTALLER_IDENTITY` | Developer ID Installer identity. Signs the `.pkg`. Unset, the package is unsigned. |
| `SE_NOTARY_PROFILE` | `notarytool` keychain profile, created once with `xcrun notarytool store-credentials`. Unset, there is no notarization and no stapling. Needs `SE_INSTALLER_IDENTITY`. |
| `SE_ARCHS` | Architectures to build. Default `x86_64`. |
| `SE_LIBASPL_DIR` | An existing libASPL checkout instead of `third_party/libASPL`. |
| `SE_SKIP_COPY=1` | Do not copy the finished `.pkg` into `src-tauri/resources/`. |
| `SE_CLEAN=1` | Discard the object cache before building. |

The header comment of `scripts/release.sh` documents the same variables. The script checks every tool and identity before it starts, and prints which steps it will skip.

The finished package is staged at `src-tauri/resources/ScreenExtendAudio.pkg`, which is where `install_audio_driver` looks for it.

The `.pkg` installs to `/Library/Audio/Plug-Ins/HAL/` (root-owned; one admin prompt). Its `postinstall` chowns the bundle `root:wheel` and restarts `coreaudiod`. If neither `launchctl kickstart` nor `killall coreaudiod` works, the device appears only after a reboot or a new login.

**A HAL plug-in must be signed and notarized to load unless SIP is disabled.**

## Status

Verified on the development Mac (macOS 10.15.8, Intel, SIP disabled):

- builds from a clean tree with `./scripts/release.sh` alone;
- the unsigned `.pkg` installs, `postinstall` restarts `coreaudiod`, and the device appears in the HAL within a second, with no reboot;
- the plug-in loads, unlinks a stale layout-1 segment, creates the layout-2 one, and logs its geometry; the user can map it `O_RDWR`, so the fast transport is available;
- sample flow through `OnProcessMixedOutput` works. 2,083,840 samples went through the shared ring with `readPos` tracking `writePos` at a backlog of 0, `overruns` 0, and the capture-time anchor published.

End to end through the shipped `start_capture()` path, driven by `say`:

- capture to Opus packet: p50 −0.57 ms, p90 3.59, p99 4.54;
- an IO cycle of 128 frames (2.67 ms);
- 0 of 3277 `capture_ns` deltas anything other than exactly 5.000 ms, with no regressions and no sequence gaps.

To reproduce, run `cargo test --lib legacy_live -- --ignored --nocapture` from `src-tauri/`.

Not verified anywhere yet:

- **Signing and notarization:** no Apple Developer identity on either development machine, so the driver has only ever been ad-hoc signed and has only ever loaded with SIP disabled. Everything below that step has been run.
- arm64: a 10.15 SDK cannot target it, so the universal build needs CI or a newer Mac.
- Non-stereo mixes: `OnProcessMixedOutput` downmixes anything that is not 2-channel with the same BS.775 coefficients as the Rust siblings, but the device advertises stereo, so that branch has never actually run.

## License

The plug-in is part of ScreenExtend and is covered by the repository's [AGPL-3.0 license](../../../LICENSE). libASPL is MIT-licensed and is fetched at build time, not vendored.
