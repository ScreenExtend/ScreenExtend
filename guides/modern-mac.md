# ScreenExtend on a modern Mac — verification, optimization and release directive

**Target machine: macOS 14.2 or newer, no upper bound. Apple Silicon strongly preferred.
Part B additionally needs an Apple Developer Program identity and a clean VM.**

You are running on a machine that ScreenExtend's two development boxes could not be. This
repository's macOS work was written, reviewed and compiled against a **macOS 10.15.8 Catalina
Intel MacBook with Command Line Tools only** — a machine on which ScreenCaptureKit does not
exist, the Core Audio Process Tap does not exist, VideoToolbox low-latency rate control does not
exist, long-term reference frames do not exist, Apple Silicon does not exist, and no Apple
signing identity has ever been present. Roughly a third of the macOS code in this tree has
therefore **never executed**. Your job is to run it, measure it, fix what is wrong, optimize
every knob the modern APIs expose, and turn a large block of `COMPILE-CHECKED` and `UNVERIFIED`
ledger rows into `VERIFIED` rows carrying real numbers — or to revert them with a recorded
reason and a number.

You have no access to the conversation that produced this file, to the other guides, or to either
development machine. Everything you need is in this file and in the repository.

---

## 0. The machine contract

### 0.1 Floor, ceiling, and why

| Bound | Value | Why |
|---|---|---|
| **Minimum OS** | **macOS 14.2** | The Core Audio Process Tap (`AudioHardwareCreateProcessTap`, `CATapDescription`) is 14.2+. Pinning here also collects ScreenCaptureKit video (12.3+), ScreenCaptureKit audio (13.0+), VideoToolbox `EnableLowLatencyRateControl` (**11.3+**, not 11.0 as an old gate in this tree still claims), VideoToolbox `EnableLTR` (12.0+), and the Ventura+ System Settings deep link. One machine closes all of them. |
| **Maximum OS** | **none** | macOS 15 (Sequoia) and 26 (Tahoe) are *more* valuable, not less. 15 is where `CGDisplayStream` is obsoleted in the SDK, where the monthly screen-recording re-consent model appears, and where an ad-hoc code identity is reported to be rejected for screen capture outright. BetterDisplay reported WindowServer crashes on Sequoia betas using the same private `CGVirtualDisplay` API this app depends on. If you have a choice, run on the newest macOS you can get. |
| **CPU** | **Apple Silicon preferred** | Two blockers only fire on arm64: `CGEventCreateScrollWheelEvent`'s variadic ABI (fixed in this tree by moving to `CGEventCreateScrollWheelEvent2`, never executed on arm64), and the CoreWLAN HostAP private SPI being called without `respondsToSelector:` (also fixed, also never executed on arm64). Apple Silicon also removes the Intel hardware-encoder size threshold and changes `THREAD_AFFINITY_POLICY` behaviour. |
| **Intel is acceptable** | with deltas | An Intel Mac on 14.2+ still closes the Process Tap, SCK video and audio, low-latency rate control, LTR and the permission model. It does **not** close the arm64 items (M1-17) or the universal-build items in Part B. Every task below marks which half it needs. Note that a **dual-GPU Intel Mac on 12.3–12.7.6** is a different guide's machine — do not adopt its items. |
| **Also required** | an **iPhone or iPad** | For the client-side halves of the hosted-network, input-remap and AirPlay-sender tasks. A second Mac is a bonus (a second AirPlay sender, and the two-machine playthrough-latency recording). |
| **Part B only** | Developer ID **Application** *and* **Installer** certificates, an App Store Connect API key or app-specific password for `notarytool`, and a **clean VM** (or a second Mac that has never run ScreenExtend) | The TCC-grant-survives-an-update test and the `spctl` assessment are meaningless on a machine that already trusts the app. |
| **Part B, owner-gated** | authorization from the repository owner to `git push` | The GitHub Actions workflows have never run. Running them requires a push, which is otherwise **forbidden** (§2). If you do not have explicit authorization in your own instructions, this single item stays open and is reported as such. |

**A machine below 14.2 must not run this guide.** The Process Tap items would silently select a
lower tier and you would report a verification that did not happen. If your Mac is 12.3–12.7.6,
you want `guides/intel-mac.md` instead. If it is 13.0–14.1, you can run everything here *except*
the Process Tap tasks (M1-07, M1-09) — say so explicitly in the report rather than marking them
done.

### 0.2 Preflight — prove the machine qualifies before you start

Run all of this and paste the output into `docs/discovery/G1-machine.md` (create it) before any
other work.

```sh
# 1. OS, build, architecture, model
sw_vers
uname -m                       # arm64 = Apple Silicon, x86_64 = Intel
sysctl -n machdep.cpu.brand_string hw.model hw.ncpu hw.memsize
system_profiler SPDisplaysDataType | head -40      # GPU(s) and displays

# 2. The three APIs this guide exists for must be present
#    (these are the same probes the code makes; if any is missing, the floor is wrong)
nm -gU /System/Library/Frameworks/CoreAudio.framework/CoreAudio | grep -c AudioHardwareCreateProcessTap
ls -d /System/Library/Frameworks/ScreenCaptureKit.framework
python3 - <<'PY'
import ctypes, ctypes.util
objc = ctypes.CDLL(ctypes.util.find_library("objc"))
objc.objc_getClass.restype = ctypes.c_void_p
for n in (b"CATapDescription", b"SCStream", b"SCShareableContent",
          b"SCContentFilter", b"SCStreamConfiguration",
          b"CGVirtualDisplay", b"CGVirtualDisplayDescriptor",
          b"CGVirtualDisplaySettings", b"CGVirtualDisplayMode"):
    print(n.decode(), "present" if objc.objc_getClass(n) else "ABSENT")
PY

# 3. Toolchain
xcode-select -p ; xcodebuild -version 2>/dev/null | head -2
xcrun --sdk macosx --show-sdk-path
xcrun --sdk macosx --show-sdk-version
rustc --version ; cargo --version
node --version ; pnpm --version
security find-identity -v -p codesigning      # Part B: Developer ID Application must appear
security find-identity -v | grep -i installer # Part B: Developer ID Installer must appear

# 4. Console session (NOT ssh). Several tasks create a real display; a headless
#    ssh session cannot, and initWithDescriptor:/applySettings: block indefinitely there.
who am i                                       # must show a console/tty, not pts over ssh
csrutil status                                 # record it; SIP ON is the interesting case
```

**Hard requirements from the preflight**

- `sw_vers` reports 14.2 or later.
- `CATapDescription` and the four `SC*` classes print `present`.
- The four `CGVirtualDisplay*` classes print `present`. If any is `ABSENT`, you have found
  something important (macOS finally removed the private class) — file it as a **blocker**
  finding immediately and continue with the audio and permission halves.
- You are on a **console session**. Everything that creates a virtual display, everything that
  measures capture, and every AirPlay test needs a real logged-in graphical session. Do not run
  those over SSH; the 10.15 dev box's entire display-verification gap exists because it was
  SSH-only.

Also, before every measurement session:

```sh
caffeinate -u -t 5     # wake the display
# then run the measurement itself under:  caffeinate -dims <cmd>
```

A locked or sleeping screen stalls WindowServer compositing and every capture number becomes
meaningless without saying so.

---

## 1. Model and orchestration directives

**Use Claude Opus 5 for every subagent you spawn. No exceptions** — not "this one is simple", not
for a file read, not for a doc edit. If a subagent spawns subagents, those are Opus 5 too.

**You are the orchestrator, not the workforce.** Partition the work in §8 across parallel Opus 5
agents, hand each one a brief specific enough that it does not re-derive context you already
hold, and spend your own context on synthesis, arbitration and the global picture. Parallelize
hard — many agents at once, not a queue of one. The one hard serialization is the machine
itself: **one release build at a time**, and **one measurement at a time** (see §3).

**You are fully autonomous. There is nobody to ask.** The person who handed you this guide is not
watching and cannot answer. Therefore:

- **Never ask a question and wait. Decide.** Make the best call on the evidence, act, and record
  it in a section of the final report titled **Decisions Made Without Consultation** so each one
  can be reviewed and reversed.
- **When in doubt, build it.** If you find a fix, a tier, a probe or an optimization worth having,
  *implement and measure it* rather than writing it up as a recommendation. Deleting a finished
  implementation costs a minute; a deferred idea costs a future agent a full re-research cycle.
- **Self-unblock.** Missing information is a research task (§9), not a blocker. Missing developer
  tooling is an install decision you make yourself. A failed approach is data — record it and try
  the next one.
- **Leave the tree reviewable.** No half-finished refactors, no debug scaffolding in shipping
  paths, no commented-out experiments. Anything incomplete is finished, reverted, or isolated on
  a clearly named local branch and called out in the report.

**Token and time cost are not constraints.** They must never influence scope. Every optimization
that shaves even **0.1 ms** on even one macOS build is in scope and gets implemented and
measured, even at high implementation cost. "Good enough", "diminishing returns", "minor" and
"speculative" are not reasons to stop. A speculative idea is resolved by trying it and measuring,
or by a **recorded rejection with a reason and a number** in `docs/OPTIMIZATIONS.md` — never by
being skipped.

---

## 2. Absolute rules

1. **Never `git push`. Never touch `origin`.** Not to `main`, not to a branch, not a tag, not a
   `--dry-run`. Do not open pull requests, do not create or edit issues or releases, do not touch
   remote refs or workflow runs. `origin` is read-only to you. Work on a local branch
   **`guide/modern-mac`** cut from `latency-overhaul`; the repository owner reviews and pushes.
   *The single exception* is Part B item M1-28, and only if your own instructions explicitly
   authorize a push. If they do not, that item stays open.
2. **Do not change how any UI looks.** The React desktop control UI (`src/`) and the client web
   page (`src-tauri/src/streamer/static/`) keep their current appearance, element structure,
   layout and copy. Performance behind the pixels is in scope; appearance is not. If a fix appears
   to need a visible change, implement the fastest version that preserves the appearance and write
   the visible alternative up under "UI-visible recommendations" in `docs/OPTIMIZATIONS.md`.
3. **Scope fence.** Only the files and subsystems this guide names (§4.2) may be modified.
   Everything else in this repository has already been audited, optimized and verified on the
   development machines, or belongs to another guide. A change outside the fence is a merge
   conflict against work you cannot see. If you find a real defect outside the fence, write a
   `findings/` file describing it and *leave it alone*.
4. **No link-time references to macOS APIs newer than 10.15.** This is the single easiest way for
   an agent on a modern Mac to destroy this product, and you will be adding modern APIs
   constantly. dyld binds every non-weak undefined symbol **before `main()`**, so one such
   reference means the app does not launch on Catalina — while launching perfectly on your Mac and
   on every machine that could debug it. Use `libc::dlsym`, `objc2::runtime::AnyClass::get`,
   `msg_send!`, and the `set_optional_*` helpers in `macos_utils/streamer/encoder.rs`. **`extern
   static` constants count**: two VideoToolbox property keys (`MaximizePowerEfficiency`, 10.14;
   `PrioritizeEncodingSpeedOverQuality`, 11.0) had to move to `set_optional_bool` for exactly this
   reason. Never add `objc2-screen-capture-kit` or any crate that links a post-10.15 framework.
   Audit after **every** macOS change:
   ```sh
   nm -u src-tauri/target/release/ScreenExtend | sort -u > /tmp/undef.txt
   grep -i -E 'displaystream|screencapturekit|CATap|SCStream' /tmp/undef.txt   # must be empty
   scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend --floor 10.15
   otool -L src-tauri/target/release/ScreenExtend                              # no ScreenCaptureKit.framework
   ```
5. **Probe capabilities, never version numbers.** Every tier is selected by attempting the real
   capability (class lookup, `dlsym`, a trial API call) and falling back at runtime without
   crashing; the selected tier is reported through `CompatibilityReport` and the log. A version
   check is a bug. There is at least one still in the tree and finding it is task M1-00.
6. **Real-time threads take no locks and do not allocate.** The Process Tap's
   `AudioDeviceIOProc`, the SCK audio sample handler, and the video capture callbacks convert into
   preallocated scratch and push into a ring; the worker thread does the rest. They talk to the
   async world over `crossbeam-channel` or the ring only. If your optimization introduces an
   allocation or a lock in a capture callback, it is not an optimization. (There is a known open
   violation here — `Producer::push` calling `Thread::unpark()` from the RT callback,
   `docs/RISK-REGISTER.md` R-249/R-426. Fixing it is task M1-07c.)
7. **`std::sync::Mutex` is the one blocking mutex type** for first-party code, and a `MutexGuard`
   is **never held across an `.await`**. Lock, copy, drop, then await. Watch the temporary-lifetime
   footgun: a guard produced in an `if let` / `match` scrutinee lives to the end of the block. The
   only two intentional async locks are `DISPLAY_CORRELATION_LOCK` in `streamer/server.rs` and
   `receive_error` in `windows_utils/driver_ipc/client.rs`. `parking_lot` stays confined to the
   vendored `windows_utils/windows_capture/` fork.
8. **`tprintln!` / `teprintln!`, never `println!` / `eprintln!`,** in the Rust core. They feed the
   2000-line log-bus ring, emit a `LogLine` event to the UI, and only echo to stdout when verbose
   mode is on (`serve -v`).
9. **A new Tauri command goes into all three `*_utils` modules** (`windows_utils`, `macos_utils`,
   `linux_utils`) **and into `collect_commands!` in `lib.rs`.** The Linux arm may `bail!` — note
   that **`linux_utils/` does not compile today** and is declared out of scope for this engagement,
   so you cannot check your Linux arm by building it; write it to the same signature as the other
   two and say in the report that it is unbuilt. **Never
   hand-edit `src/lib/bindings.ts`** — regenerate it from a debug build started with *no CLI
   arguments*, run from `src-tauri/`, then **diff it** (`node scripts/check-bindings.mjs` is the
   automated half). A new shell invocation goes into `src-tauri/capabilities/default.json`. A new
   user setting needs three touches: the TS config type + default, the Rust command, and the
   `serve`/`config` CLI path.
10. **The client page has no build step and no dependencies** and must keep parsing on old mobile
    WebKit. You are unlikely to touch it; if you do, that is another guide's territory (§4.2).
11. **The desktop UI must still run on Safari 13.1** (the WKWebView on the 10.15 floor). Shims live
    in `src/lib/legacy-webview-polyfills.ts`; `node scripts/check-legacy-webkit.mjs` re-derives the
    list from `dist/` after a build.
12. **DPR is only correct when the resolution multiplier and the OS scale step are the same
    number.** On macOS `platform::display_scale_steps()` is `[100, 200]` — HiDPI is a binary flag,
    not a ladder — so `snap_display_dpr` only ever yields 1.0 or 2.0. Pre-register every ladder
    mode at create time; a live display cannot be given new ones.
13. **Simultaneous displays are uncapped** everywhere except the macOS AirPlay tier, which returns
    `Some(1)` because macOS owns that display. Never evict a connected device to make room.
14. **No AI-tooling references in comments or code.** No model names, no "generated by", no
    agent-directed instructions in source. Commit messages read as ordinary engineering commits.
15. **The version string lives in four files** and moves together: `package.json`,
    `src-tauri/Cargo.toml`, `src-tauri/tauri.conf.json`, `src-tauri/tauri.macos.conf.json`
    (currently **0.6.3**; `node scripts/check-versions.mjs` asserts it). You should not need to
    change it.

---

## 3. Norms of research and measurement

**Research first, and prefer primary sources.** Vendor SDK docs and headers, WWDC session
transcripts, RFCs, and the source of Chromium, OBS, Moonlight/Sunshine, FFmpeg, webrtc-rs and
AudioCap. **If one shipping product demonstrably does the thing, it is possible** — "impossible"
is only allowed after a research pass found nobody doing it *and* you tried it and measured.
Never conclude something is impossible from your own priors.

**Measure before and after every change**, always as **p50 / p90 / p99 / max**, never a bare mean.
Jitter is felt more than the average; a pipeline with 20 ms mean and 60 ms p99 feels worse than
one with 30 ms mean and 33 ms p99. Nanosecond stamps come off the shared host clock
(`streamer::audio::host_now_ns`) — the same epoch the video RTP timestamps and the audio
`capture_ns` both ride, which is what makes A/V sync work at all.

**Never trust a flag without a read-back.** Every property that is set must be read back and
logged; every tier that is selected must announce itself. VideoToolbox silently ignores
properties it does not support. `kVTCompressionPropertyKey_AllowFrameReordering = false` is a
*request*; `max_num_reorder_frames` in the emitted SPS VUI is what a decoder actually obeys.
`UsingHardwareAcceleratedVideoEncoder` is reported by Chromium to be sometimes unset on the
low-latency `…rtvc` encoders, which is why the code prints `unknown` rather than guessing. Assume
nothing.

**Measurement hygiene** — this list is not optional, and every item on it cost this project a
misdiagnosis:

- **≥ 3 trials per cell**, report the median with p50/p90/p99/max, and say how many trials.
- **Check for competing work first.** One release build at a time; no compile running during a
  measurement; nothing else using the hardware encoder (that is its own test, M1-06, and it must
  be deliberate).
- **Leave ≥ 15 s between probe runs.** A probe that exits without tearing down leaves its virtual
  display attached and silently corrupts the next run's capture. This bit 4 of 12 runs during the
  original overhaul.
- **`caffeinate -u -t 5` before, and run under `caffeinate -dims`.**
- **State whether the machine was quiet**, on battery or mains, and its thermal state
  (`pmset -g thermlog` in another shell for long runs).
- **Measure sustained performance, not the first 30 seconds.** On a laptop an aggressive setting
  can win for a minute and then lose to thermal throttling. At least one 5-minute run per
  headline configuration.

---

## 4. The repository, and your territory in it

### 4.1 What ScreenExtend is

A Tauri 2 desktop app that turns any device with a web browser into a wireless second monitor. It
runs on the **host** (the PC or Mac being extended). A **client** (phone, tablet, laptop) joins by
opening a URL or scanning a QR code — no client install. The host creates a **real virtual
display per client**, GPU-encodes it, and streams H.264 over **WebRTC with WHEP signalling**. The
client page is dependency-free HTML/JS/CSS served by the host's own axum server and `include_str!`'d
into the binary — no bundler.

Two frontends: `src/` (the React desktop control UI, running in the Tauri webview on the host) and
`src-tauri/src/streamer/static/` (the client page). The Rust core is `src-tauri/src/`, with three
`cfg`-gated sibling platform modules — `windows_utils/`, `macos_utils/`, `linux_utils/` — each
meant to expose the same surface, glob-imported by `lib.rs`, so the Tauri command list is identical
across operating systems and dispatch happens at compile time. `streamer/platform.rs` is the
runtime dispatcher into the active backend. **`linux_utils/` does not compile**, implements only
part of the command set, and several of its stubs report success rather than "unsupported"; Linux is
declared out of scope for this engagement and no guide covers it. Do not touch it, and if you are
tempted to, put it in the report's "Recommendations requiring the user" instead.

