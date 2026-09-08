# P — macOS dyld floor audit (10.15.8 Catalina, Intel)

This is a verification report, not a territory read: it answers one question on real hardware —
does the shipped binary contain a link-time reference to any macOS API newer than the 10.15
floor? — and delivers the tooling that keeps the answer stable.

**Verdict: clean. 0 offenders.** The newest link-time symbol in the entire binary is
`_OBJC_CLASS_$_NSUUID`, introduced in **10.14** — a full release *below* the floor. Every
newer-than-floor API the app uses (ScreenCaptureKit, Core Audio Process Tap, `CGVirtualDisplay*`,
the version-gated VideoToolbox property keys, `os_workgroup_*`) is reached through `dlsym`,
`AnyClass::get`, or `msg_send!` and appears nowhere in the undefined-symbol table. The
`CLAUDE.md` discipline is being followed exactly.

**But the floor is enforced by nothing except that discipline** — the build pins no SDK and no
deployment target, and release CI never inspects the artifact. Three findings cover that.

## 1. Measurement conditions

| | |
|---|---|
| Host | macOS 10.15.8 Catalina (19H2036), Intel, Command Line Tools only |
| Repo | `/Users/sarvesh/Desktop/ScreenExtend` |
| Commit | `bef5f89` |
| Working tree at build time | `M src-tauri/src/macos_utils/permissions.rs`, `M src-tauri/src/macos_utils/streamer/mod.rs` (two other agents' in-flight edits; neither touches the files any finding below points at) |
| Build | `cargo build --release`, finished in **9m57s**, exit 0, 2 rustc warnings |
| Binary | `src-tauri/target/release/ScreenExtend`, 33,557,584 bytes, **x86_64 only** (`lipo -info` → `Non-fat file`) |
| Toolchain | cargo/rustc/clippy 1.96, `stable-x86_64-apple-darwin` |
| SDKs present | `MacOSX10.15.sdk`, `MacOSX11.1.sdk`, `MacOSX.sdk` → symlink to **11.1** |

A release build from the previous (rate-limited) attempt was already running when this audit
started; it was reused rather than restarted, per the brief.

## 2. Mach-O load commands

```
      cmd LC_VERSION_MIN_MACOSX
  version 10.12          <- deployment target: rustc's x86_64-apple-darwin default
      sdk 11.1           <- the SDK the link actually used
```

Both numbers are wrong for this project and neither is set by the repo. `xcrun --show-sdk-path`
on this box returns the **10.15** SDK, but `ld` falls back to `.../SDKs/MacOSX.sdk`, which is a
symlink to `MacOSX11.1.sdk` — so the link had access to an SDK a full major release above the
floor. Nothing in the tree sets `SDKROOT` or `MACOSX_DEPLOYMENT_TARGET` (the only
`-mmacosx-version-min` / `-isysroot` hits are in
`src-tauri/macos/ScreenExtendAudio/scripts/build.sh:35,56`, which builds the audio *driver*).
`"minimumSystemVersion": "10.15"` (`src-tauri/tauri.macos.conf.json:38`) is `Info.plist`
metadata and constrains the linker not at all.

→ `findings/major-build-mac-release-links-against-an-unpinned-sdk.md`
→ `findings/minor-build-mac-deployment-target-10-12-contradicts-the-10-15-floor.md`

## 3. `otool -L` — linked libraries

22 load commands. All present on 10.15:

```
CoreGraphics  ApplicationServices  CoreFoundation  IOKit  AppKit  SystemConfiguration
VideoToolbox  CoreWLAN  Security  CoreMedia  CoreVideo  CoreAudio  OSAKit
WebKit  Carbon  IOSurface  Metal  Foundation  CoreServices
/usr/lib/libSystem.B.dylib  /usr/lib/libobjc.A.dylib  /usr/lib/libiconv.2.dylib
```

Three things this list proves:

- **No `ScreenCaptureKit`.** A load command for it would be an unconditional launch failure on
  10.15 — the dylib does not exist there — and it is exactly what an accidental
  `objc2-screen-capture-kit` dependency produces. `grep -c screen-capture-kit
  src-tauri/Cargo.lock` → **0**.
- **No `libopus.dylib`.** Opus is `libloading`'d at runtime (`streamer/audio/opus_sys.rs:82`
  tries `libopus.dylib`, `libopus.0.dylib`, `opus.dylib`), so `nm -u | grep -i opus` is empty. A
  missing dylib degrades audio; it does not stop the app.
