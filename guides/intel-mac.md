# ScreenExtend — Intel Mac, macOS 12.3–12.7.6, dual-GPU

**A standalone autonomous directive.** You are a fresh Claude Code instance (Fable-class
orchestrator, Opus 5 subagents) running on a machine that ScreenExtend's two development
machines could not be. Everything you need is in this file and in the repository you are
sitting in. There is no chat history, no other guide, no orchestrator to ask, and no other
agent working alongside you.

Your job is a band nobody else can reach. macOS **12.3 through 12.7.6** is the only version
range in which **ScreenCaptureKit video exists but ScreenCaptureKit audio does not** — SCK
audio is 13.0+, the Core Audio Process Tap is 14.2+ — so on this OS the shipping system-audio
path is ScreenExtend's own `AudioServerPlugIn` virtual-audio driver, selected as the *live*
tier on a *supported, modern* operating system rather than as a legacy safety net. 12.3 is
also ScreenCaptureKit's first release, which is where Chromium documents a full-display
capture stream that starts cleanly and then delivers nothing at all — the exact failure the
1500 ms first-frame commit gate exists to catch. And an Intel Mac with two GPUs is the only
machine on earth that can prove the encoder is being pinned to the GPU that actually drives
the captured display: on every single-GPU Mac, Apple Silicon included, the read-back is
`gpu_registry_id=none` and the change is untestable by construction.

Ten inventory items are assigned to this guide. They are numbered **M2-01 … M2-10** in §8 and
every one of them must appear in your final report with a verdict. Nothing else in the
repository is yours.

---

## 0. The machine contract

### 0.1 Bounds, and why each one exists