```
src-tauri/src/
  cli.rs                     the shipped CLI: serve, status, qr, doctor, probe, logs, …
  lib.rs                     Tauri app; collect_commands!/collect_events!; setup
  logbus.rs                  tprintln!/teprintln! ring + LogLine event
  streamer/                  OS-independent streaming core
    server.rs                axum HTTP(S) per adapter; join; DISPLAY_CORRELATION_LOCK; DPR ladder
    session.rs               per-client state, overrides, OTP limiter, epochs, device trust
    webrtc_session.rs        WHEP/SDP, ICE, audio DataChannel, loss-based bitrate driver
    pipeline.rs              re-export of the active OS pipeline
    platform.rs              runtime dispatcher: DPI, thread tuning, display DPR ladder, audio start
    audio/                   AudioPacket, host clock (host_now_ns), AudioHub, Opus FFI + encoder
    input/                   remote input; macos.rs is the CGEvent backend
    static/                  the client web page
  macos_utils/               ← most of your territory
    streamer/{sck,cgds,encoder,gpu,frame,display,config,mach,qos,tuning,power,activity,pipeline}.rs
    audio/{mod,process_tap,sck_audio,format,ring,legacy/}
    permissions.rs  virtual_display.rs  networking.rs  hosted_network.rs  hostap.m
    compatibility.rs  device_reporter.rs  airplay/  mod.rs
  macos/ScreenExtendAudio/   the C++ AudioServerPlugIn (the 10.15–12.x audio tier)
docs/                        the ledgers (see §4.4)
findings/                    the work queue (see §4.5)
scripts/                     check-macos-dyld.sh, check-bindings.mjs, check-versions.mjs, …
```

### 4.2 Scope fence — the only files you may modify

**In scope:**

- `src-tauri/src/macos_utils/**` — in particular `streamer/sck.rs`, `streamer/cgds.rs`,
  `streamer/encoder.rs`, `streamer/mod.rs`, `streamer/pipeline.rs`, `streamer/qos.rs`,
  `audio/process_tap.rs`, `audio/sck_audio.rs`, `audio/mod.rs`, `audio/ring.rs`,
  `audio/format.rs`, `permissions.rs`, `virtual_display.rs`, `compatibility.rs`,
  `networking.rs`, `hosted_network.rs`, `airplay/`, `mod.rs`.
  The **legacy audio installer** (`audio/legacy/installer.rs`, `routing.rs`) is in scope **for
  Part B only** — the signed-`.pkg` install path.
- `src-tauri/src/streamer/input/macos.rs` and `streamer/input/mod.rs` (the `probe_backend` tier
  only).
- `src-tauri/src/streamer/server.rs` — **only** the `DISPLAY_CORRELATION_LOCK` release point and
  its log line (M1-25). Nothing else in that file.
- `src-tauri/Info.plist`, `src-tauri/Entitlements.plist`, `src-tauri/tauri.macos.conf.json`,
  `src-tauri/.cargo/config.toml`.
- `src-tauri/macos/ScreenExtendAudio/scripts/*` (Part B).
- The macOS test modules: `macos_utils/**/test*/`, the inline `#[cfg(test)]` modules in the files
  above, and `#[ignore]`d hardware tests.
- `docs/**` and `findings/**` — always.
- `.github/workflows/ci.yml` — the `macos-smoke` and `dyld-floor` jobs only (Part B).

**Out of scope — do not touch:**

- Everything under `windows_utils/` and `linux_utils/`.
- The client page `src-tauri/src/streamer/static/**` — its whole verification belongs to
  `clients-network-and-glass-to-glass.md`.
- The React UI `src/**`, `vite.config.ts`, `eslint.config.js`.
- `streamer/{webrtc_session,bitrate,congestion,pacer,packetizer,negotiate,sockets,repair,keyframe,cloud,tls,session}.rs`.
- `src/lib/bindings.ts` (generated).
- The version strings.

If you believe something outside the fence must change, write the finding and stop.

### 4.3 The measurement harness

Everything here is already built; extend it rather than inventing a parallel one.

```sh
# Build with the probe harness (required for --whep-selftest)
cd src-tauri && cargo build --release --features probe

# The whole host path: real virtual display, real WHEP join over loopback,
# per-stage telemetry printed on exit
./target/release/ScreenExtend probe --whep-selftest --duration 30

# Real content through the real capture path, to a file
./target/release/ScreenExtend probe --live out.h264 --duration 30 --workload motion
#   --workload <mixed|static|scroll|motion|drag>   (implies --synthetic-pattern)

# Glass-to-glass overlay: a large frame number + elapsed-ms clock painted into
# the encoded frames, for the camera method
./target/release/ScreenExtend probe --live --duration 60 --synthetic-pattern --workload motion --counter

# Single-frame capture, encoder smoke, adaptive-bitrate path
./target/release/ScreenExtend probe --capture cap.png
./target/release/ScreenExtend probe --encode out.h264
./target/release/ScreenExtend probe --bitrate

# System audio: packet rate, subscriber inter-arrival, capture→subscriber latency,
# capture_ns skew, packet-size histogram
./target/release/ScreenExtend probe --audio --duration 15

# From a running host
ScreenExtend serve -v          # one summary line per session every 5 s
ScreenExtend status --json     # the same counters, per session, from another shell
ScreenExtend doctor --json     # capability report + per-probe timings; exit 0 ok / 1 unsupported OS / 3 permission denied
ScreenExtend logs --lines 400  # the 2000-line ring backlog

# In the client page's console (a real device — you have a phone)
SEVideo.getStats()   ·   SEAudio.getSyncInfo()
```

Useful selectors for the tuning knobs: `--encoder <auto|nvidia|intel|amd|software>`,
`--scale`, `--qp`, `--h264-profile`, `--intra-refresh`, `--max-fps`, `--monitor`,
`--max-bitrate-kbps`, `--turn-*`.

The `#[ignore]`d hardware tests that exist **in this tree today** and that you will use:

```sh
cd src-tauri
cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency      -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::pipeline::tests::production_pipeline_smoke -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::hw_capability_probe      -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::encode_synthetic         -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::recreate_churn_releases_hw -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::creates_small_via_software -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::probe_main_display          -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::probe_virtual_display       -- --ignored --nocapture
cargo test --release --lib macos_utils::virtual_display::tests::time_a_retina_join         -- --ignored --nocapture
cargo test --release --lib macos_utils::networking::tests::measure_adapter_watch_cost      -- --ignored --nocapture
cargo test --release --lib macos_utils::networking::tests::report_wifi_naming_and_radio    -- --ignored --nocapture
cargo test --release --lib macos_utils::hosted_network::tests::report_hostap_surface       -- --ignored --nocapture
cargo test --release --lib macos_utils::hosted_network::tests::measure_hostap_start        -- --ignored --nocapture
cargo test --release --lib macos_utils::hosted_network::tests::sweep_security_types        -- --ignored --nocapture
cargo test --release --lib macos_utils::airplay::tests::live_end_to_end                    -- --ignored --nocapture
cargo test          --lib legacy_live                                                       -- --ignored --nocapture
```

Three more exist and are the ones `docs/BASELINE.md` and
`docs/manual-tests/macos-capture-encode.md` quote:

```sh
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate            -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate_downscaled -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::the_sps_promises_no_reordering -- --nocapture
```

**The ScreenCaptureKit equivalent of the `capture_rate` pair does not exist and you write it** —
that is task M1-00 in §5.1, and several tasks below read its output.

**Skipping convention.** A hardware- or OS-gated test never returns silently: it calls
`crate::test_support::skip(what, reason)` or `skip_because(what, reason)`, which prints
`SKIP <what>: <reason>` on stderr, and **`SE_REQUIRE_HW=1` turns a skip into a failure**. Set
`SE_REQUIRE_HW=1` on this machine for every capability it is supposed to have — that is the whole
point of the convention, and it is how you find a test that is quietly not running. Anything you
add follows the same pattern, with `#[ignore = "<reason>"]` so the harness prints the reason
instead of a bare `ignored`.

The client page's own harnesses run with `pnpm test:client` (`scripts/test-client.mjs`, seven
harnesses). Nothing in this guide changes the client page, so you should not need them — but run
them once before your first commit so a red result is not attributed to you later.

### 4.4 The ledgers — how to record everything

- **`docs/OPTIMIZATIONS.md`** — one table row per change attempted:
  `| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |`.
  Failures are recorded on purpose; a reverted optimization with a number is valuable.
  Status vocabulary:
  - **`VERIFIED`** — executed and measured on the machine.
  - **`COMPILE-CHECKED`** — builds and passes static/layout tests, never executed.
  - **`UNVERIFIED`** — written from research; no runtime signal.

  **Your job is to move the rows this guide names to `VERIFIED` with numbers, or to revert them
  with reasons.** Edit the existing row's `Status`, `After` and `Notes` columns in place; do not
  add a duplicate row for a verification. Add *new* rows for changes you make.

  **A note on the row numbers this guide cites.** Rows 220–226 were once byte-identical duplicates
  of rows 189–195 (the macOS `PF_ROUTE` watcher, the `activate_mode` condvar, `setSerialNum:`, the
  IPv6 bind, HostAP liveness, AirPlay 0x05 reports, the Wi-Fi channel/AWDL report) — a merge had
  duplicated the block. **The duplicates have been deleted**; only 189–195 survive. Where this guide
  writes "190/221" or "193/224" it is naming the same row by both of its historical numbers, so that
  a citation you find in an older document still resolves. Edit **189–195**.

- **`docs/CAPABILITY-TIERS.md`** — one row per subsystem × tier:
  `| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |`.
  Flip the macOS rows this guide names.

- **`docs/BASELINE.md`** — the pre-change numbers. Its macOS section is dated to a different
  commit and says so. **Add a new section for your machine**; do not overwrite the 10.15.8 rows,
  they are the floor that proves the old path still works.

- **`docs/TELEMETRY.md`** — what each stage name means and how to read it. Read it before your
  first measurement.

- **`docs/manual-tests/`** — the scripts this guide executes:
  `macos-capture-encode.md`, `macos-audio.md`, `macos-permissions.md`, `macos-hosted-network.md`,
  `macos-build.md`, `macos-app-signing.md`, `macos-audio-driver-signing.md`, and
  `core-closeout.md` §2. **Update each one as you go**: replace "COMPILE-CHECKED — needs a 14.2+
  Mac" with the numbers you measured, and add anything the script failed to anticipate.

- **`docs/RISK-REGISTER.md`** — the silent fallbacks, panic paths and ABI contracts. The entries
  that bind you: **R-232** (every macOS 12.3+ path), **R-233** (Apple Silicon), **R-234**
  (universal artifacts, x86_64-only `libopus.dylib`), **R-235** (Developer ID, TCC identity,
  translocation), **R-236** (`nm -u` on a real build), **R-242/R-243** (AirPlay maxFPS and whether
  a modern sender completes the handshake), **R-249/R-426** (`unpark` inside the RT audio
  callback), **R-293** (two IOSurfaces pinned against a queue depth of 2).

### 4.5 `findings/` — your work queue and your crash-recovery point

`findings/` at the repository root is gitignored via `.git/info/exclude` (not `.gitignore`).
**Every problem, opportunity, suspicion and idea becomes one file**, named
`<severity>-<component>-<slug>.md` with severity in `blocker | major | minor | speculative`:

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

**One finding per file. Write it when you find the thing, not at the end of the session. Delete
the file the moment the finding is resolved** — implemented and verified, or deliberately
rejected with the reasoning recorded in `docs/OPTIMIZATIONS.md` **first**. Deleting is how you
mark it done; do not archive it, do not move it to a `done/` folder. The lifecycle is
empty → very full → empty, and a non-empty `findings/` at the end means the work is not done.

**`findings/` is essentially empty when you start, and that is deliberate** — the queue was worked
down to nothing before this guide was written, and everything that survived the sweep became either
a task in §8 or a recorded rejection in `docs/OPTIMIZATIONS.md`. Read whatever is there once, at the
start; it is unlikely to be yours. Findings in `findings/_out-of-scope/` (Linux) are not yours, and
`findings/_briefs/` and `findings/_STATUS.md` are historical orchestration notes — read them for
context if you like, but they are not your queue.

Five earlier findings resolved *into this guide* rather than into code, so their substance is here
and their files are gone. You will meet all five as tasks, and the reason they are listed is so you
recognise them if an older document points at a filename you cannot find:

| Was | Now |
|---|---|
| `major-vt-ltr-frames-would-replace-idr-on-every-pli.md` | **M1-29**, with the design carried over in full, plus `docs/OPTIMIZATIONS.md` row 305 |
| `major-cgds-displaytime-is-a-future-presentation-time.md` | fixed in code (`streamer/clock.rs::os_frame_time_or` refuses a stamp in the future); what remains is **M1-01**'s question — does a modern macOS still schedule `displayTime` ahead of the callback, and what shape is SCK's PTS? |
| `minor-audio-mac-native-tiers-untested-while-the-dead-tier-is-well-tested.md` | **M1-10**'s second half (R-225: the tier tests are themselves version-gated) |
| `minor-vdisplay-mac-airplay-can-report-the-geometry-it-negotiated.md` | **M1-26** step 3 (`created_geometry`) |
| `minor-build-perf-gate-cannot-cover-macos-no-test-prints-a-timing.md` | **§7.2**, the macOS arm of `scripts/perf-gate.mjs` |

**The perf gate, since you are adding a macOS arm to it.** `scripts/perf-gate.mjs` takes several
`--input` files and uses the **lowest** reading per metric — the one least polluted by contention —
then compares against `ci/perf-baseline.json`. The **file-wide tolerance is 20 %**, because a shared
runner is not quiet; **a key may carry its own tolerance and `dev-windows-rtx3080ti` carries 10 %**,
because an idle dev box is a tighter instrument. Ten per cent is the tightest honest number even
there: five consecutive runs of one unchanged release binary spanned 4157–4490 µs, an 8.0 % spread.
**Do not seed a macOS key below 10 %**, and seed it with `--seed` from a genuinely quiet machine.

### 4.6 Gates and commits

Before **every** commit, from `src-tauri/`:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

After any FFI change (which is most of this guide), additionally:

```sh
cargo build --release
scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend --floor 10.15
# and, if you can get a 10.15 SDK onto the machine, the strong form:
scripts/check-macos-dyld.sh src-tauri/target/release/ScreenExtend /path/to/MacOSX10.15.sdk
```

Read `docs/manual-tests/macos-build.md` §3 for what the two forms cost: the `.tbd` export stubs
of the real 10.15 SDK are the authoritative half of the verdict, and a newer SDK's stubs are a
superset, so `--floor 10.15` only still catches symbols the headers annotate as introduced after
the floor. Treat `--floor` as a smoke test. Getting a copy of the 10.15 SDK onto this machine is
worth an hour and turns the whole guide's dyld safety from "probably" into "proved".

From the repository root, when `src/` or `src/lib/bindings.ts` changed:

```sh
pnpm lint
pnpm build
node scripts/check-legacy-webkit.mjs
node scripts/check-bindings.mjs
node scripts/check-versions.mjs
```

**Commits** are small and single-purpose; a performance commit **states its measured
before/after** in the message. Commit at least every 30 minutes. Every commit message ends with:

```
Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
Claude-Session: <the URL of your own session>
```

Branch: `guide/modern-mac`, cut from `latency-overhaul`. Create as many local sub-branches and
worktrees as you like for parallel agents; give each worktree its own `CARGO_TARGET_DIR` so
builds do not contend for the cargo lock, and **never** run more concurrent release builds than
the machine has comfortable cores.

---

## 5. The current state of this area

### 5.1 First: confirm the tree is the one this guide describes

Every macOS workstream has merged into `latency-overhaul`, so **every task in §8 is a verification
task**, not an implementation one. The check below takes a minute and exists so that you find out
now rather than three hours in. Run it and paste the output at the top of your report:

```sh
git log --oneline -5
cd src-tauri

# 1. The SCK selection probe must be a class lookup, not a version check.
grep -n "fn screencapturekit_available" -A5 src/macos_utils/streamer/mod.rs
#    expect: AnyClass::get(c"SCStream").is_some() && AnyClass::get(c"SCShareableContent").is_some()
#    inside a OnceLock. `macos_at_least(12, 3)` here would mean the tree is older than this guide.

# 2. The SCK stream configuration.
grep -n "QUEUE_DEPTH\|setQueueDepth\|minimumFrameInterval" src/macos_utils/streamer/sck.rs
#    expect: QUEUE_DEPTH = 8 and a minimumFrameInterval of 0.9/fps.

# 3. The VideoToolbox session-spec ladder and the property dump.
grep -n "HwPolicy\|PreferredEncoderGPURegistryID\|VTCopySupportedPropertyDictionaryForEncoder" \
     src/macos_utils/streamer/encoder.rs
#    expect: a HwPolicy enum with Require/Enable/Forbid, the ladder built from it, the GPU pin
#    reaching build_encoder_spec, and the property dictionary queried per session.

# 4. The SCK presentation timestamp is read, not discarded.
grep -n "os_time_ns" src/macos_utils/streamer/sck.rs
#    expect: super::clock::os_frame_time_or(...), not a literal 0.

# 5. The tests several tasks depend on.
grep -rn "fn capture_rate\|fn the_sps_promises_no_reordering" src/macos_utils/
#    expect: cgds.rs::capture_rate, cgds.rs::capture_rate_downscaled,
#            encoder.rs::the_sps_promises_no_reordering
```

**If any of the five disagrees**, you are on an older tree than this guide was written against.
Say so at the top of the report, then read `docs/OPTIMIZATIONS.md` rows 141, 143, 152, 153, 155,
280 and 283 and `docs/research/macos-capture-encode.md` §5 (R3/R7/R10/R11/R13/R14), which specify
each of those changes precisely, and land them before measuring anything — otherwise a
`COMPILE-CHECKED` row gets marked `VERIFIED` against code that is not there.

**One instrument is genuinely missing and you must write it (task M1-00).**
`cgds::tests::capture_rate` and `capture_rate_downscaled` measure the CGDisplayStream path at native
and half size and print the delivered fps, `CGDisplayStreamUpdateGetDropCount`, the mean time inside
the delivery callback, and the `displayTime → callback` distribution. **There is no ScreenCaptureKit
equivalent, and SCK's presentation timestamp has never been measured on any machine this project
owns.** Write the SCK pair, in the same style and printing the same fields plus the
`SCFrameStatus` distribution and `frames with no PTS: N/M`. Several tasks below depend on it —
M1-01 and M1-20 both read its output — so do it first. Record it as a new `docs/OPTIMIZATIONS.md`
row and as a new `#[ignore]`d hardware test using `crate::test_support::skip_because` for its
runtime gate, the way every other hardware-gated test in this tree does.

### 5.2 What has landed here, and what state it is in
**A note on the `W…` labels you will meet in the ledgers, in the risk register and in this file.**
The engagement that produced this repository's current state ran as numbered parallel workstreams:
`W1` transport · `W3a`/`W3b` Windows capture and encode · `W4`/`W4b` AMD · `W5` macOS capture and
encode · `W6` macOS permissions · `W7` macOS audio · `W8` Windows audio · `W9` Windows platform ·
`W10` input · `W11` the client page · `W12` the desktop UI · `W13a`/`W13b` build, CI and hygiene ·
`W14` AirPlay · `W15` tests. They are **provenance labels only** — they tell you which change series
a row came from so a ledger note reads in context. They are not people, none of them is still
running, nothing is waiting on one, and **every one of them has merged into `latency-overhaul`.**