- **No `libx264`.** x264 is Windows-only (`windows_utils/streamer/x264/`, and
  `x264_sys.rs:399` lists only `.dll` names). `nm -u | grep -i x264` is empty. macOS's software
  fallback is VideoToolbox, not x264.

Note that `IOSurface`, `Metal`, `CoreWLAN`, `WebKit` and `Security` each contribute **zero**
undefined symbols — they are load commands forced by `#[link(kind="framework")]` while all actual
use goes through `objc_msgSend` after a runtime class lookup. That is the objc2 design and it is
why a framework's *presence* in this list, not its symbols, is the thing to watch.

## 4. Undefined-symbol table

`nm -m -arch all` → **539 undefined symbols**, of which **1** is a weak import and **538** are
strongly bound. Every strongly-bound one is present in the 10.15 SDK's `.tbd` export stubs.

| Library | Count | Highest introduced version seen |
|---|---:|---|
| libSystem | 218 | ≤10.5 |
| AppKit | 67 | **10.13** (`NSAboutPanelOption*`) |
| CoreFoundation | 63 | 10.8 |
| CoreGraphics | 60 | ≤10.8 |
| libobjc | 41 | 10.0 |
| VideoToolbox | 22 | ≤10.9 |
| Foundation | 19 | **10.14** (`_OBJC_CLASS_$_NSUUID`) |
| CoreAudio | 11 | 10.9 (`AudioHardware{Create,Destroy}AggregateDevice`) |
| SystemConfiguration | 9 | — |
| ApplicationServices | 9 | 10.9 (`AXIsProcessTrustedWithOptions`) |
| CoreMedia | 8 | 10.7 |
| Carbon | 4 | — |
| OSAKit / IOKit / CoreVideo / CoreServices | 2 each | 10.7 |
| **Total** | **539** | **10.14** |

The single weak import is `_LSCopyApplicationURLsForBundleIdentifier` (CoreServices, 10.0), from
a C dependency. Weak imports bind to 0 rather than aborting the load, so they are never a load
bug; that there is essentially only one is itself informative — see §7.

### 4.1 The 15 Objective-C class references (the load-bug-shaped category)

These are the only classes bound at link time. Every one is Foundation/AppKit and ≤10.14:

```
NSAppleScript  NSBundle  NSData  NSDate  NSImage  NSNumber  NSObject  NSRunLoop
NSString  NSThread  NSTimer  NSURL  NSUUID(10.14)  NSUserNotification(10.7)
NSUserNotificationCenter(10.8)          + _OBJC_METACLASS_$_NSObject, _OBJC_EHTYPE_id
```

Nothing from ScreenCaptureKit (`SCStream` 12.3, `SCShareableContent`, `SCContentFilter`,
`SCStreamConfiguration`), nothing from the Core Audio tap (`CATapDescription` 14.2), and nothing
from the private display API (`CGVirtualDisplay*`, 10.14.4+). `nm -u | grep -E
'SCStream|SCShareable|SCContent|CATap|CGVirtualDisplay|AudioHardwareCreateProcessTap|
os_workgroup|CGPreflight|CGRequestScreen'` returns **nothing**.

This is the load-bearing result. `macos_utils/virtual_display.rs:26-47` uses `extern_class!` for
the four `CGVirtualDisplay*` classes and it is *safe*, because objc2's `extern_class!` resolves
the class through the ObjC runtime rather than emitting an `_OBJC_CLASS_$_` reference — the
comment in `CLAUDE.md` saying so is confirmed empirically here, not just asserted.

### 4.2 Frameworks on the streaming path (all 10.15-safe)

