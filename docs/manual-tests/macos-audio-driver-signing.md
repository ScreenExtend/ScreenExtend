# Signing and notarizing the macOS audio driver

`ScreenExtendAudio.driver` is the AudioServerPlugIn behind the macOS 10.15–12.x system-audio tier
(`macos_utils/audio/legacy/`). `coreaudiod` refuses to load a HAL plug-in that is not signed with a
Developer ID and notarized, unless SIP is disabled — so an unsigned build is a development
artifact only, and shipping one is not an option.

Neither development machine has Apple signing credentials, so **nothing in this repository has been
signed**. Everything below the signing step has been run; the signing and notarization steps
themselves have not. This document is what a machine with the credentials needs.

## The one command

From `src-tauri/macos/ScreenExtendAudio/`:

```sh
SE_SIGN_IDENTITY="Developer ID Application: <Name> (<TEAMID>)" \
SE_INSTALLER_IDENTITY="Developer ID Installer: <Name> (<TEAMID>)" \
SE_NOTARY_PROFILE="screenextend-notary" \
SE_ARCHS="x86_64 arm64" \
./scripts/release.sh
```

That fetches libASPL at its pinned commit, compiles the driver, signs the bundle, builds the
installer package, signs it, submits it for notarization, staples the ticket, and copies the result
to `src-tauri/resources/ScreenExtendAudio.pkg` — the path `install_audio_driver` resolves at
runtime.

`./scripts/release.sh` with no variables set does everything except sign and notarize, and says so
at each step. That is the local development path and it needs no Apple account.

## The variables

| Variable | What it is | Absent |
|---|---|---|
| `SE_SIGN_IDENTITY` | Developer ID **Application** identity, as `security find-identity -v -p codesigning` prints it. Signs `ScreenExtendAudio.driver` with the hardened runtime and a secure timestamp. | the bundle keeps its ad-hoc signature |
| `SE_INSTALLER_IDENTITY` | Developer ID **Installer** identity, as `security find-identity -v` prints it. A different certificate from the Application one; `productbuild --sign` will not accept the Application identity. | the `.pkg` is unsigned |
| `SE_NOTARY_PROFILE` | Name of a `notarytool` keychain profile, created once with `xcrun notarytool store-credentials <name> --apple-id <id> --team-id <TEAMID> --password <app-specific-password>`. | no notarization, no stapling |
| `SE_ARCHS` | Architectures, space separated. Default `x86_64`. Use `"x86_64 arm64"` for the shipping driver. | x86_64 only |
| `SE_LIBASPL_DIR` | An existing libASPL checkout to build against, instead of `third_party/libASPL`. | fetched at the pinned commit |
| `SE_SKIP_COPY` | Set to skip staging the `.pkg` into `src-tauri/resources/`. | it is copied |
| `SE_CLEAN` | Set to discard the object cache first. | incremental |

`release.sh` checks all of these before it compiles anything: a missing tool, an identity that is
not in the keychain, or `SE_NOTARY_PROFILE` without `SE_INSTALLER_IDENTITY` fails in seconds rather
than after a notarization round trip. Nothing is ever signed implicitly — an absent identity is a
skipped step with a line saying so, never a silent fallback to some other certificate.

## What success looks like

```
==> Preflight
  driver signing:  Developer ID Application: … (TEAMID)
  package signing: Developer ID Installer: … (TEAMID)
  notarization:    screenextend-notary
…
==> Notarizing (this takes minutes)
  … status: Accepted
==> Stapling the ticket
The staple and validate action worked!
==> Staging for the app bundle
  …/src-tauri/resources/ScreenExtendAudio.pkg
```

Then verify independently:

```sh
spctl --assess --type install -vv build/ScreenExtendAudio-*.pkg   # "accepted, source=Notarized Developer ID"
pkgutil --check-signature build/ScreenExtendAudio-*.pkg           # Developer ID Installer chain
codesign -dv --verbose=4 build/ScreenExtendAudio.driver           # hardened runtime, TeamIdentifier set
```

`codesign -dv` on the driver must show `flags=0x10000(runtime)` and a real `TeamIdentifier`. An
`adhoc` flag means `SE_SIGN_IDENTITY` was not picked up.

## The coreaudiod caveat

macOS loads `/Library/Audio/Plug-Ins/HAL/` plug-ins **only when `coreaudiod` starts**. The
package's `postinstall` runs `launchctl kickstart -k system/com.apple.audio.coreaudiod` (falling
back to `killall coreaudiod`), so a fresh install normally brings the device up within a second or
two, and `install_pkg`'s three-second poll for `driver_healthy()` sees it and returns `installed`.

Bouncing `coreaudiod` interrupts **all** audio on the machine for a moment: anything playing stops,
and some applications need to be restarted to recover their audio. That is the trade for not
requiring a reboot, and it is why the install prompt should warn.

If the kickstart fails — it can under some MDM configurations — `postinstall` says so and
`install_pkg` returns `needs_reboot`. A log out and back in is usually enough; a reboot always is.

## Verifying a load, end to end

On the target Mac, **from a console (GUI) session** — this cannot be done over SSH, see below:

```sh
# 1. the plug-in loaded and opened its shared segment
log show --last 5m --info --predicate 'subsystem == "app.screenextend.desktop.audio"'
#    expect: "shared segment /ScreenExtendAudio open: 48000 Hz x2, 131072 samples, generation N"
#            "device built: ScreenExtend Audio (app.screenextend.desktop.audio.device) 48000 Hz x2"

# 2. the device exists and the host can map the ring
clang tools/shmcheck.c -o /tmp/shmcheck && /tmp/shmcheck
#    expect: "open O_RDWR (what the host needs): granted"
#            "header magic=0x31414553 version=2 … hdr=128"

# 3. samples actually flow
clang tools/hal_render.c -o /tmp/hal_render -framework CoreAudio -framework CoreFoundation
/tmp/hal_render 6 440 &
/tmp/shmcheck watch
#    expect writePos climbing by ~48000*2 per second and "NON-SILENT"

# 4. the shipped Rust pipeline sees it
cargo test --lib legacy_live -- --ignored --nocapture
```

**Steps 3 and 4 need a console session.** macOS grants audio IO only to the session that owns the
console: over SSH, `say` fails with `-128`, `afplay` with `AudioQueueStart -66681`, and a raw HAL
IOProc runs zero IO cycles — against the built-in output device as much as against ours. Steps 1
and 2 do work over SSH, which is how the driver's load path was verified on the 10.15.8 box.