These rows exist in `docs/OPTIMIZATIONS.md` and describe your territory. The ones marked
COMPILE-CHECKED or UNVERIFIED are what you are here to flip.

| Row | Area | Change | Status today |
|---|---|---|---|
| **2** | permissions | Screen Recording read via `CGWindowListCopyWindowInfo` + `kCGWindowName`; `CGPreflightScreenCaptureAccess` demoted to a **positive-only** signal; `CGDisplayStream` removed from the authorization decision | VERIFIED on 10.15.8 for the CGDS/window-list paths; **UNVERIFIED** for the 12.3+ SCK confirmation and all 15/26 behaviour |
| **38** | hosted network | HostAP SPI guarded by `respondsToSelector:`; a probed tier (HostAP SPI → IBSS → Unsupported); a most-secure-first `securityType` ladder | VERIFIED (probe only, 10.15.8); **UNVERIFIED** for association, DHCP, throughput |
| **39** | virtual display | `create_display_with_modes` implemented; HiDPI derived from the ladder so the display is born at the client's DPR; the mode list carries the primary mode, its transpose and every ladder entry | **COMPILE-CHECKED** |
| **41** (=189/220) | network watcher | `PF_ROUTE` routing-socket watcher with a 250 ms debounce and a 30 s backstop, replacing a 1 s poll | **COMPILE-CHECKED** in the ledger; VERIFIED as a tier on 10.15.8 |
| **42** | session | `set_server_ports` sends each client a `Bye` and restarts gracefully instead of dropping connections | **COMPILE-CHECKED** |
| **43** | input | `CGEventCreateScrollWheelEvent` → the non-variadic `CGEventCreateScrollWheelEvent2` | VERIFIED on 10.15.8 Intel; **UNVERIFIED on arm64**, which is the whole point of the change |
| **66** | input | Ctrl↔Command remap conditional on the client's reported platform (`ClientPlatform::from_reported`) | VERIFIED by unit test; **UNVERIFIED** — no Mac or iPad client was available to type on |
| **79** | build | `MACOSX_DEPLOYMENT_TARGET = "10.15"` in `src-tauri/.cargo/config.toml` | **UNVERIFIED on an artifact** — no release build with the pin exists |
| **82** | CI | `.github/workflows/ci.yml` (six jobs) + `scripts/perf-gate.mjs` + `ci/perf-baseline.json` | **UNVERIFIED** — validated by parsing YAML; the workflow has never run |
| **112** | audio | Tap aggregate `kAudioDevicePropertyBufferFrameSize` set to `clamp(128, range)` | VERIFIED on the legacy tier; **COMPILE-CHECKED for the tap aggregate** (needs 14.2+) |
| **113** | audio | `Producer::push`/`Consumer::pop` split-at-wrap `copy_nonoverlapping`; `convert_interleaved` fast path for 48 kHz stereo f32 | **COMPILE-CHECKED** — no per-callback timing harness on this tier |
| **116** | audio | Volume-key `CGEventTap` callback classifies and `try_send`s to a worker instead of doing CoreAudio IPC inline | **COMPILE-CHECKED** |
| **117, 169, 170** | audio | SCK audio: both async→sync bridges 5 s → 1.5 s; `start_capture`'s unbounded `ready_rx.recv()` given a 15 s ceiling; teardown blanks the delegate ivar, stops the stream, posts a synchronous barrier on the serial delivery queue, then frees the context; the dummy video stream gains `showsCursor: false` and `queueDepth: 3` | **COMPILE-CHECKED** (13.0+ path) |
| **141, 155** | encode | `VTCopyVideoEncoderList` logged once per process; `VTCopySupportedPropertyDictionaryForEncoder` per session; `ConstantBitRate` and `BaseLayerFrameRate` probed and deliberately **not** set | VERIFIED as a mechanism on 10.15.8; the **modern encoders' key sets are UNVERIFIED** |
| **143** | encode | `PreferredEncoderGPURegistryID` from `CGDirectDisplayCopyCurrentMetalDevice` | **COMPILE-CHECKED** — a single-GPU Mac correctly reports `gpu_registry_id=none` |
| **144, 147** | encode | `DataRateLimits` removed, `MaxFrameDelayCount` unset, `MaxKeyFrameIntervalDuration` 10 s → 600 s; frames in flight counted and capped at 2, standing aside to 16 after 100 ms of no output; the blocking `CompleteFrames` gone from the encode loop | before = capture→encoded p50 **13.70** / p99 **15.60** ms; the **after run was never taken** |
| **148** | encode | Three consecutive submit failures rebuild the encoder, old session released first | **COMPILE-CHECKED** — not reproducible on demand there |
| **150** | scheduling | One scheduling scheme per thread; every `thread_policy_set` / `pthread_set_qos_class_self_np` return checked and logged; `THREAD_TIME_CONSTRAINT` computation `period/8` → `0.75 × period`, read back with `thread_policy_get` | VERIFIED on 10.15.8 Intel only |
| **152, 153** | capture | The SCK configuration and the class-lookup tier selection + 1500 ms frame gate | **COMPILE-CHECKED** / frame gate **UNVERIFIED** |
| **178** | build | `NSSupportsAutomaticGraphicsSwitching = true` in `src-tauri/Info.plist` | **COMPILE-CHECKED**; the plist is now wired through `bundle.macOS.infoPlist` |
| **190/221, 191/222** | virtual display | `activate_mode`'s settle wait is a condvar fed by `CGDisplayRegisterReconfigurationCallback` instead of a 5/25 ms sleep-poll; `setSerialNum:` is FNV-1a over the display name instead of a process-local counter | VERIFIED (registration takes); **UNVERIFIED (the saving)** — needs a console session |
| **193/224** | hosted network | The liveness watcher promotes the status when a session appears from the AP's own /24, and after 60 s with nothing associated logs the three usual causes | **COMPILE-CHECKED** |
| **203** | input / compat | `CompatibilityReport.input_backend` on all three platforms from `streamer::input::probe_backend()`; on macOS re-probed on the injector's 500 ms geometry cadence | **COMPILE-CHECKED**; **UNVERIFIED** — needs a Mac with Accessibility revoked mid-session |
| **206** | session | `DISPLAY_CORRELATION_LOCK` released as soon as the backend names its own display; a wait over 1 ms is logged | **COMPILE-CHECKED** |
| **258, 259** | permissions | `AXUIElementSetMessagingTimeout(el, 0.0)` restores the system default before returning; `check_permissions` / `request_permission` are `async` on `spawn_blocking`; the window-list probe warmed at startup | **COMPILE-CHECKED** |
| **260** | permissions | A session watchdog re-reads `screen_recording_live()` every 30 s while any session holds a live display and ends the sessions on a definite `Some(true) → Some(false)` | **UNVERIFIED** — needs macOS 15+ |
| **261, 284** | airplay | `airplay::probe()` wired into `doctor`; `Receiver::take_geometry_changed()` into the reconfig epoch; the tier stores the geometry it negotiated instead of discarding it | **COMPILE-CHECKED** |
| **262** | session | The macOS adapter thread is named `se-adapter-<ip>` and its spawn failure is reported | **COMPILE-CHECKED** |
| **264, 265** | build | `bundle.macOS.infoPlist` + `minimumSystemVersion` in both configs; the audio `.pkg` carried by `bundle.macOS.files`; the Windows CSP restored on macOS plus `worker-src 'self' blob:; child-src 'self' blob:` | **COMPILE-CHECKED** |
| **280** | encode | The software-encode setting reaches VideoToolbox via a three-state `HwPolicy` | VERIFIED that the flag reaches `EncoderConfig`; **which encoder VideoToolbox then picks is COMPILE-CHECKED** |
| **283** | encode | Long-term reference frames: `EnableLTR` probed and reported, **not enabled** | **PROBED AND REPORTED, NOT ENABLED.** The blocker the row was written under is gone — see row 305 |
| **305** | encode / repair | The VideoToolbox LTR **adapter**: enable, read the acknowledgement token off each sample, feed back the acknowledged capture instant, make `request_repair` mean `ForceLTRRefresh` | **UNVERIFIED — see this guide, M1-29.** Fully designed against code that exists; not landed, because `EnableLTR` reads `absent` on the 10.15.8 dev Mac so every line of it is unreachable there |

### 5.3 What was already tried and rejected here — with the numbers

Do not re-try these blindly. If you re-open one, say why and bring a number.

**macOS capture / encode (from the W5 close-out):**

- **One shared VideoToolbox session between clients at the same resolution.** Rejected as a
  session-lifecycle change, not an encoder one: the pipeline, the bitrate controller, the IDR
  request and the telemetry are all per-session by construction, and a shared encoder needs a
  per-client keyframe policy (a joining device needs an IDR the others do not). What landed is the
  detection half — `InstanceLimit` read out of `VTCopyVideoEncoderList` and logged, and a session
  that falls to software says so loudly. **If your machine's `InstanceLimit` turns out to be small
  and you can exhaust it with real clients, this becomes worth re-opening — bring the number.**
- **Falling back from ScreenCaptureKit to CGDisplayStream mid-session.** Rejected: the tier is
  decided at start and committed only after a delivered frame. Swapping backends under a live
  session risks a worse failure than the stall it papers over, and a WindowServer stall — the
  usual cause — is not something the other API fixes. The watchdog that *does* help is the
  first-frame deadline: after 750 ms with nothing captured the encode loop synthesizes a black IDR
  and counts `capture_no_first_frame`.
- **A Metal convert/scale stage in `gpu.rs`.** Rejected: WindowServer delivers the frame already
  scaled to the requested size and already in `420f`, and the delivered IOSurface is wrapped
  rather than copied into VideoToolbox — there is no CPU conversion to move onto the GPU. Whether
  WindowServer's own downscale costs anything is a separate question and it has an instrument
  (the `capture_rate` / `capture_rate_downscaled` pair).
- **`kVTCompressionPropertyKey_ConstantBitRate`.** Rejected: CBR spends bits to hold the rate on a
  screen that is not changing, which is the opposite of what a screen sharer wants.
  `AverageBitRate` plus the loss-based controller is the right shape. It is probed and named in
  the property line so the decision is visible.
- **`DataRateLimits`.** Removed and must stay removed: Chromium documents it as incompatible with
  `EnableLowLatencyRateControl`, and QA1958 defines it as a hard cap fighting the loss-based
  controller in `webrtc_session.rs`.
- **`CGVirtualDisplaySettings.refreshDeadline`.** Absent on the 10.15 floor, so nothing there could
  measure it; probed with `respondsToSelector:` and deliberately not set. **On your machine it is
  likely present, and it is the only latency-named knob on the WindowServer side of the boundary
  where the largest single term in the host path lives. This is M1-20 and it is one of the highest
  expected-value experiments in this guide.**

**macOS audio (from the W7 close-out):**

- **No play-cursor clamp on macOS.** Windows loopback clamps `capture_ns` to
  `min(qpc_host_ns, now − read_duration)` because a loopback endpoint timestamp is a *play cursor*
  roughly 15 ms in the future. On the macOS legacy driver tier the measured signed skew is p50
  **−0.57 ms**, min −6.23, over 3277 packets — under one 2.67 ms IO cycle and in the *right*
  direction, because the driver stamps the mix when it exists rather than when it is heard.
  Applying the Windows clamp would flatten the ladder that produces exactly-5.000 ms packet
  spacing (0 of 3277 deltas were anything else), and `ts_clamped` reads 0. **The Process Tap's own
  skew is a separate, unmeasured question and it must be measured before that tier is trusted —
  that is M1-09.**
- **`mach_wait_until` for the shm reader.** Rejected: the Mach time-constraint policy was the
  defect, not the sleep. A time-constraint thread that blocks for an interval unrelated to its
  declared period is not honouring the contract and the scheduler's answer is to demote it.
  `user_interactive` plus a 250 µs poll — a quarter of the driver's IO cycle — is the whole win.
- **A sample-rate converter.** Rejected for now: Opus encodes 8/12/16/24/48 kHz only, the config
  fixes 48 000, and all three macOS tiers negotiate or declare 48 kHz. The `teprintln!` that fires
  if SCK ever delivers another rate is the trigger to revisit. **Watch for it — you are the first
  machine that can actually run SCK audio.**
- **Feeding measured loss into `OPUS_SET_PACKET_LOSS_PERC`.** Rejected — in-band FEC is LPC-layer
  only and cannot exist under `RESTRICTED_LOWDELAY` at 5 ms frames.

**AirPlay (from the W14 close-out):**

- **Relaying macOS's own AirPlay mirror stream instead of capturing and encoding.** Rejected on
  evidence: the type-0 payloads are ciphertext, and decoding them needs real FairPlay.
- **An AWDL transport.** Rejected for now with a number: the AWDL peer-to-peer interface index
  registration returned `-65540`.

**Transport, client page, Windows** — all rejected items in those areas belong to other guides.
The two you may hear about and must not re-open here: FEC is not built (webrtc-rs ships no
packetizer for it), and HEVC is not negotiated (the RTP payloader exists but no host backend can
encode it — *including* VideoToolbox, where nothing creates an HEVC session).

### 5.4 The numbers to beat

**The floor — macOS 10.15.8 Intel, CGDisplayStream + VideoToolbox with no low latency.** This is
the slowest configuration the code supports. **A modern Mac must beat every one of these**, and if
it does not, that is the single most important finding you can produce.

| Measurement | Value (1440×900 @ 60 Hz, medians of three 6 s trials) |
|---|---|
| capture → encoded, p50 / p99 / max | **13.61 / 15.06 / 20.64 ms** |
| frames captured in 6 s | 370–373 (≈ 62 fps), 360 submitted every trial |
| dropped by WindowServer | **0** on every trial |
| encoder output drops | **0** |
| VideoToolbox frames pending at flush | **0** |
| CGDS delivery-callback mean, native 1440×900 / half 720×448 | **311.0 µs / 88.3 µs** |
| `displayTime → callback` p50 / p90 / p99 / max | **−15.53 / −15.12 / −11.97 / −11.42 ms** (all negative; it is a *future* presentation time) |
| SPS `max_num_reorder_frames` | **0** |
| hardware H.264 ladder | OK at 1280×720 … **640×360**; `OSStatus −12915` at 576×324 and below (the rule is width ≥ 640 **or** height ≥ 480, a disjunction) |
| simultaneous hardware sessions | **6 of 6** created — the scarce-resource path was unreachable there |
| `probe --whep-selftest` 30 s | 1098 RTP packets; packetize 0.09 / 0.12 / 0.33 / 0.57 ms; socket_write 0.36 / 0.72 / 3.15 / 3.72 ms; pace_wait 0.00 throughout. (The 201 ms `encode` figure on that run is the 200 ms keepalive on a still screen, not latency — 156 of 161 frames were repeats.) |
| dyld floor | 540 undefined symbols, **0 offenders**, 0 weak imports |

**macOS audio, legacy driver tier, 10.15.8 — the tier you are replacing with two better ones:**

| Measurement | Value |
|---|---|
| capture → Opus packet, p50 / p90 / p99 | **−0.57 / 3.59 / 4.54 ms** |
| Opus encode, p50 / p99 | **0.09–0.12 / 0.16–0.27 ms** |
| `capture_ns` deltas not exactly 5.000 ms | **0 of 3277** |
| IO cycle | 128 frames (2.67 ms), requested and granted; range `[15, 4096]` |
| shm ring | 2,083,840 samples, backlog 0, **overruns 0** |
| `ts_clamped` | **0** |
| Windows endpoint-loopback tier, for comparison | capture → subscriber p50 **9.44** / p99 9.44 ms |

**The Windows host half, for cross-platform context** (RTX 3080 Ti, WGC + NVENC, static
1280×720@60): capture → socket **1.57 / 8.39 / 16.78 / 18.00 ms**, improved after the Windows
capture work to **p90 3.93 / p99 6.82 ms**. Capture delivery, not the encoder, is the largest
single host-side term on every row on both platforms.