- **CoreGraphics (60)** — the CGDisplayStream fallback
  (`_CGDisplayStreamCreateWithDispatchQueue`, `_CGDisplayStreamStart/Stop`,
  `_CGDisplayStreamUpdateGetRects`, the five `_kCGDisplayStream*` keys), display configuration
  (`_CGBeginDisplayConfiguration`, `_CGConfigureDisplayWithDisplayMode`,
  `_CGDisplayCopyAllDisplayModes`, `_CGDisplaySetDisplayMode`), the fade reservation, and the
  whole `CGEvent*` input-injection surface. `CGDisplayStream` is deprecated in 14.0 but has
  existed since 10.8.
- **VideoToolbox (22)** — 7 `_VT*` functions plus 15 `_kVT*` constants, none newer than 10.9.
  The two constants that *would* have been too new (`MaximizePowerEfficiency` 10.14 and
  `PrioritizeEncodingSpeedOverQuality` 11.0) are correctly absent: `encoder.rs:561`'s
  `vt_optional_cfstring` `dlsym`s them instead. That fix is verified working.
- **CoreAudio (11)** — HAL only (`_AudioObject*`, `_AudioDevice*`, aggregate-device create/destroy
  at 10.9). The 14.2 tap entry points are absent, as designed:
  `process_tap.rs:40,44` `dlsym`s `AudioHardwareCreateProcessTap`/`...DestroyProcessTap`.
- **CoreMedia/CoreVideo (10)** — sample-buffer and IOSurface-backed pixel-buffer accessors only.
- **ApplicationServices (9)** — `AXIsProcessTrusted{,WithOptions}` (10.9) plus the AX element
  API used by the AirPlay Displays-menu driver, and `_CGDisplayCreateUUIDFromDisplayID`.

## 5. Runtime-resolved surface (the correct pattern, verified in use)

The checker reports these from the source; none of them appears in the undefined table, which is
the whole point.

| Mechanism | Names | Min version |
|---|---|---|
| `AnyClass::get` | `SCStream`, `SCShareableContent`, `SCContentFilter`, `SCStreamConfiguration` | 12.3 |
| `AnyProtocol::get` | `SCStreamOutput` | 12.3 |
| `AnyClass::get` | `CATapDescription` | 14.2 |
| `AnyClass::get` | `CGVirtualDisplay`, `...Descriptor`, `...Settings`, `...Mode` | private, 10.14.4+ |
| `AnyClass::get` | `NSArray`, `NSObject`, `NSString`, `NSUUID`, `WKWebView` | ≤10.14 (runtime anyway) |
| `dlsym` (fn) | `AudioHardwareCreateProcessTap`, `AudioHardwareDestroyProcessTap` | 14.2 |
| `dlsym` (fn) | `os_workgroup_interval_create`, `os_workgroup_join`, `os_workgroup_leave`, `os_release` | 11.0 |
| `dlsym` (fn) | `CGPreflightScreenCaptureAccess`, `CGRequestScreenCaptureAccess` | 10.15 |
| `dlsym` (fn) | `objc_setExceptionPreprocessor`, `CDVirtualDisplayCreate` | probe-only |
| `dlsym` (CFString const) | `SCStreamFrameInfoStatus`, `SCStreamFrameInfoDirtyRects` | 12.3 |
| `dlsym` (CFString const) | `kVTCompressionPropertyKey_{Quality,ColorPrimaries,YCbCrMatrix,MinAllowedFrameQP,MaxAllowedFrameQP}` | mixed |

`objc2` crates in `Cargo.lock`: `objc2`, `-app-kit`, `-cloud-kit`, `-core-audio`,
`-core-audio-types`, `-core-data`, `-core-foundation`, `-core-graphics`, `-core-image`,
`-core-location`, `-core-media`, `-core-video`, `-core-wlan`, `-encode`, `-exception-helper`,
`-foundation`, `-io-surface`, `-metal`, `-osa-kit`, `-quartz-core`, `-security`,
`-system-configuration`, `-ui-kit`, `-user-notifications`, `-video-toolbox`, `-web-kit`.
**No `objc2-screen-capture-kit`.** The C-function crates (`core-graphics`, `core-audio`,
`video-toolbox`, `core-media`, `core-video`) are what produce the `_CG*`/`_Audio*`/`_VT*`/`_CM*`/
`_CV*` undefined symbols above; the ObjC crates pull in classes only through the runtime.

## 6. The checker: `scripts/check-macos-dyld.sh`

