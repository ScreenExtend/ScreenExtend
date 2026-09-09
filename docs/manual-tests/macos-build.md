# Manual test: the macOS deployment target and the 10.15 dyld floor

**Why this exists:** the 10.15 floor is a *load* constraint, not a feature constraint. dyld binds
every non-weak undefined symbol before `main()`, so one link-time reference to an API newer than
10.15 means the app does not start on Catalina — while starting fine on the CI runner and on
every modern Mac, i.e. on every machine that could debug it. Two things now guard it:
`src-tauri/.cargo/config.toml` pins `MACOSX_DEPLOYMENT_TARGET = "10.15"`, and
`scripts/check-macos-dyld.sh` audits the built binary. Neither has been confirmed against a
**release** artifact yet, because the dev Mac is at its build capacity and the universal binary
only exists after a release build.

Everything here is read-only inspection of an artifact someone else built. It takes about five
minutes once a build exists.

---

## The "before" value, for comparison

Measured 2026-09-08 over SSH on the dev Mac (macOS 10.15.8, Intel), on a debug binary built at
`80e28c7a` on `latency-overhaul` — i.e. **before** `src-tauri/.cargo/config.toml` was merged:

```
$ otool -l src-tauri/target/debug/ScreenExtend | grep -A4 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX'
      cmd LC_VERSION_MIN_MACOSX
  cmdsize 16
  version 10.12          <- rustc's default for x86_64-apple-darwin
      sdk 11.1           <- MacOSX.sdk symlinks to 11.1 on that box, not to the 10.15 SDK

$ lipo -info src-tauri/target/debug/ScreenExtend
Non-fat file: ... is architecture: x86_64
```

`10.12` is what the pin has to change. It contradicted `minimumSystemVersion: "10.15"` in
`src-tauri/tauri.macos.conf.json`, so the binary claimed to run on three releases it cannot, and
clang's availability/weak-import machinery was calibrated three releases too low.

---

## 1. Deployment target on a release build (2 min)

Run on any Mac, against whichever release binary you have — the dev box's own
`src-tauri/target/release/ScreenExtend`, or the universal one from the release job at
`src-tauri/target/universal-apple-darwin/release/ScreenExtend`.

```sh
BIN=src-tauri/target/release/ScreenExtend
lipo -info "$BIN"
otool -l "$BIN" | grep -A3 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX'
```

**Expected.** The x86_64 slice reports `minos 10.15` (or `version 10.15` on an older toolchain
that still emits `LC_VERSION_MIN_MACOSX`). The arm64 slice legitimately reports **11.0**: Apple
Silicon postdates Catalina, and rustc clamps the deployment target up to the minimum its target
supports rather than emitting an impossible one. Anything else — `10.12` especially — means the
pin did not reach the linker; check that cargo was invoked with `src-tauri` as its working
directory, because `src-tauri/.cargo/config.toml` only applies below that directory.

**Paste back** the full `otool` block for both slices and the `lipo -info` line.

## 2. Nothing newer than the floor is linked (1 min)

```sh
otool -L "$BIN"
```

**Expected.** No `ScreenCaptureKit.framework`, no `Symbols.framework`, no `MediaExtension`.
ScreenCaptureKit does not exist on 10.15, so an `LC_LOAD_DYLIB` for it is an unconditional
launch failure there; it is what an accidental `objc2-screen-capture-kit` dependency produces.
The SCK video and audio backends reach it through `AnyClass::get` and `msg_send!` on purpose.

## 3. The full symbol audit (5 min warm, ~3 min cold)

On the dev Mac, which has the real 10.15 SDK — this is the strong verdict:

```sh
scripts/check-macos-dyld.sh "$BIN" \
  /Library/Developer/CommandLineTools/SDKs/MacOSX10.15.sdk
```

**Expected.** `PASS`, `offenders 0`. The last recorded run (x86_64 release, `docs/discovery/
P-macos-dyld-audit.md`) was 539 undefined / 538 ok / 1 weak / 0 offenders. Raising the deployment
target can flip some symbols from strong to weak imports; the script reports those as `WEAK` and
treats them as safe, so the offender count should stay at 0 and the weak count may rise.