**Competitor glass-to-glass, for positioning** (none run on this hardware; each is the best
independently measured figure available, or the vendor's own where nothing better exists):

| Product | Glass-to-glass | Basis |
|---|---|---|
| Parsec | 4–8 ms LAN @ 240 fps | vendor |
| Steam Link | 13–16 ms display latency at < 1 ms ping | user reports |
| **Luna Display (LIQUID 3.0)** | **11.3 ms mean USB / 22.4 ms mean Wi-Fi**, 50 samples | vendor bench |
| Moonlight / GameStream | 20–35 ms on a good LAN | community |
| Apple Sidecar | "consistently below 30" wired USB-C | reported |
| **Hopp (browser WebRTC)** | **98 ms** good net at 1080p, after tuning libwebrtc from 159 | published |
| Miracast | 80–100 ms optimized; spec ceiling 250 | certification spec |
| Duet Display | 95 ms USB / 108 ms Wi-Fi at 60 fps | adversarial bench |

Luna Display is the target (same job — a tablet as a second display over Wi-Fi) and Hopp is the
honest ceiling (same transport — a browser WebRTC client cannot get below its own decode and
compositor cost). The host half is 8.39 ms p90 on Windows, which leaves most of a Luna-class
budget for the network and the client. **On this Mac, establish where the macOS host half sits in
that budget.**

---

## 6. Phase 1 — read, then discover what this machine exposes

### 6.1 Read these in full before changing anything

Partition across parallel Opus 5 agents; every file below is read completely by at least one
agent, and each returns a structured report naming every function, every thread spawned, every
lock taken, every buffer copy, every FFI boundary, every `unwrap`/`expect` that can panic, every
silent fallback, and every place a fast path can degrade without telling anyone.

| Agent | Files |
|---|---|
| A | `macos_utils/streamer/{mod,sck,cgds,frame,gpu,display,config,clock,mach}.rs` |
| B | `macos_utils/streamer/{encoder,pipeline}.rs` |
| C | `macos_utils/streamer/{qos,tuning,power,activity}.rs` + `streamer/platform.rs` (macOS arms only) |
| D | `macos_utils/audio/{mod,process_tap,sck_audio,format,ring}.rs` |
| E | `macos_utils/audio/legacy/**` + `macos/ScreenExtendAudio/scripts/*` (Part B) |
| F | `macos_utils/{permissions,compatibility,device_reporter}.rs` |
| G | `macos_utils/{virtual_display,mod}.rs` + the `DISPLAY_CORRELATION_LOCK` region of `streamer/server.rs` |
| H | `macos_utils/{networking,hosted_network}.rs` + `hostap.m` |
| I | `macos_utils/airplay/**` (9 files) |
| J | `streamer/input/{macos,mod,protocol,scancode}.rs` |
| K | `streamer/audio/{mod,protocol,encoder,opus_sys}.rs` — the shared host clock and Opus wrapper |
| L | `src-tauri/{Info.plist,Entitlements.plist,tauri.macos.conf.json,.cargo/config.toml}`, `.github/workflows/{ci,build-release}.yml`, `scripts/check-macos-dyld.sh` |

And these documents, by you: `CLAUDE.md`, `docs/dev-workflow.md`, `docs/TELEMETRY.md`,
`docs/BASELINE.md`, `docs/CAPABILITY-TIERS.md`, `docs/LATENCY-MAP.md`, `docs/DATAFLOW.md` (the
macOS pixel and audio paths), `docs/RISK-REGISTER.md` §7, and the eight manual-test scripts named
in §4.4. Also `docs/research/macos-capture-encode.md`, `docs/research/macos-permissions-tcc.md`,
`docs/research/audio-capture-tiers.md` §2.6 and `docs/research/airplay-why-fast.md` — each ends
with a "cannot be verified on the dev boxes" list, and that list is largely this guide.

You should come out of Phase 1 able to answer, without looking anything up: *what happens between
the moment a pixel changes on the host and the moment it lights up on the client, naming every
function, thread, queue, copy, lock and syscall in order* — on this Mac, on this macOS.

### 6.2 Discover what the hardware actually exposes

Write the output of all of this to **`docs/discovery/G1-machine.md`**, alongside the preflight.

```sh
cd src-tauri && cargo build --release --features probe

# 1. The app's own view of the machine
./target/release/ScreenExtend doctor --json | tee /tmp/doctor.json | python3 -m json.tool
#    Record: .os.name .os.version .os.supported .audioBackend .displayBackend
#            .captureBackend .inputBackend .hostedNetworkBackend
#            .macos.probes.* (timings), .macos.translocated, .macos.designatedRequirement
#    Exit codes: 0 = fine, 1 = unsupported OS (a real failure), 3 = a required permission denied.

# 2. The VideoToolbox catalogue — one call, and it answers half this guide's open questions
cargo test --release --lib macos_utils::streamer::encoder::tests::hw_capability_probe -- --ignored --nocapture
#    Record per encoder: EncoderID, IsHardwareAccelerated, GPURegistryID, InstanceLimit,
#    and the full supported-property key set at the session's real frame size.

# 3. The display and GPU topology
system_profiler SPDisplaysDataType
ioreg -l | grep -i "IOGPUClassName\|model.*GPU" | head

# 4. The audio topology
system_profiler SPAudioDataType
#    plus the tier the app picks, from doctor's .audioBackend

# 5. The Wi-Fi surface
cargo test --release --lib macos_utils::hosted_network::tests::report_hostap_surface -- --ignored --nocapture
cargo test --release --lib macos_utils::networking::tests::report_wifi_naming_and_radio -- --ignored --nocapture

# 6. The virtual-display surface
#    (needs a console session; grep the log for the respondsToSelector line)
ScreenExtend serve -v 2>&1 | grep -E "CGVirtualDisplaySettings|display backend|refreshDeadline"

# 7. The dyld picture on this OS
cargo build --release
nm -u target/release/ScreenExtend | wc -l
nm -u target/release/ScreenExtend | grep -ci displaystream    # must be 0
otool -L target/release/ScreenExtend
otool -l target/release/ScreenExtend | grep -A4 LC_BUILD_VERSION
lipo -info target/release/ScreenExtend
```

Every value that surprises you becomes a `findings/` file before you go on.

---

## 7. Phase 2 — instrumentation and baseline on this hardware

**Take the baseline before you change a line.** Every claim later in this guide references it.
Write it as a new section of `docs/BASELINE.md` titled with your exact machine and OS build.

### 7.1 The matrix to fill

Three trials per cell, median reported, p50 / p90 / p99 / max throughout, `caffeinate -dims`, ≥ 15 s
between runs, nothing else compiling.

**Capture and encode**

| Cell | Command | Record |
|---|---|---|
| capture → encoded, native | `cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture` | p50/p90/p99/max, frames captured, WindowServer drops, encoder output drops, `vt_pending_at_flush`, and **which capture backend and which encoder** |
| capture rate, native and half | `cgds::tests::{capture_rate, capture_rate_downscaled}` | delivered fps, drop count, callback mean, `displayTime → callback` percentiles, frames with no `displayTime` |
| the SCK equivalent | the SCK version of the pair — **you write it, task M1-00** | the same, plus the SCK **presentation timestamp** distribution and the `SCFrameStatus` breakdown — never measured on any machine this project owns |
| hardware ladder | `encoder::tests::hw_capability_probe` | which sizes succeed; on Apple Silicon expect every rung down to 16×16 to pass, unlike Intel's 640-wide floor |
| SPS promise | `the_sps_promises_no_reordering` | `max_num_reorder_frames` must be 0 |
| whole host path | `probe --whep-selftest --duration 30` | every stage: capture delivery, scale, encode queue, encode, packetize, socket write, pace wait, capture → socket. Plus `frames` and `repeats`. |
| real content | `probe --live out.h264 --duration 30 --workload motion` and again with `scroll`, `static`, `drag` | capture delivery, scale, encode queue, encode, capture → encode-out, bytes |
| sustained | `probe --live --duration 300 --workload motion` | the same, plus whether the tail creeps over five minutes (thermal) |
| production smoke | `pipeline::tests::production_pipeline_smoke` | frames broadcast, bytes, IDR count, submit → broadcast p50/max |

**Audio**

| Cell | Command | Record |
|---|---|---|
| the selected tier | `probe --audio --duration 15`, with real audio playing (`say "one two three four five six seven eight nine ten"`) | `audio: capture backend = …`, `capture → subscriber` and `capture → packet` percentiles, `encode` percentiles, packets/s, payload average, silent packets, seq gaps, discontinuity, `period_frames`, `dropped_backpressure`, `ts_clamped`, the packet-size histogram |
| the Opus CTL read-back | the same run's `opus:` line | must match Windows field for field including `lookahead=120 samples (2.50 ms)` — the client uses that number to align playout |
| the `capture_ns` skew | the same run | the **signed** skew and the packet-spacing histogram — this is M1-09 and it decides whether the Windows play-cursor clamp is needed on the tap tier |
| each tier in turn | force the tier (deny Screen Recording; install/uninstall the driver) | the same table per tier — you are the only machine that can produce all three |

**Join and startup**

| Cell | Command | Record |
|---|---|---|
| time to first frame | join a real device, `ScreenExtend logs --lines 400` | display creation, mode registration, TLS handshake, ICE gathering, encoder init, first keyframe — and the total |
| Retina join | `virtual_display::tests::time_a_retina_join -- --ignored` | with and without HiDPI derived from the ladder; the saving row 39 claims |
| mode change | join, then change scale / orientation | wall time for an in-ladder change vs one outside it |
| display correlation | two clients joining simultaneously | the `display correlation lock: waited Nms behind another join` line |

**System**

`powermetrics --samplers cpu_power,gpu_power -i 1000 -n 30` during a live session, package power idle
vs streaming; `pmset -g assertions` while a session is up (M1-…, the `power.rs`/`activity.rs` half);
`thread_policy_get` read-backs from the `[qos]` log lines.

### 7.2 Extend the instrumentation where it is thin

- The macOS `probe_live` path drives the encoder directly rather than going through the pipeline,
  so it collects **no stage timings**. Fix that: route `probe --live` through the production
  pipeline on macOS the way it goes on Windows, so `capture delivery`, `scale` and `encode queue`
  have values. Without this, half the matrix above is empty on macOS and that is a defect, not a
  limitation.
- Print the **SCK presentation timestamp → callback** histogram exactly as the CGDS path prints
  `displayTime → callback`. If SCK's PTS is a real composite time rather than a scheduled
  presentation time, the capture term in `docs/LATENCY-MAP.md` becomes measurable rather than
  bounded, and that changes the whole macOS latency story.
- Add a macOS arm to `scripts/perf-gate.mjs` / `ci/perf-baseline.json`. It has none, because no
  macOS test prints a timing in the shape the gate parses — so make one print it (the SCK
  capture-rate pair from M1-00 is the obvious candidate, and `crate::test_support::percentiles`
  already produces the `p50 / p90 / p95 / p99 / max` line the gate's regex reads). Seed the key with
  `--seed` from a quiet machine; §4.5 has the tolerance rule and **10 % is the floor**.

---

## 8. Phase 3 — the work

Twenty-nine items — M1-00 and M1-01 … M1-29 — grouped into six workstreams that can run in
parallel. Each task says: what to verify, what to fix or optimize, how to measure, and what **done**
means. "Done" is always the same shape: **the named ledger rows flipped to `VERIFIED` with numbers,
or reverted with a reason and a number**, the manual-test document updated, and the relevant
`findings/` files deleted.

Suggested agent partition — six concurrent Opus 5 agents, plus you:

| Workstream | Items | Territory |
|---|---|---|
| **G1-A · capture** | M1-00 (the SCK capture-rate pair), M1-01, M1-21 (CGDS half) | `streamer/{sck,cgds,mod}.rs` |
| **G1-B · encode** | M1-02, M1-03, M1-04, M1-29, M1-05, M1-06 | `streamer/{encoder,pipeline}.rs` |
| **G1-C · audio** | M1-07, M1-08, M1-09, M1-10 | `audio/{mod,process_tap,sck_audio,format,ring}.rs` |
| **G1-D · permissions & identity** | M1-11 … M1-16 | `permissions.rs`, `compatibility.rs`, `streamer/input/` |
| **G1-E · display & session** | M1-19, M1-20, M1-21 (CGVirtualDisplay half), M1-22, M1-25, M1-26 | `virtual_display.rs`, `mod.rs`, `airplay/`, `server.rs` (lock only) |
| **G1-F · network** | M1-23, M1-24 | `networking.rs`, `hosted_network.rs`, `hostap.m` |
| **Part B (you, or a seventh agent)** | M1-17, M1-18, M1-27, M1-28 | build, signing, arm64, client-typed input |

Two serialization rules across all of them: **one release build at a time**, and **no measurement
while anything else is compiling or streaming.** Have the agents queue their measurement windows
through you.

---

### Workstream A — capture

#### M1-01 · The ScreenCaptureKit video tier
*Rows R152, R153 · tier row "Capture backend (macOS)" · `docs/manual-tests/macos-capture-encode.md` §1, §7b*

**Verify.** Join one device and confirm exactly one of these lines appears:

```
[sck]  capture started: display=1 1280x720 @ 60fps (queueDepth=8, minimumFrameInterval=0.9/fps)
[cgds] capture started: display=1 1280x720 @ 60fps (queueDepth=5, no minimumFrameTime)
```

**On 14.2+, `[cgds]` here is a failure.** CGDisplayStream is obsoleted in the macOS 15 SDK and
using it is what triggers Sequoia's recurring "ScreenExtend is recording your screen" alert. When
the fallback is taken it must always be preceded by its reason:

```
[capture] ScreenCaptureKit failed to start for display 1: <reason>; falling back to CGDisplayStream
[sck] no frame within 1500 ms of startCapture; treating ScreenCaptureKit as unusable on display 1
```

Then verify, one at a time, that each configuration change is *accepted* and *does something*:
`queueDepth` 8 (SCK requires 3..=8), `minimumFrameInterval = 0.9/fps` (deliberately slightly under
the frame period, so the OS is never the throttle), `SCFrameStatusStarted` accepted as well as
`Complete`, the presentation timestamp read off the `CMSampleBuffer` rather than discarded, the
colour space and pixel format matched to the CGDisplayStream path (`420f`), and `stop()` blanking
the delegate ivar before the delivery queue is drained to a barrier.

**Optimize.** A/B every one of them, on an animated screen, for delivered fps, drop count,
capture→encoded p50/p90/p99, and process CPU:

- `queueDepth` 3 vs 5 vs 8. The reason it was raised is the IOSurface use-count contract: the
  encode loop holds `last_frame` across the entire submit + `complete_pending()` block while the
  next frame arrives, so two surfaces are pinned in steady state (risk-register **R-293**). If your
  measurements show zero drops at depth 3, say so with the number — the depth is a cost, not free.
- `minimumFrameInterval` `0.9/fps` vs `kCMTimeZero` (no throttle at all). The research
  recommendation (R3) is to stop throttling capture entirely and let the pipeline decide.
- `SCStreamConfiguration` keys this tree does not set and 14/15/26 offer: `capturesShadowsOnly`,
  `ignoreShadowsDisplay`, `ignoreGlobalClipDisplay`, `preservesAspectRatio`, `showsCursor`,
  `streamName`, `captureResolution` / `SCCaptureResolutionType`, and the 14.0+ `SCContentFilter`
  scoping properties. Probe each with `respondsToSelector:`, set it, read it back, measure.
  Anything that reduces WindowServer's per-frame work is directly on the largest host-side term.
- `SCContentFilter` scoping: a filter that is `initWithDisplay:excludingWindows:` over our own
  windows versus the whole display.

**Measure.** `pipeline::tests::end_to_end_latency`, the `capture_rate` pair, `probe --live
--workload motion --duration 30` and a 300 s sustained run. Compare against the 10.15.8 floor in
§5.4 and against the CGDS path *on this same machine* (force it with a debug override; if there is
no override, add one behind an env var — `SCREENEXTEND_CAPTURE_BACKEND=cgds` — and document it).

**Done when:** row 152 and row 153 read `VERIFIED` with the delivered-fps / drop-count /
capture→encoded numbers per configuration, the "Capture backend (macOS)" tier row's frame gate is
`VERIFIED`, and `docs/manual-tests/macos-capture-encode.md` §1 carries the log lines this machine
actually printed.

#### M1-21 · Does CGDisplayStream still resolve on 15/26, and does the fallback still work?
*Tier row "CGDisplayStream symbols (macOS)" · risk-register R-232*

CGDisplayStream is **obsoleted** in the macOS 15 SDK. Everything about it in this tree goes
through `dlsym` at first use, cached — five functions and five CFString constants resolved through
`RTLD_DEFAULT` — precisely so that the day Apple removes it, the binary still loads and the
fallback simply reports the API as absent.

**Verify** on the newest macOS you have: `nm -u target/release/ScreenExtend | grep -i displaystream`
**must be empty**, and forcing the CGDS tier must either work or fail with a line saying the API is
absent — never a crash and never a silent hang. Record which. If CGDisplayStream still functions on
15/26, that is a result; if it does not, the "capture start fails with a line saying the API is
absent" tier has just become reachable for the first time and must be `VERIFIED` as such.

Also record whether macOS 15's recurring screen-recording alert fires for the SCK path, the CGDS
path, or both.

---

### Workstream B — encode

#### M1-02 · The VideoToolbox session-spec ladder
*Row R280 · tier row "VideoToolbox session spec (macOS)" · manual test §2, §2b, §3*

**Verify** the three rungs, in order, by reading the per-session lines:

```
[vt] 1280x720 would use encoder com.apple.videotoolbox.videoencoder.h264.rtvc advertising 41 properties
[vt] properties: RealTime=ok AllowFrameReordering=ok ProfileLevel=ok … EnableLTR=available …
[vt] H.264 encoder ready: 1280x720 @ 60fps, 5529 kbps, profile=…, low_latency=true
[vt] encoder identity: id=… hardware=true gpu_registry_id=… (…)
```

`low_latency=true` means rung 1 won. On 14.2+ it **must** be true; if it is false, find out why and
that is a blocker finding. `hardware=false` must always be followed by the loud
`[vt] this session is on the SOFTWARE H.264 encoder…` line. `gpu_registry_id=none` is **not**
evidence of software — Apple documents NULL there for a built-in-GPU hardware encoder — so only
`hardware` answers that question, and on the low-latency `…rtvc` encoders Chromium reports the
property is sometimes unset, which is why the line prints `unknown` rather than guessing.

Then verify **`HwPolicy::Forbid`**: turn on the software-encode setting (or run
`serve --software-encode`) and confirm, in this order:

```
[vt] software encode was requested, so this session forbids the hardware encoder (EnableHardwareAcceleratedVideoEncoder=false)
[vt] encoder identity: id=… hardware=false gpu_registry_id=none
[vt] this session is on the software H.264 encoder because software encode is switched on. …
```

The third line must be the *calm* one, not the loud warning — the loud one means the hardware
encoder was wanted and unavailable, and it must never appear for a session that asked for
software. **Fail if `hardware=true` with the setting on.** Record which encoder VideoToolbox picks
under `Forbid` — that is the specific thing row 280 marks `COMPILE-CHECKED`.

**Measure.** capture→encoded and encode-stage p50/p90/p99/max for: rung 1 (low latency +
hardware-required), rung 2 (hardware-required alone), rung 3 (hardware as a hint), and `Forbid`.
Four cells, three trials each. The rung-1-vs-rung-2 delta is the value of
`EnableLowLatencyRateControl` on this hardware and nobody on this project has ever seen it.

**Done when:** row 280 reads `VERIFIED` with those four cells, and the tier row's "rungs with low
latency and the GPU pin are UNVERIFIED" note is replaced by numbers.

#### M1-03 · The modern encoders' supported-property key sets
*Rows R141, R155 · tier row "VideoToolbox property support (macOS)"*

`VTCopySupportedPropertyDictionaryForEncoder` (10.13+) is queried at the session's **exact frame
size**, because the property set depends on the encoder VideoToolbox picks, which depends on the
frame size and the machine — not on the year. That mechanism is VERIFIED; the *key sets of modern
encoders* have never been seen.

**Produce the table.** For each of: the built-in display at 100 %, a phone-sized client (390×844)
at 100 %, the same phone at 50 % video scale, and a 4K client — dump the full key list, the
`EncoderID`, `IsHardwareAccelerated`, `GPURegistryID` and `InstanceLimit`, and the per-property
outcome (`set` / `rejected(status)` / `no-symbol` / `available-but-not-requested`). Put the table
in `docs/manual-tests/macos-capture-encode.md` §2.

**Then use it.** Every key in that list that this tree does not set is a candidate optimization,
and each one gets tried and measured, not skipped:

- `MaxAllowedFrameQP` / `MinAllowedFrameQP` (11.0+) — a per-frame QP floor and ceiling is the
  right shape of cap for a screen sharer, where `DataRateLimits` was the wrong one.
- `PrioritizeEncodingSpeedOverQuality` (11.0) and `MaximizePowerEfficiency` (10.14) — both are
  already reached through `set_optional_bool` (they had to be, invariant #4); confirm they are
  accepted here and A/B them.
- `BaseLayerFrameRate` / temporal layers — probed and deliberately not set. Re-open it: a base
  layer the client can decode alone is a real loss-resilience tier if the property is honoured.
- `ExpectedFrameRate`, `MaxKeyFrameInterval` / `MaxKeyFrameIntervalDuration` (currently 600 s),
  `AllowFrameReordering` (false — verify the SPS agrees), `RealTime` (this tree sets true; OBS
  ships false after measuring frame drops when combined with other settings — A/B it here).
- Anything in the dumped list this guide does not name. That is the point of dumping it.

**Done when:** rows 141 and 155 carry the modern key sets, and every candidate above is either
implemented with a measured win or recorded as a rejection with a number.

#### M1-04 · Long-term reference frames
*Row R283 and the new row 305 · tier row "Long-term reference frames (macOS)" · manual test §4 ·
the full design is task **M1-29** immediately below*

`EnableLTR` is 12.0+ and is currently **probed and reported, not enabled**, because the other half
is a client that acknowledges reference tokens over the control DataChannel — transport work that
belongs to another workstream.

**Verify** `EnableLTR=available` appears in the property line on this Mac. Record it per OS.

**Then decide, on evidence, whether to build the encoder half here.** The Windows analogue is
already built and measured: on NVENC a repair frame is **23,698 B against a 28,423 B IDR** on a
fixture that redraws every pixel every frame — the pessimistic case, and real screen content is
mostly static between frames, so the real ratio is better. The transport policy (`streamer/repair.rs`,
`RepairState::can_repair()`) already exists and already requires the encoder to report long-term
slots (`Encoder::ltr_slots`) **and** transport-wide feedback to have confirmed the client holds
one. macOS currently reports zero slots and keeps the IDR tier, which is correct-but-inert.

**Then build M1-29**, which is the whole of the remaining work and is specified below against code
that exists. If after M1-29 the tier is still inert on this machine, say so with the slot count and
leave the row `PROBED AND REPORTED, NOT ENABLED` with the reason updated.

#### M1-29 · The VideoToolbox long-term-reference adapter — token-shaped, not slot-shaped
*Row R283 and the new row 305 (`UNVERIFIED — see guides/modern-mac.md`) · tier row "Long-term
reference frames (macOS)" · manual test `macos-capture-encode.md` §4*

**This is the highest-value encoder change in the guide and the only one that is fully designed
before you start.** It was carried here from a finding that was resolved into this task; the design
below is the finding's own text, against code that is in your tree today. Read `streamer/repair.rs`
and `streamer/congestion.rs` before you begin.

**The state you inherit.** ScreenExtend's only loss-recovery mechanism on macOS is a forced IDR:
`Pipeline::request_idr` sets an `AtomicBool`, the encode loop reads it and passes `force_idr` into
`submit_one` → `Encoder::submit`, which turns it into
`kVTEncodeFrameOptionKey_ForceKeyFrame = true`. A comment in `encoder.rs` records that the
alternative was considered and rejected: *"intra-refresh requested but unsupported on macOS
VideoToolbox (no rolling intra-refresh); using PLI→IDR loss recovery instead."* That comment is
correct about rolling intra refresh and **wrong about the alternatives.** Since **macOS 12.0**
VideoToolbox has exposed long-term reference frames, which give exactly the property a WebRTC screen
sharer wants: recover from loss with a *P* frame instead of an IDR. `VTCompressionProperties.h`
documents the protocol verbatim:

> `kVTCompressionPropertyKey_EnableLTR` — "When an LTR frame is encoded, encoder will signal a
> unique token … through `kVTSampleAttachmentKey_RequireLTRAcknowledgementToken`. Clients are
> responsible for reporting acknowledged LTR frames to the encoder through
> `kVTEncodeFrameOptionKey_AcknowledgedLTRTokens`. Client can request a refresh frame at any time
> through `kVTEncodeFrameOptionKey_ForceLTRRefresh`. Encoder will encode a P frame by using one of
> acknowledged LTR frames as the reference … **or an IDR if no LTR frames have been acknowledged**."

The four symbols, all **macOS 12.0**:

| Symbol | Kind |
|---|---|
| `kVTCompressionPropertyKey_EnableLTR` | session property, CFBoolean |
| `kVTSampleAttachmentKey_RequireLTRAcknowledgementToken` | attachment on the emitted sample |
| `kVTEncodeFrameOptionKey_AcknowledgedLTRTokens` | per-frame option, array of tokens |
| `kVTEncodeFrameOptionKey_ForceLTRRefresh` | per-frame option, CFBoolean |

**Two thirds of the work is already done, and not in the shape the original design expected.**

- **The probe half landed.** `EnableLTR` membership is read out of
  `VTCopySupportedPropertyDictionaryForEncoder` per session and reported in the per-session
  `[vt] properties:` line as `available` or `absent`; `Encoder::supports_ltr()` exposes it. The
  property is deliberately **not set**, because `EnableLTR` without an acknowledgement loop is
  pointless: `ForceLTRRefresh` with nothing acknowledged emits an IDR, which is exactly the
  behaviour it would be replacing, at the cost of a reference-management mode nobody is driving.
- **`MaxKeyFrameIntervalDuration` is already 600 s** and `DataRateLimits` is already removed, so
  refresh no longer depends on a periodic IDR and there is no hard cap for one to fit inside.
- **The acknowledgement signal already exists and needed no client change.** The premise the
  original design was blocked on — *"RTCP does not acknowledge frames, so the acknowledgement signal
  has to be synthesised"* — does not hold. **Transport-wide feedback acknowledges frames.** It names
  every transport sequence number the client did and did not receive, and since the writer stamps
  those sequence numbers itself (it has to: the stock `twcc::sender` keeps its counter private), the
  host already knows which packet belonged to which access unit. In `streamer/congestion.rs`:
  `SendHistory::record` takes the frame's per-session ordinal and its host-clock capture instant and
  keeps a per-frame tally beside the per-packet ring; `interpret` votes every reported symbol —
  received or lost — against the frame its packet belonged to; and a frame **every** one of whose
  packets arrived is published on `SharedFeedback` as **`acked_frame_capture_ns()`**. A frame
  missing even one packet is never acknowledged: the point of the signal is that the encoder may
  keep referencing that picture, and a decoder missing a slice of it cannot. Three unit tests cover
  it, including that the published value never walks backwards on a reordered report.
  **The identity is the capture instant on purpose** — the encoder already stamps it, so no new
  field has to be carried through `EncodedFrame`, no control-channel opcode has to be invented, and
  the client page does not change at all.

**What actually remains is a shape mismatch, and it is the whole task.** `streamer/repair.rs`
carries the cross-platform seam — `RepairState`, `RepairAction { mark_ltr, use_ltr_bitmap }`,
`note_marked` / `note_encoded` / `take_lost` / `take_action` — with a working NVENC consumer in
`windows_utils/streamer/pipeline.rs`. **The macOS pipeline sets zero slots**, and the comment saying
why used to read "VideoToolbox exposes no long-term-reference control", which is false. What
VideoToolbox does not have is NVENC's *shape*:

| `RepairAction` (NVENC's model) | VideoToolbox |
|---|---|
| `mark_ltr()` returns the slot this picture took | no equivalent — `EnableLTR` makes the *encoder* choose which pictures are long-term |
| `use_ltr_bitmap` names the slots to predict from | no equivalent — no slot numbers exist to name |
| `invalidate_reference(index)` drops a lost picture | the inverse: the host **acknowledges** what arrived (`kVTEncodeFrameOptionKey_AcknowledgedLTRTokens`), and the encoder never predicts from a picture that was not acknowledged |
| — | `kVTSampleAttachmentKey_RequireLTRAcknowledgementToken`, an opaque token per emitted sample |
| `request_repair` → predict from an acked slot | `kVTEncodeFrameOptionKey_ForceLTRRefresh` |

**So the adapter is one direction, not four operations. Build it in exactly these four steps:**

1. **Set `kVTCompressionPropertyKey_EnableLTR`** when `Encoder::supports_ltr()` is true (already
   probed and reported per session in the `[vt] properties:` line). Use the existing `dlsym` helper
   and **check both halves** — that the symbol resolved *and* that `VTSessionSetProperty` returned
   0. Do not discard the result. Log the outcome as `set` / `rejected(status)` / `no-symbol` in the
   same property line as everything else.
2. **In `output_callback`, read the token** off the emitted sample's attachment dictionary and keep
   it beside the `FrameTag`'s capture instant, which `sourceFrameRefCon` already carries. The SCK
   backend already reads sample attachments this way — copy that pattern. Use a **bounded ring, not
   a map**: feedback older than a round trip is not worth acting on, and a map on this path is an
   unbounded allocation on the encode thread.
3. **Give the encode loop the acknowledged capture instant.** `SharedFeedback` is transport-side and
   the encode loop cannot see it, so the cheapest hook is an `Arc<AtomicU64>` on `Pipeline`, stored
   next to the existing `pipeline.set_target_bitrate(target)` call in `streamer/webrtc_session.rs`.
   On each submit, pass every token at or before that instant in `AcknowledgedLTRTokens`.
   **This one line in `webrtc_session.rs` is the single exception to §4.2's fence for this task** —
   it is the same shape as the `set_target_bitrate` call beside it, it adds no lock, and it is
   recorded as a Decision in the report. Nothing else in that file may change.
4. **`RepairState::set_ltr_slots(1)`** once LTR is on, so `request_repair` stops falling straight
   through to a keyframe, and make it mean `ForceLTRRefresh`. Keep a real `ForceKeyFrame` for a
   genuinely new client, which has no reference at all. The built-in degradation is what makes this
   safe: with nothing acknowledged, `ForceLTRRefresh` emits an IDR — exactly today's behaviour.

**Also correct the comment** in `macos_utils/streamer/pipeline.rs` so the next reader is not told
the capability is absent when it is merely differently shaped.

**Measure.** This is a p99 and recovery-time change, not a median one, so measure it as one:

- **Bytes per recovery event**, LTR refresh versus IDR, at 720p and at 1080p. The NVENC analogue
  measured a repair frame at **23,698 B against a 28,423 B IDR** on a fixture that redraws every
  pixel every frame — the pessimistic case, since real screen content is mostly static between
  frames, so expect a better ratio here.
- **Time from PLI to the first correctly decoded frame on the client.**
- **The client's `videoDelayEmaMs` tail during a loss burst** (`SEVideo.getStats()` on a real
  device).
- **IDRs per minute** with and without repair, under induced loss. Induce it with `dnctl` + `pfctl`
  on this Mac:
  ```sh
  sudo dnctl pipe 1 config bw 0 delay 0 plr 0.03
  echo 'dummynet out proto udp from any to <client-ip> pipe 1' | sudo pfctl -f - -e
  # undo
  sudo pfctl -d; sudo dnctl -q flush
  ```
  Run at 1 %, 3 % and 5 %. `idrs_sent`, `keyframe_requests`, `keyframe_requests_suppressed` and
  `repairs` / `keyframes_avoided` are all in `ScreenExtend status --json`.

**A caution from the research.** No shipping consumer of these keys was found — Chromium's
VideoToolbox encoder contains no LTR code at all — so this path is comparatively untested in the
wild, and step 1's "did the property actually take" read-back is **not optional**. If the property
reports set and the token attachment never appears on any sample, that is the finding: record it
with the encoder id and the OS build, revert, and leave the row `PROBED AND REPORTED, NOT ENABLED`.

**Done when:** row 283 and the new row 305 read `VERIFIED` with the four measurements above, or
reverted with the reason and the number; the "Long-term reference frames (macOS)" tier row is
flipped; and `docs/manual-tests/macos-capture-encode.md` §4 carries this machine's numbers.

#### M1-05 · Encoder back-pressure against a one-in/one-out encoder
*Rows R144, R147 · tier row "Encoder back-pressure (macOS)" · manual test §7*

The in-flight cap has two tiers: **1.** an external cap of 2, and **2.** the cap standing aside to
16 after 100 ms with no output, because a hard cap against a *frame-holding* encoder is a deadlock,
not back-pressure. The 10.15 box only has the frame-holding kind, so **only tier 2 was ever
exercised.** A low-latency `…rtvc` session should be one-in/one-out, which means **you are the
first machine that can test tier 1.**

**Verify** under load (drag a video around the extended display for a minute):

```
[vt] frames in flight reached 2 (VideoToolbox reports Some(2) pending)
[vt] no output for 100 ms with 2 frame(s) in flight; this encoder holds frames, so the in-flight cap stands aside up to 16
[vt] the drain thread fell behind; 1 encoded frame(s) discarded, forcing an IDR
[vt] encode loop finished: max 2 frame(s) in flight, 0 capture(s) skipped for back-pressure
```

The first line is the depth gauge; **our count and VideoToolbox's own `NumberOfPendingFrames`
should agree** — if they do not, the encoder is holding frames for a reason of its own and no
external cap will help, and that disagreement is the single most diagnostic result on this path.
The second line **should never appear on a low-latency session**; if it does, this encoder holds
frames too and that is a finding. The third should be rare.

**Measure and optimize.** Sweep the cap: 1, 2, 3, 4. Report capture→encoded p50/p90/p99/max and
delivered fps for each. **Delivered fps must not fall.** Rows 144 and 147 have a *before* number
(p50 13.70 / p99 15.60 ms on the 10.15.8 box) and no *after* — take the after on this machine, at
this machine's resolution, and state both.

Then check `MaxFrameDelayCount`: it is currently unset because Apple documents it as **blocking
the calling thread**, and that thread is the deadline-scheduled encode loop. Confirm the
documentation on this OS by setting it to 1 and observing whether the loop blocks; if it does not
block here, an internal cap may be cheaper than the external one. Measure, then decide.

#### M1-06 · Three consecutive submit failures rebuild the encoder
*Row R148 · manual test §2 "the scarce-resource case"*

Not reproducible on demand on the dev box. Reproduce it here by forcing contention:

1. Start a 4K screen recording in QuickTime (or a video call, or OBS with hardware encode).
2. Join a device in ScreenExtend.
3. Expect `[vt] HW encoder unavailable for …`, the ladder falling through, and then the **loud**
   software line. **Fail if the session silently lands on software with no line naming the
   reason** — that is the documented mechanism behind "it only works if I quit everything else".
4. Then force actual submit failures (exhaust the encoder while a session is live) and confirm the
   three-strikes rebuild fires, releases the old session first, and the stream recovers.

Record `InstanceLimit` for this machine's encoder and how many simultaneous hardware sessions you
can actually create (`hw_capability_probe` reports it; the 10.15 box managed 6 of 6, so the
scarce-resource path was unreachable there). On Apple Silicon the media engine's cap differs from
Intel Quick Sync's — that number is worth having on its own.

**You own row 148.** An Intel-Mac run on 12.x can also fire this and will record a second data point
in the row's Notes, but the status is yours to flip. If your machine cannot reproduce it at all, say
so with what you tried, and leave the row `COMPILE-CHECKED` with that evidence attached rather than
silently.

---

### Workstream C — audio

#### M1-07 · The Core Audio Process Tap tier
*Rows R112, R113 · tier row "System audio capture (macOS)" rung 1 · `docs/manual-tests/macos-audio.md` tier 1*

**This tier has never run.** It is preferred over ScreenCaptureKit audio because it is audio-only:
no dummy video stream, no Screen Recording permission, and **no screen-recording indicator** — the
UX regression that SCK audio forces on 13.0–14.1.

**Verify** the tier is selected and starts:

```
audio: capture backend = process_tap
audio: tap aggregate IO cycle: was <n> fr, range [<lo>,<hi>], asked 128, got <n> (<x.xx> ms @ 48 kHz, OSStatus 0)
audio: Process Tap started (source <r>Hz x<n>ch <kind>, downmixed to 48kHz stereo)
audio: output device <id> was <r> Hz; set to 48000 (OSStatus 0)      # only if it was not already
```

and on teardown:

```
audio: restored output device <id> to <r> Hz
```

**Check the restore actually happens.** Starting the tap sets the user's *default output device*
to 48 kHz because nothing downstream resamples. On a 96 kHz DAC, or a Mac whose owner chose
44.1 kHz for bit-perfect playback, that is an audible change for every running application. Note
the rate in Audio MIDI Setup, stream, stop, and confirm it is unchanged. Then kill the host
mid-stream (`kill -9`) and confirm the rate is still restored or, if not, that this is recorded as
a defect with a proposed fix.

**Record the `tap aggregate IO cycle:` line — it is itself a measurement.** Nobody, including the
AudioCap reference this aggregate is modelled on, has published what the HAL grants for a *tap*
aggregate: the line prints what it was, the range it reported, what `TARGET_BUFFER_FRAMES` asked
for after clamping to that range, and what it granted. Expect the IOProc interval to fall from
~10.7 ms (the 512-frame default) to ~2.7 ms at 128 frames. `dropped_backpressure` must stay 0 and
the silence watchdog must not fire.

**The documented failure mode, and why the check is not the obvious one.** If an aggregate device's
tap-list UID does not match the `CATapDescription` UUID, the tap returns `noErr` and **pure
silence** — the documented failure. **This tree cannot reach it**: `process_tap.rs` builds the
sub-tap dictionary from the description's own `UUIDString`, so the two cannot disagree by
construction. What exists instead is a **watchdog on the outcome**, in `audio/mod.rs`'s worker: if
`backend.nonsilent_samples()` is still zero five seconds in, it warns once —

```
audio: process_tap has produced only silence for 5s — if the host IS playing audio, check Screen
Recording / audio permissions and whether another process holds an exclusive tap on the output device
```

Verify the watchdog by making it fire honestly (mute the host, or start the tap with nothing
playing) and confirm it says exactly that, once, and never again in the session. Then — since you
are the first machine that can — **prove the structural claim** by temporarily giving the sub-tap
dictionary a UUID that is not the description's, confirming the tap returns `noErr` and silence and
that the watchdog catches it, and reverting. That converts "cannot happen by construction" from an
argument into an observation, and it is the only way anyone will ever see the documented failure
mode in this codebase.

**Verify the teardown order** is stop → `destroyIOProc` → `destroyAggregate` → `destroyTap`, by
RAII, so a leaked tap cannot persist a hidden capture. Kill the process at each stage and check
with `system_profiler SPAudioDataType` that no orphan aggregate device survives.

**Measure.** `probe --audio --duration 15` with real audio playing, three runs. Expected:
`capture → packet` p50 under 5 ms, `period_frames` 128–256, `encode` p99 well under 1 ms. Compare
against the legacy tier's p50 −0.57 / p90 3.59 / p99 4.54 ms and Windows' p50 9.44 ms.

##### M1-07b · Optimize the tap
Every knob, measured:

- `kAudioDevicePropertyBufferFrameSize` swept over the granted range: 64 (if allowed), 128, 192,
  256, 512. Report `capture → packet` percentiles and `dropped_backpressure` for each. 128 is the
  current request because it is a quarter of Opus's 5 ms frame and Chromium's floor; confirm or
  move it with a number.
- The `CATapDescription` variants: global tap versus a process list; mono-mix versus stereo-mix
  versus per-process streams; `setMuteBehavior:` / `setExclusive:` / `setPrivate:` — probe each
  with `respondsToSelector:`, and record which exist on this OS.
- Whether the tap can avoid setting the default output device's sample rate at all (e.g. by
  accepting the device's rate and rejecting only what Opus cannot take). Rejecting a resampler was
  the right call at 48 kHz-everywhere; a tap that forces a 96 kHz DAC down to 48 kHz for every
  other app on the machine is a different question and it deserves a measured answer.

##### M1-07c · The real-time callback violation
`Producer::push` in `audio/ring.rs` calls `Thread::unpark()` **inside the `AudioDeviceIOProc`**.
Rust's macOS parker takes a `pthread_mutex` in `unpark` whenever the target is parked, and the
consumer parks whenever the ring is short of a frame — i.e. on every idle gap. That is a blocking,
**non-priority-inheriting** lock acquisition on a real-time thread, and it violates invariant §2.6.
Risk-register **R-249 / R-426**.

Fix it: a lock-free wake (a POSIX semaphore, a `dispatch_semaphore_t`, or simply letting the
consumer spin at its existing 250 µs poll — the legacy tier already does exactly that and measures
p99 4.54 ms with it). **Measure both**, on this machine, on the tap tier: `capture → packet`
percentiles and the worst-case callback duration under load. Then delete the finding.

#### M1-08 · The ScreenCaptureKit audio tier
*Rows R117, R169, R170 · tier row rung 2 · manual test tier 2*

Also never run. SCK has no audio-only mode, so it rides a **2×2 pixel, 1 fps dummy video stream**
with `showsCursor: false`, `queueDepth: 3`, `capturesAudio: true` and
`excludesCurrentProcessAudio: true`, and it needs the Screen Recording permission.

**Verify** by denying the Process Tap (there is no supported way to deny it directly — instead
force the tier, adding `SCREENEXTEND_AUDIO_BACKEND=screencapturekit` if no override exists, and
document it):

```
audio: capture backend = screencapturekit
audio: ScreenCaptureKit audio started (48kHz stereo + 2x2 dummy video)
```

and the failure paths, each bounded at **1.5 s** (they are taken under the `AudioHub` mutex that
holds every other session's audio subscribe, which is why the bound matters):

```
audio: screencapturekit backend failed to start (SCShareableContent had no displays (Screen Recording permission?)); trying next
audio: screencapturekit backend failed to start (SCK startCapture timed out); trying next
```

**Watch for this line, and report it loudly if it appears:**

```
audio: ScreenCaptureKit delivered <n> Hz, not the 48000 it was asked for; dropping these blocks
```

`setSampleRate: 48000` is a *request*. SCK derives audio from the system mix and nothing in the
macOS path resamples, so a stream honoured only approximately would encode about 9 % fast with A/V
drift accumulating at roughly 80 ms per second. Blocks at the wrong rate are dropped rather than
emitted. **You are the first machine that can see this. If that line ever appears, it upgrades a
speculative finding to a real one and the sample-rate-converter rejection in §5.3 must be
re-opened.**

Verify the teardown barrier: blank the delegate ivar, stop the stream, post a **synchronous**
barrier onto the serial delivery queue, then free the context box. Kill the process mid-delivery
and confirm no use-after-free (run under `MallocScribble=1 MallocPreScribble=1` and, if you can,
`AddressSanitizer`).

**Measure.** `capture → packet` p50 expected under 10 ms (SCK chunks are larger than a tap block).
Also measure the **cost of the dummy video stream**: package power and CPU with SCK audio running
versus the tap running versus nothing, over 60 s. That number is the argument for the tap tier and
nobody has it.

**Also test the planar path.** `format.rs` parses the delivered `AudioStreamBasicDescription` and
handles **planar** float, which Windows never produces and which only SCK on macOS delivers. Assert
which layout you actually receive and that `parses_sck_planar_float_stereo` describes reality.

#### M1-09 · The tap's own `capture_ns` skew
*The W7 rejection note, "macOS audio: what was rejected, and why"*

This is a decision, not just a measurement. Windows clamps the audio capture timestamp to
`min(qpc_host_ns, now − read_duration)` because a loopback endpoint stamp is a play cursor ~15 ms
in the future. macOS's legacy driver tier measured a signed skew of p50 **−0.57 ms** (min −6.23)
over 3277 packets — under one 2.67 ms IO cycle and in the *right* direction — so the clamp was
**rejected there** and `ts_clamped` reads 0.

**The Process Tap is a different question.** Its aggregate carries its own latency and safety
offset. Measure the signed skew and the packet-spacing histogram (`probe --audio` prints both),
over at least 3000 packets with real audio playing and again across a 3-second silent gap. Then
decide, and record the decision with the number:

- If the skew is small and negative (a past instant), **do not clamp** — the same reasoning as the
  legacy tier, and clamping would flatten the spacing ladder.
- If the skew is positive (a future instant, like the Windows play cursor), **implement the clamp
  for this tier only**, with `capture_ts_clamped` counting how often it bites, and measure the
  effect on the client's A/V offset (`SEAudio.getSyncInfo()` on a real device).

**Done when:** the note in `docs/OPTIMIZATIONS.md` gains a paragraph headed with the tap's measured
skew, and `docs/manual-tests/macos-audio.md` tier 1 carries the number.

#### M1-10 · Audio tier ordering
*Tier row "macOS audio tier ordering"*

The driver tier is a **general fallback** appended after whichever native tiers probe present —
not a below-13.0-only tier. The case this ordering exists for has never been exercised: **a 13.x+
Mac with Screen Recording denied must fall through SCK to an installed driver rather than having no
candidate at all.**

Construct it: deny Screen Recording, install the ScreenExtend Audio driver, and confirm

```
audio: screencapturekit backend failed to start (…); trying next
audio: capture backend = virtual_device
```

Then the harder ordering case on **your** machine: with the tap available *and* Screen Recording
denied, confirm the tap is still selected first and unaffected (it needs no screen recording at
all — that is its whole advantage). And with every candidate failing, confirm
`all capable audio backends failed to start` rather than a silent no-audio session.

Also fix the tier tests: `macos_utils/audio/test/mod.rs` opens two of them with
`if macos_at_least(13, 0) { return; }`, so **the tier tests are themselves version-gated** and
return before asserting anything on any Mac from 2022 on (R-225). Separate ranking from probing so
the ranking is testable on every OS. (That is R-225; two sibling tests in the same file already
assert that the tiering source contains no `macos_at_least(` call, so the mechanism to hold the
property exists — extend it to the ranking.)

---

### Workstream D — permissions, identity and input

#### M1-11 · Screen Recording authorization, including the SCK probe tier
*Row R2 · tier row "Screen Recording authorization (macOS)" · `docs/manual-tests/macos-permissions.md` §1–§4, §9*

The ladder is: **1.** the `CONFIRMED` latch · **2.** `CGPreflightScreenCaptureAccess`
**positive-only** · **3.** `CGWindowListCopyWindowInfo` + `kCGWindowName` · **4.**
`SCShareableContent` (background) · **5.** a cached preflight negative. Tiers 1–3 and 5 are
verified on 10.15.8; **tier 4 has never run**, and none of it has been seen on 15 or 26.

Run `docs/manual-tests/macos-permissions.md` §1 through §4 verbatim and record, for every step: the
macOS build, what the UI showed, what `doctor --json` reported, and **the wall time from the user
action to the UI reacting**. The acceptance criteria:

| Situation | Required |
|---|---|
| Screen Recording denied | the gate opens, the row shows a red X |
| granted while running | the gate closes within one 200 ms poll **or** the modal switches to the restart wording |
| unreadable | the app starts anyway; the failure appears when a device connects |
| Accessibility denied | the app starts and streams; the row stays visible, marked optional |
| any state | `check_permissions` returns in well under 100 ms (target < 5 ms) |

The gate must **never** sit on "Checking for changes…" forever. That was the original showstopper.

Specifically for this machine: `doctor --json | jq '.macos.probes'` and record
`cgPreflightScreenCaptureAccess` versus `windowListNameProbe`. **They are allowed to disagree, and
which way they disagree is the diagnosis.** On 10.15.8 the preflight symbol does not even exist
(the `dlsym` returns null) — on your Mac it will resolve, and the question is whether it ever
reflects a mid-process grant (every reproduction found says no; confirm on 14/15/26).

Then the tier-4 question nobody has answered: does `SCShareableContent`'s background probe give a
*live* answer where the preflight does not, and what does it cost? Time it. If it is a reliable
live signal on 14+, it should be promoted above the window-list read on those releases — but only
if it is reliable *and* cheap, because the window-list probe measured 37 ms cold and **0.6 ms warm**
on 10.15.8 and that is a hard number to beat.

Finally, the Tahoe risk: trycua/cua#870 shows `kCGWindowName` present while capture returned
desktop-only on 26.1. **If window names are visible without the grant on 26, the window-list probe
degrades to a positive-only signal there and the SCK path must carry the read.** Test it directly:
revoke Screen Recording on 26, and check whether `window_name_probe()` still returns `Some(true)`.

#### M1-12 · The System Settings deep link
*Tier row "System Settings deep link (macOS)"*

Tier 1 is `x-apple.systempreferences:com.apple.settings.PrivacySecurity.extension?Privacy_*`
(Ventura+); tier 2 is the pre-13 `com.apple.preference.security?Privacy_*`. The modern id is tried
first and the legacy one runs only if `open` fails.

Click **Open Settings** on each permission row on every macOS you have. System Settings must open
**scrolled to and highlighting** the right pane, not the top of Privacy & Security. Record which
macOS builds land correctly and which do not; a build that lands at the top of the pane is a real
result and the anchor string for it needs finding.

#### M1-13 · App relaunch for a new grant
*Tier row "App relaunch for a new grant (macOS)"*

Tier 1 is `open -n -a <bundle>` through LaunchServices; tier 2 is `AppHandle::restart()`. Tier 1
needs a real `.app` bundle, which the dev box never had — it ran the bare binary.

Build the app (`pnpm tauri build --config src-tauri/tauri.macos.conf.json`), move it to
`/Applications` **in Finder**, and run `docs/manual-tests/macos-permissions.md` §4:

```sh
ps -o pid,ppid,command -p $(pgrep -x ScreenExtend)
```

**The parent must be `launchd` (pid 1)**, not a dead pid and not another ScreenExtend. That is the
entire reason the relaunch goes through `open -n -a`: TCC attributes a request to the *responsible
process*, and an heir of a dying parent inherits the parent's attribution. Then confirm in
Console.app that no `tccd` line reads `Failed to match existing code requirement for subject`.

#### M1-14 · macOS 15+ mid-session re-consent and the session watchdog
*Row R260 · manual test §7 and §8 · **needs macOS 15 or later***

macOS 15 re-asks for screen-recording consent roughly every 30 days. If the user dismisses the
dialog, **the capture APIs keep delivering frames — they just contain wallpaper, the menu bar and
our own windows.** Before the watchdog, the host reported a healthy stream while the client showed
a blank desktop, with nothing in the log until something restarted the pipeline.

`macos_utils::start_session_watchdog` re-reads `permissions::screen_recording_live()` every 30 s
**while at least one session holds a live display**, and acts only on a definite
`Some(true) → Some(false)` transition.

1. Start the host, join a device, confirm the stream is live.
2. Revoke Screen Recording (or move the approval date backwards in
   `~/Library/Group Containers/group.com.apple.replayd/ScreenCaptureApprovals.plist`, which needs
   Full Disk Access, and dismiss the dialog without re-approving).
3. Within ~30 s the log must show `[tcc] Screen Recording authorization was revoked while
   streaming.` and **the connected device must be dropped, not left watching wallpaper.**
4. Re-grant, relaunch, reconnect: the stream comes back.

Two things it deliberately does not do, and you must confirm it still does not: it never treats a
`None` read as a refusal (a `None` means "could not learn the answer" — no other titled window on
screen — and acting on it would reproduce the false-positive defect the window-list probe exists to
fix), and it does not raise the permission modal on the host (that needs a new event and a fourth
button, which is a UI change and is out of scope).

**Also measure the watchdog's own cost**: 30 s is a long time to notice a black stream. Sweep it —
30 s, 10 s, 5 s — and report the probe cost per poll against how long a user watches wallpaper.
If 5 s is free, use 5 s and say so with the number.

#### M1-15 · Permission probe cost
*Rows R258, R259*

- `accessibility_api_probe` must restore the AX messaging timeout to the system default
  (`AXUIElementSetMessagingTimeout(el, 0.0)`) before returning. Verify by measuring a *subsequent*
  unrelated AX call's latency, before and after the fix.
- `check_permissions` and `request_permission` are `async` and run their FFI on `spawn_blocking`,
  and the window-list probe is warmed at startup beside `input::prime()`. Verify with
  `doctor --json | jq '.macos.probes'`: `windowListNameProbeUs` should be roughly 500–1500 µs warm
  (37 ms cold / 0.6 ms warm on 10.15.8), and `axSystemWideElementUs` is the expensive one
  (20–23 ms there), which is why it is a background secondary probe.
- **`check_permissions` wall time from the renderer must be under 5 ms**, always. Measure it from
  the UI with the gate open and polling at 200 ms for 60 s, and confirm the modal does not stutter.

#### M1-16 · Input backend tiering, with Accessibility revoked mid-session
*Row R203 · tier row "Remote input injection" · `docs/manual-tests/core-closeout.md` §2, permissions §8*

`CompatibilityReport.input_backend` is filled from `streamer::input::probe_backend()`; on macOS the
values are `CGEvent` (Accessibility granted), `NeedsAccessibility` (the API is there and the OS
silently discards every event), or `Unsupported`. The tier is re-probed on the injector's existing
**500 ms geometry cadence**, never once at construction, because TCC can be granted or revoked
while the process runs.

Verify with a client attached and remote control on:

1. Accessibility granted: the cursor moves. `doctor --json | jq .inputBackend` reads `CGEvent`.
2. Revoke Accessibility **mid-session**. Within 500 ms the tier must read `NeedsAccessibility` and
   the report must say so — the client's remote control is enabled, its events are acknowledged,
   and the host does not move, which is exactly the state that used to be undiagnosable.
3. Re-grant **mid-session**. The grant must take effect **within 500 ms with no relaunch.**

#### M1-17 · The arm64 input ABI · **Apple Silicon only**
*Row R43 · risk-register R-233*

`CGEventCreateScrollWheelEvent` is **variadic**, and a variadic call across the arm64 ABI is not
the same as a fixed-argument one — passing scroll deltas through it on Apple Silicon is an ABI
blocker, not a style problem. This tree replaced it with the non-variadic
`CGEventCreateScrollWheelEvent2` (10.13+, below the floor), which is VERIFIED on Intel and has
**never executed on arm64.**

Verify on Apple Silicon: scroll from a real client, both line units and pixel units, both axes,
and run the two `#[ignore]`d input tests in `streamer/input/macos.rs` (they assert the function
pointer identity and that the constructed event is non-null). Then check the *values* arrive
correctly — a wrong ABI can produce an event that is non-null and carries garbage deltas.

While you are on arm64, also confirm the second arm64-only blocker: the CoreWLAN HostAP private
SPI is now guarded by `respondsToSelector:`, where an unguarded `msg_send!` raised an Objective-C
exception through a non-unwinding Rust frame — **a process abort, not a `false`**. `report_hostap_surface`
(M1-24) is the test; on Apple Silicon the expected answer is that the selector is absent, and the
app must report `Unsupported` and keep running.

#### M1-18 · The Ctrl↔Command remap, typed from a real Apple client
*Row R66*

`ClientPlatform::from_reported` parses the `os` string the join request already carries, and
`modifier_keycode` swaps Ctrl and Command for Apple clients. Unit-tested key by key in both
directions; never typed on a real Mac or iPad client because none was available.

Join from a Mac or iPad *and* from a non-Apple client (Android or Windows), and type, on each:
Cmd/Ctrl+C, +V, +X, +A, +Z, +Shift+Z, +Tab, +Space, arrow combinations with Shift and Option/Alt,
and a few non-modifier keys to confirm they are identical in both tables. Record what actually
reached the host. Then verify the host's own modifier state does not leak across a client swap:
join from an Apple client, hold a modifier, disconnect, join from a non-Apple client, and confirm
the modifier is not stuck.

---

### Workstream E — display and session

#### M1-19 · Virtual display creation, the mode ladder and the settle
*Rows R39, R190/R221, R191/R222 · tier rows "Virtual-display DPR / mode list (macOS)" · **needs a console session***

Three things land together here and none of their savings has ever been measured, because
`initWithDescriptor:` / `applySettings:` block indefinitely over SSH:

- `create_display_with_modes` (the trait default discarded the ladder entirely), with HiDPI derived
  from the ladder so **the display is born at the client's DPR** — removing one `applySettings:`
  and one up-to-3-second `activate_mode` per Retina join, inside `DISPLAY_CORRELATION_LOCK`.
- Multi-mode registration in one `applySettings:`: the primary mode, its transpose, and every
  ladder resolution with its transpose, so an orientation flip or an in-ladder geometry change is a
  plain `CGDisplaySetDisplayMode` instead of a mode-list rewrite plus a settle poll.
- `activate_mode`'s settle wait as a **condvar fed by `CGDisplayRegisterReconfigurationCallback`**
  (registered from a thread running a CFRunLoop) instead of a 5 ms/25 ms sleep-poll, with a
  generation counter read before the geometry read so an event landing between the two is not lost.

**Measure**, from a console session:

```sh
cargo test --release --lib macos_utils::virtual_display::tests::time_a_retina_join -- --ignored --nocapture
```

and by hand: total join wall time for a 1× client and a 2× (Retina) client; the settle duration the
log reports; an in-ladder geometry change; an orientation flip; and an out-of-ladder geometry
change for contrast. Report all of them p50/p90/p99 over ≥ 5 joins each.

Also verify `setSerialNum:` is FNV-1a over the display's name (`"ScreenExtend - <device>"`) rather
than a process-local counter starting at 1 — join two devices simultaneously and confirm the two
displays are distinguishable and stable across restarts.

**Done when:** row 39 reads `VERIFIED` with the join-time numbers, rows 190/221 and 191/222 have
their savings measured, and the "Virtual-display DPR (macOS)" and "mode list" tier rows are flipped.

#### M1-20 · `CGVirtualDisplaySettings.refreshDeadline` — the one latency knob on WindowServer's side
*Tier row "`CGVirtualDisplaySettings` extras (macOS)" · manual test §10*

`refreshDeadline`, `rotation` and `isReference` were added to `CGVirtualDisplaySettings` at some
version after 10.14.6 — absent on the 10.15 floor, present at 26.4. They are probed with
`respondsToSelector:` and logged once, and **deliberately not set**:

```
[display] CGVirtualDisplaySettings: refreshDeadline <present|absent>, rotation <…>, isReference <…>
```

**This is one of the highest expected-value experiments in the guide.** The largest single
host-side latency term on both platforms is capture delivery — the compositor-to-callback hop —
and `refreshDeadline` is the only latency-named knob on the WindowServer *side* of that boundary.

Record whether the probe reports it present. If it does:

1. Take the control: `capture_rate` (`displayTime → callback` and delivered fps) with the property
   unset, three trials.
2. Set it to one frame period. Re-measure.
3. Set it to half a frame period. Re-measure.
4. Try the extremes the API accepts (0, and something absurdly large) to learn its semantics —
   it is undocumented, so its *shape* is a finding in itself.
5. Also measure capture→encoded end to end, not just the delivery histogram, because a deadline
   that improves delivery jitter while dropping frames is not a win.

**Keep it only if the numbers move**, and record the rejection with the numbers if they do not.

While you are there, evaluate `rotation`: it would replace the transposed modes registered in
M1-19 with a real rotation, which would halve the mode list. And `isReference`, which may change
the colour pipeline WindowServer applies — measure whether it changes anything about the delivered
pixel format or the callback cost.

#### M1-21 (display half) · Does `CGVirtualDisplay` still work on 15/26?
*Risk-register R-232 · research `macos-capture-encode.md` §2.6*

BetterDisplay reported WindowServer crashes on Sequoia betas using the same private API. This app
depends on it completely: `probe_display_backend()` returns `CgVirtualDisplay` when all four of
`CGVirtualDisplay`, `CGVirtualDisplayDescriptor`, `CGVirtualDisplaySettings` and
`CGVirtualDisplayMode` resolve, and falls back to the AirPlay tier otherwise.

On the newest macOS you have:

1. Confirm all four classes resolve (the preflight already told you).
2. Create a display and join a real client. Then create **four** displays and join four clients.
   Simultaneous displays are uncapped on this backend by design (`max_concurrent_displays()`
   returns `None`) and nothing must evict a connected device to make room.
3. Leave them up for an hour under load. Watch for WindowServer instability
   (`log stream --predicate 'process == "WindowServer"'`), and for the display surviving a display
   sleep/wake, a lock/unlock, a Space switch and an external-monitor hotplug.
4. Note `probe_display_backend()` is cached in a `OnceLock`, so if the classes vanish at runtime
   (they will not) the tier does not re-probe. That is correct; record it.

There is also a dormant hint in the code worth chasing: if `CGVirtualDisplay` is absent but
CoreDisplay exports `CDVirtualDisplayCreate`, the code logs "a direct C path may be possible on
this OS; please report this". If you ever see that line, **research the C path** — it would be a
new display tier above the AirPlay fallback.

#### M1-22 · Graceful port change with a client attached
*Rows R42, R262*

`set_server_ports` sends each client a `Bye` and shuts down gracefully instead of dropping
connections, finishing the restart on its own thread; `remove_all_displays` no longer spawns a
thread it immediately joins; the adapter thread is named `se-adapter-<ip>` and its spawn failure is
reported.

Verify: join a real device, change the HTTP and HTTPS ports from the UI (and again from
`ScreenExtend config set serverPorts.http …`), and confirm the client sees a clean close and
rejoins on the new port without a dropped connection or an orphaned display. Check the thread name
appears (`sample ScreenExtend 5` or Instruments) and that a forced spawn failure is reported rather
than silently leaving an adapter unserved.

#### M1-25 · `DISPLAY_CORRELATION_LOCK` hold time
*Row R206*

The lock serializes virtual-display creation across its `spawn_blocking`/settle awaits — it is one
of only two intentional `tokio::sync` locks in the tree (§2.7). It is now released **as soon as the
backend names its own display**, rather than being held through the settle, and a wait over 1 ms is
logged:

```
display correlation lock: waited <N>ms behind another join
```

Verify with **two clients joining simultaneously** (script both joins to fire within ~50 ms of each
other; do it ten times). Record the second joiner's total wait, the logged lock wait, and the total
join time for each, and compare against a single join. Then verify the correctness half: two
displays created concurrently must be correlated to the right sessions, with the right names and
the right geometries, every time.

#### M1-26 · AirPlay — what a modern sender can settle
*Rows R284, R261, R295 · risk-register R-242, R-243*

**Scope note, decided and recorded:** the AirPlay *display tier* is only ever **selected** below
macOS 10.14.4, which is below this app's own `minimumSystemVersion` of 10.15 — so the shipped
binary refuses to launch on every OS where that backend would be chosen. It stays in the tree as an
unexercised safety net, and this guide does **not** ask you to make it selectable. What it asks is
the three things a *modern AirPlay sender* can settle, which have never been attempted:

1. **Does a modern macOS or iOS sender complete the fake-receiver handshake at all?**
   `/pair-verify` is a hard 403 and `LEGACY_PAIRING` is a compile-time `const false`. Run
   `cargo test --release --lib macos_utils::airplay::tests::live_end_to_end -- --ignored --nocapture`
   with an iPhone, iPad or second Mac as the sender, and record exactly where the handshake stops.
   The original research called this "the single cheapest open question in this territory — ten
   minutes on any Mac, never run." Do it first.
2. **Does macOS honour the receiver's advertised `maxFPS = 1`?** If it does not, a live session
   means the host is encoding its whole desktop at full rate into a socket we discard, competing
   for the GPU with our own VideoToolbox encode — the difference between "slow to join" and
   "halves your framerate". One `tprintln!` of `stats.packets` per second answers it.
3. **`created_geometry`.** The tier now stores the geometry it negotiated instead of discarding it,
   and `Receiver::take_geometry_changed()` is wired into the reconfig epoch through the session
   watchdog. Confirm it reports something real, and delete the finding.

Also confirm `airplay::probe()` appears in `doctor --json` and reports something sensible on a
modern Mac (it should say the tier exists and is not selected).

Do **not** invest in decoding or relaying the mirror stream. That was tried and rejected on
evidence: the type-0 payloads are ciphertext and decoding needs real FairPlay.

---

### Workstream F — network

#### M1-23 · The `PF_ROUTE` adapter watcher on a modern macOS
*Row R41 (and the duplicate R189/R220) · tier row "Adapter-change source (macOS)"*

Tier 1 is the kernel routing socket (`PF_ROUTE`: `RTM_NEWADDR`, `RTM_DELADDR`, `RTM_IFINFO`) with a
250 ms debounce and a 30 s backstop; tier 2 is a 1 s poll. It was preferred to
`SCDynamicStoreSetNotificationKeys` because it needs no framework, no run loop and no entitlement.

```sh
cargo test --release --lib macos_utils::networking::tests::measure_adapter_watch_cost -- --ignored --nocapture
```

Expect **30 enumerations per 30 s at idle to become 0**. Then verify the event half: switch Wi-Fi
networks, plug and unplug a USB-Ethernet or Thunderbolt adapter, toggle a VPN, and confirm
`sync_streamers` is reached immediately rather than up to a second later, and that the QR values and
join URLs in the UI update. On Apple Silicon Macs with no Ethernet, use a USB-C dock or a
Thunderbolt bridge.

#### M1-24 · The macOS hosted Wi-Fi network, end to end
*Row R38, R193/R224 · tier rows "Hosted Wi-Fi network (macOS)", "security type", "liveness", "DHCP" · `docs/manual-tests/macos-hosted-network.md` §1–§9*

This is the largest single open question on macOS and it is worth a dedicated agent. The host AP is
the reachable analogue of AWDL — **one 802.11 hop between the Mac and the client instead of two
through a router** — and the AirPlay research argues that is where the p99 latency win lives.

**What is already known, from macOS 10.15.8 Intel:**

```
responds : startHostAPModeWithSSID:securityType:channel:password:error:   true
responds : stopHostAPMode / startHostAPMode: / enableHostAPMode / disableHostAPMode   true
responds : startIBSSModeWithSSID:security:channel:password:error:         true
backend  : HostAP ; 5 GHz channels 36 40 44 48 52 … 165 ; would pick 36

securityType 1024 : accepted=false in 2.13s
securityType  512 : accepted=false in 2.08s
securityType  256 : accepted=false in 2.07s
securityType  128 : accepted=true  in 2.21s, ap_mode=true at 2.21s
t+ 5s … t+45s     : mode_is_ap=true  ipv4=None          ← nothing serves DHCP
```

Three facts to carry forward: **securityType 128 is accepted** and is the first new HostAP security
constant this project has found (outside the `[-127, 127]` window a 2012 gist swept, which is why
the shipped ladder starts at 1024 and works down); **nothing serves DHCP**, so on that floor the SPI
brings up a *radio* and not a *network* — a client sees the SSID, associates, gets no lease and
cannot reach the host, which is the concrete shape of the user's report that the hosted network is
"slow and randomly fails"; and **the station address survives the switch**, so a verification that
accepts it reports the AP as working on the address of the network the Mac just left (hence
`wait_for_ipv4` takes the pre-start address and refuses it).

**Your tasks, in order:**

1. **`report_hostap_surface` on this machine.** If `startHostAPModeWithSSID:…` reports **`false`**
   — the outcome the Apple Silicon question hangs on — that is the single most valuable result in
   this document. Record the seven booleans verbatim, and confirm the app does not crash, reports
   `IBSS` or `Unsupported`, and names the tier.
2. **`measure_hostap_start`** — the shipped ladder, stopping at the first success. Record which
   security value is accepted on this macOS and how long each refusal takes.
3. **`sweep_security_types`** if you can reach the machine physically (it takes the Wi-Fi interface
   down repeatedly, up to ten minutes off the network). For every accepted value, **scan from a
   second device** and record what the network actually advertises — open, WEP, WPA2-PSK or
   WPA3-SAE — then try to associate an iPhone and an Android phone to each. If a WPA2 value exists,
   replace `SECURED_CANDIDATES` in `hosted_network.rs` with it and delete the guesses.
4. **The two surfaces nothing in this tree has ever called** — §9 of the manual test, and the most
   valuable remaining experiment:
   - `startHostAPMode:` (no SSID, no security type). It plausibly reads both from the Internet
     Sharing preferences, and macOS's own Internet Sharing produces a WPA2 AP through the same
     CoreWLAN machinery — which would bring `bootpd` and NAT with it, i.e. **the DHCP that the
     SSID-taking variant does not provide.** Configure Internet Sharing (Ethernet → Wi-Fi), call
     it, and record whether the interface gets an address, whether `bootpd` starts, and what the
     network advertises.
   - `enableHostAPMode` / `disableHostAPMode`, in case the pair is a prepare/commit sequence and
     the SSID variant alone is only half of it.

   **If `startHostAPMode:` works, the shipping path should become "configure sharing, then
   `startHostAPMode:`", with the SSID variant kept only where that fails.** Implement it, do not
   merely recommend it.
5. **Association, DHCP and a real session.** With the AP up, confirm the log says

   ```
   [hosted-network] '<ssid>' verified — channel 36 (5 GHz), <security>, host address 192.168.x.x
   ```

   rather than

   ```
   [hosted-network] '<ssid>' is advertised but the interface never got an IPv4 address; clients
   will see the network and fail to join because nothing is handing out DHCP leases
   ```

   then join from the phone and complete a session. Deliberately break it (stop `bootpd`) and
   confirm the app reports **advertised**, never **verified** — that tri-state is the whole point of
   the liveness tier, and on the 10.15.8 floor `Advertised` is the *only* answer the SPI produces.
6. **Channel choice and AWDL.** `PREFERRED_5GHZ_CHANNELS` is `[36, 40, 48, 153, 157, 161]` and
   2.4 GHz falls back to 11 rather than 6, because 6, 44 and 149 are AWDL's social channels. **That
   change is not settled** and the two effects point in opposite directions: sharing a social
   channel means contending with every AWDL peer in the room, but the *stutter* people measure comes
   from the radio leaving the link channel — so sitting on a social channel might make the hop free.
   `SCREENEXTEND_HOSTAP_CHANNEL=<n>` forces a channel without a rebuild. A/B channels 36, 44 and 157
   with a 60 s 10 ms-interval ping from the client, forcing AWDL activity by opening and dismissing
   the AirDrop sheet on the host and on a second Apple device at t=20 s and t=40 s. Metric: p99 RTT
   and the count of samples above 50 ms; community reports describe a 1–2 s hop every 10–12 s
   costing 50–200 ms stalls, so look for that shape. **Then set the constant from the result and
   write the reasoning into the comment above it.**
7. **Throughput and latency against a router.** `iperf3` host↔client (a) both on a normal router,
   (b) client on the ScreenExtend host AP, (c) as (a) with the router loaded by a large transfer
   from a third device. Then run a 5-minute streaming session in each configuration and compare
   p50/p95/p99 plus `getStats` jitter and `framesDropped`. **If p99 does not improve, the
   one-hop-link hypothesis is wrong and must be recorded as such.**
8. **Liveness and teardown.** Confirm the liveness watcher promotes the status when a session
   appears from the AP's own /24, clears the state within 5 s when the OS takes the AP away, and
   after 60 s with nothing associated logs the three usual causes in order. Then stop the network
   and confirm `interfaceMode()` returns to Station and the Mac rejoins its normal network without
   the user touching the menu bar.
9. **The IBSS tier**, if the HostAP SPI is absent here: confirm the fallback starts, that the log
   states the ~20 Mbit/s ceiling, and how a modern phone reacts to a WEP-104 ad-hoc network. Apple
   removed "Create Network…" from the Wi-Fi menu on Apple Silicon and in the redesigned Ventura
   menu, so this tier may already be dead in practice — **which is itself the result worth
   recording.**

**A caution taken from the dev box:** if this Mac's only network is the Wi-Fi interface, starting
the AP drops every remote session. §8 of the manual test has the detached-run script with a
watchdog that restores the radio (`networksetup -setairportpower` needs no admin rights). Prefer a
Mac with Ethernet, or run from the console.

---

### Part B — release artifact, identity and CI

#### M1-27 · The release artifact, the universal binary, and the signed audio driver
*Rows R79, R264, R265 · risk-register R-234, R-235, R-236 · tier rows "macOS 10.15 floor enforcement (build-time)", "macOS audio driver load" · manual tests `macos-build.md`, `macos-app-signing.md`, `macos-audio-driver-signing.md`*

**Needs a Developer ID Application certificate, a Developer ID Installer certificate, a
`notarytool` credential, and a clean VM.**

Build the real thing:

```sh
pnpm install
pnpm tauri build --config src-tauri/tauri.macos.conf.json
# and the universal artifact:
rustup target add x86_64-apple-darwin aarch64-apple-darwin
pnpm tauri build --target universal-apple-darwin --config src-tauri/tauri.macos.conf.json
```

Then, in order:

**a. Deployment target and architecture** (`macos-build.md` §1). Against
`src-tauri/target/universal-apple-darwin/release/ScreenExtend`:
```sh
lipo -info "$BIN"
otool -l "$BIN" | grep -A3 -E 'LC_BUILD_VERSION|LC_VERSION_MIN_MACOSX'
```
The x86_64 slice must report **`minos 10.15`**; the arm64 slice legitimately reports **11.0**
(Apple Silicon postdates Catalina and rustc clamps up rather than emitting an impossible target).
Anything else — **`10.12` especially** — means `src-tauri/.cargo/config.toml` did not reach the
linker; check cargo was invoked with `src-tauri` as its working directory. The recorded *before*
value on a pre-pin debug build was `LC_VERSION_MIN_MACOSX version 10.12, sdk 11.1`. This closes
row 79, which is the last thing standing between the pin and a verified artifact.

**b. Nothing newer than the floor is linked** (§2): `otool -L "$BIN"` must show no
`ScreenCaptureKit.framework`, no `Symbols.framework`, no `MediaExtension`.

**c. The full symbol audit** (§3), on both slices:
```sh
scripts/check-macos-dyld.sh "$BIN" /path/to/MacOSX10.15.sdk     # the strong verdict
scripts/check-macos-dyld.sh "$BIN" --floor 10.15                # the smoke test
```
Expect `PASS`, `offenders 0`. The last recorded run was 539 undefined / 538 ok / 1 weak / 0
offenders on an x86_64 dev build. Raising the deployment target can flip symbols from strong to
weak imports; the script reports those as `WEAK` and treats them as safe, so the weak count may
rise and the offender count must stay 0.

**d. Prove the gate has teeth** (§4, do it once): add an `extern_class!` for `SCStream` somewhere
in `macos_utils/`, build, and confirm the script exits 1 with
`FAIL _OBJC_CLASS_$_SCStream … ABSENT from the 10.15 SDK stubs`. Then revert. A gate nobody has
seen fail is a gate nobody knows works.

**e. The bundle carries what it must** (§5):
```sh
app=src-tauri/target/universal-apple-darwin/release/bundle/macos/ScreenExtend.app
plutil -p "$app/Contents/Info.plist" | grep -E 'NSAudioCapture|NSAllowsLocalNetworking|LSMinimumSystemVersion|AutomaticGraphicsSwitching'
defaults read "$app/Contents/Info.plist" NSSupportsAutomaticGraphicsSwitching   # -> 1
ls -l "$app/Contents/Resources/resources/"                                      # libopus.dylib + ScreenExtendAudio.pkg
```
`NSAudioCaptureUsageDescription` missing is a **hard process kill** the first time a user enables
System Audio, with nothing in our own log. This closes rows 264 and 178.

**f. The CSP** (§5). `tauri.macos.conf.json` used to set `app.security.csp: null`, so every shipped
macOS build ran the control UI with no policy at all. It now carries the Windows policy plus
`worker-src 'self' blob:; child-src 'self' blob:` (the second because Safari 13.1 has no
`worker-src` and falls back to `child-src`, and the monaco language services are workers). Open the
app, exercise the log terminal and the JSON config editor, and confirm the Web Inspector console
shows **no** `Refused to …` line and that
`document.querySelector('meta[http-equiv="Content-Security-Policy"]')` is non-null. If a directive
genuinely cannot be satisfied, narrow *that directive* — do not put `"csp": null` back. Closes row
265.

**g. `libopus.dylib` must be universal.** `src-tauri/resources/PROVENANCE.md` carries a warning that
the shipped one is **x86_64-only**, which means system audio is broken on every Apple Silicon Mac
running the universal build. Check with `lipo -info src-tauri/resources/libopus.dylib`. If it is
thin, build a universal one from the vendored sources the same way the existing one was built
(plain `clang -shared`, both arches, `lipo -create`), update `PROVENANCE.md` and `SHA256SUMS`, and
**verify system audio actually works on arm64** — this is risk-register R-234 and it is a shipping
bug, not a nicety.

**h. Code identity** (`macos-app-signing.md`). TCC keys a grant on `(service, bundle-id)` plus a
**`csreq`** blob captured at grant time; when the running binary's identity does not match, the user
sees a ticked Screen Recording box over a grant that does not apply and `tccd` logs
`Failed to match existing code requirement for subject app.screenextend.desktop`.
```sh
codesign -dv --verbose=4 "$app"     # Authority = Developer ID Application; TeamIdentifier set; not adhoc
codesign -d -r- "$app"              # the designated requirement
spctl -a -vvv -t install "$app"     # accepted, source=Notarized Developer ID
xcrun stapler validate "$app"
```
Then **seed the checked-in expectation**, which does not exist yet because nobody has had a signed
build:
```sh
codesign -d -r- "$app" 2>&1 | sed -n 's/^designated => //p' \
  > src-tauri/resources/EXPECTED_DESIGNATED_REQUIREMENT.txt
```
Commit it. From then on the release job fails with a diff when the identity drifts, instead of
printing a warning. And confirm two consecutive release builds produce **byte-identical**
designated requirements — if they do not, every update silently drops the user's grant.

**i. The grant survives an update** (needs the clean VM, ~30 min). Install version *n*, launch,
grant Screen Recording, join a device. Install *n+1* over it. Launch: the grant must still apply,
with no re-prompt and **no** `Failed to match existing code requirement` line in
`log stream --predicate 'subsystem == "com.apple.TCC"'`. Then
`doctor --json | jq '.macos.designatedRequirement, .macos.translocated'` must show the same
requirement and `false`.

**j. Translocation** (`macos-permissions.md` §5). Download the DMG in Safari, copy the app to
`~/Downloads` **with `cp` in a terminal** (dragging in Finder clears the quarantine flag
permanently), confirm `xattr -p com.apple.quarantine` prints a value, double-click it, and confirm
`doctor --json` reports `macos.translocated: true` with an executable path under
`/private/var/folders/…/AppTranslocation/…`. Grants do not stick for a translocated app. Record
whether the app says anything useful about it — it currently does not offer to move itself to
`/Applications`, and **building that offer is in scope** (it is a new dialog, so keep it to the
existing modal's visual language and record it as a Decision).

**k. The re-signed-binary csreq mismatch** (`macos-permissions.md` §6). Grant to the Developer
ID-signed app, confirm capture works, then `codesign --force --deep --sign - …` and relaunch.
System Settings will still show the toggle ON while capture fails. `doctor` must show
`code object is not signed at all` / no TeamIdentifier, which is what identifies this cause. **On
macOS 15 specifically, an ad-hoc identity is reported to be rejected for screen capture outright
regardless of the toggle** — record whether the probes reflect that. **Fail if `doctor` cannot
distinguish this from a genuine denial.**

**l. The audio driver, signed and notarized** (`macos-audio-driver-signing.md`). `coreaudiod`
refuses to load a HAL plug-in that is not Developer ID signed and notarized unless SIP is disabled.
The whole path is one command, from `src-tauri/macos/ScreenExtendAudio/`:
```sh
SE_SIGN_IDENTITY="Developer ID Application: <Name> (<TEAMID>)" \
SE_INSTALLER_IDENTITY="Developer ID Installer: <Name> (<TEAMID>)" \
SE_NOTARY_PROFILE="screenextend-notary" \
SE_ARCHS="x86_64 arm64" \
./scripts/release.sh
```
It fetches libASPL at its pinned commit, compiles, signs the bundle with the hardened runtime and a
secure timestamp, builds and signs the `.pkg`, notarizes, staples, and copies the result to
`src-tauri/resources/ScreenExtendAudio.pkg` — the path `install_audio_driver` resolves at runtime.
Preflight checks every variable before compiling anything. Then verify independently:
```sh
spctl --assess --type install -vv build/ScreenExtendAudio-*.pkg   # accepted, source=Notarized Developer ID
pkgutil --check-signature build/ScreenExtendAudio-*.pkg
codesign -dv --verbose=4 build/ScreenExtendAudio.driver           # flags=0x10000(runtime), real TeamIdentifier
```
`adhoc` in the flags means `SE_SIGN_IDENTITY` was not picked up. **Then install it on a machine with
SIP ON** — this is the whole point, and it flips the "macOS audio driver load" tier row from
UNVERIFIED to VERIFIED for the first time. The `postinstall` runs
`launchctl kickstart -k system/com.apple.audio.coreaudiod` (falling back to `killall coreaudiod`),
which interrupts **all** audio on the machine for a moment; if the kickstart fails (it can under
MDM), `install_pkg` returns `needs_reboot`. Verify the load end to end with the four steps in that
document (`log show` for the plug-in's own subsystem, `shmcheck`, `hal_render` + `shmcheck watch`,
and `cargo test --lib legacy_live -- --ignored`).

**Note the tier ordering while you are here:** on your Mac the driver tier must **not** be selected
(the tap wins). Verify the driver loads, is present in the HAL, and is correctly *not chosen* — a
present-but-unselected driver is the case M1-10 exercises.

#### M1-28 · The CI workflows' first run — **owner-authorized push only**
*Row R82 · risk-register R-248*

`.github/workflows/ci.yml` (six jobs: `frontend`, `rust`, `macos-smoke`, `dyld-floor`,
`blob-checksums`, `perf-gate`) and `build-release.yml` have **never executed.** They were validated
by parsing YAML and checking every job and step key. Running them requires a `git push`, which §2
forbids.

**If — and only if — your own instructions explicitly authorize a push**, push the branch and make
the workflows green. Otherwise this item stays open and is reported as
"still UNVERIFIED — requires a push the engagement forbids."

Either way, do the part you can do locally: **run the `macos-smoke` job's assertions by hand on
this machine**, because they encode the tier expectations this guide is verifying and a wrong
expectation is a bug in the gate:

```sh
cd src-tauri && cargo build --bin ScreenExtend
./target/debug/ScreenExtend doctor --json > doctor.json ; echo "exit $?"
jq -e '.os.supported == true' doctor.json
for f in .os.name .os.version .os.supported .audioBackend .displayBackend; do
  jq -r "$f" doctor.json; done
jq -r '"probes: \(.macos.probes | tojson)"' doctor.json
cargo test --lib macos_utils::streamer::encoder -- --ignored --nocapture
```

The job asserts a **runner-OS → audio-tier mapping**: `macos-13 → screencapturekit`,
`macos-14 → process_tap`, `macos-15 → process_tap`. Your machine must satisfy the same mapping. It
also accepts `doctor` exit code **3** (a required permission is legitimately missing on a hosted
runner) but treats exit **1** (unsupported OS) as a failure. If your machine's real behaviour
contradicts the job, **fix the job** — that is a genuine finding about the gate.

The `dyld-floor` job runs on `macos-15` and does the `--floor 10.15` variant plus the `otool -L`
and deployment-target checks from M1-27; validate those assertions locally too. And add the missing
macOS arm to the `perf-gate` job (`scripts/perf-gate.mjs`, `ci/perf-baseline.json`) using whichever
macOS test you made print a timing in Phase 2. Remember the tolerance rule from §4.5: the file-wide
default is **20 %** for a shared runner, a key may carry its own, and **10 % is the tightest honest
number even for a quiescent machine** — five consecutive runs of one unchanged release binary on the
idle Windows dev box spanned 4157–4490 µs, an 8.0 % spread. Do not seed a macOS key below 10 %.

---

## 9. Phase 4 — the research agenda for this area

Run these in parallel with Phase 3, as dedicated Opus 5 research agents. Each produces a written
report into `docs/research/` (append to the existing macOS files rather than creating rivals) with
citations, concrete actionable findings, and an explicit "what we should do about it" section.
**Prefer primary sources**: Apple headers and framework docs, WWDC session transcripts, the
Chromium and WebKit source and bug trackers, OBS, Moonlight/Sunshine, FFmpeg's VideoToolbox
encoder, AudioCap, and BetterDisplay's issue tracker.

1. **ScreenCaptureKit, 14 through 26.** Every `SCStreamConfiguration` and `SCContentFilter`
   property added since 12.3, what each costs, and which ones reduce WindowServer's per-frame work.
   WWDC 2022–2025 SCK sessions. Specifically: is there anything resembling a "deliver as soon as
   composited" mode; does `SCStreamFrameInfoDisplayTime` mean what CGDisplayStream's `displayTime`
   meant (a *scheduled* time) or a real composite time; and what exactly is the failure Chromium
   documents for full-display capture on 12.3 — a hard failure our fallback catches, or silent
   wrong content, which it does not.
2. **VideoToolbox low-latency rate control, in depth.** What `EnableLowLatencyRateControl`
   actually changes (WWDC21 "Explore low-latency video encoding with VideoToolbox"), and the
   unresolved question in this repository's own research: **does it forbid Constrained Baseline?**
   The header and Apple's docs both say "Only High profiles"; WWDC21 presents Constrained Baseline
   as a low-latency feature. It matters directly, because browser clients frequently advertise only
   `42e01f`. Resolve it by experiment on this machine if the sources still disagree.
3. **The Core Audio Process Tap, in depth.** Every `CATapDescription` initializer and property, the
   aggregate-device construction the AudioCap reference uses, what the HAL grants for a tap
   aggregate's buffer frame size, whether a tap can avoid changing the default output device's
   sample rate, and the 15/26 changes. Also: does a tap survive a default-output-device change, a
   sample-rate change by another app, and a display sleep?
4. **The macOS 15 and 26 permission model.** The re-consent interval and what exactly triggers it;
   whether `CGPreflightScreenCaptureAccess` ever reflects a live grant on 14/15/26; whether
   `kCGWindowName` remains a valid liveness signal on 26; whether an ad-hoc identity is genuinely
   rejected for screen capture on 15; and whether
   `com.apple.developer.persistent-content-capture` is grantable to a product like this one and
   what Apple's form requires (there are no public criteria — find out what applicants report).
5. **`CGVirtualDisplay` on modern macOS.** What BetterDisplay, Deskpad, and the other consumers of
   this private API report on Sequoia and Tahoe; the current class dump; what `refreshDeadline`,
   `rotation`, `isReference`, `transferFunction` and anything newer do; and whether
   `CDVirtualDisplayCreate` in CoreDisplay is a viable public-ish C path.
6. **CoreWLAN HostAP on modern macOS and on Apple Silicon.** Class dumps show
   `startHostAPModeWithSSID:…` present through CoreWLAN 1375.2 (Monterey era) and nothing after;
   Apple Silicon Macs are widely reported not to support HostAP on the built-in interface. Find the
   primary evidence, find what `startHostAPMode:` (the no-argument variant) reads its configuration
   from, and find what modern macOS's own Internet Sharing does under the hood — that is the tier
   worth having if the SPI is gone.
7. **Apple Silicon media engine.** Its H.264 encode latency characteristics, its concurrent-session
   limit, whether `PreferredEncoderGPURegistryID` means anything on a unified-memory machine, and
   whether the 16×16 minimum Chromium reports for hardware encode holds.
8. **What the competition does on macOS.** Luna Display's 11.3/22.4 ms bench and how it gets a
   display (it uses a hardware dongle, which is a different game — find out what its software-only
   mode does); Sidecar's transport and its "consistently below 30 ms" wired claim; Deskpad and
   BetterDisplay's virtual-display approach; and how OBS configures VideoToolbox on Apple Silicon
   (it ships `RealTime = false` after measuring frame drops — find out under what conditions).

---

## 10. Phase 5 — the hyper-optimization sweep

Once the verifications above are done and the ledger is honest, sweep for wins. **Every knob gets
tried and measured. A 0.1 ms win on one macOS build is in scope.** Nothing here may be skipped as
"minor" or "speculative"; each item ends as an implemented change with a number or a recorded
rejection with a number.

**Capture**
- Every `SCStreamConfiguration` key in the dumped property set that this tree does not set.
- Capturing at the client's exact target resolution (which the virtual display already provides)
  versus capturing large and scaling — the delivery-callback cost is proportional to output size
  (311 µs at 1440×900 vs 88 µs at 720×448 on the floor box), so this is real work moved off the
  process.
- IOSurface lifetime and purgeability: is any per-frame allocation happening at all? Verify with a
  malloc-stack instrument, not by reading the code.
- Whether releasing `last_frame` *before* awaiting `complete_pending()` (R-293) lets the queue depth
  come back down without drops.
- Whether the frame repeater's keepalive can be cheaper on a static desktop while still letting a
  newly joining or recovering client see content within the 750 ms first-frame deadline.

**Encode**
- The full property sweep from M1-03, each key measured independently and then in combination.
- Multiple encoder sessions to overlap frames, if `InstanceLimit` allows it on this machine.
- Whether a per-frame QP cap (`MaxAllowedFrameQP`) gives the "one complex frame cannot blow the
  buffer" property that `DataRateLimits` was rejected for failing to give safely.
- `MaxKeyFrameInterval` at 600 s plus LTR repair versus a periodic IDR — measure the bitrate spike
  each avoids.

**Scheduling and power**
- The `[qos]` read-backs: `thread_policy_get(THREAD_TIME_CONSTRAINT_POLICY)` and
  `pthread_get_qos_class_np` on every thread, on this OS. Row 150 changed the computation to
  `0.75 × period`; sweep it (0.5, 0.75, 0.9) and measure the tail. On Apple Silicon expect
  `thread_policy_set(AFFINITY) failed` — `THREAD_AFFINITY_POLICY` is unimplemented there and
  returns an error rather than doing nothing quietly; that is expected and harmless, but confirm it
  is logged as expected rather than as an error.
- P-core versus E-core placement on Apple Silicon: QoS class is the only lever
  (`user_interactive` / `user_initiated`), and getting the capture and encode threads off the
  efficiency cores is frequently a large win that is frequently missed. Measure it.
- `power.rs` / `activity.rs`: confirm the idle and display-sleep assertions are actually taken
  (`pmset -g assertions` during a session) and that they are released on teardown. Then measure
  what they cost in package power over 5 minutes — an assertion that is not needed is battery the
  user is paying for.
- App Nap: confirm the process is not being napped when the control window is hidden. If it is,
  that is a latency cliff nobody has looked for.

**Audio**
- The tap buffer-frame sweep (M1-07b) and the ring's wake mechanism (M1-07c).
- End-to-end audio latency measured separately: capture → Opus → DataChannel → decode → worklet →
  speaker, against a real device with `SEAudio.getSyncInfo()`. 5 ms Opus frames are good; verify
  nothing downstream adds 100 ms of buffer.
- Whether the A/V sync path ever makes video wait for audio. It must not.

**Startup and join**
- Time-to-first-frame is part of "feels slow". Cut display creation, mode registration, TLS
  handshake, ICE gathering, encoder init and first keyframe, each measured. The Windows-side
  measurement of the same path put the whole join at 1043–1172 ms with the handshake free — get the
  macOS number and beat it.

**Allocation and lock audit**
- Zero allocations per frame on the hot path; pool and reuse every buffer; verify with an allocation
  profiler, not by reading the code.
- Every `std::sync::Mutex` on the hot path: can it be an atomic, an `ArcSwap`, a per-session
  unshared value, or a channel? `arc-swap` is already a dependency and already used on macOS.

---

## 11. Verification and gates

### 11.1 Per-commit
From `src-tauri/`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
After any FFI change: `cargo build --release` then `scripts/check-macos-dyld.sh` (both forms if you
have the 10.15 SDK) plus `nm -u | grep -i -E 'displaystream|screencapturekit|CATap'` empty and
`otool -L` clean. From the root when `src/` changed: `pnpm lint`, `pnpm build`,
`node scripts/check-legacy-webkit.mjs`, `node scripts/check-bindings.mjs`,
`node scripts/check-versions.mjs`.

### 11.2 FFI verification
Every FFI definition you touch or add — VideoToolbox, Core Audio, `CATapDescription`, the SCK
runtime interop, CoreWLAN, AX, `CGVirtualDisplay*` — is verified against the real headers on this
machine. This repository already has the pattern (`nvenc_layout.rs`, `intel_layout.rs`,
`opus_layout.rs`, `amd_layout.rs`): **extend it to every FFI surface you touch.** A wrong struct
offset is silent memory corruption, not a compile error. Where a struct crosses the boundary, write
a layout test that asserts every field offset and the total size against the SDK header on this
machine, and record the SDK version the test was generated from.

### 11.3 Tests
Keep the existing convention: inline `#[cfg(test)]` modules, collected in per-module `test/`
directories; hardware- and OS-gated tests are `#[ignore]`d with an explicit reason so a skipped test
is *visibly* skipped, never silently passing. **Do not gate a tier test on a version number** — that
is the defect R-225 records. Add tests for every path you make reachable for the first time.

### 11.4 The final production build gate
After all feature work and immediately before writing the report:

```sh
pnpm install
pnpm tauri build --config src-tauri/tauri.macos.conf.json
pnpm tauri build --target universal-apple-darwin --config src-tauri/tauri.macos.conf.json
```

**The one failure you may accept is the Tauri updater signing key error** — the private key for the
updater manifest is not on this machine and that is expected. **No other failure is acceptable.**
Zero cargo errors, zero TypeScript errors, zero linker errors, zero missing-resource errors, zero
bundling errors. If the build produces any other error, it is not done — go fix it. Do not write the
report over a broken build and do not describe a build error as a known issue you are leaving
behind.

Then confirm the tree is clean and reviewable: nothing half-finished, no debug scaffolding in
shipping paths, no commented-out experiments, no env-var overrides you added for testing left
undocumented.

State the build result explicitly at the top of the report.

---

## 12. Deliverables and the final report

### 12.1 What must be true when you finish

- **`findings/` is empty** of your own findings, and any pre-existing finding in your territory
  (§4.5) is deleted because it was resolved. Anything deleted *without* being done is listed in the
  report with the reason.
- **`docs/OPTIMIZATIONS.md`** carries every row this guide names, flipped to `VERIFIED` with
  numbers or reverted with a reason and a number, plus new rows for every change you made and every
  rejection you recorded. **Row 305 (`UNVERIFIED — see guides/modern-mac.md`) is M1-29's row and
  flipping it is the single most valuable ledger edit available to you.**
- **`docs/CAPABILITY-TIERS.md`** has every macOS tier row this guide names flipped, with the
  measured "Expected delta" replaced by the observed one.
- **`docs/BASELINE.md`** has a new section for this machine, with the full matrix from §7.1. The
  10.15.8 section is untouched.
- **`docs/manual-tests/macos-{capture-encode,audio,permissions,hosted-network,build,app-signing,audio-driver-signing}.md`**
  each carry this machine's real log lines and numbers where they previously said
  "COMPILE-CHECKED — needs a 14.2+ Mac", plus anything the script failed to anticipate.
- **`docs/RISK-REGISTER.md`** entries R-232, R-233, R-234, R-235, R-236, R-242, R-243, R-249/R-426
  and R-293 are closed or updated with what was found.
- **`docs/discovery/G1-machine.md`** exists and carries the preflight and the hardware discovery.
- The branch `guide/modern-mac` is local, committed in small measured units, and **nothing is
  pushed** (unless M1-28 was explicitly authorized, in which case say exactly what was pushed).

### 12.2 The report

Write it as the final message and also as `docs/RESULTS-modern-mac.md`. It must contain:

1. **The build result**, first: which configurations you built, that they succeeded, and that the
   only error encountered was updater signing.
2. **Machine facts** — exact OS name, version and build; model identifier; CPU and GPU; RAM; display
   configuration and refresh rates; the client devices used with their OS and browser versions; the
   AP model and band; whether the machine was on mains or battery; and whether SIP was on.
3. **The §5.1 check**, verbatim: the five greps and whether the tree matched. If it did not, say what
   you landed before anything else could be measured.
4. **Per-item status**, one line each, against this guide's own numbering M1-00 and M1-01 … M1-29:
   `VERIFIED (numbers) | reverted (reason) | still UNVERIFIED (what was missing)`. **No item may be
   silently absent.**
5. **Before/after numbers for every change**, as p50 / p90 / p99 / max, with the trial count and
   whether the machine was quiet.
6. **The ledger diff** — the exact rows added or edited in `docs/OPTIMIZATIONS.md` and
   `docs/CAPABILITY-TIERS.md`, and the `docs/BASELINE.md` cells filled.
7. **Findings** — created, resolved-and-deleted, and retagged, by filename.
8. **Comparison against the numbers in §5.4** — the 10.15.8 floor (every number must be beaten, and
   any that is not is a headline finding) and the competitor table (Luna 22.4 ms Wi-Fi / 11.3 ms
   USB, Hopp 98 ms browser WebRTC, Parsec 4–8 ms LAN, Sidecar < 30 ms wired). State where the macOS
   host half now sits in a Luna-class budget.
9. **Decisions Made Without Consultation** — every judgment call the reader might have made
   differently, with your reasoning, so each can be reviewed and reversed.
10. **Verification status** — what remains `COMPILE-CHECKED` or `UNVERIFIED` after your run, what
    hardware or OS each remaining item needs, and a concrete manual test to close it.
11. **What did not work, with numbers.** Negative results are deliverables. A rejection with a
    measurement is worth more than a silence.
12. **Recommendations requiring the user** — anything needing a push, a purchase, an Apple developer
    form (`com.apple.developer.persistent-content-capture`), a UI-visible change you declined to
    make, or anything you judged too risky to do unsupervised. Include the **exact signing and
    notarization procedure** you ran (or would run), every environment variable and what it is, what
    success looks like, and the `coreaudiod`/reboot caveat.
13. **Branch state** — the branch name and one line per commit. Nothing pushed.

### 12.3 The standard

> "Even if there's some obscure fix for exactly one version of an operating system or weird browser
> that shaves off exactly 0.1 millisecond, I want that. I want absolutely every possible item
> optimized. Never assume anything is impossible unless web search and actually trying it. If even
> one app — closed source, paid, whatever — has figured it out, there is a way and we must figure it
> out as well."

Take that literally. Be exhaustive. Measure everything. Research before you conclude. Try before you
dismiss. Keep the 10.15 machines working while making this one fast — a legacy path is never deleted
because it is slow; it is made as fast as *it* can be. Change nothing about how the frontend looks.
Never push. Decide rather than ask, and write down every decision. And write down every number you
produce, including the ones that disappointed you.