```sh
./scripts/check-macos-dyld.sh <binary> <sdk-path> [options]

./scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend \
    /Library/Developer/CommandLineTools/SDKs/MacOSX10.15.sdk
```

Options: `--allowlist <file>` (default `scripts/macos-dyld-allowlist.txt`), `--src <dir>` (Rust
source to scan for runtime-resolved names, default `src-tauri/src`), `--floor <x.y>` (default
`10.15`), `--no-source-scan`, `--quiet`. Exit **0** clean, **1** offenders found, **2** usage or
environment error.

**How it decides.** The authoritative test is the SDK's `.tbd` export stubs, which are generated
from the dylibs that shipped with that SDK — "in the 10.15 stub" means "the 10.15 dyld can bind
it". The script builds a 601,854-symbol index from `System/Library/**/*.tbd` + `usr/lib/*.tbd`
and checks every strongly-bound undefined symbol against it. A symbol fails when it is absent
from the stubs, or when its SDK-header availability annotation says it was introduced after the
floor. Weak imports are always safe (they bind to 0 instead of aborting) and are reported as
`WEAK`.

Two subtleties the implementation had to handle, both discovered by getting them wrong first:

- `.tbd` files list plain C symbols under `symbols:` but Objective-C classes under a separate
  `objc-classes:` key as **bare** names, so the `_OBJC_CLASS_$_X` / `_OBJC_METACLASS_$_X` symbols
  the linker actually emits must be reconstructed. Without that, all 15 ObjC class refs — every
  one of them a 10.0–10.14 Foundation class — look "absent from the SDK" and the check reports 16
  false failures.
- `dyld_stub_binder` is the one exported symbol with no leading underscore. Filtering the index
  on `^_` drops it and makes *every* binary fail.

The `INTRO` column is **diagnostic only**: it is scraped from the availability macro nearest the
declaration in the SDK headers, which can misattribute when several declarations share a 3-line
window. The stub verdict is what gates; never relax it because `INTRO` looks old.

**Performance.** Two per-SDK caches are built in `$TMPDIR` on first run (export index ~30 s;
header availability corpus, 549,559 lines, ~1 min). Cold ≈ 1m51s, **warm ≈ 29 s**.

**All four decision paths were exercised on the Mac, not reasoned about:**

| Test | Result |
|---|---|
| Real release binary, floor 10.15 | `EXIT=0` — 539 undefined, 538 ok, 1 weak, 0 offenders |
| Same binary, `--floor 10.13` | `EXIT=1` — `FAIL _OBJC_CLASS_$_NSUUID 10.14 Foundation` |
| Synthetic binary with an unresolvable symbol | `EXIT=1` — `FAIL _ScreenExtendFakeFutureAPI ABSENT from the 10.15 SDK stubs` |
| Same synthetic binary, symbol allowlisted | `EXIT=0` — `allowlisted 1` |

`scripts/macos-dyld-allowlist.txt` ships empty, and says in its header that the correct fix for a
too-new symbol is `dlsym`/`AnyClass::get`/`msg_send!`, not an allowlist entry.

## 7. Why "0 weak imports" is the real story