| | Value | Why |
|---|---|---|
| **Minimum OS** | macOS **12.3** (Monterey) | ScreenCaptureKit's first release. Below this `AnyClass::get(c"SCStream")` returns `None` and the SCK tier is not a candidate at all, so M2-01 has nothing to test. 12.3 specifically is the release Chromium reports the "starts and delivers nothing" full-display failure against. |
| **Maximum OS** | macOS **12.7.6** (the last Monterey build) | At **13.0** `SCStream`'s `capturesAudio` appears, `native_backends()` starts returning `ScreenCaptureKitAudio`, and the virtual-audio driver stops being the *selected* tier. Everything at or above 13.0 belongs to a different guide. If your Mac is on 13+, **stop** — you are not the right machine, and running this guide there produces a report about a tier the code did not choose. |
| **CPU** | **Intel x86_64** | Apple Silicon has no dual-GPU configuration, has no VideoToolbox hardware-encode frame-size floor (Chromium's measured minimum there is 16×16), and never reports a non-null GPU registry id. Half this guide is unreachable on arm64. |
| **GPU** | **Two GPUs**: an Intel integrated (UHD 630 / Iris Plus) **and** a discrete AMD Radeon Pro | M2-02, M2-03 and M2-04 are *only* testable here. The target machine is a 2018 or 2019 15-inch or 16-inch MacBook Pro (`MacBookPro15,1`, `15,3`, `16,1`, `16,4`). A 2017/2019 iMac with a discrete Radeon works. An Intel Mac mini or MacBook Air with an eGPU attached also works and is arguably a *better* test of cross-GPU IOSurface migration, because the bridge is Thunderbolt rather than on-package PCIe. |
| **Chassis** | A **laptop with a battery**, at a **console session** | `power.rs` takes `PreventUserIdleSystemSleep` / `PreventUserIdleDisplaySleep` IOKit assertions and `activity.rs` takes an `NSActivityLatencyCritical` token; neither can be observed without `pmset -g assertions` on a machine that actually has a power source to assert against. The project's dev Mac is reached over SSH with no user at the console, so every WindowServer- and display-dependent measurement there is qualified. Yours must not be. **Do not run this guide over SSH.** |
| **Toolchain** | Xcode Command Line Tools (`clang++`), Rust ≥ 1.83, Node ≥ 20, pnpm | `src-tauri/macos/ScreenExtendAudio/scripts/build.sh` uses `clang++` directly — no CMake, no Xcode project. |
| **Driver loading** | **SIP disabled**, *or* an Apple Developer ID (Application + Installer) | A Core Audio HAL plug-in is refused by `coreaudiod` unless it is signed **and notarized**, unless SIP is off. Without one of the two, M2-07, M2-08 and M2-09 cannot run at all. Prefer SIP disabled on a scratch machine: it is reversible (`csrutil enable` from Recovery) and needs no Apple account. Say in the report which you used — a Developer-ID run additionally closes the tier-1 half of `docs/CAPABILITY-TIERS.md`'s *macOS audio driver load* row, which no machine on this project has ever closed. |
| **Also useful** | A phone or tablet to join with; a second audio output (USB DAC, HDMI, Bluetooth headphones); QuickTime or OBS for encoder contention | M2-05 needs a competing hardware H.264 encode; M2-08/M2-09 need a real device to switch the default output away to and back. |

### 0.2 Preflight — run this before you change a single line

```sh
# --- the OS band --------------------------------------------------------------
sw_vers                                  # ProductVersion must be 12.3 … 12.7.6
uname -m                                 # must print x86_64 (NOT arm64)
sysctl -n machdep.cpu.brand_string
system_profiler SPHardwareDataType | grep -E 'Model Identifier|Chip|Total Number of Cores'

# --- two GPUs, and which one drives which display -----------------------------
system_profiler SPDisplaysDataType       # expect TWO "Chipset Model" blocks
pmset -g | grep -i gpu                   # gpuswitch = 2 means automatic switching is on
ioreg -l -w0 | grep -i "AGDCSwitch\|GPUConfigurationVariable" | head

# --- SIP and signing ----------------------------------------------------------
csrutil status                           # "disabled" is what makes the unsigned driver load
security find-identity -v -p codesigning # empty is fine if SIP is disabled

# --- toolchain ----------------------------------------------------------------
xcode-select -p && clang++ --version
xcrun --sdk macosx --show-sdk-path && xcrun --sdk macosx --show-sdk-version
rustc --version && cargo --version
node -v && pnpm -v

# --- the repository -----------------------------------------------------------
git -C . rev-parse --abbrev-ref HEAD     # expect latency-overhaul
git -C . log --oneline -1
git -C . status --short                  # must be clean before you start

# --- does the tree build at all, before you touch it? -------------------------
cd src-tauri && cargo check --all-targets && cargo test --lib -- --quiet
```

**Abort conditions.** `ProductVersion` outside 12.3–12.7.6; `uname -m` returning `arm64`;
`SPDisplaysDataType` reporting one chipset (you can still do M2-01, M2-05, M2-06, M2-07 …
M2-10, but say plainly in the report that M2-02, M2-03 and M2-04 were unreachable — do not
fabricate a dual-GPU result); `csrutil status` enabled **and** no Developer ID (the audio half
is unreachable — again, say so rather than guessing).

### 0.3 Confirm the tree is the one this guide describes

The macOS capture/encode work — `docs/OPTIMIZATIONS.md` rows **138–155**, **280–284**, **292–296**
— **has merged into `latency-overhaul`**, so every task in §8 is a *verification* task and there is
no Task 0. The check below takes a minute and exists so that you find out now rather than three
hours in. Run it and paste the output at the top of your report:

```sh
cd src-tauri
grep -n 'QUEUE_DEPTH\|minimumFrameInterval' src/macos_utils/streamer/sck.rs
#   expect QUEUE_DEPTH = 8 and a minimumFrameInterval of 0.9/fps
grep -rn 'gpu_registry_id\|PreferredEncoderGPURegistryID' src/macos_utils/
#   expect it in encoder.rs (the spec + the read-back), config.rs and pipeline.rs, and
#   display::metal_registry_id as the source — NOT gpu.rs, which stays an empty placeholder
grep -rn 'VTCopySupportedPropertyDictionaryForEncoder\|VTCopyVideoEncoderList' src/macos_utils/
grep -rn 'no frame within' src/macos_utils/streamer/mod.rs
#   expect the 1500 ms first-frame commit gate — the whole of M2-01
grep -n 'os_time_ns' src/macos_utils/streamer/sck.rs
#   expect super::clock::os_frame_time_or(...), not a literal 0: the SCK presentation timestamp
#   is read now. What has never been *measured* is its distribution — that is M2-01 item 5.
grep -rn 'fn capture_rate\|fn the_sps_promises_no_reordering' src/macos_utils/
#   expect cgds.rs::capture_rate, cgds.rs::capture_rate_downscaled, encoder.rs::the_sps_...
```

**If any of those disagrees**, you are on an older tree than this guide was written against. Say so
at the top of the report, then land the missing rows before measuring anything: each is written out
in full in `docs/OPTIMIZATIONS.md` with its hypothesis and its before/after,
`docs/research/macos-capture-encode.md` §5 (R1–R18) carries the API-level design, and
`docs/manual-tests/macos-capture-encode.md` names every log line each must emit. Do not try to
verify a row whose code is not present — that is how a `COMPILE-CHECKED` row gets marked `VERIFIED`
against nothing.

**One instrument is genuinely missing on every tree and you write it.** There is no ScreenCaptureKit
equivalent of the `capture_rate` pair, and SCK's presentation-timestamp distribution has never been
measured on any machine this project owns. Writing it is part of M2-01 and several of its
sub-questions depend on it. Add it as an `#[ignore]`d hardware test whose runtime gate goes through
`crate::test_support::skip_because`, like every other hardware-gated test in this tree.

---

## 1. Model and orchestration directives

**You are the orchestrator.** You plan, partition, integrate, arbitrate and verify. Push as
much concrete work down to subagents as you can — every file read, every research question,
every self-contained implementation, every review pass, every measurement run. Your context is
the scarce resource and it should be spent on synthesis and judgment, not on reading files a
subagent could have read for you.

**Every subagent runs on Opus 5.** No exceptions, no "this one is simple." If a subagent spawns
its own subagents, those are Opus 5 too.

**You are fully autonomous. There is nobody to ask.** The user has no access to this run until
it is finished. A question asked mid-run is a question that never gets answered.

- **Never ask and wait. Decide.** Record every judgment call in a **Decisions Made Without
  Consultation** section of the final report so the user can review and reverse any of them.
- **When in doubt, build it.** A finished implementation the user does not want is a one-minute
  revert; a deferred idea costs a future agent a full re-research cycle. You are already
  holding the context — spend it.
- **Self-unblock.** Missing information is a research task. Missing tooling is an install
  decision you make (ordinary developer tooling only — do not install persistent system
  services or change machine configuration outside the repository; recommend those in the
  report instead. Disabling SIP is the one exception and only because §0.1 requires it).
- **Leave the tree reviewable.** No half-finished refactors, no debug scaffolding in shipping
  paths, no commented-out experiments.

**Parallelise hard.** Run measurement passes concurrently where the hardware allows and
serially where it does not. On this machine specifically: **one release build at a time** (fat
LTO on four to eight cores is minutes, and two of them contend for the cargo lock), and **one
capture/encode measurement at a time** — a second VideoToolbox session or a second
CGDisplayStream/SCK stream changes the number you are taking. Research subagents, code review
subagents and doc subagents can all run while a measurement runs; another *measurement* cannot.

---

## 2. Absolute rules

### 2.1 Git

- **Never `git push`. Never touch `origin`.** Not to `main`, not to a branch, not a tag, not a
  force-push, not a `--dry-run`. Do not open pull requests, do not create issues or releases,
  do not modify workflow runs. `origin` is read-only to you. The user reviews and pushes
  personally, after the run.
- Work on a local branch: `git checkout -b guide/intel-mac`. Branch further per hypothesis if
  it helps; local history is yours to do anything with.
- Commits are small, single-purpose and reviewable. **A performance commit states its measured
  before and after in the message.** Commit at least every 30 minutes — an interrupted run
  should lose reasoning, not work.
- Every commit message ends with exactly:

  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: <the URL of your own session>
  ```

### 2.2 The scope fence

Everything outside the list below **has already been audited, optimised and verified on this
project's development machines**. A change outside the fence is a merge conflict against work
you cannot see, and it will be reverted.

**You may modify:**

```
src-tauri/src/macos_utils/streamer/sck.rs          M2-01
src-tauri/src/macos_utils/streamer/encoder.rs      M2-02, M2-05, M2-06
src-tauri/src/macos_utils/streamer/gpu.rs          M2-02, M2-04
src-tauri/src/macos_utils/streamer/mod.rs          M2-01 only: start_capture, the commit gate,
                                                   screencapturekit_available
src-tauri/src/macos_utils/streamer/cgds.rs         M2-01, M2-04 only: the fallback path and the
                                                   capture-rate harness
src-tauri/src/macos_utils/streamer/pipeline.rs     M2-04, M2-05 only: encoder lifecycle, the
                                                   in-flight cap, the first-frame deadline
src-tauri/src/macos_utils/streamer/qos.rs          M2-10 (read-backs and their logging)
src-tauri/src/macos_utils/streamer/tuning.rs       M2-10
src-tauri/src/macos_utils/streamer/power.rs        M2-10
src-tauri/src/macos_utils/streamer/activity.rs     M2-10
src-tauri/src/macos_utils/audio/legacy/**          M2-07, M2-08, M2-09
src-tauri/src/macos_utils/audio/mod.rs             M2-07 only: the tiering in probe_audio_backend
                                                   / native_backends / capable_backends
src-tauri/macos/ScreenExtendAudio/**               M2-07, M2-08, M2-09
src-tauri/Info.plist                               M2-03 only: NSSupportsAutomaticGraphicsSwitching
src-tauri/tauri.macos.conf.json                    M2-03 only: proving the plist reaches the bundle
tests under any of the above                       always
docs/OPTIMIZATIONS.md, docs/CAPABILITY-TIERS.md,
docs/BASELINE.md, docs/manual-tests/macos-*.md,
docs/research/*.md (append only), findings/        always
```

**You may not modify** — read freely, change nothing: `src/` (the React desktop UI),
`src-tauri/src/streamer/` (the cross-platform core: `server.rs`, `session.rs`,
`webrtc_session.rs`, `bitrate.rs`, `packetizer.rs`, `negotiate.rs`, `sockets.rs`, `pacer.rs`,
`repair.rs`, `keyframe.rs`, `congestion.rs`, `telemetry.rs`, `cloud.rs`, `input/`, `audio/`
except where a change is forced by an ABI you are changing), `src-tauri/src/streamer/static/`
(the client web page), `src-tauri/src/windows_utils/`, `src-tauri/src/linux_utils/`,
`src-tauri/src/macos_utils/{permissions,virtual_display,hosted_network,networking,
device_reporter,compatibility}.rs` (compatibility may take a *new field value* if a tier you
own reports differently, nothing more), `src-tauri/src/macos_utils/airplay/`,
`src-tauri/src/{lib,cli,logbus,single_instance}.rs`, `.github/workflows/`.

If a fix you need genuinely requires a file outside the fence, **write a `findings/` file
describing the change precisely enough that someone else could apply it, record it in
`docs/OPTIMIZATIONS.md` under "Recommendations requiring work outside this guide's fence", and
move on.** Do not make the change.

### 2.3 The invariants that bind this area

These come from `CLAUDE.md` and from the code. Violating any of them is a build break or a
shipped crash.

1. **No link-time references to macOS APIs newer than 10.15.** This is the single easiest way
   for you, specifically, to destroy this product. You are on 12.x. Every SCK symbol, every
   12.x VideoToolbox property key, every `CATapDescription`-era class is *present on your
   machine* and will link happily — and the resulting binary will fail to load on Catalina
   **before `main()` runs**, taking every unrelated feature with it. dyld binds every non-weak
   undefined symbol eagerly. `extern static` constants count: two VideoToolbox property keys
   (`MaximizePowerEfficiency`, 10.14; `PrioritizeEncodingSpeedOverQuality`, 11.0) had to be
   moved to `set_optional_bool` for exactly this reason. The only permitted mechanisms are
   `libc::dlsym`, `objc2::runtime::AnyClass::get`, `msg_send!`, `ClassBuilder`, and the
   `vt_optional_cfstring` / `set_optional_*` helpers already in `encoder.rs`. **Never** add
   `objc2-screen-capture-kit` or any `#[link(name = "ScreenCaptureKit")]`. Audit after every
   change:

   ```sh
   nm -u src-tauri/target/release/ScreenExtend | grep -i -E 'screencapturekit|displaystream|CATap|_SC[A-Z]'
   # must be empty
   scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend --floor 10.15
   otool -L src-tauri/target/release/ScreenExtend | grep -i screencapturekit   # must be empty
   ```

   The `--floor 10.15` form is what you use when you have no 10.15 SDK: it checks the header
   `INTRO` column against the newest SDK's stubs. If you *can* obtain a `MacOSX10.15.sdk`,
   pass its path instead — that is the authoritative tier, because the `.tbd` export stubs say
   exactly what 10.15's dyld can bind.

2. **Probe capabilities, never version numbers.** Every tier is selected by attempting the real
   capability and falling back at runtime without crashing; the selected tier is reported
   through `CompatibilityReport` and the log. `macos_utils/streamer/mod.rs` still *defines*
   `macos_at_least()` — it forks `sw_vers`, parses it, and **fails open**, returning `true` when it
   cannot read the version — but **nothing on the capture, encode or audio tiering path calls it any
   more**: `screencapturekit_available()` is `AnyClass::get(c"SCStream") && AnyClass::get(c"SCShareableContent")`
   in a `OnceLock`, and the VideoToolbox spec ladder replaced the old `macos_at_least(11, 0)` gate
   (whose version was wrong anyway — `EnableLowLatencyRateControl` is 11.3, not 11.0). Two tests in
   `macos_utils/audio/test/mod.rs` assert that the tiering source contains no `macos_at_least(`
   call, so the property is enforced rather than merely intended. **If you add a version check
   anywhere, you are adding a bug**, and one of those tests will tell you.

3. **Real-time threads take no locks and do not allocate.** The audio capture callbacks (the
   shared-memory reader's `AudioDeviceIOProc`, the playthrough IOProc, the `CGEventTap`
   callback) and the video capture callbacks (`did_output_sample_buffer` in `sck.rs`,
   `handle_frame` in `cgds.rs`) talk to the async world over `crossbeam-channel` only. The
   volume-key tap in particular runs with `kCGEventTapOptionDefault`, which means **every
   keyboard and mouse event in the whole login session waits behind it** — one CoreAudio IPC
   there and macOS disables the tap with `kCGEventTapDisabledByTimeout`. It classifies and
   `try_send`s. Keep it that way.

4. **`std::sync::Mutex` is the one blocking mutex for first-party code, and a `MutexGuard` is
   never held across an `.await`.** Keep critical sections short — lock, copy, drop, then
   await. Watch the temporary-lifetime footgun: a guard produced in an `if let` / `match`
   scrutinee lives to the end of the block, not the end of the line. The only two intentional
   async locks in the codebase are `DISPLAY_CORRELATION_LOCK` in `streamer/server.rs` and
   `receive_error` in `windows_utils/driver_ipc/client.rs`; do not add a third. `parking_lot`
   stays confined to the vendored `windows_utils/windows_capture/` fork.

5. **`tprintln!` / `teprintln!`, never `println!` / `eprintln!`,** in the Rust core. They feed
   the 2000-line ring backlog, emit a `LogLine` event to the desktop UI's Settings terminal, and
   echo to stdout only under `serve -v`. Direct `println!` is permitted only inside
   `#[cfg(test)]` modules and in `cli.rs`'s probe printing, which is outside your fence anyway.

6. **The shared-memory ring layout is a hard ABI contract.**
   `src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp` and
   `src-tauri/src/macos_utils/audio/legacy/shm_reader.rs` must change together. This is
   enforced, not merely requested: `legacy/test/ring.rs` `include_str!`s the C++ header,
   scrapes every constant and every `static_assert(offsetof(...))`, and fails `cargo test` if
   the two drift (`layout_matches_the_drivers_own_header`). The current layout is **version 2**,
   magic `0x31414553`, 128-byte header, 131072 f32 capacity, with `writePos` (driver),
   `readPos` (host), `overruns`, `generation`, and a seqlock-guarded
   `anchorSeq`/`anchorPos`/`anchorHostTime` triple carrying `mach_absolute_time` for a known
   sample — that anchor is what A/V sync rides. If you bump the layout, bump `LAYOUT_VERSION`
   on both sides; a POSIX shared-memory object can only be sized once on macOS, so the driver
   unlinks and recreates a segment of the wrong size at load.

7. **The desktop UI's appearance does not change.** Nothing in your fence touches it, so this
   should be free; if a change you want would be user-visible, do not make it — write it up in
   `docs/OPTIMIZATIONS.md` under "UI-visible recommendations" instead.

8. **A new Tauri command goes into all three `*_utils` modules and `collect_commands!` in
   `lib.rs`** — the Linux arm may `bail!`, but note that **`linux_utils/` does not compile today**
   and Linux is out of scope, so you cannot check that arm by building it — and
   **`src/lib/bindings.ts` is never hand-edited**; it is
   regenerated by a debug build started with no CLI arguments (`pnpm tauri dev`, run from
   `src-tauri/`), then diffed. `node scripts/check-bindings.mjs` is the no-build cross-check.
   You are unlikely to need a new command; if you do, `lib.rs` is outside your fence, so this
   becomes a `findings/` file.

9. **A new shell invocation goes into `src-tauri/capabilities/default.json`** with argument
   validators, or it fails at runtime. `installer.rs` shells out through the
   `elevated-command` crate to `/bin/sh -c`, and `routing.rs` shells out to `launchctl`; both
   are already covered. Adding a new one puts you outside the fence — write the finding.

10. **Simultaneous displays are uncapped** on every backend except the macOS AirPlay tier.
    Never evict a connected device to make room for a new one.

11. **DPR correctness:** the resolution multiplier and the OS scale step must be the same
    number. On macOS `platform::display_scale_steps()` is `[100, 200]`, so `snap_display_dpr`
    only ever yields 1.0 or 2.0 and HiDPI is a binary flag on `CGVirtualDisplaySettings`. This
    is outside your fence; it is here so you recognise it when a Retina client joins.

12. **No AI-tooling references in comments.** No "generated by", no agent names, no session
    URLs anywhere in source. The commit trailer is the one exception and it lives in the commit
    message, not in a file.

---

## 3. Norms of research and measurement

**Research before you conclude.** Load web search. Prefer primary sources: Apple's own
headers and archived QA notes, WWDC session transcripts, RFCs, and the source of Chromium,
OBS, Moonlight/Sunshine, FFmpeg and webrtc-rs. **If one shipping product demonstrably does the
thing, the thing is possible** — "impossible" is a conclusion you may reach only after (a) a
research pass found nobody doing it and (b) you tried it and measured. Never conclude from
priors.

**Measure before and after every change, as p50 / p90 / p99 / max — never a bare mean.** Jitter
is felt more than average latency; a pipeline with 20 ms mean and 60 ms p99 feels worse than
one with 30 ms mean and 33 ms p99. Nanosecond stamps come off the shared host clock,
`streamer::audio::host_now_ns()` — the same epoch the RTP timestamps ride, which on macOS is a
monotonic `Instant` epoch backed by `mach_absolute_time`.

**Never trust a flag without a read-back.** Every property you set must be read back and
logged; every tier you select must announce itself. This path has three live examples of why:

- `VTSessionSetProperty` returns `noErr` for properties the selected encoder ignores, and the
  `hardware` flag the old code recorded on success was the flag that had been **requested**, not
  the one that was granted — a session created on the `Require` rung can still be software.
  `kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder` is the read-back.
- `kVTCompressionPropertyKey_UsingGPURegistryID` (10.15) returns **NULL for both an
  integrated-GPU hardware encoder and a software encoder**. It cannot be used to detect the
  software fallback. That is `UsingHardwareAcceleratedVideoEncoder` and `EncoderID`'s job.
- A `CATapDescription`-era tap can return `noErr` and deliver pure silence. Different tier,
  same lesson: acceptance is not delivery.

**Measurement hygiene on this machine.**

- Release build only for latency numbers (`lto = "fat"`, `codegen-units = 1` are already in
  `Cargo.toml`'s release profile). A debug build's numbers are not comparable to anything in
  `docs/BASELINE.md`.
- ≥ 3 trials per cell; report the median of the trials alongside p50/p90/p99/max within a
  trial, and say how much the trials disagreed.
- **Check for competing compilers first**: `ps -eo comm | grep -E '/cargo$|rustc$|clang'`. A
  fat-LTO link next door invalidates a run.
- Keep the screen awake: `caffeinate -u -t 5` immediately before, and run the trial under
  `caffeinate -dims`. A locked screen stalls compositing and the capture numbers silently
  become meaningless.
- **Leave ≥ 15 s between probe runs.** A probe that exits without tearing down leaves its
  virtual display attached briefly and corrupts the next run's capture. This bit four of twelve
  runs during the original overhaul; it is a real effect, not a superstition.
- On a dual-GPU laptop, **state the power source and the thermal state** with every number.
  `pmset -g batt`, and `sudo powermetrics --samplers smc -n 1 -i 1000 | grep -i -E 'CPU die|GPU
  die|Fan'` before and after a long run. A number taken while the chassis is thermally throttled
  is a different number.

**Token and time cost are not constraints.** Every optimisation that shaves **0.1 ms** on this
one OS band on this one machine class is in scope and gets implemented and measured, even at
high implementation cost. "Good enough", "diminishing returns", "minor" and "speculative" are
not reasons to stop. A speculative idea is resolved by trying it and measuring, or by a
**recorded rejection with a reason and a number** in `docs/OPTIMIZATIONS.md` — never by being
skipped.

---

## 4. The repository, the harness, and the paperwork

### 4.1 What ScreenExtend is

A Tauri 2 desktop app that turns any device with a web browser into a wireless second monitor.
It runs on the **host** (the PC or Mac being extended). A **client** — a phone, tablet or
laptop — joins by opening a URL or scanning a QR code; there is nothing to install on the
client. The host creates a **real virtual display** per client, GPU-encodes it to H.264, and
streams it over **WebRTC with WHEP signalling** from its own axum HTTP(S) server. System audio
rides an Opus-encoded DataChannel beside it. Remote keyboard, mouse, pen and clipboard come
back the other way.

Two frontends live in the repo: `src/` is the React desktop control UI that runs in the host's
Tauri webview, and `src-tauri/src/streamer/static/` is the dependency-free browser page served
to joining devices — plain HTML/JS/CSS, `include_str!`'d into the binary, **no bundler**.
Neither is yours.

The Rust core is `src-tauri/src/`, with three `cfg`-gated sibling platform modules —
`windows_utils/`, `macos_utils/`, `linux_utils/` — meant to expose the same surface, glob-imported
by `lib.rs`, so the Tauri command list is identical on every OS and dispatch happens at compile
time. `streamer/platform.rs` is the runtime dispatcher that calls into the active backend for
capture, encode, tuning and audio start. **`linux_utils/` does not compile**, implements only part
of the command set, and several of its stubs report success rather than "unsupported"; Linux is
declared out of scope for this engagement and no guide covers it.

### 4.2 The territory, file by file

```
src-tauri/src/macos_utils/streamer/
  mod.rs          CaptureBackend trait, CaptureError, PIXEL_FORMAT_420F/BGRA,
                  macos_at_least(), screencapturekit_available(), start_capture()
                  — the SCK→CGDisplayStream fallback and (row 153) the first-frame commit gate
  sck.rs          the ScreenCaptureKit backend. Runtime interop ONLY: AnyClass::get,
                  msg_send!, a ClassBuilder-built SCStreamOutput delegate, SCK CFString
                  constants via sck_optional_cfstring (dlsym). Hot path is
                  did_output_sample_buffer → frame_is_complete_and_dirty → publish_sample.
  cgds.rs         the CGDisplayStream fallback. Deprecated API, de-linked behind dlsym
                  (row 140) — the API is *obsoleted* in the macOS 15 SDK.
  encoder.rs      the VideoToolbox H.264 compression session: the spec ladder in
                  Encoder::new, configure(), configure_rate_control(), set_bitrate(),
                  log_hw_acceleration(), the Annex-B conversion, and the ignored tests
                  encode_synthetic / recreate_churn_releases_hw / creates_small_via_software /
                  hw_capability_probe.
  gpu.rs          an empty placeholder (`Gpu { _private: () }`) — deliberately, per research R12:
                  WindowServer already delivers the frame scaled and in 420f, so a Metal
                  convert/scale stage was rejected. Row 143's registry id does NOT live here: it
                  is read by `display::metal_registry_id(display)`, stored on `EncoderConfig`
                  (`config.rs`) by `pipeline.rs`, and consumed by `encoder.rs::build_encoder_spec`.
  pipeline.rs     the encode loop, the in-flight cap, the first-frame deadline, probe_live,
                  probe_bitrate, display geometry and mode helpers, and the ignored tests
                  end_to_end_latency / production_pipeline_smoke.
  frame.rs        Frame / FrameSink / Backing::PixelBuffer
  qos.rs          pin_current_thread_user_interactive / _user_initiated /
                  _time_constraint(period_ns) / raise_current_thread_precedence /
                  pin_current_thread_encode_affinity
  tuning.rs       apply_process_tuning()
  power.rs        KeepAwake — IOPMAssertionCreateWithName for PreventUserIdleSystemSleep and
                  PreventUserIdleDisplaySleep
  activity.rs     LatencyActivity — NSProcessInfo beginActivityWithOptions:
                  (UserInitiated | LatencyCritical)
  mach.rs         mach_absolute_time
  display.rs, config.rs, clock.rs

src-tauri/src/macos_utils/audio/
  mod.rs          AudioBackend enum, probe_audio_backend(), native_backends() (OnceLock-cached),
                  capable_backends() (recomputed — install state flips at runtime),
                  make_backend(), start_capture(), the encoder worker, anchored_ns(),
                  choose_capture_ns(), SETUP_TIMEOUT = 15 s
  ring.rs         the SPSC f32 ring with a capture-time Anchor
  format.rs       ASBD parsing, planar→interleaved, BS.775 downmix
  process_tap.rs  14.2+ tier — not a candidate on your OS
  sck_audio.rs    13.0+ tier — not a candidate on your OS
  opus_encoder.rs a re-export of the shared streamer/audio Opus wrapper
  legacy/
    mod.rs        LegacyVirtualDeviceSource::start — the whole tier's assembly order, and
                  ControlRuntime, the crossbeam select loop that owns routing/volume/underruns
    probe.rs      LegacyState { Ready, NeedsInstall, InstalledButUnhealthy }, 2 s CACHE_TTL,
                  invalidate(), device_present(), driver_healthy()
    installer.rs  install_pkg() / uninstall() behind one elevated prompt, wait_for_device(),
                  the coreaudiod kickstart retry, InstallOutcome
    routing.rs    Router<HalDefaultDevicePort> (activate/restore/on_default_changed),
                  RoutingState with owner_pid, recover()/recover_on_launch()/watchdog_recover(),
                  install_watchdog_agent()/uninstall_watchdog_agent(), RoutingListeners,
                  FormatListener, preferred_playthrough_device()
    shm_reader.rs the layout-2 shared-memory transport and the HAL-input fallback,
                  Transport::{Shm, HalInput}, CaptureTargets::publish, clamp_read
    playthrough.rs the monitor IOProc, MonitorGain, setpoint_frames/drift_trim/ramp
    volume_proxy.rs compute_gain, apply(), VolumeListeners on VolumeScalar + Mute
    volume_keys.rs the CGEventTap backstop for NX_KEYTYPE_SOUND_UP/DOWN/MUTE, opt-in via
                  set_legacy_volume_key_proxy or SCREENEXTEND_LEGACY_VOLUME_TAP
    hal.rs        the CoreAudio HAL helpers (device_by_uid, default_output_device,
                  buffer_frame_size_range, output_volume_scalar, …)
    branding.rs   DEVICE_UID, INSTALL_PATH, SHM_NAME
    test/         live.rs (legacy_live_capture_roundtrip, legacy_live_latency_and_monotonicity —
                  both #[ignore]d, both require the driver loaded), ring.rs
                  (layout_matches_the_drivers_own_header), routing.rs, mod.rs

src-tauri/macos/ScreenExtendAudio/     the AudioServerPlugIn itself (C++, libASPL/MIT)
  src/main.cpp, device.cpp/.hpp, shm_ring.cpp/.hpp, volume_control.cpp/.hpp,
  branding.hpp (every branded string), log.hpp
  scripts/build.sh (clang++ direct; SE_ARCHS), fetch-libaspl.sh (pinned commit),
  package.sh, sign_and_notarize.sh, release.sh (build→sign→package→notarize→staple→stage),
  postinstall (chown root:wheel, chmod go-w, launchctl kickstart -k
              system/com.apple.audio.coreaudiod, else killall coreaudiod)
  tools/shmcheck.c (segment inspector), hal_test.c, hal_render.c
  Info.plist, README.md
```

### 4.3 The measurement harness

The shipped binary is both the GUI app and a full CLI. `probe` is intercepted in `cli.rs`
before the Tauri plugin router; it is a developer harness, not a shipped command.

```sh
cd src-tauri && cargo build --release --features probe
BIN=./target/release/ScreenExtend

# whole host path: a real WHEP join over loopback against a real virtual display,
# per-stage telemetry printed on exit. Needs --features probe.
$BIN probe --whep-selftest --duration 30

# real content through the real capture path, to an Annex-B file
$BIN probe --live out.h264 --duration 30

# scripted synthetic workloads (implies --synthetic-pattern)
$BIN probe --live --duration 30 --workload <mixed|static|scroll|motion|drag>

# glass-to-glass overlay: a large frame number + elapsed-ms clock painted into the
# encoded frames, so a 240 fps camera shot of host and client can be differenced
$BIN probe --live --duration 60 --synthetic-pattern --workload motion --counter

# single-frame capture, encoder smoke, adaptive-bitrate path
$BIN probe --capture cap.png ; $BIN probe --encode out.h264 ; $BIN probe --bitrate

# system audio: capture period as delivered, packet age at the subscriber,
# capture→subscriber latency, capture_ns skew, packet-size histogram
$BIN probe --audio --duration 15

# tuning flags accepted by every mode
#   --encoder <auto|nvidia|intel|software>   --scale <10..100>   --fps <n>   --max-fps <n>
#   --qp <1..51>   --h264-profile <baseline|main|high>   --intra-refresh <on|off>
#   --monitor <n>   --max-bitrate-kbps <n>   --disable-gpu-encode
#   --stun / --turn-* / --port / --https-port / --bind-ip / --lan-ip
```

From a running host:

```sh
ScreenExtend serve -v          # headless host; one summary line per session every 5 s
ScreenExtend status --json     # the same counters, per session, from another shell
ScreenExtend doctor --json     # capability report + per-probe timings
ScreenExtend logs --lines 400  # the 2000-line ring backlog
```

The `#[ignore]`d hardware tests are the finer instruments:

```sh
cd src-tauri
cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::pipeline::tests::production_pipeline_smoke -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::hw_capability_probe -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::encode_synthetic -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::creates_small_via_software -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::recreate_churn_releases_hw -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::probe_main_display -- --ignored --nocapture
cargo test --lib legacy_live -- --ignored --nocapture     # both driver round-trip tests
```

Three more exist and are the ones `docs/BASELINE.md` quotes:

```sh
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate            -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate_downscaled -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::the_sps_promises_no_reordering -- --nocapture
```

`capture_rate` prints the delivered frame count, the WindowServer drop count from
`CGDisplayStreamUpdateGetDropCount`, the mean time inside the delivery callback, and the
`displayTime → callback` histogram; `the_sps_promises_no_reordering` decodes the emitted SPS and
asserts `max_num_reorder_frames == 0`, because `AllowFrameReordering = false` is a request and the
VUI field is what a decoder actually obeys. **The SCK equivalent of the `capture_rate` pair does not
exist and you write it** (§0.3).

**Skipping convention.** A hardware- or OS-gated test never returns silently: it calls
`crate::test_support::skip(what, reason)` or `skip_because(what, reason)`, which prints
`SKIP <what>: <reason>` on stderr, and **`SE_REQUIRE_HW=1` turns a skip into a failure**. Set
`SE_REQUIRE_HW=1` for every capability this machine is supposed to have — that is how you find a
test that is quietly not running. Anything you add follows the same pattern, with
`#[ignore = "<reason>"]` so the harness prints the reason rather than a bare `ignored`.

The client page has its own harnesses, run with `pnpm test:client` (`scripts/test-client.mjs`, seven
of them). Nothing in this guide touches the client page, so you should not need them — but run them
once before your first commit so a red result is not attributed to you later.

`docs/TELEMETRY.md` explains every counter. The degradation counters that matter here are
`capture_fallback`, `capture_no_first_frame`, `frames_repeated`, `frames_dropped_ring`,
`encoder_output_drops`.

### 4.4 The ledgers

- **`docs/OPTIMIZATIONS.md`** — one row per change attempted:
  `| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |`.
  Failures are recorded on purpose; a reverted optimisation with a number is valuable.
  Status vocabulary: **`VERIFIED`** (executed and measured on a machine, which machine named),
  **`COMPILE-CHECKED`** (builds and passes static/layout tests, never executed),
  **`UNVERIFIED`** (written from research, no runtime signal). **Your job is to move this
  guide's rows to `VERIFIED` with numbers, or to revert them with reasons.** Append new rows
  for anything you add; never renumber existing ones.
- **`docs/CAPABILITY-TIERS.md`** — one row per subsystem × tier:
  `| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |`.
- **`docs/BASELINE.md`** — the pre-change numbers and the competitor table. Fill the macOS
  cells for *your* machine as a new sub-table; do not overwrite the 10.15.8 one, which is a
  different machine and a different tier and is the floor everything is compared against.
- **`docs/manual-tests/macos-capture-encode.md`** and **`docs/manual-tests/macos-audio.md`** —
  the scripts you are executing. Update them where you find them wrong, and record your
  machine's readings in the "Record:" slots they already contain.
- **`docs/research/*.md`** — append-only. `macos-capture-encode.md` §5 R11/R13/R14/R15/R18 and
  `audio-capture-tiers.md` are the design documents behind most of your tasks.

### 4.5 `findings/` — the work queue and the crash-recovery point

`findings/` at the repository root is gitignored (through `.git/info/exclude`, not
`.gitignore`). **Every problem, opportunity, suspicion and idea becomes one file** —
`<severity>-<component>-<slug>.md`, severity in `blocker | major | minor | speculative` — with
this schema:

```
# <Title — one line>
Status: open
Severity: blocker | major | minor | speculative
Component: <subsystem>, <files>, <platform>
Evidence: what you observed, with numbers and file:line references
Hypothesis: what you think is going on, and what you are unsure about
Proposed fix: concrete enough that someone else could implement it
Verification: how to prove it worked; what to measure before and after
Context: anything a fresh agent would otherwise have to re-derive
```

One finding per file — batched files cannot be individually completed, which breaks the whole
mechanism. **A file is deleted the moment its finding is resolved** — implemented and verified,
or deliberately rejected with the reasoning recorded in `docs/OPTIMIZATIONS.md` *first*.
Deleting is how you mark it done: no archive, no `done/` folder. Write the file when you find
the thing, not at the end of a session; its entire value is in being accurate at whatever
arbitrary moment you get cut off. Hand the relevant files to subagents as part of their briefs
and have them update or delete their own.

The lifecycle is **empty → very full → empty**. A non-empty `findings/` at the end means the
work is not done. Anything you deliberately will not do gets its file deleted *and* a line in
the final report saying why.

**`findings/` is essentially empty when you start, and that is deliberate** — the queue was worked
down to nothing before this guide was written, and everything that survived became either a task in
§8 or a recorded rejection in `docs/OPTIMIZATIONS.md`. Read whatever is there once, at the start; it
is unlikely to be yours, and files under `findings/_out-of-scope/` (Linux) never are.
`findings/_briefs/` and `findings/_STATUS.md` are historical orchestration notes, not a queue.

### 4.6 The gates

Before every commit, from `src-tauri/`:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

After any FFI change (which is most of what you will do), additionally:

```sh
# from the repo root
scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend --floor 10.15
#   ... or, if you have one, against the real thing, which is the authoritative tier:
scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend /path/to/MacOSX10.15.sdk
```

If you touch `src/` or `src/lib/bindings.ts` (you should not), also from the root:
`pnpm lint`, `pnpm build`, `node scripts/check-legacy-webkit.mjs`,
`node scripts/check-bindings.mjs`, `node scripts/check-versions.mjs`.

---

## 5. Where this area stands right now

### 5.1 What has landed, and what it is worth
**A note on the `W…` labels you will meet in the ledgers, in the risk register and in this file.**
The engagement that produced this repository's current state ran as numbered parallel workstreams:
`W1` transport · `W3a`/`W3b` Windows capture and encode · `W4`/`W4b` AMD · `W5` macOS capture and
encode · `W6` macOS permissions · `W7` macOS audio · `W8` Windows audio · `W9` Windows platform ·
`W10` input · `W11` the client page · `W12` the desktop UI · `W13a`/`W13b` build, CI and hygiene ·
`W14` AirPlay · `W15` tests. They are **provenance labels only** — they tell you which change series
a row came from so a ledger note reads in context. They are not people, none of them is still
running, nothing is waiting on one, and **every one of them has merged into `latency-overhaul`.**


The macOS capture/encode path was rebuilt in the original engagement. The relevant
`docs/OPTIMIZATIONS.md` rows, and what each is worth to you:

| Row | What | Status you inherit |
|---|---|---|
| 138 | CGDisplayStream: `IOSurfaceIncrementUseCount`/`Decrement` around every held surface (RAII `HeldSurface`), `kCGDisplayStreamQueueDepth` 2→5, `kCGDisplayStreamMinimumFrameTime` deleted | VERIFIED on 10.15.8 |
| 139 | the first frame of a session is exempt from the empty-dirty-rect skip | VERIFIED on 10.15.8 |
| 140 | CGDisplayStream de-linked — five functions and five CFStrings behind one cached `dlsym` | VERIFIED on 10.15.8 |
| 141 | `VTCopyVideoEncoderList` logged once per process; `VTCopySupportedPropertyDictionaryForEncoder` per session; every `macos_at_least` gate deleted from this path | VERIFIED as a *mechanism* on 10.15.8; **the key sets of 12.x encoders are UNVERIFIED — that is you** |
| 142 | `UsingHardwareAcceleratedVideoEncoder` read back and stored; a software session logs at error level; `CompatibilityReport.encoder_backend` reports the probed tier | VERIFIED on 10.15.8 |
| **143** | **`PreferredEncoderGPURegistryID` from `CGDirectDisplayCopyCurrentMetalDevice` of the captured display** | **COMPILE-CHECKED — reads `gpu_registry_id=none` on every single-GPU Mac. This is M2-02 and only you can close it.** |
| 144 | `DataRateLimits` removed; `MaxFrameDelayCount` unset; `MaxKeyFrameIntervalDuration` 10 s → 600 s | VERIFIED on 10.15.8 |
| 145 | per-frame stamps travel through VideoToolbox as `sourceFrameRefCon` instead of through a `Mutex<VecDeque>` | VERIFIED on 10.15.8 |
| 146 | encoder output channel bounded at 4, drop-oldest, counted, and the count forces an IDR | VERIFIED on 10.15.8 |
| **147** | **frames in flight capped at 2, standing aside to 16 after 100 ms of no output; the blocking `CompleteFrames` gone from the encode loop** | VERIFIED on 10.15.8 — **but only tier 2 was exercised there, because that encoder structurally holds frames. A 12.x low-latency `…rtvc` encoder is one-in/one-out and exercises tier 1, which nothing has.** |
| **148** | **three consecutive submit failures rebuild the encoder** | VERIFIED as code; the *trigger* was never reproducible on the dev box. **The row's owner is the modern-Mac run**, which has the same contention setup; M2-05's step 4 fires it here too, and what you contribute is a **second data point** — record your numbers in the row's Notes rather than flipping its status. |
| 149 | a 750 ms first-frame deadline yields a synthesized black IDR, counted as `capture_no_first_frame` | VERIFIED on 10.15.8 |
| **150** | **one scheduling scheme per thread; every `thread_policy_set`/`pthread_set_qos_class_self_np` return checked and logged once; `THREAD_TIME_CONSTRAINT` computation `period/8` → `0.75 × period`, read back with `thread_policy_get`; `os_workgroup` deleted; `setpriority(-10)` deleted** | VERIFIED on 10.15.8 over SSH — **never with a console session, a battery, or App Nap in play. That is M2-10.** |
| 151 | the 60 fps floor in `live_encoder_config` deleted | VERIFIED on 10.15.8 |
| **152** | **SCK: `queueDepth` 3→8, `minimumFrameInterval` 0.9/fps, `SCFrameStatusStarted` accepted, presentation timestamp read, colour matched to the CGDisplayStream path, `stop()` blanks the delegate ivar and drains the delivery queue before freeing its context** | **COMPILE-CHECKED — the dev Mac is 10.15.8, where SCK does not exist.** The row's **owner is the modern-Mac run**, which A/Bs the configuration on 14.2+; here it is half of M2-01 and what you contribute is 12.3's answer, recorded in the row's Notes. |
| **153** | **capture tier chosen by `AnyClass::get`, and SCK not committed to until it has delivered a frame (1500 ms)** | The class probe is VERIFIED (10.15.8 correctly reports CGDisplayStream); **the frame gate is UNVERIFIED and this guide OWNS it** — 12.3 is the release Chromium documents the "starts and delivers nothing" failure against, so this is the only machine that can genuinely fire it. That is M2-01's core. |
| 154 | `pipeline::start` returns a `SessionCapture` instead of `mem::forget`ing the backend | VERIFIED on 10.15.8 |
| 155 | `ConstantBitRate` and `BaseLayerFrameRate` probed and reported, deliberately not set | VERIFIED on 10.15.8 |
| **178** | **`NSSupportsAutomaticGraphicsSwitching = true` in `src-tauri/Info.plist`** | **COMPILE-CHECKED. This is M2-03.** |
| 264 | `bundle.macOS.infoPlist` and `minimumSystemVersion` declared in both configs; the audio `.pkg` carried by `bundle.macOS.files` | COMPILE-CHECKED — the bundle assertions run in release CI, which has never run. **The row's owner is the modern-Mac run** (which builds signed and universal); M2-03 step 1 and M2-09 step 1 give it its first real evidence, recorded in the Notes. |
| 280–284 | the VideoToolbox session-spec ladder, LTR probing, encoder identity reporting | mixed; the low-latency rung and `EnableLTR` are absent on 10.15 |
| 283 | LTR: still probed and reported, still **not** enabled | DEFERRED. `EnableLTR` without an acknowledgement loop is a loss, not a partial win: the encoder enters a reference-management mode nobody drives and `ForceLTRRefresh` with nothing acknowledged emits an IDR — today's behaviour at a cost. The transport half (`SharedFeedback::acked_frame_capture_ns()`) landed with `w1-closeout`. **It is not yours; it belongs to a modern-Mac run.** |
| **292** | **the measured hardware-encode floor on a 2019 Intel Mac, and the decision not to work around it** | VERIFIED on 10.15.8 — see §5.3, this substantially changes M2-06 |

### 5.2 The audio tier as it stands

The virtual-audio driver tier is the **most thoroughly verified subsystem in this codebase** —
on 10.15.8, where it is the *only* candidate. `docs/CAPABILITY-TIERS.md` records it as
VERIFIED end to end: built, installed, loaded, captured, torn down. What you add is that the
same tier is *selected*, *correct* and *fast* on a supported modern OS, on different hardware,
with real competing audio devices, and with a console user pressing the volume keys.

The numbers to beat, all from macOS 10.15.8 Intel with SIP disabled, driver built from the tree
and loaded by a `coreaudiod` kickstart (no reboot), three 10 s runs through the shipped
`start_capture()` path driven by `say`:

| | p50 | p90 | p99 |
|---|---|---|---|
| capture → Opus packet | **−0.57 ms** | **3.59 ms** | **4.54 ms** |
| Opus encode | 0.09–0.12 ms | | 0.16–0.27 ms |
| capture → subscriber (`probe --audio`) | **1.84 ms** | **3.67 ms** | **5.24 ms** (max 6.76) |

- `capture_ns` deltas: **0 of 3277** were anything other than exactly 5.000 ms.
- IO cycle: **128 frames (2.67 ms)**, requested and granted.
- **2,083,840 samples** through the shared-memory ring with `readPos` tracking `writePos`,
  backlog 0, **overruns 0**.
- A fourth run with a 3 s silent gap: exactly **1 delta of 1684** was not 5.000 ms (the
  re-seat), zero regressions, zero sequence gaps. The virtual device runs no IO cycle while
  nothing is playing, so the packet stream genuinely pauses and costs nothing on the wire —
  which is as close to DTX as this pipeline gets, since Opus in `RESTRICTED_LOWDELAY` is
  CELT-only and cannot do it. Expect ~50 packets/s while a phrase plays, not 200.
- For contrast, the Windows endpoint-loopback tier measures capture → subscriber
  p50 **9.44** / p99 **9.44 ms**. That is what a 2.67 ms IO cycle buys against a 10 ms engine
  period, and it is the reason this tier is not merely a compatibility shim.

The `p50` being **negative** is deliberate and correct: the driver stamps the mix with
`mach_absolute_time()` in `OnProcessMixedOutput`, which is when the mix exists and *before* it
would have been heard, so a packet reaches the encoder ahead of its own audio's playout.

### 5.3 What was already tried and rejected here — with the numbers

Do not re-try these blindly. Each one is a recorded rejection; if you think it is wrong, the
way to reopen it is a measurement on *your* hardware, not an argument.

**macOS capture and encode**

- **One shared VideoToolbox session between clients at the same resolution** — rejected as a
  session-lifecycle change rather than an encoder one. The detection half landed instead:
  `InstanceLimit` is logged and a software fall-back is logged loudly.
- **Mid-session SCK → CGDisplayStream fallback** — rejected. The tier is committed only after a
  delivered frame (that is the 1500 ms gate), and the first-frame deadline — 750 ms →
  synthesized black IDR, counted as `capture_no_first_frame` — is the watchdog that helps
  instead. Do not add a mid-session swap.
- **A Metal convert/scale stage in `gpu.rs`** — rejected (research R12). WindowServer delivers
  the frame already scaled and already in `420f`; a compute pass would add a copy to remove
  nothing. This is why `gpu.rs` is an empty placeholder, and it is also what makes
  `NSSupportsAutomaticGraphicsSwitching` safe to set: QA1734 attaches a condition (the app must
  work correctly with multiple GPUs) that is satisfied by construction, because nothing in this
  process does direct GPU work.
- **`kVTCompressionPropertyKey_ConstantBitRate`** — rejected. CBR spends bits on a still screen
  to hold the rate, which is the opposite of what a screen sharer wants; it is also why the
  removed `DataRateLimits` was not replaced with one.
- **`BaseLayerFrameRate`** (hierarchical-P temporal layering) — probed and reported, not set: it
  adds reference depth to buy a scalability nothing here consumes.
- **`DataRateLimits`** — removed outright. QA1958 defines it as a hard cap over a sliding window
  while `AverageBitRate` is a target; Chromium documents the cap as undocumented-incompatible
  with `EnableLowLatencyRateControl` and as making the encoder undershoot regardless. The
  loss-based controller in `webrtc_session.rs` already owns the rate. **If you reintroduce it,
  you are fighting two things at once.**
- **Upscaling a small client to reach the Intel hardware encoder** — rejected with a measurement
  (row 292). Spending bitrate to save CPU at the one content size where software H.264 is cheap
  is the wrong trade. The defect was that it happened *silently*; the loud identity line is the
  fix.

**macOS audio**

- **A play-cursor clamp on macOS** — rejected on the number. Measured skew p50 **−0.57 ms**
  (min −6.23) over 3277 packets: under one 2.67 ms IO cycle and in the *right* direction.
  Applying the Windows clamp would flatten the ladder that produces exactly-5.000 ms spacing
  (0 of 3277 deltas were anything else). Windows needs the clamp because its endpoint
  timestamp is a play cursor about 15 ms in the *future*; macOS's is a capture time slightly
  in the past. **Do not port the clamp.**
- **`mach_wait_until` for the shared-memory reader** — rejected. The time-constraint policy was
  the defect; `user_interactive` plus a 250 µs poll is the whole win.
- **A sample-rate converter** — rejected. Opus fixes 48 kHz and all three tiers negotiate or
  declare it. The `teprintln!` that fires if a backend ever delivers another rate is the
  trigger to revisit; the HAL-input fallback now *refuses* a non-48 kHz device rather than
  reinterpreting whatever arrives as stereo f32.

### 5.4 The baselines you are measured against

**The 10.15.8 Intel dev Mac** — CGDisplayStream + VideoToolbox, no low-latency rate control, no
SCK, no LTR. It is the **slowest configuration the code supports**, and your 12.x dual-GPU Mac
should beat every one of these numbers.

Capture → encoded (`pipeline::tests::end_to_end_latency`, 6 s, 1440×900 @ 60, medians of three
trials that agreed to within 0.26 ms at p50):

| | p50 | p99 | max |
|---|---|---|---|
| capture → encoded | **13.61 ms** | **15.06 ms** | **20.64 ms** |

with 370–373 frames captured in 6 s (~62 fps), **0** WindowServer drops on every trial, **0**
encoder output drops, and `vt_pending_at_flush = Some(0)`.

Capture rate (`cgds::tests::capture_rate`, 5 s, 1440×900 @ 60):

| Capture size | delivered | WindowServer drops | callback mean | displayTime → callback p50 / p90 / p99 / max |
|---|---|---|---|---|
| native 1440×900 | 195 (39.0 fps) | **0** | 311.0 µs | −15.53 / −15.12 / −11.97 / −11.42 ms |
| half 720×448, same process | 196 (39.2) | **0** | 88.3 µs | −15.33 / −14.76 / −11.23 / −11.11 ms |
| half 720×448, alone | 292 (58.4) | **0** | 121.4 µs | −15.57 / −15.29 / −14.80 / −3.88 ms |

Three things to carry forward. WindowServer drops are **zero everywhere** — that is the
queue-depth change working, and it is measured (`CGDisplayStreamUpdateGetDropCount` is read on
every delivery), not assumed. The callback cost is proportional to **output size, not frame
count** — 311 µs at 1440×900 against 88 µs at 720×448 in the same process, a 3.5× drop for a 4×
smaller surface — so asking the capture API for a smaller output really does move work off this
process. And `displayTime` is **ahead** of the callback by almost exactly one frame period, on
every percentile: it is a *scheduled presentation time*, not a composite time, so anything
treating it as "when this frame was captured" understates host latency by a frame. **SCK's
presentation timestamp has never been measured on any machine this project owns. Measuring it
is part of M2-01.**

Whole host path (`probe --whep-selftest`, 30 s, 10.15.8): PASSED, 1098 RTP packets,
`capture=cgdisplaystream encoder=videotoolbox-hardware`; packetize
0.09 / 0.12 / 0.33 / 0.57 ms, socket_write 0.36 / 0.72 / 3.15 / 3.72 ms, pace_wait 0.00
throughout. **Read the 201 ms `encode` p50 there as the keepalive, not as latency** — 156 of
161 frames in that window are `frames_repeated`, because the box has no user at the console and
a completely still screen. Your machine has a console user; expect this row to mean something
on yours, and say so.

The hardware-encode ladder on that same 2019 Intel machine:

| Size | Hardware H.264 |
|---|---|
| 1280×720, 1024×576, 960×540, 854×480, 768×432, 720×480, **640×360** | OK |
| 576×324, 512×288, 480×270, 426×240, 320×180 | **`OSStatus -12915`** |

640×360 passing with a height of 360 confirms Chromium's rule is a **disjunction**: `width ≥
640` **or** `height ≥ 480`, not a pair of minimums. The same test created **6 of 6** concurrent
hardware sessions, so the scarce-resource path was not reachable there.

**The Windows reference host** — Windows 11 26200, RTX 3080 Ti, NVENC + WGC: capture → socket
**1.57 / 8.39 / 16.78 / 18.00 ms** (p50/p90/p99/max), improved after later transport work to
p90 **3.93** / p99 **6.82**. That is the fast path this product has; a macOS number in the same
band would be a headline result.

**The competition** (none measured on this hardware; each is the best independent figure
available, or the vendor's own where nothing better exists):

| Product | Glass-to-glass | Basis |
|---|---|---|
| Parsec | 4–8 ms LAN | vendor |
| Steam Link | 13–16 ms | user reports |
| **Luna Display** | **11.3 ms USB, 22.4 ms Wi-Fi** (50 samples) | vendor bench — **the target**: same job, tablet as a second display over Wi-Fi |
| Moonlight / GameStream | 20–35 ms good LAN | community |
| Apple Sidecar | "consistently below 30" wired | reported |
| **Hopp** (browser WebRTC) | **98 ms** at 1080p after tuning libwebrtc, down from 159 | published — **the honest ceiling** for any browser client |
| Miracast | 80–100 ms optimised, 250 ms spec ceiling | certification spec |
| Duet Display | 95 USB / 108 Wi-Fi | adversarial bench |

Luna is the target and Hopp is the ceiling; a browser client cannot get below its own decode and
compositor cost.

---

## 6. Phase 1 — read, then find out what this machine actually is

Do not change a line during Phase 1. Its output is knowledge, a discovery document, and a
populated `findings/` queue.

### 6.1 Read in full (delegate; one subagent per cluster, all Opus 5)

**Cluster A — capture and encode**
`src-tauri/src/macos_utils/streamer/{mod,sck,cgds,encoder,gpu,pipeline,frame,qos,tuning,power,activity,mach,display,config,clock}.rs`;
`src-tauri/src/streamer/{platform,config,pipeline}.rs` (read-only — how the OS backend is
called); `docs/research/macos-capture-encode.md` in full (it is long and it is the design
document for eight of your ten items); `docs/manual-tests/macos-capture-encode.md`.

**Cluster B — audio**
`src-tauri/src/macos_utils/audio/{mod,ring,format}.rs` and all of `audio/legacy/`;
`src-tauri/src/streamer/audio/` (read-only — `AudioPacket`, `AudioDiagnostics`, `AudioHub`, the
13-byte DataChannel header, `host_now_ns`/`host_ns_to_rtp90k`, `opus_sys.rs`, `encoder.rs`);
all of `src-tauri/macos/ScreenExtendAudio/` including the scripts and `tools/shmcheck.c`;
`docs/research/audio-capture-tiers.md`; `docs/manual-tests/macos-audio.md` and
`macos-audio-driver-signing.md`.

**Cluster C — the ledgers and the contract**
`CLAUDE.md`; `docs/OPTIMIZATIONS.md` (at minimum rows 138–155, 178, 264, 280–292);
`docs/CAPABILITY-TIERS.md` (the macOS block, lines ~31–92); `docs/BASELINE.md`;
`docs/TELEMETRY.md`; `docs/RISK-REGISTER.md`; every existing `findings/*.md`.

Each subagent returns a written summary plus new `findings/` files — not a file dump. You read
the summaries.

### 6.2 Discover the machine

Write the raw output of all of this into **`docs/discovery/intel-mac-<model>-<osbuild>.md`**
(create the file; the directory exists). This document is what makes every later number
reproducible.

```sh
sw_vers ; uname -a ; sysctl -n machdep.cpu.brand_string
system_profiler SPHardwareDataType SPDisplaysDataType SPPowerDataType SPAudioDataType
ioreg -rc IOAccelerator -d1 | grep -E '"IOClass"|"IORegistryEntryID"|"model"'
pmset -g ; pmset -g assertions ; pmset -g batt
csrutil status ; nvram boot-args 2>/dev/null

# what VideoToolbox will actually give this machine, per size — build and run the ladder
cd src-tauri && cargo test --release --lib \
  macos_utils::streamer::encoder::tests::hw_capability_probe -- --ignored --nocapture

# the app's own view
cargo build --release --features probe
./target/release/ScreenExtend doctor --json | tee ../docs/discovery/doctor-intel-mac.json

# audio devices, before you install anything
system_profiler SPAudioDataType
```

Answer these explicitly in the discovery document, because every later task depends on them:

1. **Which GPU drives which display?** Get `[MTLDevice registryID]` for the built-in display and
   for a ScreenExtend virtual display, via `CGDirectDisplayCopyCurrentMetalDevice(displayID)`.
   Write a tiny `#[ignore]`d test in `encoder.rs`'s test module that prints them — the API is
   CoreGraphics 10.11, below the floor, and reachable by `msg_send!` without linking Metal.
2. **Which GPU is active at rest, and does launching ScreenExtend change that?**
   `system_profiler SPDisplaysDataType` before launch, after launch, and after a join.
3. **What is on this machine's `VTCopyVideoEncoderList`?** Encoder IDs, `IsHardwareAccelerated`,
   `GPURegistryID`, `InstanceLimit` per entry. Two GPUs may mean two hardware H.264 encoders.
4. **What does `VTCopySupportedPropertyDictionaryForEncoder` return at 1280×720, 640×360 and
   1920×1080?** Which encoder id would be selected, and which properties it advertises — in
   particular `EnableLowLatencyRateControl`, `EnableLTR`, `MaxAllowedFrameQP`,
   `MinAllowedFrameQP`, `MaximizePowerEfficiency`, `PrioritizeEncodingSpeedOverQuality`,
   `ConstantBitRate`, `BaseLayerFrameRate`.
5. **Is `SCStream` present, and does it deliver?** `AnyClass::get(c"SCStream")` and friends, then
   an actual start.
6. **What is the audio situation before the driver is installed?**
   `probe_audio_backend()` must return `NeedsDriverInstall` on a 12.x machine with no driver —
   *not* `Unsupported`, and *not* `ScreenCaptureKitAudio`. If it returns anything else, that is
   a blocker-severity finding and the first thing you fix.

---

## 7. Phase 2 — instrument and baseline, before you change anything

Take the full baseline on this machine **before** any of your own changes, on the tree as it
stands, before Phase 3. Every later claim references it.

Fill this matrix into `docs/BASELINE.md` as a new section titled
`## Matrix — macOS 12.x Intel dual-GPU (<model>, <build>)`. Do not touch the 10.15.8 section.

| Cell | Command | Record |
|---|---|---|
| capture → encoded, SCK | `cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture` | p50/p90/p99/max, frames in 6 s, drops, `vt_pending_at_flush`, which capture backend the log named |
| capture → encoded, CGDisplayStream | same, with SCK forced off (see §8, Task M2-01, for the forcing mechanism) | the same fields, for the A/B |
| capture rate, native | `cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate -- --ignored --nocapture` | delivered fps, WindowServer drops, callback mean, `displayTime → callback` histogram |
| capture rate, SCK | the SCK equivalent — **write it if it does not exist** | delivered fps, `SCFrameStatus` distribution, PTS → callback histogram |
| whole host path | `./target/release/ScreenExtend probe --whep-selftest --duration 30` | encode / packetize / socket_write / pace_wait / capture→socket, all four percentiles; `frames_repeated`; RTP packet count |
| live content | `probe --live --duration 30 --workload motion` then `--workload static`, `--scroll`, `--drag` | per-stage percentiles per workload |
| hardware ladder | `encoder::tests::hw_capability_probe -- --ignored --nocapture` | the size at which `OSStatus -12915` starts, on **this** machine and **each** GPU |
| concurrent encoders | the same test's second half | how many simultaneous hardware sessions before refusal |
| SPS | `encoder::tests::the_sps_promises_no_reordering` (write it if absent) | `max_num_reorder_frames`, `matrix_coefficients`, `colour_primaries`, `video_full_range_flag` |
| audio | `probe --audio --duration 15` with music playing | capture → subscriber percentiles, packets/s, `seq gaps`, `discontinuity`, the packet-size histogram, the `capture_ns` delta distribution, `opus:` line field for field |
| power | `pmset -g assertions` during a 10-minute stream; `sudo powermetrics -n 6 -i 10000 --samplers smc,cpu_power,gpu_power` | package power idle vs streaming, GPU power, fan, thermal pressure |
| dyld floor | `scripts/check-macos-dyld.sh … --floor 10.15` and `nm -u … \| grep -i -E 'screencapturekit\|displaystream'` | offender count (must be 0) and an empty grep |

Take three trials of every timing cell. State the power source, whether the lid was open,
whether an external display was attached, and whether the machine was thermally throttled.

---

## 8. Phase 3 — the work

Ten tasks. Each one names what to verify, what to fix, how to measure, and what "done" means.
"Done" is always either **a ledger row flipped to `VERIFIED` with numbers** or **the change
reverted with a recorded reason and a number**. No item may be silently absent from the report.

Suggested partition into four parallel workstreams. Measurement runs serialise on the machine;
implementation, research and review do not.

- **WS-A (capture)** — M2-01, M2-04 (capture half)
- **WS-B (encode)** — M2-02, M2-05, M2-06, M2-04 (encode half)
- **WS-C (audio)** — M2-07, M2-08, M2-09
- **WS-D (platform)** — M2-03, M2-10, plus the Phase 4 research agenda

---

### M2-01 — ScreenCaptureKit's first release, and the first-frame commit gate

**Source:** `docs/CAPABILITY-TIERS.md` *Capture backend (macOS)* (the frame gate);
`docs/OPTIMIZATIONS.md` rows 152 and 153; `docs/manual-tests/macos-capture-encode.md` §1;
`docs/research/macos-capture-encode.md` R7 and R10.
**Files:** `macos_utils/streamer/sck.rs`, `macos_utils/streamer/mod.rs::start_capture`,
`macos_utils/streamer/cgds.rs`.

**The claim under test.** Chromium reports that on macOS 12.3 a ScreenCaptureKit stream created
against a **full-display** `SCContentFilter` can start cleanly — `startCaptureWithCompletionHandler:`
calls back with a nil error — and then deliver **no sample buffers at all**. From outside, that
is indistinguishable from a slow stream. So the tier must not be *committed* on the strength of
`start()` returning `Ok`; it is committed only after a frame has actually arrived, with a
1500 ms deadline, and on expiry it must fall back to CGDisplayStream **without failing the
session**.

**What to verify.**

1. Which tier starts, on this OS build, for **(a)** the built-in display, **(b)** an external
   display if you have one, **(c)** a ScreenExtend virtual display. Exactly one of these lines
   appears per session:

   ```
   [sck]  capture started: display=1 1280x720 @ 60fps (queueDepth=8, minimumFrameInterval=0.9/fps)
   [cgds] capture started: display=1 1280x720 @ 60fps (queueDepth=5, no minimumFrameTime)
   ```

   A `(queueDepth=3)` with no interval in it means the tree is older than this guide — see §0.3.

2. Whether the gate ever fires:

   ```
   [sck] no frame within 1500 ms of startCapture; treating ScreenCaptureKit as unusable on display 1
   [capture] ScreenCaptureKit failed to start for display 1: <reason>; falling back to CGDisplayStream
   ```

   **On 12.3 specifically, try hard to make it fire.** Full-display filter, virtual display,
   a display that has just been created, a display whose mode is being changed, a second
   concurrent stream. If you can reproduce the Chromium failure, that is the single most
   valuable result in this task — record the exact conditions.

3. That `end_to_end_latency` still produces frames whichever tier wins, and that
   `degradation.capture_no_first_frame` in `status --json` is **0** on a busy display and
   non-zero only on a genuinely still one.

4. `SCFrameStatusStarted` acceptance (row 152). `Started` is the first frame after
   `startCapture` and carries the whole display; the pre-row-152 code discarded it, so the first
   thing a joining client could see was whatever changed next. Confirm the frame is accepted and
   that a client joining onto a still display paints within ~1 s.

5. **The SCK presentation timestamp, measured.** The stamp is now *read* — `publish_sample` passes
   it through `super::clock::os_frame_time_or(arrived_host_ns, os_host_ns)`, which converts a
   `CMClockGetHostTimeClock` value to the shared host clock and **falls back to the callback's own
   arrival stamp when the OS stamp is absent (0) or in the future**. What has never been measured on
   any machine this project owns is *its distribution*, and therefore how often that fallback fires.
   Print the same histogram the CGDisplayStream path prints — `PTS → callback` p50/p90/p99/max, plus
   `frames with no PTS: N/M` and `frames whose PTS was in the future: N/M` — from the SCK
   capture-rate test you wrote in §0.3. On 10.15.8 CGDisplayStream's `displayTime` is a *scheduled*
   presentation time, banded at −15.5 ms at 60 Hz, i.e. ahead of the callback on every single frame.
   **If SCK's PTS is behind the callback on 12.x, it is a real composite time and is strictly better
   than callback arrival** — the fallback simply stops firing and the capture term in
   `docs/LATENCY-MAP.md` becomes measurable rather than bounded. Report the four percentiles, the
   sign, and the fallback rate.

6. **`queueDepth` 3 vs 8, measured, not assumed.** Row 152 raised it because Apple raised its own
   documented default and describes depth as memory against stalling, not as latency — but the
   comment it replaced claimed "3 is lowest latency". Settle it: A/B 3 and 8 over 30 s of motion
   and report delivered fps, `capture_delivery` percentiles and any dropped/incomplete frames at
   each. Keep whichever wins and record the loser's number.

7. **`minimumFrameInterval` 1/fps vs 0.9/fps, measured.** OpenDisplay measured ~51 fps out of a
   request for exactly 1/60, and OBS ships a 10 % shortening for the same beat-frequency reason.
   A/B them and report delivered fps at a 60 Hz target.

**How to force the fallback for the A/B.** There is no `--capture-backend` flag on macOS — that
flag is Windows-only (`wgc|dxgi|auto`). Add a **test-only, env-gated** switch in
`start_capture`: an environment variable (e.g. `SCREENEXTEND_MACOS_CAPTURE=cgds|sck|auto`)
read once, logged when it is not `auto`, and defaulting to today's behaviour. This is a
measurement instrument, not a user setting: it must never appear in the CLI, the config store
or the UI, and its log line must say plainly that a non-default backend was forced. Record it
as a ledger row.

**Done when.** Row 152 and row 153's frame gate are `VERIFIED` (or reverted) with: the tier line
for all three display kinds, whether the gate fired and under what conditions, the PTS histogram
with its sign, and the two A/B tables. `docs/manual-tests/macos-capture-encode.md` §1 gets your
readings written into its "Record" slot.

---

### M2-02 — `PreferredEncoderGPURegistryID`: pin the encoder to the display's GPU

**Source:** `docs/OPTIMIZATIONS.md` row 143; `docs/CAPABILITY-TIERS.md` *VideoToolbox session
spec (macOS)*; `docs/research/macos-capture-encode.md` R15 and §2's key table;
`docs/manual-tests/macos-capture-encode.md` §9.
**Files:** `macos_utils/streamer/encoder.rs`, `macos_utils/streamer/gpu.rs`.
**Status inherited:** `COMPILE-CHECKED`. **Only a dual-GPU Mac can move it.**

**The mechanism.** On a multi-GPU Mac, WindowServer vends the display's `IOSurface` from the GPU
driving *that display*, while VideoToolbox picks an encoder by its own criteria. When they
differ, every frame's surface migrates across the PCIe (or Thunderbolt) bridge. The fix is two
API calls, both below the 10.15 floor and both reachable without linking Metal:

- `CGDirectDisplayCopyCurrentMetalDevice(displayID)` — CoreGraphics, **10.11**
- `[MTLDevice registryID]` by `msg_send!` — the value
- `kVTVideoEncoderSpecification_PreferredEncoderGPURegistryID` — **10.14**, passed in the
  encoder specification dictionary

**`Preferred`, never `Required`.** `RequiredEncoderGPURegistryID` implies
`EnableHardwareAcceleratedVideoEncoder` and would defeat the whole fallback ladder in
`Encoder::new`. This is not a preference; it is a correctness constraint on the rung structure.

**The read-back trap.** `kVTCompressionPropertyKey_UsingGPURegistryID` (10.15) returns **NULL
for both a built-in-GPU hardware encoder and a software encoder**. It cannot tell you which of
those you got. `UsingHardwareAcceleratedVideoEncoder` and the encoder id are what answer that.
The identity line therefore prints `gpu_registry_id=none (integrated or software)` and says so;
do not "fix" that into a claim.

**What to verify.**

1. `gpu_registry_id` in the per-session identity line is **non-`none`** on this machine — the
   first time in this project's history.

   ```
   [vt] encoder: id=com.apple.videotoolbox.videoencoder.h264.gva hardware=true gpu_registry_id=4294967… instance_limited=true
   [vt] encoder identity: id=… hardware=true gpu_registry_id=…
   ```

2. It **equals** the registry id of the GPU that `CGDirectDisplayCopyCurrentMetalDevice` reports
   for the captured display. Print both and compare. **If they differ, that is the finding** —
   every frame's surface is crossing the bridge.
3. The measured effect. A/B with the pin set and with it removed, at 1920×1080 @ 60 and at the
   virtual display's native geometry:
   - `capture → encoded` p50/p90/p99/max from `end_to_end_latency`
   - `encode` percentiles from `probe --whep-selftest --duration 30`
   - `powermetrics` GPU power and package power over 5 minutes of `--workload motion`
4. The interaction with M2-03. Setting `NSSupportsAutomaticGraphicsSwitching` means macOS may
   start you on the integrated GPU; pinning the encoder to the display's GPU is the other half
   of the same decision. **Measure the four combinations** (plist on/off × pin on/off) — that is
   the only way to know whether the two are additive, neutral or fighting.
5. What happens when the GPU switches **under a live session** — that is M2-04, but take the
   `gpu_registry_id` reading at the moment of the switch here.

**Done when.** Row 143 reads `VERIFIED` with both registry ids, the four-way A/B table, and the
power numbers — or reverted with the number that says the pin cost more than it saved.

---

### M2-03 — `NSSupportsAutomaticGraphicsSwitching`

**Source:** `docs/OPTIMIZATIONS.md` rows 178 and 264; `docs/research/macos-capture-encode.md`
R15(a); Apple QA1734; `docs/manual-tests/macos-capture-encode.md` §9 step 3 and
`macos-build.md`.
**Files:** `src-tauri/Info.plist` (the key), `src-tauri/tauri.macos.conf.json` (proving the
plist reaches the bundle).
**Status inherited:** `COMPILE-CHECKED`, and row 178 notes it was **inert** until
`bundle.macOS.infoPlist` was declared (row 264).

**The mechanism.** QA1734: *"By default, once your application creates an OpenGL context (by
either calling OpenGL directly or an API that relies on OpenGL such as Core Animation, Core
Image, etc), the MacBook Pro automatically switches to the higher-end discrete GPU for
performance concerns and **won't switch back until the application quits**."* A Tauri app is a
WKWebView, which creates exactly that kind of context. Sustained discrete-GPU power on a
2018–2019 chassis that is simultaneously encoding 1080p60 is a thermal-throttle generator — and
thermal throttling is indistinguishable, from the user's chair, from the reported symptom *"all
other apps must be quit for the app to be usable."*

QA1734 attaches a condition: the app must work correctly with multiple GPUs. That is satisfied
by construction here — `gpu.rs` is an empty placeholder and nothing in this process does direct
GPU work; the surfaces come from WindowServer and go to VideoToolbox.

**What to verify.**

1. The key actually reaches the built app:

   ```sh
   pnpm tauri build --config src-tauri/tauri.macos.conf.json
   app="src-tauri/target/release/bundle/macos/ScreenExtend.app"
   defaults read "$app/Contents/Info.plist" NSSupportsAutomaticGraphicsSwitching   # -> 1
   defaults read "$app/Contents/Info.plist" NSAudioCaptureUsageDescription
   defaults read "$app/Contents/Info.plist" LSMinimumSystemVersion                 # -> 10.15
   ls "$app/Contents/Resources/resources/"                                         # libopus.dylib, ScreenExtendAudio.pkg
   ```

   This is the half row 264 could never close, because release CI has never run.

2. The app actually stays on the integrated GPU. Run the **bundled `.app`**, not the bare
   binary — the key is read from the bundle's `Info.plist`.

   ```sh
   system_profiler SPDisplaysDataType | grep -A3 'Chipset Model'   # before launch
   # launch ScreenExtend.app, idle, no client
   system_profiler SPDisplaysDataType | grep -A3 'Chipset Model'   # after launch
   pmset -g | grep gpuswitch
   ```

   A third-party GPU-state utility (gfxCardStatus) is a convenience, not evidence;
   `SPDisplaysDataType` and `ioreg` are.

3. The battery and thermal cost. Idle for 30 minutes with the app open and no client, on
   battery, with and without the key:
   `pmset -g batt` sampled every minute, plus
   `sudo powermetrics -n 30 -i 60000 --samplers smc,gpu_power`. Report the delta in mW and in
   estimated runtime.
4. That streaming still switches to the discrete GPU when the *display* needs it, and that the
   encoder does not silently become slower because it is now on the integrated GPU. This is the
   pairing with M2-02: measure `capture → encoded` in both states.

**Done when.** Row 178 reads `VERIFIED` with the `defaults read` output, the before/after GPU
state, and the power delta — or reverted with the measured streaming regression that made the
key not worth it. Row 264's plist-reaches-the-bundle assertion gets its first real evidence
either way.

---

### M2-04 — cross-GPU IOSurface migration, and a GPU switch under a live capture

**Source:** `docs/research/macos-capture-encode.md` R15 (the migration half).
**Files:** `macos_utils/streamer/{cgds,sck,gpu,encoder}.rs`.
**Status inherited:** never attempted anywhere.

This is the failure mode M2-02 and M2-03 are each half of, observed directly.

**What to do.**

1. **Force a switch mid-session.** Ways to make macOS switch a dual-GPU Mac while a stream is
   live, in increasing order of violence: plug or unplug mains power (with "Automatic graphics
   switching" enabled in Energy Saver); open an application that demands the discrete GPU
   (anything with a Metal or OpenGL context — a game, a video editor, `gfxCardStatus` forcing);
   attach or detach an external display, which on these machines forces the discrete GPU
   unconditionally; sleep and wake the display.
2. **Requirements.** Frames must not stall, tear, or stop. The session must not end. If the
   encoder is pinned to a registry id that has just stopped driving the display, the pin is now
   wrong — decide and implement what should happen: re-read
   `CGDirectDisplayCopyCurrentMetalDevice` on the display-reconfiguration callback and rebuild
   the encoder, or accept the migration cost and log it. **Decide, implement, measure, record
   in "Decisions Made Without Consultation".** Note that `CGDisplayRegisterReconfigurationCallback`
   is already used elsewhere in the codebase for the display-mode settle (see
   `docs/CAPABILITY-TIERS.md` *Display mode settle (macOS)*), so the pattern exists.
3. **Measure the migration.** Take `capture → encoded` and `encode` percentiles continuously
   across the switch (a 60 s `--workload motion` run with the switch forced at t=30 s), and
   report the excursion: how many frames, how large, how long to recover.
4. Watch for the encoder rebuild path (row 148) firing here — three consecutive submit failures
   rebuild the encoder. A GPU switch is a plausible real-world trigger and one of the few you
   can arrange. If it fires, that closes half of M2-05's item 3.

**Done when.** A new `docs/OPTIMIZATIONS.md` row records the behaviour under a live switch with
numbers, and either a fix landed or a recorded decision that the migration cost is acceptable —
with the measured cost.

---

### M2-05 — VideoToolbox `InstanceLimit` and encoder contention on Intel

**Source:** `docs/research/macos-capture-encode.md` R14; `docs/OPTIMIZATIONS.md` rows 141, 142,
148; `docs/manual-tests/macos-capture-encode.md` §2 "The scarce-resource case".
**Files:** `macos_utils/streamer/encoder.rs`.
**Status inherited:** the mechanism is VERIFIED on 10.15.8, but the **trigger was never
reachable there** — that machine created 6 of 6 concurrent hardware sessions.

`kVTVideoEncoderList_InstanceLimit` (10.14) exists precisely to advertise that a hardware
encoder is a global scarce resource, and it is the documented mechanism behind *"it only works
if I quit everything else"*. Nothing in this product had ever printed it before row 141.

**What to verify.**

1. `instance_limited=true` appears in the once-per-process catalog line on this machine, and
   what the limit is:

   ```
   [vt] encoder: id=com.apple.videotoolbox.videoencoder.h264.gva hardware=true gpu_registry_id=… instance_limited=true
   ```

2. **Reproduce the contention.** Start a 4K screen recording in QuickTime, or a video call, or
   OBS with hardware encode — anything that takes a hardware H.264 session — then join a device.
   Expect, in order:

   ```
   [vt] HW encoder unavailable for 1280x720 (low_latency=true, OSStatus …); trying a weaker encoder spec
   [vt] encoder identity: id=… hardware=false gpu_registry_id=none (integrated or software)
   [vt] this session is on the SOFTWARE H.264 encoder. …
   ```

   **Fail the task if the session silently lands on software with no line naming the reason.**
   Loudness is the deliverable here, not avoidance.

3. **How many concurrent hardware sessions this machine allows**, and whether the answer differs
   per GPU. Run `hw_capability_probe`'s concurrency half, then repeat with the encoder pinned to
   each registry id in turn (M2-02's mechanism gives you the pin). Two GPUs may mean two
   independent instance pools — that would be a genuinely new capability finding and is worth
   real effort to establish.
4. **The rebuild path (row 148).** Three consecutive `VTCompressionSessionEncodeFrame` failures
   must rebuild the encoder with the old session released first. Contention is the way to fire
   it: start the stream, then start the competing encoder, then stop it. Confirm the rebuild
   line appears, the picture resumes, and the session does not end. Before row 148, one error
   froze the picture forever with a single line in the log. **Row 148 is owned by the modern-Mac
   run**; write your transcript and numbers into its Notes as a second data point rather than
   flipping its status — but if it fires here and *not* there, say so loudly, because that would
   mean the trigger is an Intel-encoder property rather than a contention property.
5. Whether the ladder's *demotion* is visible in `CompatibilityReport.encoder_backend` and in
   `status --json`, not only in the log.

**Done when.** Row 141's `InstanceLimit` half and row 148 are `VERIFIED` with the contention
transcript, the concurrency count per GPU, and the rebuild evidence.

---

### M2-06 — the Intel hardware-encode threshold, on *this* Intel Mac

**Source:** `docs/OPTIMIZATIONS.md` row 292; `docs/research/macos-capture-encode.md` R11 and
§2.3; `docs/manual-tests/macos-capture-encode.md` §2 "The measured floor on the dev box".
**Files:** `macos_utils/streamer/encoder.rs` (`hw_capability_probe`).
**Status inherited:** **already `VERIFIED` on the 10.15.8 2019 Intel dev box.** This task is a
*second data point*, not a first — and the second point is what turns one machine's table into
a rule.

The dev box's measured ladder is reprinted in §5.4. The rule it confirmed is Chromium's
disjunction: hardware H.264 requires `width ≥ 640` **or** `height ≥ 480`. Row 292 also records
the decision **not** to work around it — ScreenExtend does not upscale a small client to reach
the hardware encoder, because spending bitrate to save CPU at the one content size where
software H.264 is cheap is the wrong trade. That decision stands unless your numbers overturn
it.

**What to add.**

1. Run `hw_capability_probe` on this machine and this OS and record the full ladder. Extend the
   size list to cover the sizes PLAN calls for — **320×240, 400×300, 568×320, 640×360, 640×480,
   1280×720** — alongside the existing rungs, so both tables are comparable. A phone at 50 %
   video scale (e.g. 195×422) is the real-world case; include it.
2. **Run it per GPU.** With the encoder pinned to the integrated GPU's registry id, then to the
   discrete one. If the thresholds differ, that is new and important: it means the floor is a
   property of the *encoder*, not of "Intel Macs", and the loud software line should name which.
3. Record the **read-back properties** at each rung that succeeds — `UsingHardwareAcceleratedVideoEncoder`,
   the encoder id, and the `VTCopySupportedPropertyDictionaryForEncoder` key set at that exact
   size. Row 141's whole point is that the property set depends on the encoder VideoToolbox
   picks, which depends on the frame size and the machine, not on the year.
4. Confirm the loud line still fires and reads correctly at a sub-threshold size. There is no
   size-specific message; what fires is the general software-fallback warning, which **names both
   plausible causes** because the encoder cannot tell them apart from the inside:

   ```
   [vt] this session is on the SOFTWARE H.264 encoder. Expect higher latency and CPU. The usual
   causes are an encode size below this Mac's hardware minimum (~640x480 on Intel) and another
   application already holding the hardware encoder; both are machine-wide, not specific to this
   device.
   ```

   Note the **calm** sibling that must appear instead whenever software was *asked for*
   (`[vt] this session is on the software H.264 encoder because software encode is switched on.`) —
   the loud one appearing for a session that requested software is a defect. `creates_small_via_software`
   already asserts `Encoder::new` succeeds at 480×270, 448×252 and 160×90 by falling back; what you
   add is that the line above accompanies it. **If your ladder shows the threshold is not ~640×480
   on this part, the message is now wrong and fixing its wording is in your fence.**

5. Measure what software actually costs at those sizes on this CPU — `encode` percentiles at
   480×270 and 320×180, software vs the smallest hardware rung — so row 292's "software H.264 is
   cheap at that size" claim carries a number from a second machine.

**Done when.** Row 292 gains a second machine's ladder, per GPU, with the software-cost numbers,
and either confirms the disjunction or overturns it with data.

---

### M2-07 — the ScreenExtend Audio driver as the *selected* tier on a supported modern OS

**Source:** `docs/CAPABILITY-TIERS.md` *System audio capture (macOS)* rung 3, *macOS audio tier
ordering*, *Legacy-tier capture transport*; `docs/manual-tests/macos-audio.md` tier 3.
**Files:** `macos_utils/audio/mod.rs` (tiering), `macos_utils/audio/legacy/**`,
`src-tauri/macos/ScreenExtendAudio/src/shm_ring.hpp`.

**Why this band matters.** On 10.15.8 the driver is the *only* candidate, so "it was selected"
proves nothing about the tiering. On 12.x it is still the only candidate — Process Tap and SCK
audio are both absent — but now on an OS Apple still supports, with a modern HAL, a modern
`coreaudiod`, and modern audio hardware. This is the first time the tier is exercised as a
shipping path rather than as a floor.

**Install the driver first:**

```sh
cd src-tauri/macos/ScreenExtendAudio
./scripts/fetch-libaspl.sh          # pinned commit, into third_party/
./scripts/release.sh                # build → package → (sign/notarize if credentials) → stage
# the finished package lands at src-tauri/resources/ScreenExtendAudio.pkg
```

**What to verify.**

1. **Tier selection.** With the driver *not* installed, `probe_audio_backend()` must return
   `NeedsDriverInstall` and `capable_backends()` must be empty, and `start_capture` must fail
   with the message that names the install — not with "unsupported". With it installed:

   ```
   audio: capture backend = virtual_device
   ```

   `CompatibilityReport.audio_backend` must carry the same string. Nothing else in the codebase
   prints that line, so it is the answer.

2. **No version gate anywhere on the path.** Confirm by reading: `native_backends()` dlsyms
   `AudioHardwareCreateProcessTap` and looks up `CATapDescription`; the SCK probe looks up
   `SCStream`, `SCShareableContent`, `SCContentFilter` and `SCStreamConfiguration`. There must be
   no `sw_vers` fork on this path. (`os_version()` in `audio/mod.rs` is used only to compose the
   *error message*; that is fine.) A 12.x machine is where a mistaken `>= 13.0` gate would show
   up as SCK audio being offered and failing.

3. **The transport.** Expect the shared-memory fast path:

   ```
   audio(legacy): buffer frame size range [<lo>,<hi>] → requested 128, got 128 (2.67 ms @ 48 kHz), st=0
   audio(legacy): capture transport = shm (/screenextend.audio)
   audio(legacy): virtual device active (transport=shm, playthrough_dev=<id>, volume_keys=device_volume_control)
   ```

   `transport=hal_input` means the driver could not create its segment, or `coreaudiod`'s umask
   denied `O_RDWR`. Inspect directly:

   ```sh
   cd src-tauri/macos/ScreenExtendAudio
   clang tools/shmcheck.c -o /tmp/shmcheck -lm
   /tmp/shmcheck            # one snapshot
   /tmp/shmcheck watch      # poll: writePos delta and RMS
   log stream --predicate 'subsystem == "app.screenextend.desktop.audio"'
   ```

   A healthy segment reads `open O_RDWR (what the host needs): granted`,
   `magic=0x31414553 version=2 rate=48000 ch=2 fmt=1 cap=131072 hdr=128`, a climbing `writePos`
   with `readPos` tracking it, small `backlog`, `overruns=0`, non-zero RMS, and a non-zero
   `anchor: hostTime`. **`anchor: hostTime=0` means the driver never published a capture time**
   — packets then fall back to the encoder's own clock and A/V sync suffers. That is a
   blocker-severity finding.

4. **Does this machine grant 128 frames?** The dev box did. `buffer_frame_size_range` on modern
   hardware may report a different range. Record `[lo, hi]`, what was requested, what was
   granted, and the resulting IO period — the whole latency story of this tier hangs off it.

5. **The numbers.** Reproduce §5.2's table on this machine:

   ```sh
   (ScreenExtend probe --audio --duration 15 > /tmp/probe.out 2>&1 &) ; sleep 3
   say "one two three four five six seven eight nine ten"
   sleep 13 ; cat /tmp/probe.out
   cd src-tauri && cargo test --lib legacy_live -- --ignored --nocapture
   ```

   Record: capture → subscriber p50/p90/p99/max; capture → Opus packet p50/p90/p99; the
   `capture_ns` delta distribution (**how many of N were not exactly 5.000 ms**); packets/s
   while a phrase plays; `seq gaps` (must be 0); `discontinuity` (1 per silent gap is correct);
   `overruns`; and the `opus:` line **field for field** — it must match Windows including
   `lookahead=120 samples (2.50 ms)`, because that number is what the client uses to align
   playout, and macOS loads a different libopus build.

6. **The HAL-input fallback.** Force it (unlink the segment, or run the app as a user that
   cannot map it) and confirm it (a) works, (b) says so, and (c) **refuses a non-48 kHz device**
   rather than reinterpreting whatever arrives as stereo f32. Then measure its added latency
   against the shm path — that delta is the value of the fast transport, and nobody has it.

7. **The ABI test still passes**: `cargo test --lib layout_matches_the_drivers_own_header`.

8. **Non-stereo mixes.** The driver's `OnProcessMixedOutput` downmixes anything not 2-channel
   with the same BS.775 coefficients as the Rust siblings, but the device advertises stereo so
   that branch **has never run**. Try to make it run — an aggregate device, a multichannel
   output, a Soundflower-style chain — and if you cannot, say so.

**Done when.** The *System audio capture (macOS)* rung-3 row and the *Legacy-tier capture
transport* row carry a second machine's numbers, on a supported OS, with the tier named as
selected — and the `macOS audio tier ordering` row's "the driver is a general fallback, not a
below-13.0-only tier" claim is confirmed by the code path actually taken on 12.x.

---

### M2-08 — the volume keys while the virtual device is the default output

**Source:** `docs/OPTIMIZATIONS.md` row 116; `docs/CAPABILITY-TIERS.md` *macOS volume keys while
the virtual device is default*.
**Files:** `macos_utils/audio/legacy/{volume_proxy,volume_keys}.rs`,
`src-tauri/macos/ScreenExtendAudio/src/volume_control.cpp`.
**Status inherited:** `COMPILE-CHECKED`. Nobody has ever pressed the keys.

This is the classic regression a virtual output device causes, and repairing it is half of why
this tier exists at all. There are two tiers:

- **Tier 1, the default — `volume_keys=device_volume_control`.** The driver implements real
  Volume and Mute controls on its output scope, so macOS's own volume keys and the menu-bar
  slider act on *it*; `volume_proxy::apply()` then mirrors the scalar and the mute flag onto the
  real output device and sets the playthrough gain.
- **Tier 2, opt-in — `volume_keys=event_tap_fallback`.** A `CGEventTap` intercepts
  `NX_KEYTYPE_SOUND_UP` / `_DOWN` / `_MUTE` directly. Enabled by
  `set_legacy_volume_key_proxy(true)` from the UI or by setting `SCREENEXTEND_LEGACY_VOLUME_TAP`
  in the environment. **Requires Accessibility.**

**What to verify.**

1. Which mechanism is live, from the start line's `volume_keys=` field.
2. **Tier 1, by hand.** With the driver as default output and playthrough running to your real
   speakers: press F10 (mute), F11 (down), F12 (up). Each must (a) move the macOS volume HUD,
   (b) actually change what you hear, and (c) leave the menu-bar slider consistent with what you
   hear. Repeat with the slider itself and with a Bluetooth headset as the playthrough target.
3. **The refusal path.** Some output devices reject volume or mute control — that is why the
   volume keys can look dead with nothing in the log to say so. Find one on this machine (HDMI
   outputs and many USB DACs qualify) and confirm the once-per-device warning:

   ```
   audio(legacy): output device 57 refused volume/mute mirroring (OSStatus volume=… mute=…); the host's own volume keys will not move it while ScreenExtend Audio is the default output
   ```

   Confirm it is **once per device**, not once per keystroke — `apply()` runs on every property
   notification, which is per keypress.
4. **Tier 2, the tap.** Grant Accessibility, set `SCREENEXTEND_LEGACY_VOLUME_TAP=1`, restart,
   confirm the start line flips to `event_tap_fallback`, and re-run the key tests. Then confirm
   the thing that makes the tap safe: **the callback does no CoreAudio IPC and no allocation** —
   it classifies and `try_send`s to a worker. The tap is created with `kCGEventTapOptionDefault`,
   so every keyboard and mouse event in the login session waits behind it; a slow callback
   overruns `kCGEventTapDisabledByTimeout` and macOS disables the tap. Verify the re-enable path
   (`CGEventTapEnable(port, true)` on the disabled-by-timeout event) by inducing it if you can.
5. **Measure the cost of tier 2.** With the tap active, measure input latency for ordinary
   typing and mouse movement — this is the one change in this guide that can slow down the
   user's whole machine. If it measurably does, that is a finding and possibly a reason to
   restrict the tier further.
6. Confirm `mute_forces_zero_gain_regardless_of_scalar` and `unmuted_gain_is_the_clamped_scalar`
   (the unit tests in `legacy/test/mod.rs`) describe what you actually observe.

**Done when.** The *macOS volume keys* tier row is `VERIFIED` with both tiers exercised by hand,
the refusal path observed on a real device, and the tap's system-wide cost measured.

---

### M2-09 — the `.pkg` install flow end to end, behind one admin prompt

**Source:** `docs/OPTIMIZATIONS.md` row 115; `docs/manual-tests/macos-audio.md` "Verifying the
driver directly"; `docs/manual-tests/macos-audio-driver-signing.md`.
**Files:** `macos_utils/audio/legacy/{installer,routing}.rs`,
`src-tauri/macos/ScreenExtendAudio/scripts/package.sh` and `scripts/postinstall`.

The dev Mac installed this driver from a tree it had built itself. **You are the first machine
that is not the machine that built it**, and — critically — the first to install it *from the
bundled app*, where the package is resolved as
`resolve("resources/ScreenExtendAudio.pkg", BaseDirectory::Resource)` out of
`Contents/Resources/resources/`, put there by `tauri.macos.conf.json`'s
`bundle.macOS.files` mapping. Row 264 exists because that mapping was once absent and
`install_audio_driver` on a released app took its `Err` branch, making the whole 10.15–12.x
audio tier unreachable for anyone who did not build from source.

**What to verify.**

1. **The path resolves in the bundled app.** Build the `.app` (§8, M2-03 step 1), run it from
   `/Applications` — not from `target/release/` — and trigger the install from the per-device
   Audio toggle. `ls "$app/Contents/Resources/resources/ScreenExtendAudio.pkg"` must exist and
   the command must find it.
2. **One admin prompt.** `installer::run_privileged` goes through the `elevated-command` crate
   with the app's own name and icon. Count the prompts: **one**. The `coreaudiod` restart retry
   rides the elevation already granted, so a second prompt is a defect.
3. **`coreaudiod` restart behaviour.** `postinstall` tries
   `launchctl kickstart -k system/com.apple.audio.coreaudiod` and falls back to
   `killall coreaudiod`. Record which one worked on 12.x, and whether the device appeared
   without a reboot. If neither works, `install_pkg` re-tries the kickstart itself and only then
   returns `NeedsReboot`:

   ```
   [audio] driver installed but no device appeared; restarting coreaudiod and re-checking
   ```

   Record which `InstallOutcome` you got: `Installed`, `NeedsReboot`, `Cancelled` or `Failed`.
   **`wait_for_device()` polls 10 × 300 ms = 3 s.** If your machine reliably takes longer, that
   is a real finding with a real fix (raise the bound, or wait on the
   `kAudioHardwarePropertyDevices` listener the tier already registers instead of polling).
4. **Reboot.** Reboot and confirm the driver is still loaded and the device still present, and
   that the app does not re-prompt.
5. **Default output save / switch / restore.** `Router::activate` saves the current default
   output's UID (never our own; falls back to any other device with output channels), sets
   ScreenExtend Audio as default, and writes
   `~/Library/Application Support/app.screenextend.desktop/legacy_audio_routing.json` with
   `saved_uid`, `changed: true`, a timestamp and **`owner_pid`**. A clean stop restores it:

   ```
   audio(legacy): stopped; default output restored
   audio(legacy): crash-recovery agent removed
   ```

   Verify with the file contents and with `system_profiler SPAudioDataType` before, during and
   after.
6. **The user switching away is respected, not fought.** Change the default output in System
   Preferences while audio is live. Expect:

   ```
   audio(legacy): user switched default output to <uid>; releasing
   ```

   and playthrough released, `format_listener` dropped, the state file cleared. The app must not
   grab it back.
7. **Crash recovery, for real.** Start a session with audio on, then `kill -9` the host.
   Confirm: the state file survives with `changed: true` and the dead process's `owner_pid`; the
   LaunchAgent `app.screenextend.desktop.audiowatchdog` (`~/Library/LaunchAgents/`) is loaded
   with `RunAtLoad` and `StartInterval 60`; within 60 s the default output is restored; and the
   agent then **removes itself** because its job is done. Then:
   - `launchctl list | grep screenextend` must find nothing afterwards.
   - Verify the `owner_alive()` guard: with the host **running**, the agent must do nothing.
     `kill(pid, 0)` is the check — a `pgrep` would be a fork; a state file from an older build
     carries `owner_pid: 0`, which reads as "no owner" and falls back to the single-instance
     lock check (`another_instance_running()` takes the `fs4` lock on `screenextend.lock` and
     immediately drops it). This guard exists because the agent bootstraps with `RunAtLoad` and
     fires the instant the tier starts, and `probe --audio` takes no single-instance lock —
     which is how the bug was found.
   - Confirm the recovery line: `audio(legacy): crash recovery restored the previous default output device`.
8. **Uninstall.** `uninstall_audio_driver` must recover the routing first, boot out the agent,
   remove `/Library/Audio/Plug-Ins/HAL/ScreenExtendAudio.driver`, restart `coreaudiod`,
   `shm_unlink` the segment, and report `Uninstalled`. Confirm the device is gone and
   `probe_audio_backend()` goes back to `NeedsDriverInstall`.
9. **Reinstall over an existing install**, and **install while a session is live** — both must
   behave.
10. **If you have a Developer ID**: run `SE_SIGN_IDENTITY=… SE_INSTALLER_IDENTITY=…
    SE_NOTARY_PROFILE=… ./scripts/release.sh`, then install the signed+notarized package
    **with SIP enabled**, and confirm `coreaudiod` loads it. That closes tier 1 of the *macOS
    audio driver load* row, which no machine on this project has ever closed. Also run
    `spctl -a -vv` and `codesign -d -r-` on the driver and record the designated requirement.

**Done when.** Row 115 is `VERIFIED` with the prompt count, the `coreaudiod` outcome, the
`InstallOutcome` value, the routing file contents at each stage, and the crash-recovery
transcript with timings.

---

### M2-10 — power, idle assertions and thread QoS on a laptop at a console

**Source:** `docs/OPTIMIZATIONS.md` row 150 and its siblings; `docs/manual-tests/macos-capture-encode.md`
§6 and §9.
**Files:** `macos_utils/streamer/{power,activity,qos,tuning}.rs`.
**Status inherited:** row 150 is `VERIFIED` on the dev Mac — **over SSH, headless, on mains
power, with no user at the console.** Every user-facing property of this code is therefore
unverified.

**What to verify.**

1. **The assertions are actually taken and actually effective.** `KeepAwake::begin` creates
   `PreventUserIdleSystemSleep` and `PreventUserIdleDisplaySleep` via
   `IOPMAssertionCreateWithName` and logs `[power] keep-awake asserted for session (N
   assertion(s))`. Confirm with `pmset -g assertions` during a live session that both appear
   attributed to ScreenExtend, and — the real test — that the display and the system **do not
   sleep** during a 20-minute idle stream with the Energy Saver timeouts set to 2 minutes.
   Confirm they are released on session end (`pmset -g assertions` again).
2. **The `NSActivityLatencyCritical` token.** `activity.rs` takes
   `UserInitiated | LatencyCritical`. Its purpose is to keep App Nap and timer coalescing off
   the capture/encode path. Test it by backgrounding the app (another app full-screen over it)
   during a live stream and measuring `capture → encoded` and delivered fps before and after.
   **This is one of the concrete mechanisms behind the reported symptom "all other apps must be
   quit for the app to be usable"** — if backgrounding costs measurable latency here, the
   activity token is not doing its job and that is a major finding.
3. **The thread policies, read back.** Row 150's discipline is one scheduling scheme per thread,
   every return value checked, and the time-constraint policy read back with
   `thread_policy_get`. Expect once per session:

   ```
   [qos] encode thread on the deadline scheduler: period=16.67ms computation=12.50ms constraint=16.67ms preemptible=true
   ```

   If the line is missing, a `[qos] … failed (rc=…)` line says why, and every latency number
   below it means something different. Record the values as read back, not as requested — the
   computation budget is `0.75 × period` (it was `period/8`, which is the bug row 150 fixed).
   `THREAD_AFFINITY_POLICY` failing is expected only on Apple Silicon; on your Intel machine it
   should succeed, so a failure there is a finding.
4. **Battery vs mains.** Take the whole Phase 2 latency matrix twice — on mains and on battery,
   with the machine at a steady thermal state each time. macOS throttles differently on battery
   and this product's users use laptops. Report the delta per stage.
5. **Thermals over a long run.** 30 minutes of `probe --live --duration 1800 --workload motion`
   with `sudo powermetrics -n 30 -i 60000 --samplers smc,cpu_power,gpu_power` alongside. Report
   package power, GPU power, fan RPM, and whether `capture → encoded` p99 degrades over the run.
   A creeping tail is thermal throttling and it is the honest answer to "why does it get worse
   after ten minutes".
6. **`apply_process_tuning()`** — read what it does, confirm every call's return is checked, and
   measure whether it matters by A/B-ing it out.
7. The lid-closed / clamshell case with an external display, if you have one.

**Done when.** Row 150 gains a console-session, on-battery, thermally-characterised verification
with the read-back policy values, and the App Nap question is answered with numbers.

---

## 9. Phase 4 — the research agenda

Run these in parallel with Phase 1 and Phase 3, one Opus 5 subagent each. Each produces a
written report appended to `docs/research/` with citations and an explicit "what we should do
about it" section, and each spawns `findings/` files. **Prefer primary sources**: Apple headers
and archived QA notes, WWDC transcripts, Chromium and OBS source, the AVFoundation and
CoreAudio release notes.

1. **ScreenCaptureKit on 12.3 specifically.** What exactly did Chromium observe, in which bug,
   against which filter type, and was it ever fixed within Monterey? Read
   `media/capture/video/mac/` and the associated crbug threads. Also: OBS's
   `mac-screencapture` plugin shipped SCK support in 12.5+ and has its own compatibility
   comments; read them. Deliverable: the precise reproduction conditions, so M2-01 can aim at
   them instead of hoping.
2. **`SCStreamConfiguration` on Monterey vs Ventura.** Which keys exist at 12.3, which arrive at
   12.5/13.0, and what each is worth: `queueDepth`, `minimumFrameInterval`, `pixelFormat`,
   `colorSpaceName`, `showsCursor`, `capturesShadowsOnly`, `scalesToFit`,
   `preservesAspectRatio`, `backgroundColor`. Anything settable at 12.3 that we are not setting
   is a candidate. Every one must be reached by `msg_send!` on a runtime-looked-up class.
3. **`SCStreamFrameInfo` attachments at 12.3.** `SCStreamFrameInfoStatus`,
   `SCStreamFrameInfoDisplayTime`, `SCStreamFrameInfoScaleFactor`,
   `SCStreamFrameInfoContentRect`, `SCStreamFrameInfoDirtyRects`, `SCStreamFrameInfoContentScale`.
   Which exist on 12.3? `sck_optional_cfstring` returns `None` for any that do not, and the
   code must keep working — verify that it does, per key.
4. **VideoToolbox on Monterey, Intel.** The full property set of `…gva` (the Intel/AMD hardware
   encoders) and `…rtvc` (the low-latency one). Is `EnableLowLatencyRateControl` available on an
   Intel Mac at 12.x, or only on Apple Silicon? Apple's WWDC 2021 "Explore low-latency video
   encoding with VideoToolbox" is the primary source. **This one materially changes M2-05 and
   M2-06**: if `…rtvc` is Apple-Silicon-only, then the encoder back-pressure tier 1 (a
   one-in/one-out encoder) is *not* reachable on your machine either, and that must be said
   plainly rather than assumed.
5. **Dual-GPU IOSurface migration, measured by others.** How do OBS, Chromium and Syphon handle
   an IOSurface that belongs to a different GPU than the consumer? Is there an explicit
   migration API, or does the driver do it? What does it cost? Primary sources: the IOSurface
   headers, Chromium's `gpu/ipc/common/gpu_memory_buffer_impl_io_surface.cc`, and the AGDC
   (Apple Graphics Device Control) reverse-engineering literature.
6. **Automatic graphics switching, in practice.** QA1734 is from the OpenGL era. What triggers a
   switch on a 2018–2019 machine in 2026 — Metal context creation, `CAMetalLayer`,
   `CVDisplayLink`, a WKWebView compositing layer? Which of those does a Tauri app do
   unavoidably? Is `NSSupportsAutomaticGraphicsSwitching` still honoured on Monterey?
7. **AudioServerPlugIn on Monterey.** What changed in `coreaudiod` between Catalina and
   Monterey that affects a HAL plug-in: the load path, the sandbox, the umask on `shm_open`,
   `kAudioDevicePropertyBufferFrameSize` clamping, the volume-control property contract. libASPL's
   own issue tracker and the BlackHole/existing-virtual-device literature are the sources. This
   directly de-risks M2-07 and M2-08.
8. **Why is AirPlay faster?** The standing highest-priority research question of the whole
   engagement, and your machine can *run* AirPlay mirroring to an iPad while also running
   ScreenExtend. Even without touching `macos_utils/airplay/` (outside your fence), you can
   **measure both on the same hardware, same AP, same client, with the same camera method** and
   publish the gap. AWDL's peer-to-peer link layer, RTP over UDP with Apple's own timing instead
   of ICE/DTLS/SRTP, and a receiver-side buffering strategy are the three candidate
   explanations. A measured side-by-side on a dual-GPU Intel Mac would be the most valuable
   single data point this guide could produce beyond its ten items.

---

## 10. Phase 5 — the hyper-optimisation sweep

After the ten verifications, sweep every knob this area exposes. Each one gets tried and
measured, or gets a recorded rejection with a number. "Speculative" is not a reason to skip; it
is a reason to measure.

**Capture (SCK, on 12.x, for the first time)**
- `queueDepth` across 3, 5, 8 — delivered fps, `capture_delivery` percentiles, incomplete frames.
- `minimumFrameInterval`: exactly `1/fps`, `0.9/fps`, and `kCMTimeZero` (research R3 argues for
  removing the throttle entirely and letting the display's own cadence govern). Three-way A/B.
- Pixel format: `420f` (full range) vs `420v` (video range) vs `BGRA`. Research R8 recommends
  `420v` with the colour space set explicitly. Measure encode input cost and check the SPS VUI
  (`video_full_range_flag`, `matrix_coefficients`, `colour_primaries`) matches on both paths, so
  the two capture backends produce identically-tagged streams.
- Delivery queue QoS: the SCK output is attached to a `UserInteractive` serial queue targeted at
  the global UI queue. Try `UserInitiated` and a plain serial queue and measure the callback
  latency histogram.
- Dirty-rect skipping: currently a frame is skipped when `SCStreamFrameInfoDirtyRects` is
  present and **empty**. Measure how often that happens on a real desktop, and whether a
  *partial* dirty region could drive anything (it cannot drive the encoder — VideoToolbox has no
  region hint — but it could drive the keepalive cadence).
- The 200 ms keepalive repeat: on a machine with a console user, how many frames are
  `frames_repeated`? On the dev box it was two thirds. If it is much lower here, the keepalive
  interval is a knob worth sweeping.

**Encode**
- The whole spec ladder, per rung, with `VTCopySupportedPropertyDictionaryForEncoder` read at
  the real frame size before each attempt: does the ladder ever try a rung the property
  dictionary already said would fail? That is a free startup saving.
- `MaxKeyFrameInterval` / `MaxKeyFrameIntervalDuration` (600 s after row 144): confirm no
  periodic IDR appears in a 10-minute `--live` capture (`ffprobe -show_frames | grep -c
  'pict_type=I'`).
- `MaxAllowedFrameQP` 45 (the current ceiling) — sweep 39/45/51 and measure bytes-per-frame and
  perceived quality at a fixed bitrate.
- `MaximizePowerEfficiency=false` and `PrioritizeEncodingSpeedOverQuality=true`: both are set
  through `set_optional_bool`. **Read back whether they were accepted at all on this machine's
  encoder** (they are 10.14 and 11.0 keys and must never be linked statically — that is the
  dyld-floor rule that forced them into `set_optional_bool` in the first place). Then A/B each,
  on battery and on mains, for latency *and* for power. The power half has never been measured
  anywhere.
- `RealTime=true`, `AllowFrameReordering=false`, `MaxFrameDelayCount` — read back which of these
  the 12.x encoder honours, and confirm the SPS `max_num_reorder_frames` is 0 regardless.
- Encoder session reuse across a resolution change: today a geometry change rebuilds the
  session. Measure the rebuild cost at 1080p and decide whether a cached session per geometry is
  worth it.
- Concurrent sessions across two GPUs (from M2-05): if two pools exist, can a second client be
  placed on the other GPU deliberately? That would be a new capability tier.

**Audio**
- `kAudioDevicePropertyBufferFrameSize`: sweep the whole `[lo, hi]` range this machine reports,
  not just 128. Measure capture → subscriber percentiles and CPU at each. The dev box's 128
  frames / 2.67 ms may not be this machine's floor.
- The drain loop's `park_timeout(125 µs)` in `audio/mod.rs::worker` — sweep 62.5 / 125 / 250 /
  500 µs against latency and CPU.
- `RING_CAPACITY` (80 ms) and `MONITOR_RING_CAPACITY` (65536) — measure the actual occupancy and
  trim if there is headroom, since the encoder ring's depth is latency when it fills.
- The playthrough setpoint: `SETPOINT_PERIODS = 3`, `MIN_SETPOINT_FRAMES = 128`. Measure the
  underrun rate at 2 and at 3 periods on this machine's real output devices, including a
  Bluetooth sink where the output IO period is very different.
- The driver side: `OnProcessMixedOutput`'s cost per callback, the seqlock anchor publication
  rate, and whether `readPos` publication actually changes the driver's overrun accounting.
- Opus: `RESTRICTED_LOWDELAY`, 5 ms frames, `OPUS_SET_PREDICTION_DISABLED` and constrained VBR
  are shared with Windows and are outside your fence to *change* — but the **packet-size
  histogram with real audio playing on this tier** has never been taken and belongs in your
  report.

**Scheduling and power** — everything in M2-10 §7, plus: does pinning the encode thread to a
performance core help on a 6-core i7/i9? Does the audio worker's `user_initiated` QoS beat
`user_interactive` or lose to it?

---

## 11. Verification and gates

### 11.1 Every commit

From `src-tauri/`:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

After any FFI or macOS-API change (from the repo root):

```sh
scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend --floor 10.15
nm -u src-tauri/target/release/ScreenExtend | grep -i -E 'screencapturekit|displaystream|CATap'
otool -L src-tauri/target/release/ScreenExtend | grep -i -E 'screencapturekit|coreaudiokit'
```

The `nm` and `otool` greps must be **empty**. An accidental `objc2-screen-capture-kit`
dependency shows up as `ScreenCaptureKit.framework` in `LC_LOAD_DYLIB`, which is an
unconditional launch failure on Catalina.

### 11.2 ABI and layout tests

```sh
cargo test --lib layout_matches_the_drivers_own_header   # shm_ring.hpp ↔ shm_reader.rs
cargo test --lib macos_utils::streamer::sck::tests::delegate_class_registers
cargo test --lib macos_utils::audio::                    # the whole audio test tree
cargo test --lib legacy_                                 # routing, ring, gain, setpoint
```

If you change the shared-memory layout, `layout_matches_the_drivers_own_header` is the gate that
proves both sides moved. If you add an FFI surface, add a layout test in the same style — a
wrong struct offset is silent corruption, not a compile error.

### 11.3 The manual checks that remain after your run

Write these into `docs/manual-tests/macos-capture-encode.md` and `macos-audio.md` as remaining
gaps, honestly:

- Anything that needed a Developer ID and you did not have one (signing, notarization, SIP-on
  load, the designated requirement, TCC identity surviving an update).
- Anything that needed a second GPU and you had one GPU.
- macOS 13, 14, 15 and 26 — explicitly not yours.

### 11.4 The final production build gate

**After all feature work and immediately before writing the report**, run a full production
build and make it pass:

```sh
pnpm tauri build --config src-tauri/tauri.macos.conf.json
```

**The one failure you may accept is the Tauri updater signing key error** — the private key is
not on this machine and that is expected. Zero `cargo` errors, zero TypeScript errors, zero
linker errors, zero missing-resource errors, zero bundling errors. If the build produces
anything else, **it is not done — go fix it.** Do not write the report over a broken build and
do not describe a build error as a known issue you are leaving behind.

Then also, and passing:

```sh
cd src-tauri && cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cd .. && pnpm lint && pnpm build && node scripts/check-legacy-webkit.mjs
node scripts/check-bindings.mjs && node scripts/check-versions.mjs
```

Then confirm the tree is clean and reviewable: nothing half-finished, no debug scaffolding in
shipping paths, no commented-out experiments, `git status --short` empty.

---

## 12. Deliverables and the final report

### 12.1 What must be in the repository when you stop

- **`docs/OPTIMIZATIONS.md`** — every row this guide owns moved to `VERIFIED` with numbers or
  reverted with a reason; new rows appended for everything you added; every rejection recorded
  with its number.
- **`docs/CAPABILITY-TIERS.md`** — the macOS rows this guide touches updated with your machine's
  verdict, naming the OS build and the hardware.
- **`docs/BASELINE.md`** — a new `## Matrix — macOS 12.x Intel dual-GPU` section with the full
  Phase 2 and post-change matrices. The 10.15.8 section untouched.
- **`docs/manual-tests/macos-capture-encode.md`** and **`macos-audio.md`** — every "Record:" slot
  filled with your readings, and corrected wherever the script was wrong.
- **`docs/discovery/intel-mac-<model>-<osbuild>.md`** — the machine's raw discovery output.
- **`docs/research/`** — one appended report per Phase 4 question.
- **`findings/`** — **empty**, or every remaining file deleted with its reason in the report.
- **`guides/`** — untouched. This file is a directive, not a working document.

### 12.2 The report

Write it as the last thing, into the final assistant message and, in full, into a new file
`docs/RESULTS-intel-mac.md`. It must contain, in this order:

1. **The build result, at the top.** Which machine, that `pnpm tauri build --config
   src-tauri/tauri.macos.conf.json` succeeded, and that the only error encountered was updater
   signing.
2. **Machine facts.** Exact `ProductVersion` and build, model identifier, CPU, both GPUs and
   their registry ids, RAM, SIP state, whether a Developer ID was available, display
   configuration and refresh rates, audio devices, AP model and band if a client joined, and the
   client device and browser versions.
3. **Per-item status — one line each for M2-01 … M2-10**, in the form
   `VERIFIED (numbers) | reverted (reason) | still UNVERIFIED (what was missing)`. **No item may
   be silently absent.** If §0.3's check disagreed with the tree, say so and say what you landed
   before anything else could be measured.
4. **Before/after numbers for every change**, as p50 / p90 / p99 / max, with the trial count,
   the power source, and whether the machine was quiet and thermally steady.
5. **The ledger diff** — the exact rows added or edited in `docs/OPTIMIZATIONS.md` and
   `docs/CAPABILITY-TIERS.md`, and the `docs/BASELINE.md` cells filled.
6. **Findings** — created, resolved-and-deleted, and retagged, by filename.
7. **Comparison against the baselines named in §5.4** — the 10.15.8 dev Mac (which you should
   beat on every row), the Windows NVENC host, and the competitor table. Say plainly where this
   machine sits between Luna Display's 22.4 ms Wi-Fi and Hopp's 98 ms.
8. **Decisions Made Without Consultation** — every judgment call the user might have made
   differently, with your reasoning, so each can be reviewed and reversed.
9. **Side work** — every bug found and fixed, every doc corrected, every test added that was not
   in the ten items. What it was, why it was worth doing, where it lives.
10. **What could not be done and why** — recorded honestly rather than left blank, **including
    negative results, which are results.** A knob that did nothing, measured, is a deliverable.
11. **Recommendations requiring the user** — anything needing a Developer ID, a different Mac, a
    push to GitHub, or a UI-visible change you declined to make, each with its measured upside.
12. **Commits** — the branch name (`guide/intel-mac`) and one line per commit. **Nothing
    pushed.**

### 12.3 The standard

> "Even if there's some obscure fix for exactly one version of an operating system or weird
> browser that shaves off exactly 0.1 millisecond, I want that. I want absolutely every possible
> item optimized. Never assume anything is impossible unless web search and actually trying it.
> If even one app — closed source, paid, whatever — has figured it out, there is a way and we
> must figure it out as well."

Take that literally. Be exhaustive. Measure everything. Research before you conclude. Try before
you dismiss. Keep the old machines working while making this one fast. Change nothing about how
the frontend looks. Never push. Decide rather than ask, and write down every decision. And write
down every number you produce, including the ones that disappointed you.

You have one uninterrupted run and nobody to consult. Use it well.