On a machine without the 10.15 SDK — any CI runner, any modern Mac — the same script still runs
with `--floor 10.15` against the newest SDK, but read what that costs: the `.tbd` stub index is
the authoritative half of the verdict, and a newer SDK's stubs are a superset, so absence stops
being detectable. The header `INTRO` column still fails anything annotated as introduced after
the floor, which is the half that catches a newly linked framework API. Treat a runner pass as a
smoke test and step 3 on the dev box as the real one.

First run on any machine builds two caches in `$TMPDIR` (export index ~30 s, header corpus
~1 min); later runs are ~29 s.

## 4. Prove the gate has teeth (10 min, optional, once)

Worth doing once so the gate is known to fail rather than assumed to:

1. Add `extern_class!` for `SCStream` (or any 12.3+ class) somewhere in `macos_utils/`.
2. `cargo build --release`.
3. `scripts/check-macos-dyld.sh …` must exit 1 with
   `FAIL _OBJC_CLASS_$_SCStream … ABSENT from the 10.15 SDK stubs`.
4. Revert.

---

**Record the result** as a row in `docs/OPTIMIZATIONS.md` and flip the build row in
`docs/CAPABILITY-TIERS.md` from `UNVERIFIED` to `VERIFIED`.

---

## 5. The macOS bundle carries its Info.plist, its CSP and the audio package (W13b)

Three things reach a macOS build by convention rather than by anything that was ever
asserted, and all three fail silently.

`src-tauri/tauri.macos.conf.json` is **auto-merged by Tauri on macOS** — the
`--config src-tauri/tauri.macos.conf.json` in release CI is redundant, not load-bearing.
This was established the hard way: adding `resources/ScreenExtendAudio.pkg` to
`bundle.resources` there broke a plain `cargo check` on the Mac
(`resource path 'resources/ScreenExtendAudio.pkg' doesn't exist`, from `tauri-build`'s
resource validation in the build script), even though the file is only ever named in the
macOS overlay and `--config` was not passed. That is why the package is now carried by
`bundle.macOS.files` — the bundler copies those at packaging time, so a tree without the
gitignored package still checks and builds.

Run after `pnpm tauri build --config src-tauri/tauri.macos.conf.json`:

```sh
app=src-tauri/target/release/bundle/macos/ScreenExtend.app     # or universal-apple-darwin/
plutil -p "$app/Contents/Info.plist" | grep -E \
  'NSAudioCapture|NSAllowsLocalNetworking|LSMinimumSystemVersion|AutomaticGraphicsSwitching'
defaults read "$app/Contents/Info.plist" NSSupportsAutomaticGraphicsSwitching   # -> 1
ls -l "$app/Contents/Resources/resources/"                     # libopus.dylib + the .pkg
```

Expect `NSAudioCaptureUsageDescription`, both ATS keys, `LSMinimumSystemVersion = 10.15`
and `NSSupportsAutomaticGraphicsSwitching = 1`. A missing usage description is a hard
process kill the first time a user enables System Audio, with nothing in our own log.
The release workflow now asserts all four plus the package, so this is the local rehearsal
of a gate that already exists.

**The CSP.** `tauri.macos.conf.json` used to set `app.security.csp: null`, so every shipped
macOS build ran the control UI with no policy at all while Windows ran with one. It now
carries the Windows policy plus `worker-src 'self' blob:; child-src 'self' blob:` — the
second is there because Safari 13.1 has no `worker-src` and falls back to `child-src`, and
the monaco language services are workers. Verify on the 10.15 box:

1. Open the app, then Settings.
2. Exercise the log terminal and the JSON config editor (the editor is monaco; if its
   workers are blocked it silently drops to the main thread, or the chunk fails outright).
3. Open the Web Inspector console and confirm there is **no** `Refused to ...` CSP line.
4. In the console, `document.querySelector('meta[http-equiv="Content-Security-Policy"]')`
   should be non-null (Tauri injects the policy as a meta tag).

If a directive genuinely cannot be satisfied on 13.1, narrow *that directive* and leave a
note here — do not put `"csp": null` back.