Clang emits an automatic `weak_import` for a symbol whose `API_AVAILABLE` version exceeds the
deployment target — that is the mechanism that normally makes a too-new API *survive* on an old
OS. It does not operate here: the Rust FFI declarations (`unsafe extern "C" { ... }`, and the
`objc2-*` crates' generated externs) carry no availability attributes, so clang has nothing to
key on. Result: 538 of 539 undefined symbols are **strong**, and there is no automatic safety net
whatsoever. Every newer-than-floor API must be resolved at runtime by hand, exactly as
`CLAUDE.md` says, and the "check with `nm -u`" instruction is the only backstop. That is why the
checker exists and why it belongs in CI.

## 8. Stale references and doc/code discrepancies

- `CLAUDE.md` says "Current version: **0.5.4**"; `package.json`/`Cargo.toml`/`tauri.conf.json`
  are at **0.6.3** (commit `4b745d1`).
- `CLAUDE.md`'s dyld rule cites the two VideoToolbox constants that "had to move to
  `set_optional_bool`". Confirmed accurate: neither `MaximizePowerEfficiency` nor
  `PrioritizeEncodingSpeedOverQuality` appears in the undefined table.
- `macos_utils/streamer/sck.rs:24` says the runtime-interop approach "makes the historical
  weak-link / dlopen ScreenCaptureKit build task (**TODO 5.1**) moot". There is no TODO 5.1
  document in the tree — another reference surviving only in history.

## 9. Capability probing

Probing is by capability, not version, in the two places the tier doctrine cares about most:
`virtual_display.rs:336-339` probes `AnyClass::get("CGVirtualDisplay")` and friends;
`process_tap.rs:59` probes `tap_fns().is_some() && AnyClass::get("CATapDescription")`;
`sck_audio.rs:68-71` probes all four SCK classes.

One exception, and it is on the video path: `macos_utils/streamer/mod.rs:80`

```rust
pub fn screencapturekit_available() -> bool {
    macos_at_least(12, 3)
}
```

This is a version number where a class lookup would do — `sck.rs:199`'s `sck_class` already does
the right thing three files away. It is dyld-safe either way, so it is not a load bug, but it is
a tier-doctrine defect. That file is currently modified in another agent's working tree; whoever
owns it should fold in the change.

## 10. `cargo clippy --all-targets -- -D warnings`

**Fails, exit 101.** Three errors, all in `src-tauri/src/macos_utils/audio/mod.rs`, all
pre-existing at `bef5f89` (that file is untouched by the other agents' in-flight edits):

| Location | Lint | Detail |
|---|---|---|
| `macos_utils/audio/mod.rs:260:38` | `clippy::redundant_closure` | `get_or_init(\|\| std::thread::current())` → pass `std::thread::current` |
| `macos_utils/audio/mod.rs:270:29` | `unused_assignments` | `let mut burst_t0: u64 = 0;` overwritten at :284 before being read |
| `macos_utils/audio/mod.rs:271:28` | `unused_assignments` | `let mut burst_i: u64 = 0;` overwritten at :285 before being read |

`cargo build --release` reports the two `unused_assignments` as warnings ("`screenextend` (lib)
generated 2 warnings"); `redundant_closure` is clippy-only, which is why the earlier estimate of
"two warnings" was one short. `findings/minor-build-mac-clippy-warnings-fail-gate.md` is updated
with the exact three.

## 11. Findings written

| File | Severity |
|---|---|
| `findings/major-build-mac-release-links-against-an-unpinned-sdk.md` | major |
| `findings/major-build-no-dyld-floor-gate-in-release-ci.md` | major |
| `findings/minor-build-mac-deployment-target-10-12-contradicts-the-10-15-floor.md` | minor |
| `findings/minor-build-mac-clippy-warnings-fail-gate.md` (updated, was already open) | minor |

No finding was written for a load-bug candidate in the binary, because there are none.

## 12. Questions for the orchestrator

1. **The arm64 slice has never been audited by anyone.** The dev box can only produce x86_64
   (`lipo -info` → `Non-fat file`), and the 10.15 SDK cannot target arm64. CI ships
   `universal-apple-darwin`. The checker already passes `-arch all` to `nm`, so pointing it at
   the CI artifact covers both slices in one run — but that requires wiring it into the release
   job, and the runner has no 10.15 SDK. The stubs-only option in
   `findings/major-build-no-dyld-floor-gate-in-release-ci.md` is the cheapest way through; it
   needs a decision about vendoring ~a few MB of `.tbd` files.
2. **Is 10.15 still the intended floor?** `CGDisplayStream` is deprecated in 14.0 and obsoleted
   in the 15 SDK (another territory has a finding on that); the legacy audio tier exists purely
   for 10.15–12.x; the AirPlay display tier is already documented as below the supported floor.
   If the floor moved to 11.0 or 12.3 a large amount of runtime-interop machinery could become
   ordinary linking. Nothing in this audit argues for moving it — it just notes that the cost of
   holding it is real and currently unpriced.
3. **`macos_utils/streamer/mod.rs:80` is version-gated** (§9) and that file is checked out dirty
   by another agent. Someone should own the one-line change to a class probe so it does not fall
   between two work items.
