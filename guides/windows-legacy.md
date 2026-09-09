# Guide: Windows below 24H2 — WGC property tiering, DXGI Desktop Duplication, legacy audio and the hosted network

**Read this file end to end before you touch anything.** It is the complete directive for one
autonomous run. You have no access to the conversation that produced it, to the other guides, or
to either of this project's development machines. Everything you need is here.

---

## 0. What this run is for, and what machine it needs

### 0.1 Purpose

ScreenExtend has two development machines. One of them is a Windows 11 build **26200** laptop with
an RTX 3080 Ti Laptop GPU beside an Intel UHD iGPU. That box proved a great deal — but it sits
above the entire Windows band this guide covers, and it is a machine on which **DXGI Desktop
Duplication does not work at all**. As a direct consequence, a large block of shipped Windows code
has never executed:

- The **Windows.Graphics.Capture property tiering** that decides whether a Windows 10 or pre-24H2
  Windows 11 host stays on WGC or is pushed onto DXGI. On 26200 every property is present, so the
  *absent-property* branch — the one that matters for every host below build 26100 — has never
  been taken.
- **The entire DXGI Desktop Duplication path.** On the dev box `\\.\DISPLAY1` is enumerated by
  exactly one adapter (the NVIDIA one), and both `IDXGIOutput1::DuplicateOutput` and
  `IDXGIOutput5::DuplicateOutput1` return `DXGI_ERROR_UNSUPPORTED` (`0x887A0004`) on it. Against a
  freshly created virtual display, duplication *is* created and then returns `ACCESS_LOST` on every
  acquire, forever. Every DXGI row in the ledger that claims a *latency* improvement is therefore
  `COMPILE-CHECKED`, and every `docs/BASELINE.md` DXGI cell reads **unmeasurable**.
- The **low-latency WASAPI engine period** request. The dev box's OEM driver reports
  `default = fundamental = min = max = 480 frames`, so the probe correctly declines and the request
  branch has never run.
- The **process-loopback refusal** path, which only executes on a Windows build that exports
  `ActivateAudioInterfaceAsync` but does not support the process-loopback activation type.
- The **audio format-conversion tiers 2 and 3**, unreachable on an endpoint that is already
  48 kHz float32 stereo.
- The **`netsh` legacy hosted-network tier**, which cannot run on a driver that reports
  `hosted_network_capable => false`.
- The **locale-proof hosted-network probe**, which can only be distinguished from the old string
  match on a non-English Windows install.
- The **virtual-display failure tiers** (`DriverNotInstalled`, `DriverNotResponding`) and the
  software-adapter branch of the compatibility report.

This run closes all of it, on hardware where each branch is actually reachable, and then
hyper-optimises what it finds.

### 0.2 The machine contract

| Requirement | Value | Why |
|---|---|---|
| **Minimum OS** | Windows 10 **2004**, build **19041** | `IsCursorCaptureEnabled` arrives at 19041; the app's own stated floor is `Windows 10 20H1 (build 19041)` (`windows_utils/compatibility.rs::MIN_BUILD`). |
| **Maximum OS** | build **22631** (Windows 11 23H2) — **strictly below 26100** | Build 26100 (Windows 11 24H2) is where `GraphicsCaptureSession::MinUpdateInterval` and `DirtyRegionMode` appear. The project's dev box is 26200 and already covers everything at or above 26100. Above 22631 the absent-property branch stops being reachable and this whole guide loses its subject. |
| **Best case** | **both** a Windows 10 22H2 (19045) machine and a Windows 11 23H2 (22631) machine | 19045 and 22631 sit on *different* property tiers — 19045 lacks `IsBorderRequired` (20348) and `IncludeSecondaryWindows` (22000) as well. Either alone is worth the run; both is materially better. |
| **Graphics** | a **hybrid-graphics laptop** (Intel iGPU + discrete GPU) **on which DXGI Desktop Duplication actually works** | This is the hard one. Verify it in the preflight *before* starting. A single-GPU machine is acceptable as a second box but cannot close L-04. |
| **Wi-Fi** | a **hosted-network-capable** Wi-Fi adapter | `netsh wlan show drivers` must print `Hosted network supported : Yes`. This is the last Windows band where that is common; it is the only place tier 3 of the hosted-network ladder can be exercised at all. |
| **Audio** | an endpoint that can be driven by the in-box **High Definition Audio Device** driver (i.e. **not** USB-only audio), **plus** at least one non-48 kHz / non-stereo / non-float32 endpoint | The in-box HD Audio driver documents 128–480 frame buffers, which is how the low-latency engine-period branch executes. A 44.1 kHz USB DAC, a 24-bit interface or a 5.1 HDMI sink is how audio format tiers 2 and 3 execute. |
| **Locale** | a **non-English** Windows display language available (German, French, Japanese or Chinese) | The old hosted-network probe string-matched English `netsh` output. The replacement can only be *proved* locale-proof by running it under a non-English display language. |
| **Virtual display** | the ScreenExtend IDD driver — and the ability to run **without** it first | `DriverNotInstalled` is only reachable on a machine that has not installed it yet. Take that reading before you install. |
| **Client** | a phone or tablet on the same network | Several items need a real join. |
| **Tools** | `ffprobe` / `ffplay` on `PATH`, `iperf3`, Process Explorer, a second Wi-Fi client device | Step-level requirements are named per task. |
| **Privileges** | Administrator (driver install, `netsh wlan start hostednetwork`, driver swap) | |

A VM is adequate for the WGC tiering questions (L-01, L-06, L-12, L-14) and useless for anything
with a latency number in it — a VM's software rasteriser makes every percentile meaningless. Say
which machine each number came from.

### 0.3 Preflight — prove the machine qualifies before you start

Run all of this and paste the output into `docs/discovery/windows-legacy-machine.md` (create it).
If step 4 fails, **stop and report** rather than proceeding: a machine where DXGI duplication does
not work cannot close the largest half of this guide, and pretending otherwise wastes the run.

```powershell
# 1. OS identity. `winver` is authoritative; OSVersion under-reports on some hosts.
winver
[System.Environment]::OSVersion
Get-ComputerInfo | Select-Object WindowsProductName, WindowsVersion, OsBuildNumber, OsLanguage
(Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion') |
  Select-Object ProductName, DisplayVersion, CurrentBuild, UBR

# 2. Graphics adapters and drivers.
Get-CimInstance Win32_VideoController |
  Select-Object Name, DriverVersion, DriverDate, AdapterCompatibility, VideoProcessor

# 3. Audio endpoints and their drivers.
Get-CimInstance Win32_SoundDevice | Select-Object Name, Manufacturer, Status
Get-PnpDevice -Class MEDIA | Select-Object FriendlyName, Status

# 4. Wi-Fi driver capability (English install; on a localized one this prints a translated phrase).
netsh wlan show drivers

# 5. Toolchain.
rustc --version ; cargo --version ; node --version ; pnpm --version
where.exe ffprobe ; where.exe ffplay ; where.exe iperf3
```

Then build and ask the app itself:

```powershell
cd src-tauri
cargo build --release --features probe
cargo run --release -- doctor --json
cargo run --release --features probe -- probe --dxgi dxgi_preflight.bmp
```

**The gate is `probe --dxgi`.** It must print a line of the shape

```
dxgi: duplication ready on \\.\DISPLAYn via DuplicateOutput1 on <adapter> (vendor 0x____, LUID 0x____): WxH, rotation=..., logical=WxH
dxgi probe: wrote WxH (updates=N, cursor_drawn=true, cursor_pos=X,Y, cursor_size=WxH) -> dxgi_preflight.bmp
```

`via DuplicateOutput` instead of `DuplicateOutput1` is fine and is itself a result worth recording.
What disqualifies the machine is

```
probe failed: no adapter can duplicate \\.\DISPLAY1 (<adapter>: Error { code: HRESULT(0x887A0004), ... })
```

which is exactly what the dev box prints and is the reason this guide exists.

Also confirm, from the same `doctor --json`:

- `capture` reads `WindowsGraphicsCapture` (WGC works here) — and the log carries exactly one of
  `dxgi: duplication fallback available via <api>` / `dxgi: duplication fallback is NOT available on
  this machine (…)`.
- `display_backend` — record it **before** installing the IDD driver. If it reads `IddDriver`
  already, the driver is installed and you have lost the `DriverNotInstalled` reading; uninstall it
  (`ScreenExtend drivers remove`, Administrator), re-read, then reinstall.
- `hosted_network_backend`, `audio_backend`, `encoder_backend`, `input_backend`.

---

## 1. Model and orchestration directives

**Every subagent you spawn runs on Claude Opus 5.** No exceptions, no "this one is simple". If a
subagent spawns its own subagents, those are Opus 5 too. You are the orchestrator: you plan,
partition, integrate, arbitrate and verify — you do not personally do the bulk of the reading. Push
as much concrete work down to Opus 5 subagents as you can: every file read, every research
question, every self-contained implementation, every measurement pass, every review. Your context
is the scarce resource and it should be spent on synthesis and judgment. Parallelise hard — many
agents at once, not a queue of one. §8 of this guide suggests a partition; you are not bound to it.

**You are running fully autonomously. There is nobody to ask.** The user cannot see this
environment until you are finished. A question asked mid-run is a question that never gets
answered — it just stalls the work. Therefore:

- **Never ask and wait. Decide.** Make the best call on the evidence, act, and record it. Every such
  call goes in the final report under a clearly labelled **Decisions Made Without Consultation**
  section so the user can review and reverse any of them.
- **When in doubt, build it.** If you find a fix or improvement worth having, *implement it* rather
  than writing it up as a recommendation. Deleting a finished implementation costs the user a
  minute; a deferred idea costs a future agent a full re-research and context-rebuilding cycle. You
  are already holding all the context. Spend it.
- **Lean inclusive on scope, inside the fence.** A bug, a dead code path, a stale comment, a missing
  test found while working is fixed, not hoarded — provided it is inside §2.3's territory.
- **Self-unblock.** Missing information is a research task. Missing tooling is an install decision
  you make yourself. A failed approach is data: record it and try the next one.
- **Leave the tree reviewable.** No half-finished refactors, no debug scaffolding in shipping paths,
  no commented-out experiments. Anything incomplete is finished, reverted, or isolated on a clearly
  named local branch and called out in the report.

**Token and time cost are not constraints and must never influence scope.** Every optimisation that
shaves even **0.1 ms** on even one Windows build is in scope and gets *implemented and measured*.
"Good enough", "diminishing returns", "minor" and "speculative" are not reasons to stop. A
speculative idea is resolved by trying it and measuring, or by a **recorded rejection with a reason
and a number** in `docs/OPTIMIZATIONS.md` — never by being skipped.

---

## 2. Absolute rules

### 2.1 Git

- **Never `git push`. Never touch `origin` in any way.** No pushes, no tags, no PRs, no issues, no
  workflow runs, no remote branches, not even a `--dry-run` you then "just try for real". `origin`
  is read-only to you. The user reviews and pushes personally after the run.
- Work on a local branch: `git checkout -b guide/windows-legacy` from `latency-overhaul`.
- Locally you are unconstrained: branch, commit, stash, worktree, rewrite local history freely.
- Commits are small and single-purpose. A performance commit **states its measured before/after in
  the message**. Commit at least every 30 minutes so a lost session loses at most 30 minutes.
- Every commit message ends with:

  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: <the URL of your own session>
  ```

- Before any `reset --hard`, run `git status --short` and confirm the tree is clean.

### 2.2 Appearance

**Nothing about how any user interface looks may change.** The React desktop control UI (`src/`)
and the client web page (`src-tauri/src/streamer/static/`) keep their current look, element
structure, layout and behaviour. This is a constraint on appearance, not on performance — but
nothing in this guide's territory should require touching either of them anyway. If you believe a
fix needs a UI-visible change, **do not make it**: implement the fastest version that preserves the
appearance, and write the UI-visible alternative into `docs/OPTIMIZATIONS.md` under
"UI-visible recommendations" with its measured upside.

### 2.3 Scope fence — this is the hard boundary

Everything outside this territory has already been audited, optimised and verified on the
project's two development machines. **A change out there is a merge conflict against work you
cannot see.**

**Tier A — freely writable. This guide's own territory:**

```
src-tauri/src/windows_utils/streamer/dxgi/**          (mod.rs, cursor.rs)
src-tauri/src/windows_utils/streamer/capture.rs
src-tauri/src/windows_utils/windows_capture/**        (the vendored fork)
src-tauri/src/windows_utils/audio/**                  (loopback, silence, engine, format,
                                                       device, guards, process_loopback, test/)
src-tauri/src/windows_utils/hosted_network.rs
src-tauri/src/windows_utils/networking.rs
src-tauri/src/windows_utils/compatibility.rs          (Windows capability reporting)
src-tauri/src/windows_utils/streamer/test/**          (the tests for the above)
docs/manual-tests/windows-capture.md
docs/manual-tests/windows-audio-tiers.md
docs/manual-tests/core-closeout.md                    (§3 and §6 only)
docs/OPTIMIZATIONS.md · docs/CAPABILITY-TIERS.md · docs/BASELINE.md
docs/discovery/windows-legacy-*.md                    (new files you create)
findings/**
```

**Tier B — writable only to fix a defect this guide's own verification exposes.** These files carry
items PLAN assigns to this guide (L-05, L-06, L-07, L-08, L-18) but they are shared surfaces. Read
them freely. Change them only when *your* measurement demonstrates a concrete defect, keep the diff
minimal and single-purpose, and say so explicitly in the ledger row:

```
src-tauri/src/windows_utils/virtual_display.rs
src-tauri/src/windows_utils/driver_ipc/**
src-tauri/src/windows_utils/streamer/tuning.rs
```

**Tier C — read only. Never modify. A needed change becomes a `findings/` file plus a
`docs/OPTIMIZATIONS.md` row marked as a recommendation, not a diff:**

```
src-tauri/src/windows_utils/streamer/pipeline.rs      (encoder/backend selection)
src-tauri/src/windows_utils/streamer/{nvidia,intel,amd,amd_sys,x264}/**
src-tauri/src/windows_utils/streamer/{scaler.rs,tuning.rs internals beyond Tier B,clock.rs,
                                      workload_window.rs}
src-tauri/src/streamer/**                              (transport, session, server, webrtc,
                                                        bitrate, pacer, keyframe, repair, audio
                                                        protocol/encoder, static/)
src-tauri/src/macos_utils/** · src-tauri/src/linux_utils/** · src-tauri/macos/**
src/**                                                 (the React desktop UI)
src-tauri/src/{lib.rs,cli.rs,main.rs,logbus.rs,single_instance.rs}
```

Two consequences worth stating plainly:

- **L-17 (Intel Quick Sync) is a measure-and-report item only.** You run it, you record what a real
  Intel host answers, you flip the ledger and capability-tier rows with numbers. You do **not**
  edit `windows_utils/streamer/intel/`. Anything that needs an edit there becomes a finding.
- **L-04 (hybrid-adapter selection)** is closed by reading the `adapters:` line and, if the choice
  is wrong, fixing it in `windows_capture/d3d11.rs` (Tier A). The `pipeline.rs` half is Tier C.

### 2.4 Code invariants (from `CLAUDE.md`; the ones that bind this territory)

1. **Probe capabilities, never version numbers.** Every tier is selected by attempting the real
   capability and falling back at runtime without crashing; the selected tier is reported through
   `CompatibilityReport` and the log. `if build >= 26100` is a bug even when it would be correct.
   This guide exists precisely because a *hard-failing* capability request behaved like a version
   check.
2. **Never delete a legacy path because it is slow. Make it as fast as *it* can be.** The goal is
   every device running at the fastest that device is capable of. A Windows 10 host on DXGI has a
   ceiling and your job is to reach *its* ceiling, not to compare it to 26200 and give up.
3. **Real-time threads take no locks and do not allocate.** The audio capture thread (a dedicated OS
   thread with COM MTA + MMCSS "Pro Audio") and the video capture callbacks talk to the async world
   over `crossbeam-channel` only. If your optimisation adds an allocation in a capture callback, it
   is not an optimisation.
4. **`std::sync::Mutex` is the one blocking mutex type for first-party code, and a `MutexGuard` is
   never held across an `.await`.** Lock, copy, drop, then await. Watch the temporary-lifetime
   footgun: a guard produced in an `if let` / `match` scrutinee lives to the end of the block, not
   the end of the line. The only two intentional async locks in the codebase are
   `DISPLAY_CORRELATION_LOCK` (`streamer/server.rs`) and `receive_error`
   (`windows_utils/driver_ipc/client.rs`). **`parking_lot` stays confined to the vendored
   `windows_utils/windows_capture/` fork** — that module is a fork of the `windows-capture` crate
   and keeps its own locks and style; leave that convention as it is.
5. **Use `tprintln!` / `teprintln!`, never `println!` / `eprintln!`,** in the Rust core. They push
   into a 2000-line ring backlog, emit a `LogLine` event to the UI and only echo to stdout when
   verbose mode is on. **Every log line is an IPC event to the UI**, which is why the DXGI
   re-establishment line is rate-limited — respect that when adding lines to hot paths.
6. **The silent render companion is mandatory, not an optimisation.** Without it the WASAPI loopback
   event does not fire (measured: 0/20 event signals without, 20/20 with). Do not "clean it up".
7. **The shared-memory and wire-format ABIs are contracts.** The 13-byte audio DataChannel header
   (seq u32, capture ns u64, flags u8) is shared with the client page, which you may not change —
   so you may not change the header either.
8. **A new Tauri command goes into all three `*_utils` modules and `collect_commands!`**, and
   `src/lib/bindings.ts` is generated — **never hand-edit it**. You should not need a new command;
   if you do, regenerate bindings from a debug build with **no CLI arguments**, run from
   `src-tauri/`, and diff the result.
9. **A new shell invocation goes into `src-tauri/capabilities/default.json`** with argument
   validators, or it fails at runtime. `netsh`, `certutil`, the `nefconc` sidecar and the app's own
   self-invocation are already enumerated there.
10. **DPR correctness:** the resolution multiplier and the OS scale step must be the same number
    (resolution ×d, DPI 100·d %). Snap with `platform::snap_display_dpr`. Pre-register every ladder
    mode **at create time** — a live display cannot be given new ones — and re-apply the DPI
    **after** the resolution change, which resets it.
11. **Simultaneous displays are uncapped** on Windows. Never evict a connected device to make room.
12. **No AI-tooling references in source comments**, and no references to documents that do not
    exist in the tree.

---

## 3. Norms of research and measurement

**Research first, and prefer primary sources.** Microsoft Learn's WinRT and WASAPI reference pages,
the Windows SDK headers themselves, the `windows-rs` crate source, and the source of **OBS**,
**Sunshine/Moonlight**, **FFmpeg**, **Chromium** and **webrtc-rs**. Where two or three independent
shipping projects set the same flag the same way, that is your answer. **Never conclude something
is impossible from your own priors** — conclude it only after a search found nobody doing it *and*
you tried it and measured. If even one shipping product demonstrably does the thing, it is possible
and your job is to work out how.

**Measure before and after every change, as p50 / p90 / p99 / max — never a bare mean.** Jitter is
felt more than the average; a pipeline with a 20 ms mean and a 60 ms p99 feels worse than one with a
30 ms mean and a 33 ms p99. Nanosecond stamps come off the shared host clock
(`streamer::audio::host_now_ns`), which is the same epoch the RTP timestamps ride.

**Never trust a flag without a read-back.** Every property set must be read back and logged; every
tier selected must announce itself. This codebase has three separate cautionary tales:
`AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` reads backwards from its meaning (it is the *expensive*
converter); x264's `encoder_open` returning non-null for `X264_CSP_BGRA` means "High 4:4:4
Predictive", not "BGRA supported"; and `IsBorderRequired` being present and settable does **not**
mean the border is gone (that additionally needs
`GraphicsCaptureAccess.RequestAccessAsync(Borderless)` consent). Assume nothing.

**Measurement hygiene, and one trap that will bite you:**

- At least **3 trials** per cell; report the median with p50/p90/p99/max, and state the spread.
- Check for competing compilers before measuring. During the original overhaul the concurrent
  `rustc` count on the dev box swung between 2 and 17 and moved the CPU-bound percentiles by a
  factor of two. If your machine is not quiet, say so with the number.
- **Leave ≥ 15 s between probe runs.** A probe that exits without tearing down leaves its virtual
  display attached, and the next run attaches to the stale display and streams nothing. This
  silently corrupted **4 of 12** runs during the original matrix; a 15 s gap removed it completely
  (6/6 valid). Runs that open with `[display] already extended (2 monitor(s), 2 source(s))` are
  invalid and must be discarded, not averaged in.
- Release build, always, for any number: `--release` plus `--features probe`. The shipping profile
  is `lto = "fat"`, `codegen-units = 1`; a debug number is not comparable with anything in
  `docs/BASELINE.md`.

**Parallelise.** Run independent measurement passes concurrently where the hardware allows and
serially where it does not — one release build at a time, one probe at a time on a given display.

---

## 4. The product, the repo, and the machinery you will use

### 4.1 What ScreenExtend is

A Tauri 2 desktop app that turns any device with a web browser into a wireless second monitor. It
runs on the **host** (the PC being extended). A **client** — phone, tablet, laptop — joins by
opening a URL or scanning a QR code; there is no client install. The host creates a **real virtual
display per client** (on Windows, via a bundled signed Indirect Display Driver), captures it,
GPU-encodes it to H.264, and streams it over WebRTC with WHEP signalling. Audio is captured with
WASAPI loopback, Opus-encoded at 5 ms frames and sent on a DataChannel. Input (mouse, keyboard, pen,
clipboard) travels back over the same peer connection and is injected on the host.

Two frontends live in the repo: `src/` is the React desktop control UI that runs in the Tauri
webview on the host; `src-tauri/src/streamer/static/` is the dependency-free browser page the host
serves to joining devices, `include_str!`'d into the binary with no bundler step. Neither is in
this guide's territory.

The Rust core is `src-tauri/src/`, with three `cfg`-gated sibling platform modules —
`windows_utils/`, `macos_utils/`, `linux_utils/` — meant to expose the same surface, glob-imported
by `lib.rs` so the Tauri command list is identical across operating systems. **`linux_utils/` does
not compile**, implements only part of the command set, and several of its stubs report success
rather than "unsupported"; Linux is declared out of scope and no guide covers it.

### 4.2 The Windows map, in the order the pixels move

```
windows_utils/
  virtual_display.rs        the IDD virtual display; probe_display_backend() ->
                            IddDriver | DriverNotInstalled | DriverNotResponding;
                            QueryDisplayConfig name correlation; the DPR mode ladder
  driver_ipc/               the named-pipe client to the driver's user-mode host
                            (client.rs, driver_client.rs, sync/, core.rs, mock.rs)
  streamer/
    capture.rs              monitor enumeration, DPI awareness, ChangeDisplaySettingsExW,
                            the DPI get/set private DisplayConfig calls, probe_to_png
    dxgi/mod.rs             the DXGI Desktop Duplication engine  <- the big one
    dxgi/cursor.rs          GPU cursor sprite build + the quad renderer
    pipeline.rs             backend selection and the live capture loop   [Tier C]
    tuning.rs               process/thread priority, MMCSS, CPU-set confinement
    scaler.rs               VideoProcessorBlt downscale + NV12 conversion  [Tier C]
    clock.rs                QPC <-> host-clock conversion for DXGI present times
    {nvidia,intel,amd,x264}/  the four encoders                            [Tier C]
  windows_capture/          the vendored windows-capture fork:
    graphics_capture_api.rs   WgcFeatures, the property probes, the frame-arrived handler
    d3d11.rs                  AdapterInfo, GpuPreference, EnumAdapterByGpuPreference
    monitor.rs, frame.rs, settings.rs, capture.rs, encoder.rs, winrt.rs
    FORK.md                   what diverges from upstream, and why
  audio/
    loopback.rs             the endpoint-loopback capture thread and the tier ladder
    process_loopback.rs     tier 2: ActivateAudioInterfaceAsync on the synthetic device
    silence.rs              the mandatory silent render companion + the period request
    engine.rs               GetSharedModeEnginePeriod / GetDevicePeriod
    format.rs               mix-format parsing, downmix (ITU BS.775), int->float
    device.rs               IMMNotificationClient, default-endpoint lookup
    guards.rs               RAII COM / event / CoTaskMem wrappers
    test/                   format.rs, opus_layout.rs
  hosted_network.rs         the four-tier AP ladder + the WLAN capability/status probes
  networking.rs             adapter enumeration (WMI), hosted_network_ipv4(), link medium
  compatibility.rs          check_system_requirements -> CompatibilityReport
  device_reporter.rs, mod.rs
```

Cross-platform pieces you will *read* but not change: `streamer/telemetry.rs` (the histograms and
counters), `streamer/platform.rs` (the runtime dispatcher), `streamer/audio/mod.rs`
(`AudioDiagnostics`, `host_now_ns`, the AudioHub), `streamer/server.rs` (the axum server and
`DISPLAY_CORRELATION_LOCK`).

### 4.3 The measurement harness

The developer probe harness is reached as `ScreenExtend probe`. It is **not** declared in
`tauri.conf.json` — `cli::dispatch` intercepts `probe` before the plugin router because the harness
has ~25 hand-parsed tuning flags (`streamer::config::Config::from_args`). It runs inside the Tauri
app on purpose, because `--live` and `--whep-selftest` need a real virtual display and therefore
`platform::setup`.

```powershell
cd src-tauri

# whole host path: a real WHEP join over loopback against a virtual display created for the run,
# per-stage telemetry printed on exit. Needs --features probe (it carries a TLS verifier that
# accepts any certificate, so it is not in a normal build).
cargo run --release --features probe -- probe --whep-selftest --duration 30 `
    --encoder <auto|nvidia|intel|amd|software> --capture-backend <wgc|dxgi|auto>

# real content through the real capture path, to an Annex-B file
cargo run --release --features probe -- probe --live out.h264 --duration 30 `
    --capture-backend <wgc|dxgi> --workload <mixed|static|scroll|motion|drag>

# glass-to-glass overlay: a large frame number + elapsed-ms clock painted into the encoded frames
cargo run --release --features probe -- probe --live --duration 60 --synthetic-pattern `
    --workload motion --counter

# single frames and smoke tests
cargo run --release --features probe -- probe --capture cap.png     # WGC -> PNG
cargo run --release --features probe -- probe --dxgi dxgi.bmp       # duplication -> BMP
cargo run --release --features probe -- probe --encode out.h264     # 300 synthetic frames
cargo run --release --features probe -- probe --bitrate             # adaptive-bitrate path

# system audio: packet rate, subscriber inter-arrival, capture->subscriber latency,
# the signed skew of capture_ns against the host clock, and the AudioDiagnostics summary
cargo run --release --features probe -- probe --audio --duration 15
cargo run --release --features probe -- probe --audio --process-loopback --duration 15
```

From a running host:

```powershell
cargo run --release -- serve -v      # headless host; one telemetry line per session every 5 s
cargo run --release -- status --json # the same counters, per session, from another shell
cargo run --release -- doctor --json # the capability report + per-probe timings
cargo run --release -- logs --lines 400
cargo run --release -- network status | network start <ssid> <pass> | network stop
```

**What a correct reading looks like.** The `serve -v` telemetry line has the shape

```
telemetry: capture=wgc[<feature summary>] encoder=... frames=... <stage>=p50/p90/p99/max ... | <counters>
```

Stages you care about: `capture_delivery` (the compositor's own timestamp to the first instruction
of the frame handler), `scale`, `encode_queue`, `encode`, `packetize`, `socket_write`,
`capture_to_encode`, `capture_to_socket`. Counters (only non-zero ones are printed, so an absent
counter is zero): `frames_repeated`, `frames_dropped_stale` (WGC pool drain discards),
`frames_dropped_ring` (DXGI staging-ring overruns), `frames_dropped_capture` (DXGI
`AccumulatedFrames` past the first — presents the OS coalesced away), `frames_dropped_broadcast`,
`idrs_sent`. `capture_features` carries the WGC feature summary string.

The audio diagnostics line:

```
audio diag: fmt=<rate>Hz x<ch> period=<n>fr buffer=<n>fr, encode=<hist>ms, wake=<hist>ms,
            capture->packet=<hist>ms (p50/p90/p99/max), discontinuity=, silent=, dev_changes=,
            companion_restarts=, starvations=, dc_drops=, lagged=, ts_clamped=
```

### 4.4 Ledgers, findings and gates

**`docs/OPTIMIZATIONS.md`** — one table row per change attempted:
`| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |`. Failures are
recorded on purpose; a reverted optimisation with a number is valuable information. Status
vocabulary:

- **`VERIFIED`** — executed and measured on a machine, with numbers in the row.
- **`COMPILE-CHECKED`** — builds and passes static/layout tests, never executed.
- **`UNVERIFIED`** — written from research, no runtime signal at all.

**Your job is to move this guide's rows to `VERIFIED` with numbers, or revert them with reasons.**

**`docs/CAPABILITY-TIERS.md`** — one row per subsystem × tier:
`| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |`.

**`docs/BASELINE.md`** — the pre-change numbers. You are adding a whole new machine class to it;
give it its own clearly-labelled section rather than overwriting the 26200 rows.

**`findings/`** at the repo root (excluded from git via `.git/info/exclude`) is the work queue and
the crash-recovery point. **Every problem, opportunity, suspicion and idea becomes one file**, named
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

One finding per file — batched files cannot be individually completed. **A file is deleted the
moment its finding is resolved** — implemented and verified, or deliberately rejected with the
reasoning recorded in `docs/OPTIMIZATIONS.md` *first*. Deleting is how you mark it done; do not
archive, do not move to `done/`. The lifecycle is empty → very full → empty, and a non-empty
`findings/` at the end means the work is not done. Write the file when you find the thing, not at
the end of a session: the value of the mechanism is entirely in it being accurate at any arbitrary
moment you might be cut off. Hand the relevant files to subagents as part of their briefs and have
them update or delete their own.

**Gates, before every commit**, from `src-tauri/`:

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

From the repo root, only if `src/` or `src/lib/bindings.ts` changed (it should not):

```powershell
pnpm lint ; pnpm build ; node scripts/check-legacy-webkit.mjs
node scripts/check-bindings.mjs ; node scripts/check-versions.mjs
```

Run the client-page harnesses once before your first commit, so a red result is never attributed to
you later — nothing in this guide touches the client page:

```powershell
pnpm test:client        # scripts/test-client.mjs, seven harnesses
```

**Skipping convention.** A hardware- or OS-gated test never returns silently: it calls
`crate::test_support::skip(what, reason)` or `skip_because(what, reason)` (re-exported into the
Windows test modules as `windows_utils::streamer::test::skip`), which prints `SKIP <what>: <reason>`
on stderr, and **`SE_REQUIRE_HW=1` turns a skip into a failure**. This machine is exactly where that
matters: **set `SE_REQUIRE_HW=1`** for every capability it is supposed to have — DXGI duplication
above all — because a green tick over a test that returned at its first line is worse here than
anywhere else in the project. Anything you add follows the same pattern, with
`#[ignore = "<reason>"]` so the harness prints the reason rather than a bare `ignored`.

CI is disabled in this repo — `.github/workflows/ci.yml` was removed and only `build-release.yml`
remains — so **nothing runs these automatically.** You run them locally, every time.

---

## 5. Current state of this area

### 5.1 The ledger rows this guide owns
**A note on the `W…` labels you will meet in the ledgers, in the risk register and in this file.**
The engagement that produced this repository's current state ran as numbered parallel workstreams:
`W1` transport · `W3a`/`W3b` Windows capture and encode · `W4`/`W4b` AMD · `W5` macOS capture and
encode · `W6` macOS permissions · `W7` macOS audio · `W8` Windows audio · `W9` Windows platform ·
`W10` input · `W11` the client page · `W12` the desktop UI · `W13a`/`W13b` build, CI and hygiene ·
`W14` AirPlay · `W15` tests. They are **provenance labels only** — they tell you which change series
a row came from so a ledger note reads in context. They are not people, none of them is still
running, nothing is waiting on one, and **every one of them has merged into `latency-overhaul`.**


These are the shipped changes whose status this run must move. Quoted so you do not have to
reconstruct them.

| Row | Area | What landed | Current status |
|---|---|---|---|
| **95** | capture-win (wgc) | `MinUpdateInterval`, `DirtyRegionMode` and `IsBorderRequired` are each probed with `ApiInformation::IsPropertyPresent` and the setter skipped when absent, instead of `GraphicsCaptureApi::new` failing on the first missing one. The feature set is logged and carried in telemetry as `capture_features`. | **VERIFIED on 26200 (no regression); the pre-26100 half is COMPILE-CHECKED.** "The change here with the largest population behind it and the least ability to measure it locally." |
| **96** | capture-win (wgc) | Frame-pool depth is per-path — 2 for the handler that encodes inline, 3 for the ring that only stages — and `TryGetNextFrame` drains to the **newest** frame, counting discards into `frames_dropped_stale`. `Recreate` had passed buffer count 1 while `Create` passed 3. | VERIFIED (counter moves under load on 26200) |
| **97** | capture-win (wgc) | A keyframe request always produces an IDR within one frame period, including on a static desktop. | COMPILE-CHECKED as to the request path |
| **98** | capture-win (wgc) | The WGC D3D device is created on the encoder's adapter: `IDXGIFactory6::EnumAdapterByGpuPreference` (probed by `QueryInterface`) plus `VendorId`. Which GPU captures, which encodes, and whether they match is logged once per session. | VERIFIED (`same_adapter=true` on the dev box for NVENC and x264) |
| **99** | tuning-win | Capture and encode threads confined to the highest-`EfficiencyClass` CPU sets (`GetSystemCpuSetInformation` + `SetThreadSelectedCpuSetMasks`), probed by "does this machine report more than one distinct efficiency class". | VERIFIED (the dev box is hybrid Intel and takes the branch). **The negative branch is unexercised.** |
| **101** | capture-win (dxgi) | The cursor is composited **incrementally**: only the rectangle it previously occupied is restored and only the rectangle it now occupies is drawn, instead of recopying the whole desktop texture on every pointer update (8.29 MB at 1080p for a ≤32×32 change). | VERIFIED (dev box; cursor correctness by eye) |
| **102** | capture-win (wgc) | `DirtyRegionSettings::ReportOnly` (probed) plus a skip when `Frame::dirty_regions()` comes back empty, capped at 5 consecutive skips. | VERIFIED on 26200; **no effect pre-24H2, where the property is absent** |
| **103** | capture-win (dxgi) | `AcquireNextFrame`/`ReleaseFrame` paired by an RAII guard on every exit path, and `DXGI_ERROR_INVALID_CALL` recovered by re-duplicating (capped at 20) instead of killing the capture thread. | VERIFIED (the invalid-call death no longer reproduces) |
| **104** | capture-win (dxgi) | `IDXGIOutput5::DuplicateOutput1` with an explicit format list tried before `IDXGIOutput1::DuplicateOutput`, probed by `QueryInterface`; **every** adapter that enumerates the display is tried, not just the first. The compatibility report's `dxgi` probe creates a duplication and drops it. | VERIFIED, **and the answer on the dev box is negative** — that machine is WGC-only |
| **105** | capture-win (dxgi) | Duplication re-established without ever producing a frame fails the capture after 40 attempts; the "re-established" line is rate-limited to the first and every tenth. | VERIFIED (reproduced the spin, then the bound) |
| **106** | capture-win (dxgi) | The DXGI capture thread stages the composed frame into one of **three slot textures** on the duplication device and hands the index to a `dxgi-encode` thread instead of encoding inline. Overruns counted in `frames_dropped_ring`; the per-60-frame line prints the frame budget beside the measured average. | **COMPILE-CHECKED — the throughput claim has no number anywhere.** This is the single biggest measurement gap in the guide. |
| **107** | capture-win (dxgi) | No video processor on the composing adapter means the encoder is configured at native resolution and the client scales, with a log line, instead of the session failing. | VERIFIED that the dev box's duplication devices *do* expose a video processor, so **the fallback branch is COMPILE-CHECKED** |
| **212** | capture (Windows) | DXGI `AccumulatedFrames` past the first counted into `frames_dropped_capture`; `ProtectedContentMaskedOut` reported once per capture. | VERIFIED (counter reachable) |
| **196** | vdisplay (Windows) | `display_device_name` correlates a new display to its GDI device name with `QueryDisplayConfig` instead of the 100 ms `EnumDisplayDevices` diff poll. | COMPILE-CHECKED + unit; **UNVERIFIED against a full ladder / multi-display run** |
| **197** | net (Windows) | The legacy hosted-network capability is `WlanQueryInterface(wlan_intf_opcode_hosted_network_capable)` and its status is `WlanHostedNetworkQueryStatus`, not `netsh` output. | VERIFIED on 26200 as `Some(false)` — **the negative case only**; the positive case and every non-English locale are unmeasured |
| **198** | vdisplay (Windows) | `DriverClient` resynchronises with `request_state()` when the event stream reports a gap; a `.v2` pipe name is preferred at connect. | VERIFIED by unit test; **neither half reachable on real hardware here** |
| **199** | vdisplay (Windows) | Driver round trips serialised by their own lock; the state mutex taken only for the field accesses. | COMPILE-CHECKED; **UNVERIFIED — needs a deliberately wedged driver** |
| **22** | compat-win | `CompatibilityReport` gains `capture_backend`, `encoder_backend`, `hosted_network_backend`; `audio_backend` and `display_backend` probed rather than hardcoded; `os_supported` is a capture-backend probe rather than `CurrentBuild >= 19041`. | VERIFIED (positive paths); **UNVERIFIED — the `DriverNotInstalled` and software-adapter branches** |
| **21** | net-win | Hosted network became a probed ladder — Mobile Hotspot (6 GHz > 5 GHz > auto) → Wi-Fi Direct legacy AP → `netsh` legacy → unsupported — and "started" requires the AP's virtual adapter to hold a routable IPv4. | VERIFIED (capability probe and band support on the dev box); **UNVERIFIED (throughput and RTT per band — needs a second device and `iperf3`)** |
| **68** | audio-win | The silent render companion negotiates the engine period via `IAudioClient3::GetSharedModeEnginePeriod` + `InitializeSharedAudioStream(min)`. | **VERIFIED as a correctly-detected no-op** on the dev box; **the request branch is COMPILE-CHECKED** |
| **67** | audio-win | `capture_ns` anchored to `min(qpcpos, now − read_duration)` — the play-cursor clamp. | VERIFIED on 26200 (`probe --audio`, 3 runs, idle); **not verified with a client that actually has audio on** |
| **19** | audio-win | WASAPI packets stamped with `GetBuffer`'s `qpcpos` converted to the host clock, not `host_now_ns()` at encode time. | COMPILE-CHECKED |
| **73** | audio (all) | Opus `OPUS_SET_VBR_CONSTRAINT` 0 → 1; DTX and in-band FEC removed; encoder state read back. | VERIFIED in force (read back); **the packet-size distribution is UNVERIFIED** — an idle host encodes 3-byte silence frames |
| **235** | encode / intel | `MFX_WRN_DEVICE_BUSY` retries changed from `sleep(1ms)` to spin → yield → 200 µs sleeps under a 50 ms wall-clock budget, with a log line. | COMPILE-CHECKED (the busy path never triggered) |
| **237** | encode / intel | `libvpl.dll` deliberately **not** bundled; the dispatcher search order is OS path → exe directory → `resources/`. | VERIFIED on the dev box (`C:\Windows\System32\libvpl.dll`, oneVPL Dispatcher 2.7.0, WHQL-signed); **untested on a clean Intel install** |
| **277** | encode | The per-frame changed-region hint evaluated and **not** consumed. | VERIFIED that the primary consumer (x264's CPU colour convert) no longer exists; the rest is open |

### 5.2 The capability-tier rows this guide owns

`docs/CAPABILITY-TIERS.md` carries these; each has a status line you will be editing.

- **Screen capture (Windows)** — WGC → DXGI Desktop Duplication → none. Probe:
  `GraphicsCaptureSession::IsSupported()` for tier 1 (the `UniversalApiContract 8` conjunct was
  removed — it could only produce false negatives); tier 2 is a real `duplicate_once` on the first
  desktop-attached output, created and dropped. Status: VERIFIED on 26200, **with the DXGI fallback
  reported unavailable there**.
- **DXGI duplication entry point** — `DuplicateOutput1` → `DuplicateOutput`. Status: VERIFIED as
  **neither available** on 26200.
- **DXGI downscale (Windows)** — `VideoProcessorBlt` on the composing adapter → encode at native and
  let the client scale. Tier 2 is COMPILE-CHECKED.
- **Virtual display (Windows)** — `IddDriver` → `DriverNotInstalled` → `DriverNotResponding`. The
  two failure tiers are UNVERIFIED.
- **Virtual-display name correlation (Windows)** — `QueryDisplayConfig` → `EnumDisplayDevices` diff.
  COMPILE-CHECKED + unit; UNVERIFIED against a full ladder.
- **Driver IPC protocol (Windows)** — v2 pipe → v1 pipe. VERIFIED by unit only.
- **System audio capture (Windows)** — endpoint loopback → process loopback. Both tiers exercised on
  26200 and measured **equal**: wake p99 9.44 ms (tier 1) vs 10.49 ms (tier 2); capture→packet p50
  5.77 vs 6.29 ms. **The "refuses on Win 10 19045" half is UNVERIFIED.**
- **Engine period (Windows audio)** — low-latency `InitializeSharedAudioStream` at the reported
  minimum → default. VERIFIED as a no-op; the request branch COMPILE-CHECKED.
- **Audio format conversion (Windows)** — endpoint already 48 kHz f32 stereo → engine converts
  (`AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`) → userspace downmix/int-convert. **Tiers 2 and 3 were
  unreachable in production until now.**
- **System audio (Windows)** — WASAPI loopback → none. The "none" branch is unexercised.
- **Hosted network (Windows)** — Mobile Hotspot → Wi-Fi Direct legacy AP → `netsh` legacy →
  unsupported. VERIFIED on 26200 down to tier 1 only.
- **Legacy hosted-network capability (Windows)** — `WlanQueryInterface(hosted_network_capable)` →
  `netsh wlan show drivers` string match. VERIFIED as the **negative** answer only.
- **oneVPL dispatcher (Windows)** — OS search path → exe directory → `resources/` (deliberately
  empty). VERIFIED on the dev box only.
- **Quick Sync slice layout (Windows)** — `MaxSliceSize` → the `NumSlice` height heuristic.
  VERIFIED on Intel UHD with `LowPower = ON`: 30 frames at 1280×720 gave 151 slices, p50 1191 B,
  **max 11876 B** — the cap is best-effort on the IDR.

### 5.3 The numbers to beat — the 26200 dev box

All from `docs/BASELINE.md` and the W3a after-matrix. Release build, `--features probe`, real
capture of a virtual display created for the run at **1280×720@60**, WHEP over loopback, 30 s.
`capture → socket`, milliseconds, **p50 / p90 / p99 / max**.

| Encoder | Path selected | real frames / 30 s | capture → socket |
|---|---|---|---|
| NVENC ring, before | `nvenc-zero-copy-ring` | 67 | 1.57 / 8.39 / 16.78 / 18.00 |
| NVENC ring, after | `nvenc-zero-copy-ring` | **206** (206, 206, 206) | 1.84–2.36 / 3.67–7.34 / 6.82–37.75 / 7.08–63.19 |
| NVENC ring, after, quietest pass | | 206 | **1.84 / 3.93 / 6.82 / 7.08** |
| x264, before | `software-x264` | 81 | 2.36 / 5.24 / 23.07 / 24.82 |
| x264, after | `software-x264` | 159–450 | 4.19–8.39 / 7.34–12.58 / 9.44–27.26 / 11.05–109.67 |
| Quick Sync, before | `intel-cpu-bridge` | 79 | 11.53 / 16.78 / 25.17 / 26.05 |
| Quick Sync, after | **`intel-same-adapter`** | 126–357 | 8.39–11.53 / 13.63–15.73 / 16.78–20.97 / 21.17–33.94 |
| any, DXGI | — | 0 | **unmeasurable** |

Per-stage, `selftest` static, NVENC/WGC: `capture_delivery` **0.98 / 4.19 / 6.82 / 6.95**,
`encode` 0.00 / 1.18 / 1.44 / 15.09. On the `live` 1920×1080 path: `capture_delivery`
**6.29 / 9.44 / 20.97 / 22.54**, `capture → encode-out` **12.58 / 15.73 / 27.26 / 28.45**.

Other pinned numbers you will compare against:

- **CPU-bridge readback**, 1920×1080 → 1280×720, 60 iterations, median µs/frame:
  `blt 5, readback 1192, total 1196` in BGRA; `blt 4, readback 740, total 743` in NV12 (−38 %).
- **`frames_dropped_stale`** on the NVENC ring: **5, 0 and 3** across three 30 s trials, against
  321–421 delivered frames. There is no queue for a shallower pool to shorten.
- **`frames_dropped_broadcast`**: zero in all twelve runs of the after-matrix.
- **Virtual-display mode changes** (median of 3, 26200, bundled IDD): in-ladder, resolution
  unchanged **111 ms**; in-ladder, new resolution **308 ms**; monitor departure alone **179 ms**;
  remove + recreate + reattach **491 ms**; then set the new mode **281 ms**. A geometry outside the
  pre-registered ladder costs **~772 ms** plus a desktop-wide window reflow.
- **Audio**: engine periods `default = fundamental = min = max = 480 frames (10.000 ms)`; wake
  interval p50/p90/p99 **9.44 ms**, max 10.3–10.6 ms. Capture → subscriber
  **9.44 / 9.44 / 9.44–10.49 / 10.9 ms**. `capture_ns` skew after the clamp: min −3.04 / mean +7.69 /
  max +10.9 ms, ≤ 1 of 1601 packets ahead of the host clock. Opus read-back:
  `application=2051 bitrate=128000 complexity=5 signal=3002 vbr=1 vbr_constraint=1 lsb_depth=24
  prediction_disabled=0 lookahead=120 samples (2.50 ms)`.
- **NVENC concurrent sessions**: **8** opened on the RTX 3080 Ti, the 9th refused, the eight already
  open unaffected.
- **x264 standalone**: 120 frames 1920×1080@60, p95 encode 4537 µs against a 16666 µs budget.

### 5.4 Competitor targets

Nothing below was run on this project's hardware; each is the best independently measured figure
available, or the vendor's own where nothing better exists. Glass-to-glass, milliseconds:

| Product | ms | Basis |
|---|---|---|
| Parsec | 4–8 LAN | vendor |
| Steam Link | 13–16 at <1 ms ping | user reports |
| **Luna Display (LIQUID 3.0)** | **11.3 USB / 22.4 Wi-Fi**, 50 samples | vendor bench |
| Moonlight / GameStream | 20–35 on a good LAN | community |
| Apple Sidecar | "consistently below 30" wired | reported |
| **Hopp (browser WebRTC)** | **98** at 1080p after tuning libwebrtc, from 159 | published |
| Miracast / Wi-Fi Display | 80–100 optimised; spec ceiling 250 | certification spec |

Luna Display is the target (same job — a tablet as a second display over Wi-Fi). Hopp is the honest
ceiling for a browser client. ScreenExtend's host half on 26200 is **3.93 ms at p90** on the fast
path, which leaves most of a Luna-class budget for the network and the client. **The question this
guide answers is what that number is on a Windows 10 hybrid laptop, on both capture backends.**

### 5.5 What has already been tried and rejected here — do not re-try these blindly

Each of these was considered on this exact code and rejected **with a measurement**. Re-opening one
is allowed; re-opening it without addressing the number below is not.

**`Direct3D11CaptureFramePool::CreateFreeThreaded`.** `WgcFeatures` probes it and reports
`+free-threaded-pool`, and the pool is still created with `Create`, which binds `FrameArrived` to
the calling thread's `DispatcherQueue` — so every frame costs a DWM signal, a dispatcher enqueue, a
thread wake, a `GetMessageW` return and a `DispatchMessageW`. Sunshine uses `CreateFreeThreaded`;
OBS uses `Create`. Rejected **on the measurement, not the difficulty**: the stage it would shrink is
`capture_delivery`, measured at p50 0.98 / p90 4.19 / p99 6.82 ms static and p50 6.29 / p90 9.44 /
p99 20.97 ms live, while a thread wake on a hot thread is tens of microseconds. Against that, the
handler would run on an arbitrary thread-pool thread, so the MMCSS "Pro Audio" registration, the
`TIME_CRITICAL` priority and the P-core confinement applied in `LiveCapture::new` all land on the
wrong thread; arrival *ordering* is lost; and the `WM_QUIT` shutdown handshake needs replacing.
**Revisit only if `capture_delivery` p50 ever falls to the same order as a thread wake** — and note
that the pre-26100 population is a different one, so if *your* machine's `capture_delivery` is
dominated by the pump rather than by the compositor, that is a new number and the rejection does not
bind.

**Double-buffering the CPU-bridge staging texture.** `Scaler::read_back` / `TextureReader::read_back`
do `CopyResource` into a single staging texture then a blocking `Map`. Rejected: the whole readback
is **1192 µs** BGRA / **740 µs** NV12 while the `VideoProcessorBlt` beside it is 4–5 µs, so
pipelining the map costs a full frame period (16.7 ms at 60 Hz) to remove at most 1.2 ms of stall;
`DO_NOT_WAIT` polling costs a captured frame instead.

**Removing the per-frame `Flush` in the NVENC ring stage.** Kept deliberately and without a
measurement: the destination is a **cross-device** shared resource
(`SHARED_NTHANDLE | SHARED_KEYEDMUTEX`). A keyed mutex orders access between devices but does not
force the producer's command list to be submitted; the failure mode of getting this wrong is a torn
frame under load on some drivers.

**High-resolution waitable timers and `ThreadMemoryPriority`.** No-ops here. The frame path is
event-driven end to end (DWM signals the pool; DXGI's wait is `AcquireNextFrame`'s own kernel wait,
not a sleep); the three timed waits that exist — the 200 ms keepalive, the ring encode thread's tick
and the repeater's tick — are none of them precision-sensitive. `ThreadMemoryPriority` defaults to
`MEMORY_PRIORITY_NORMAL` and nothing here lowers it. (`ThreadPowerThrottling` *is* set, for a
different reason: Windows 11 throttles threads of a minimised or occluded window, which is this app's
normal state while streaming.)

**Frame-pool depth 3 (ring) / 2 (inline).** `frames_dropped_stale` reads 5, 0 and 3 against 321–421
delivered frames, so there is no backlog for depth 2 to shorten on the ring path. Depth 3 buys
jitter tolerance that the drain then discards for free. Re-measure only if that counter climbs into
the tens per second.

**`BROADCAST_CAPACITY = 4`.** `frames_dropped_broadcast` was zero in all twelve after-matrix runs.
Raising it is the wrong direction: a video receiver four frames behind should skip to the newest
frame, not work through a backlog.

**Sub-frame / slice-mode output.** Rejected on NVENC, AMF and x264 against a measured ~2 ms ceiling.

**`libvpl.dll` deliberately not bundled** — Gen12+ drivers install it; pre-Gen12 hosts sit behind a
Media SDK 1.x runtime the version gate refuses anyway; and `intel/libvpl` publishes source-only
releases, so the only binary available to vendor is a WHQL-signed driver component, which is not a
provenance chain a signed installer can record.

**oneVPL surface import** rejected on the per-stage split. **`nal-hrd=cbr`** rejected. **Per-frame
NVENC map/unmap** measured and left alone.

**The per-frame changed-region hint evaluated and not consumed** (row 277). Both Windows capture
backends compute a dirty/move-rect union and discard it after the skip decision. The named consumers
were x264's CPU colour convert (now gone by another route — every CPU-bridge path has a scaler and
takes NV12 from the video processor), Intel's `mfxExtDirtyRect`, and AMF's ROI map. What remains
needs a `Duplicator` accessor (**which now exists — `changed_bounds()`**) plus a `ReadyMsg` field and
a coordinate scale into encoder space. **Re-open this if a measurement shows the encoders spending
time on unchanged macroblocks** — and note that on a DXGI-first host, which is what this guide runs
on, the arithmetic is different from the dev box's.

**The classic WGC path still encodes in the frame callback.** Row 106 decoupled capture from encode
on the DXGI path only. The classic (non-ring) WGC path — what an Intel, AMD or software encoder gets
— still runs the whole encode inside `on_frame_arrived`, on the WGC delivery thread, holding the
handler mutex. Left alone deliberately, because DXGI's `AcquireNextFrame`/`ReleaseFrame` is a strict
pair with no buffering (while the encode ran, the duplication was simply not being read) whereas WGC
has a frame pool in front of the callback, so a slow callback costs *stale frames* rather than
compositor stalls. **Extending the ring to the non-NVENC backends is still worth doing**, and
`DxgiRing` is the shape to lift; it was not attempted because the measurable half — the DXGI path —
could not be measured on the dev box. **On your machine it can.** This is one of the largest
opportunities in the guide (see task 3.11).

### 5.6 Known drift you will find

**One item here has been fixed since this guide was drafted; verify rather than re-fix.**
`windows_utils/compatibility.rs::probe_encoder` used to report `"x264 (no AMD hardware encoder)"`
with the comment *"AMD encode is a stub; those hosts run the CPU encoder"*, which stopped being true
the day the AMF backend landed. It now classifies adapters by **`Vendor::from_pci_id`** (keeping
`select_vendor` only for an id we do not know, because description strings are localized,
OEM-rebranded, and WARP's contains no vendor name at all while reporting Microsoft's `0x1414`), and
its AMD arm calls `amd::encoder::probe_availability`, which loads the runtime, takes a context on
the AMD adapter, creates the AVC component and reads `GetCaps` — reporting `amf`, `amf (cpu bridge)`
or `x264 (AMF unavailable: <reason>)`. Ledger row 298. **What you verify** is that on *your*
hardware the string is honest: run `ScreenExtend doctor --json` and check `encoder_backend` against
what the pipeline actually selects in a live session, and look at the desktop UI's compatibility
card by eye (**appearance unchanged** — only the string it displays changes). If your box has no AMD
adapter the string must simply not claim one. Note that the AMF backend itself is still
`UNVERIFIED`; `guides/windows-amd.md` owns proving it on a Radeon.

The `probe --help` text in `cli.rs` lists `--encoder <auto|nvidia|intel|software>` and omits `amd`,
which `Config::from_args` does accept (`"amd" | "amf" | "radeon" | "vce" | "vcn"`). `cli.rs` is Tier
C — file a finding rather than editing it.

---

## 6. Phase 1 — read, then discover what this machine actually exposes

### 6.1 Read in full, before changing anything

Spawn Opus 5 subagents and partition so that every file below is read **in full** by at least one
agent — not skimmed, not grepped. Each agent reports: every function with a one-line purpose, every
thread spawned, every lock taken, every buffer copy, every FFI boundary, every `unwrap`/`expect`
that can panic, every silent fallback, every `TODO`/`FIXME`/`HACK`, and every place a fast path can
degrade to a slow path without telling anyone.

| Agent | Territory |
|---|---|
| R1 | `windows_utils/windows_capture/**` including `FORK.md`, plus `windows_utils/streamer/capture.rs` |
| R2 | `windows_utils/streamer/dxgi/mod.rs` and `dxgi/cursor.rs`, line by line |
| R3 | `windows_utils/audio/**` including `test/` |
| R4 | `windows_utils/hosted_network.rs`, `networking.rs`, `compatibility.rs` |
| R5 | `windows_utils/virtual_display.rs`, `driver_ipc/**` (read-only unless a defect appears) |
| R6 | `windows_utils/streamer/{pipeline.rs,tuning.rs,scaler.rs,clock.rs}` — read-only context: how the capture backends are chosen and consumed |
| R7 | `streamer/telemetry.rs`, `streamer/platform.rs`, `streamer/audio/mod.rs`, `streamer/server.rs` — read-only: the harness and the counters |
| R8 | `docs/manual-tests/windows-capture.md`, `windows-audio-tiers.md`, `core-closeout.md`; `docs/TELEMETRY.md`; `docs/research/windows-capture-encode.md`, `audio-capture-tiers.md`, `virtual-display-and-hosted-network.md` |

`docs/research/*` already contains a completed research pass. **Read it; do not redo it.** Phase 4
below is the *new* research this guide needs, not a repeat.

### 6.2 Discover the hardware, and write it down

Write `docs/discovery/windows-legacy-machine.md` containing the full preflight output plus:

```powershell
# WGC feature set, straight from the app
cargo run --release -- serve -v | Tee-Object -FilePath wgc-discovery.log
# ...join from the phone, then:
Select-String -Path wgc-discovery.log -Pattern 'wgc: feature set|adapters:|live capture ready|falling back to DXGI'

# Every probed backend
cargo test --release --lib report_backends -- --ignored --nocapture

# Display backend, before and after installing the IDD driver
cargo test --release --lib report_display_backend -- --ignored --nocapture

# WLAN capability, locale-independent, next to the string it replaces
cargo test --release --lib report_wlan_hosted_network_capability -- --ignored --nocapture
netsh wlan show drivers
netsh wlan show hostednetwork

# Tethering (Mobile Hotspot) capability
cargo test --release --lib report_tethering_capability -- --ignored --nocapture

# Audio endpoint reality
cargo run --release --features probe -- probe --audio --duration 15
```

The expected WGC feature summary per build:

| Host | Build | Expected `wgc: feature set` |
|---|---|---|
| Windows 10 22H2 | 19045 | `+cursor -border -secondary-windows -min-update-interval -dirty-region +free-threaded-pool (pre-26100)` |
| Windows 11 21H2 / 22H2 / 23H2 | 22000 / 22621 / 22631 | `+cursor +border +secondary-windows -min-update-interval -dirty-region +free-threaded-pool (pre-26100)` |
| Windows 11 24H2+ | 26100+ | every flag `+`, `(build 26100+)` |

Property → build, from `WgcFeatures`: `IsCursorCaptureEnabled` 19041 · `IsBorderRequired` 20348 ·
`IncludeSecondaryWindows` 22000 · `MinUpdateInterval` 26100 · `DirtyRegionMode` 26100 ·
`Direct3D11CaptureFramePool::CreateFreeThreaded` 17763. `+free-threaded-pool` should read `+` on
every machine in that table; a `-` there is a finding.

Every discrepancy between the table and what the machine prints becomes a `findings/` file
immediately.

---

## 7. Phase 2 — baseline on this hardware, before you change anything

You are adding a machine class that `docs/BASELINE.md` has never had. Fill the matrix below and
write it into `docs/BASELINE.md` under a new heading naming the exact build and hardware. Three
trials per cell, median reported with the spread, 15 s between runs, quiet machine (state the
concurrent `rustc` count).

**Matrix A — `selftest`, the whole host path**, 1280×720@60 against a virtual display created for
the run:

| Encoder | Capture | frames (repeats) | RTP/s | capture delivery | scale | encode queue | encode | packetize | socket write | capture → socket |
|---|---|---|---|---|---|---|---|---|---|---|
| auto (whatever the box picks) | **wgc** | | | | | | | | | |
| auto | **dxgi** | | | | | | | | | |
| nvidia *(if present)* | wgc / dxgi | | | | | | | | | |
| intel | wgc / dxgi | | | | | | | | | |
| software (x264) | wgc / dxgi | | | | | | | | | |

**The `wgc` vs `dxgi` pair on the same machine is the single most valuable comparison in this
guide.** It is the only place in the project where both Windows capture backends can be measured
against each other on identical hardware, and it is what tells you whether row 95 (keeping pre-26100
hosts on WGC) was worth the population it affects.

**Matrix B — `live`, real content at the display's native resolution**, per workload:

| Workload | Encoder | Capture | frames | capture delivery | scale | encode | capture → encode-out |
|---|---|---|---|---|---|---|---|
| static / scroll / motion / drag | auto | wgc | | | | | |
| static / scroll / motion / drag | auto | dxgi | | | | | |

**Matrix C — audio**, before any change:

| Configuration | period (fr) | wake p50/p90/p99/max | capture→packet p50/p90/p99/max | skew min/mean/max | discontinuity / silent / starvations / dc_drops / ts_clamped |
|---|---|---|---|---|---|
| OEM driver, endpoint loopback | | | | | |
| OEM driver, `--process-loopback` | | | | | |
| in-box HD Audio driver, endpoint loopback | | | | | |
| the non-48 kHz endpoint as default | | | | | |

Also record, for every run: the `wgc: feature set` line, the `adapters:` line, the
`pipeline: live capture ready -- …` path line, and — on DXGI runs — the
`dxgi: duplication ready on … via … on …` line and the per-60-frame
`encode-path latency: path=dxgi+<backend>, avg_ms=…, max_ms=…, frames=…, budget_ms=…` line.

---

## 8. Phase 3 — the work

Twenty-one items. Each names what to verify, what to fix, how to measure, and what *done* looks
like. **No item may be silently absent from the final report**: each ends `VERIFIED (numbers)`,
`reverted (reason)` or `still UNVERIFIED (what was missing)`.

Suggested Opus 5 agent partition — five workstreams, run in parallel except where noted:

| Workstream | Items | Notes |
|---|---|---|
| **W-A — WGC tiering** | 3.1, 3.2, 3.19, 3.20, 3.21 | can start immediately |
| **W-B — DXGI** | 3.3 – 3.6, 3.11 | the largest; give it two agents |
| **W-C — audio** | 3.12 – 3.17 | needs the driver swap and a reboot; sequence carefully |
| **W-D — network** | 3.9, 3.10 | needs the locale change and a reboot; sequence after W-C's driver swap or on the second machine |
| **W-E — display & driver** | 3.7, 3.8, 3.18 | 3.7's `DriverNotInstalled` reading must be taken **before** anything else installs the driver |

Serialise anything that needs a reboot, a driver swap or a locale change; parallelise everything
else. Only one release build at a time.

---

### 3.1 — L-01. WGC below build 26100: the absent-property skip

**Files:** `windows_utils/windows_capture/graphics_capture_api.rs` (`WgcFeatures`,
`session_property`, `wgc_features`, the five guarded setter blocks),
`windows_utils/streamer/capture.rs`. **Rows:** 95, 96, 102. **Risk:** R-227.

**Verify.** Start the host and join. **PASS** is these two lines, in this order:

```
pipeline: starting live monitor capture (WGC): display=..., name=..., gpu=..., native=WxH, encode=WxH, downscale=..., fps=..., bitrate_bps=...
wgc: feature set <flags> (pre-26100); optional properties this build lacks are skipped, not fatal
```

with the flags matching §6.2's table for your build. **FAIL** is either of

```
WGC capture failed for <device>: <error>; falling back to DXGI Desktop Duplication
pipeline: starting DXGI duplication capture: device=..., native=..., ...
```

That fallback is the exact regression row 95 removed. Paste the `<error>` verbatim: it names which
property or call refused.

| Symptom | Meaning |
|---|---|
| Fallback line, error names a property | A property is still requested unconditionally instead of behind `IsPropertyPresent`. |
| Fallback line, error from `IsSupported` | WGC is genuinely off here (Server core, some VM graphics stacks). Not a tiering bug. |
| Neither `pipeline: starting` line | The session never started — a join or virtual-display failure, not a capture one. |
| Both lines, in that order | WGC started and then died mid-session. Note how long it survived. |
| A `-` for a property your build *should* have | The probe is too strict; a capability is being left on the table. |
| A `+` for a property your build should *not* have | Worse. `IsPropertyPresent` lied, or the summary is not derived from the same probe the session uses. Report immediately. |
| `(build 26100+)` on 19045 or 22631 | The tier name and the probe disagree. |

One caveat on `border`: the pipeline asks for `DrawBorderSettings::WithoutBorder`, but suppressing
the yellow capture border **also** needs the user to have granted
`GraphicsCaptureAccess.RequestAccessAsync(Borderless)`. Without that consent the setter succeeds and
the border stays. `+border` means the property exists, not that the border is gone. If you see a
border on the client, that is this — and it is worth deciding, with a measurement of how often it
matters, whether the app should be requesting that consent. It currently does not. That is a real
opportunity: file it, research the API's UX cost, and if the consent prompt is acceptable,
**implement it** (`windows_capture/` is Tier A).

**Fix and optimise.** Anything that still hard-fails on an absent property. Then go further: with
`MinUpdateInterval` and `DirtyRegionMode` absent, the pre-26100 host gets *every* frame the
compositor produces and re-encodes frames that are byte-identical to their predecessor — row 102's
skip is inert here because `Frame::dirty_regions()` has nothing behind it. **That is the single
biggest pre-26100-specific inefficiency in the capture path.** Research and implement a
build-independent change detector: candidates are a cheap GPU-side hash or a downsampled compare
pass on the captured texture, `IDXGIOutputDuplication`'s metadata via a *parallel* duplication
(almost certainly wrong — it competes for the same resource), or accepting the cost and instead
tightening what the encoder does with an unchanged frame. Measure bytes-per-second and `encode`
percentiles with and without whatever you build, on `--workload static` and `--workload scroll`.
A change detector that costs more than the encode it saves is a recorded rejection with numbers.

**Done:** row 95's `COMPILE-CHECKED` half becomes `VERIFIED` with the feature line quoted per
machine, and the *Screen capture (Windows)* tier row names both builds.

### 3.2 — L-19. Cursor correctness on both backends, at several DPRs and both orientations

**Files:** `windows_utils/streamer/dxgi/{mod.rs,cursor.rs}`, `windows_utils/streamer/capture.rs`.
**Row:** 101. **Manual test:** `windows-capture.md` §6.

The DXGI path composites the cursor itself and does so **incrementally** — it restores only the
rectangle the cursor previously occupied (`drawn_rect`) and draws only the one it now occupies —
which is what turned an 8.29 MB full-frame `CopyResource` per pointer update into two small blits.
Two coordinate systems are involved and confusing them is the bug class: `pointer_pos` is DXGI's
report in **desktop** coordinates (used only to decide whether the pointer moved) and `draw_pos` is
the sprite's top-left in **composite** coordinates (output-relative, hotspot subtracted).

**Verify, on WGC first** with a live session: move the mouse across the extended display slowly,
then fast, then along each of the four edges, then in and out of the corners. On the *client's*
screen assert: the cursor is visible; it leaves **no trail**; there is exactly **one** cursor, not a
duplicate a few pixels off; it does not stick, tear or vanish crossing an edge or leaving the
display.

Then DXGI, via the probe:

```powershell
cargo run --release --features probe -- probe --live --capture-backend dxgi --duration 60 dxgi_cursor.h264
ffplay dxgi_cursor.h264
```

Repeat the whole thing at DPR **1.0, 1.25, 1.5, 2.0** and in **both orientations** (landscape and
portrait — the rotated path goes through `blit_verts` and the `QuadRenderer` rather than the
incremental copy, so the incremental logic is exercised only in the identity case; check that the
rotation change correctly invalidates `composite_valid`).

Trails and duplicates are the two shapes an incremental composite fails in. If you see either,
record where on the display and how fast you were moving, then fix it.

### 3.3 — L-02. The DXGI Desktop Duplication path, end to end

**Files:** all of `windows_utils/streamer/dxgi/`. **Rows:** 101, 103, 104, 105, 106, 107, 212.
**Tiers:** *DXGI duplication entry point*, *DXGI downscale (Windows)*. **Manual test:**
`windows-capture.md` §5, §5b.

This is the centre of the guide. Every one of these has a log line that proves it.

**(a) The healthy shape.**

```powershell
cargo run --release --features probe -- probe --capture-backend dxgi --whep-selftest --duration 30
```

```
dxgi: duplication ready on \\.\DISPLAYn via DuplicateOutput1 on <adapter> (vendor 0x____, LUID 0x____): WxH, rotation=..., logical=WxH
pipeline: live capture ready -- DXGI duplication (DuplicateOutput1) -> <path>
```

`via DuplicateOutput` instead is fine and expected on Windows 8.1-era stacks; **record which one you
got** — that is the answer the *DXGI duplication entry point* tier row is waiting for. Then run the
same command with `--capture-backend wgc` for the same-machine comparison.

**(b) Every adapter tried.** `find_outputs` enumerates *all* adapters that claim the device name and
`Duplicator::new` tries each in turn, because on a hybrid laptop only the adapter that composes the
desktop can duplicate it. On failure the message must name **every** adapter it tried and why each
declined. A bare HRESULT with no adapter named is a regression. Force the failure if you can (a
second display attached to the other GPU is one way) and read the message.

**(c) `DuplicateOutput1` and the format list.** `DUP1_FORMATS` is `B8G8R8A8_UNORM`,
`R10G10B10A2_UNORM`, `R16G16B16A16_FLOAT`, most-wanted first. **If your machine has an HDR-capable
display, turn HDR on and re-run** — that is the case the two HDR formats exist for and nothing has
ever exercised it. Record which format the duplication came back with, what the picture looks like
on the client (a 10-bit or FP16 surface reaching a BGRA8 pipeline is a colour bug waiting to happen)
and whether the composite path handles it. This is probably a finding; it may be a real defect.

**(d) The RAII acquire/release guard.** `AcquiredFrame` releases on every exit path, including
early `?` returns. Confirm no acquire leaks: a leak manifests as every subsequent
`AcquireNextFrame` returning `DXGI_ERROR_INVALID_CALL` forever. Add a fault-injection test if you
can (an `#[cfg(test)]` seam that forces an early return between acquire and release).

**(e) The `DXGI_ERROR_INVALID_CALL` cap.** `INVALID_CALL_MAX_RECOVERIES = 20`. Reproduce against a
freshly created IDD virtual display: the IDD is still attaching, `ACCESS_LOST` fires repeatedly, and
the next acquire is invalid. Expect

```
dxgi: AcquireNextFrame returned DXGI_ERROR_INVALID_CALL (recovery n/20); re-duplicating
```

and then either recovery or a clean failure. **Time how long the settle takes on your hardware** —
if 20 is too few or far too many for a real IDD attach, that is a tunable with a number behind it.

**(f) The never-produced-a-frame bail.** `REDUP_WITHOUT_FRAME_MAX = 40`. The correct behaviour is

```
dxgi: duplication re-established (1 since the last frame)
dxgi: duplication re-established (10 since the last frame)
...
dxgi capture: duplication was re-established 41 times without ever delivering a frame; this output cannot be captured with DXGI Desktop Duplication
```

A count that climbs a little and then resets as frames start flowing is the normal settling of a
virtual display — that is what `redup_without_frame = 0` on a successful acquire is for. **What is a
bug** is an unbounded run of those lines (the cap is not working) or a line printed for every single
re-establishment (the rate limit is not working — and every log line is a UI IPC event).

**(g) Incremental cursor compositing** — covered by 3.2.

**(h) The 3-slot staging ring and the `dxgi-encode` thread** — this is task 3.11, the biggest one.

**(i) `AccumulatedFrames` counted.** `frames_dropped_capture` should be non-zero under load: every
frame past the first in an `AccumulatedFrames` report is a present the client never saw. Drive the
display hard and confirm the counter moves. If it stays zero under a load that visibly drops frames,
the counter is wrong.

**(j) `ProtectedContentMaskedOut` reported once per capture.** Play DRM-protected video (a
Netflix/Prime tab in Edge with hardware DRM) on the extended display and confirm exactly one

```
dxgi: the desktop image has protected content masked out — DRM-protected video will reach the client as a black rectangle
```

and that the client shows a black rectangle rather than an error. Also **fix the message**: it
currently contains a run of spaces from a mis-wrapped string literal
(`dxgi/mod.rs`, the `protected_content_warned` block). That is Tier A and one line.

**(k) The changed-region hint.** `collect_change` reads move rects then dirty rects, unions them
into `changed_bounds`, and returns true only when the driver said *positively* that nothing changed.
`MAX_CONSECUTIVE_SKIPS = 5` is the valve against a driver that under-reports. Log the distribution
of changed-region area as a fraction of the output over a real session per workload
(`static`/`scroll`/`motion`/`drag`); that histogram is the evidence row 277 has been waiting for.
If a large fraction of frames have a small changed region, the encoder-side consumer becomes worth
building — but the encoders are Tier C, so what you produce is the *number*, a finding, and a ledger
row with the measurement in it.

**Done:** rows 103, 104, 105, 107 and 212 become `VERIFIED` with numbers from a machine where the
path runs; the *DXGI duplication entry point* and *DXGI downscale* tier rows get a real answer;
`docs/BASELINE.md`'s `unmeasurable` DXGI cells are filled.

### 3.4 — L-03. DXGI downscale tier 2: no video processor

**Files:** `windows_utils/streamer/dxgi/`, `windows_utils/streamer/scaler.rs` (Tier C — read).
**Row:** 107. **Tier:** *DXGI downscale (Windows)*.

`Scaler::new` casts the duplication device to `ID3D11VideoDevice` and its context to
`ID3D11VideoContext`. Both fail on a device created without `D3D11_CREATE_DEVICE_VIDEO_SUPPORT` —
which `create_device_on` **silently retries without**, after printing
`dxgi: device with VIDEO_SUPPORT failed (…); retrying without` — or on an adapter with no video
engine. An indirect-display adapter is not obliged to have one.

**Verify** on a machine (or a display) whose composing adapter lacks a video engine, with a client
whose video-scale is under 100 %. The expected line names the fallback and the session continues at
native resolution. The failure this replaced was `error: building GPU downscaler for --scale` and no
session at all.

If you cannot find such an adapter, force the branch: temporarily make `create_device_on` skip the
`VIDEO_SUPPORT` flag under an env var, take the reading, and revert. Say in the ledger that the
branch was forced rather than encountered — a forced reading is worth far more than nothing and far
less than the real thing.

### 3.5 — L-04. Hybrid-adapter selection

**Files:** `windows_utils/windows_capture/d3d11.rs` (Tier A), `windows_utils/streamer/capture.rs`
(Tier A), `windows_utils/streamer/pipeline.rs` (Tier C — read). **Row:** 98. **Risk:** R-231.

Once per session the host logs

```
adapters: capture=<name> (vendor 0xXXXX, LUID 0x...) encode=<name> (vendor 0xXXXX, LUID 0x...) same_adapter=true|false
```

**`same_adapter=true` is the goal**: it means the captured texture never leaves the GPU that
produced it, which is the precondition for the zero-copy path. `same_adapter=false` means every
frame takes a GPU→CPU→GPU round trip, confirmed by

```
NVENC zero-copy path unavailable (...); falling back to CPU bridge (higher latency)
```

Then one of these path lines:

| Path line | Meaning |
|---|---|
| `NVENC ZERO-COPY RING (K=3) decoupled path` | best; the one the DXGI fallback was costing pre-26100 hosts |
| `NVENC ZERO-COPY cross-adapter GPU path` | two GPUs, still no CPU copy |
| `INTEL Quick Sync same-adapter path` | fine |
| `AMD AMF same-adapter GPU path` | fine; the encoder itself belongs to `windows-amd.md` |
| `NVENC CPU-bridge fallback` | works, but every frame round-trips through system memory |
| `SOFTWARE x264 CPU path` | no hardware encoder was usable |

The unanswered question, and the point of the item: on a hybrid laptop **the capture adapter is
not ours to choose for an indirect display.** Duplication and WGC are both adapter-local, so the
capture adapter is whichever GPU the OS chose to compose that display on — and before IddCx 1.4 the
OS composes an indirect display on the POST adapter (the iGPU on a hybrid laptop), with
`IddCxAdapterSetRenderAdapter` the only way to ask otherwise. **The bundled driver declares IddCx
1.2.** So establish, with the LUIDs, which adapter composes the ScreenExtend virtual display on your
machine, which one `capture_gpu_preference` asked for, and what the mismatch costs per frame. Then
work out whether `EnumAdapterByGpuPreference` is making the *best available* choice given the
constraint, and whether there is a cheaper cross-adapter path than the current full readback (a
`SHARED_CROSS_ADAPTER` staging texture is the obvious candidate; it cannot be an encoder input, but
it can be a transfer surface — measure it against the 1192 µs / 740 µs readback figures before
building it).

Also confirm the software-rasteriser shout works: if the display is composed by WARP you must see

```
dxgi: output <name> is composed by a SOFTWARE rasteriser -- ...
adapters: capture is running on a SOFTWARE rasteriser -- ...
```

This is one of the two paths that make `CompatibilityReport.software_adapter_only` true (task 3.7).

### 3.6 — L-20 / L-21. Latency on this band, and no regression at 26100

**Files:** all of the above. **Manual test:** `windows-capture.md` §7, §8.

Run the full Matrix A and Matrix B from §7 again *after* your changes. **The interesting comparison
is this machine before and after, not this machine against the dev box.** A pre-26100 host was on
DXGI before row 95 and is on WGC now; the delta between those two on the same hardware is the whole
result, and `probe --capture-backend dxgi` against `--capture-backend wgc` is exactly that
comparison in one command each.

Run at least one five-minute `serve -v` session under ordinary use and take the last telemetry line.
**Watch the p99 and max, not the p50.** A p50 that stays flat while the p99 creeps upward over five
minutes is a queueing problem, and that is where these bugs live.

If a build-26100 machine is also available, run §6.2 and §7 unchanged on it: the feature summary
must read every flag `+` ending `(build 26100+)`, the path line must still be the ring, and the DXGI
fallback line must not appear anywhere in the log. A `-` on a 24H2 host means the probe now rejects
a property that used to be requested successfully.

### 3.7 — L-06. Virtual-display failure tiers and the software-adapter branch

**Files:** `windows_utils/virtual_display.rs` (Tier B), `windows_utils/driver_ipc/` (Tier B),
`windows_utils/compatibility.rs` (Tier A). **Rows:** 22, 199. **Tier:** *Virtual display (Windows)*.
**Manual test:** `core-closeout.md` §6.

`probe_display_backend()` returns `IddDriver` → `DriverNotInstalled` → `DriverNotResponding`. It
checks that `\\.\pipe\virtualdisplaydriver` exists, then does a real `DriverClient::new()` connect;
a process that already holds the client short-circuits, because the driver serves one client at a
time and a second connect would report a healthy host as not responding. **Failures are never
cached**, because `drivers install` flips this at runtime.

Take three readings, in this order:

1. **Before installing the driver.** `cargo test --release --lib report_display_backend -- --ignored`
   and `doctor --json` must both say `DriverNotInstalled`, and `check_system_requirements` must
   carry a **blocking** `Virtual Display Driver` entry saying "Not installed."
2. **After `ScreenExtend drivers install`** (Administrator). Both must say `IddDriver`, with no
   restart of the app in between — that is what "failures are never cached" buys.
3. **With the driver wedged.** Start the host with a client joined, then suspend the driver's
   user-mode host process (Process Explorer → Suspend) so the pipe stops being drained, and join a
   second client. Expect `DriverNotResponding`, a blocking entry saying "Installed but not
   answering", the second join failing with a logged `SendError::PipeBroken(TimedOut)` **within a
   few seconds rather than hanging**, and `[virtual-display] driver commit took …`. Crucially,
   **`ScreenExtend status --json` and the device list must still answer while the driver is
   wedged** — that is what taking the round trip out of the state mutex (row 199) buys, and it is
   the only way to verify it.

**The software-adapter branch.** `CompatibilityReport` reports `software_adapter_only` when every
DXGI adapter is a software rasteriser (`DXGI_ADAPTER_FLAG_SOFTWARE` or a description containing
"Basic Render Driver"). A VM with no GPU passthrough is the cheapest way to reach it. Confirm the
`Hardware graphics adapter` entry appears with severity `optional`.

**While you are in `compatibility.rs`:** verify the AMD reporting described in §5.6 — it was fixed
(row 298) and what remains is confirming the string is honest on your hardware.

### 3.8 — L-07 / L-08. Driver IPC resync and display-name correlation

**Files:** `windows_utils/driver_ipc/client.rs` (Tier B), `windows_utils/virtual_display.rs`
(Tier B). **Rows:** 196, 198, 199. **Tiers:** *Driver IPC protocol*, *Virtual-display name
correlation*.

**IPC resync.** `DriverClient` resynchronises with `request_state()` when the driver event stream
reports a gap (`EventOrLag`), instead of only logging it — the cached monitor list is fed by that
stream and `refresh_state` reads the same cache, so a dropped `Changed` left `monitors()`
disagreeing with the driver with no path to recovery. Neither half is reachable today (the driver is
the only writer and the channel bound of 64 is more than any burst). **Try to reach it anyway:**
suspend the consumer side under load, or attach several displays in rapid succession, and see
whether the lag path fires. If it genuinely cannot be reached on real hardware, say so with what you
tried, and leave the row `COMPILE-CHECKED` with that evidence attached rather than silently.

The `.v2` pipe preference: nothing is *sent* to negotiate, because the shipped driver is a signed
third-party binary and how it reacts to a message it cannot decode is not testable. Confirm the
shipped driver connects as v1 and that no v2 message is ever emitted.

`receive_error: RwLock<…>` is one of the two intentional `tokio::sync` locks in the codebase.
Leave it as an async lock; do check that nothing new holds it across an await it should not.

**Name correlation.** Run

```powershell
cargo test --release --lib report_display_name_correlation -- --ignored --nocapture
cargo test --release --lib report_active_display_paths -- --ignored --nocapture
```

against **the full DPR ladder and several simultaneous displays**. The match is by prefix because
the EDID monitor-name descriptor is **13 bytes**, so `ScreenExtend - iPad` comes back truncated and
several of our own displays share a prefix. Ambiguity deliberately returns `None`, which drops back
to the `EnumDisplayDevices` diff poll. Measure **how often** it is ambiguous with 2, 3 and 4
displays attached, and how much wall time tier 1 saves on the join path when it does resolve (the
tier-2 poll is 100 ms granularity, bounded only by the 5 s attach timeout, and serialised across
concurrent joins by `DISPLAY_CORRELATION_LOCK`). If ambiguity is the common case with more than one
display, that is a defect worth fixing — the EDID serial number is another 13-byte descriptor and
the correlation could use both.

### 3.9 — L-09. The hosted-network ladder, with a client associating on each band

**Files:** `windows_utils/hosted_network.rs`, `windows_utils/networking.rs`. **Row:** 21.
**Tiers:** *Hosted network (Windows)*, *Legacy hosted-network capability*.

The ladder is Mobile Hotspot (`NetworkOperatorTetheringManager`, band 6 GHz > 5 GHz > auto) →
Wi-Fi Direct legacy AP (`WiFiDirectAdvertisementPublisher` + `LegacySettings`, **no band control**)
→ `netsh wlan hostednetwork` → unsupported. "Started" requires the AP's virtual adapter to hold a
routable IPv4 (`wait_for_ap_address`, 8 s with a 400 ms poll), because an AP that never serves DHCP
is a failure the user would otherwise experience as a wrong OTP.

**This band is the last place tier 3 can be exercised at all.** `netsh wlan hostednetwork` was
deprecated after Windows 10 1607 and is gone in practice on modern drivers.

Exercise each tier in turn and, for each, actually **associate a phone and measure**:

```powershell
cargo run --release -- network start <ssid> <password>
cargo run --release -- network status
# from the phone, once associated:
iperf3 -c <host-ip> -t 30 -P 4        # throughput
ping -n 200 <host-ip>                  # RTT distribution, from the host to the phone
cargo run --release -- network stop
```

Record per tier and per band: the `[hosted-network]` lines verbatim, the band chosen, the host
IPv4, iperf3 throughput, ping p50/p90/p99/max, and then a **real ScreenExtend session over that
AP** with its `capture → socket` and client-side numbers. The comparison that matters is 5 GHz
Mobile Hotspot vs whatever Wi-Fi Direct's group-owner negotiation lands on (normally 2.4 GHz) vs
`netsh`, on the same host and the same client.

To force tier 2, arrange for `tethering_capability()` not to return `Enabled` (group policy, or a
machine where it genuinely is not). To force tier 3, force both modern tiers to fail. If you cannot
force them cleanly, add a **temporary, clearly-marked** env-var override in `hosted_network.rs`,
take the readings, and then decide: either keep it as a documented developer override (it is the
only way anyone will ever measure tiers 2 and 3 again) or revert it. Record which you chose and why.

Also verify the failure semantics: a network that starts but never gets an IPv4 must log

```
[hosted-network] '<ssid>' is advertised on <backend> but its adapter never got an IPv4 address; clients will be able to see it and not join (no DHCP)
```

and `start_hosted_network` must still return `true` with `advertised: true` and `host_ipv4: None` —
check that the UI and the CLI both surface that distinction rather than claiming success.

### 3.10 — L-10. The localized hosted-network probe

**Files:** `windows_utils/hosted_network.rs` (`wlan_hosted_network_capable`,
`wlan_hosted_network_active`, `supports_legacy_hosted_network`, `legacy_started`). **Row:** 197.
**Tier:** *Legacy hosted-network capability*. **Manual test:** `core-closeout.md` §3.

`WlanQueryInterface(wlan_intf_opcode_hosted_network_capable)` returns a `BOOL` and
`WlanHostedNetworkQueryStatus` returns a `WLAN_HOSTED_NETWORK_STATE`; neither is localized. The
`netsh` phrases (`Hosted network supported`, `: Yes`, `Status`, `: Started`) are, so on a German,
French, Japanese or Chinese Windows the old probe answered "no" unconditionally and a *successfully
started* hosted network read back as *stopped*.

1. On a machine whose WLAN driver **does** support the hosted network (`netsh wlan show drivers`
   says `Hosted network supported : Yes` in English), record
   `cargo test --release --lib report_wlan_hosted_network_capability -- --ignored --nocapture`.
   This is the **positive case, which no machine on this project has ever produced** — the dev box
   reads `Some(false)`.
2. Change the Windows display language to German (or French / Japanese / Chinese) and reboot.
3. Run the same test. **Expect identical answers.**
4. `ScreenExtend network start <ssid> <pass>` and `network status` must agree on the localized
   install; the started/stopped read-back is `WLAN_HOSTED_NETWORK_STATE`, not a `": Started"` match.
5. Also confirm the *fallback* path's honesty: `supports_legacy_hosted_network` only reaches the
   `netsh` parse when the WLAN service declines to answer, and on a localized Windows it logs
   `did not name hosted-network support (localized Windows?); assuming the legacy tier is
   unavailable`. Verify that branch is genuinely unreachable when the WLAN service answers.

**A note on `start_legacy`.** It shells out to the app's own executable with the legacy
`hostednetwork <ssid> <password>` subcommand under `elevated_command::Command`, which raises a UAC
prompt. Verify that path works on a localized install (the elevation dialog, the `netsh` invocation
inside it, and the capability entry in `src-tauri/capabilities/default.json`). `cli.rs`/`lib.rs`
own the legacy subcommand and are Tier C — a defect there is a finding.

### 3.11 — L-02(h). The DXGI staging ring and the `dxgi-encode` thread — the largest measurement gap

**Files:** `windows_utils/streamer/dxgi/`, and the `DxgiRing` consumer in `pipeline.rs` (Tier C —
read only). **Row:** 106.

Before this change, capture of frame N+1 could not begin until the encode of frame N returned, so
the achievable rate was `1 / (capture + composite + encode)` rather than `1 / capture` — and on
every CPU-bridge backend the encode is the large term. **This is the path every Windows 10 and
pre-24H2 Windows 11 host used to fall back to**, so it is the common case rather than the rare one.
The row is `COMPILE-CHECKED` and the throughput claim has **no number anywhere in the project.**

Produce it. Measure, on your machine, with DXGI forced:

- delivered frames per second and `capture → socket` percentiles, with the ring as shipped;
- the same with the ring bypassed (encode inline on the capture thread) — you may add a temporary
  developer switch to do this, in Tier A, clearly marked, and decide afterwards whether to keep it;
- `frames_dropped_ring` under each of the four workloads;
- the per-60-frame line `encode-path latency: path=dxgi+<backend>, avg_ms=…, max_ms=…, frames=…,
  budget_ms=…`, and specifically how often `avg_ms` exceeds `budget_ms`.

Then attack the caveat the row states honestly and which nobody has measured: **the encode thread
shares the duplication device with the capture thread, and D3D11 multithread protection holds the
device lock for the whole of a blocking `Map`.** So on a CPU-bridge backend the *readback* (1192 µs
BGRA / 740 µs NV12 at 720p) still stalls the capture thread even though the encode itself no longer
does. Measure how much of the capture thread's time goes into that lock. Then evaluate — and, if it
wins, **build** — the obvious fixes: a second D3D11 device for the encode side with a
`SHARED_NTHANDLE` slot texture; or moving only the readback to its own thread; or `DO_NOT_WAIT` with
an explicit fence. Each one is a measurement, not an opinion.

Finally: **extending the ring to the non-NVENC backends.** `DxgiRing` is the shape to lift. This was
explicitly deferred because "the measurable half — the DXGI path — could not be measured on this
machine anyway". On your machine it can. If the measurement says a generic slot-texture ring in
front of the classic WGC path wins, the change is a `pipeline.rs` change and therefore Tier C — so
what you deliver is the *measured case for it*: a finding with numbers, a ledger row, and a written
design naming which device owns the slots for each backend. If it can be done entirely inside
`windows_capture/` and `dxgi/`, build it.

### 3.12 — L-11. The low-latency engine period, on a driver that has one

**Files:** `windows_utils/audio/silence.rs`, `engine.rs`, `format.rs`. **Row:** 68. **Tier:**
*Engine period (Windows audio)*. **Manual test:** `windows-audio-tiers.md` §1.

The engine runs **one periodicity for the whole endpoint**, and Microsoft documents that any stream
may set it. Our *capture* stream cannot ask — `IAudioClient3::InitializeSharedAudioStream` lists its
supported flags exhaustively and `AUDCLNT_STREAMFLAGS_LOOPBACK` is not among them (measured refusal:
`AUDCLNT_E_INVALID_STREAM_FLAG`, `0x88890021`). The **silent render companion** has no loopback flag,
so it can, and the capture stream inherits the result. `acquire()` already starts the companion
*before* opening loopback, which is the ordering this needs.

1. `probe --audio --duration 15` and record the `audio: companion sees engine periods …` line and
   the `wake` histogram. Dev-box baseline: `default=480fr (10.000ms) fundamental=480fr min=480fr
   max=480fr`, wake p50 **9.44 ms**.
2. Device Manager → *Sound, video and game controllers* → the audio device → **Update driver** →
   *Browse my computer* → *Let me pick from a list* → **High Definition Audio Device** → reboot.
3. Re-run. Expect `min=128fr (2.667ms)`, the line

   ```
   audio: companion pulled the endpoint engine period to <n> frames (<x.xxx> ms); the loopback capture stream inherits it
   ```

   and a wake histogram around **2.7 ms** instead of 9.4 ms. Note that the granted value is read
   back with `GetCurrentSharedModeEnginePeriod`, not assumed — quote what it actually granted.
4. **Then check the regressions this is most likely to cause**, all in the same summary line:
   `discontinuity`, `silent` and `dc_drops` must not increase, and `starvations` must stay 0.
   Microsoft warns that small buffers cost power — at 2.67 ms the capture thread wakes ~375 times/s
   instead of ~100 — so **sample the process's CPU before and after** and report both.
5. Also exercise the refusal branches: `AUDCLNT_E_ENGINE_PERIODICITY_LOCKED` (start another
   application that pins the engine period first — a DAW, or a second instance asking for a
   different period) and `AUDCLNT_E_INVALID_DEVICE_PERIOD`. Both must log the named reason and fall
   through to the default path **on a fresh `IAudioClient`** — a refused
   `InitializeSharedAudioStream` leaves the client in an undefined state for a second `Initialize`,
   which is why `init_render_stream` re-`Activate`s. Confirm that re-activation actually happens and
   that audio still works after a refusal.
6. Restore the OEM driver afterwards and re-measure, so the OEM numbers in your baseline are
   comparable.

**Expected result:** −5 to −7.3 ms off the capture period, the largest single controllable term in
the Windows audio budget.

**Then optimise further.** The companion currently asks for `min_frames` unconditionally when
`min < default`. Is the minimum always the right ask? Sweep the legal values (multiples of
`fundamental` between `min` and `max`) and plot wake-interval jitter, `starvations`, `dc_drops` and
CPU against each. If 2×min is materially more robust for a fraction of a millisecond, that is a
better default and it is one line to change. Measure it.

### 3.13 — L-12. Process loopback refusing on Windows 10 19045

**Files:** `windows_utils/audio/process_loopback.rs`, `loopback.rs`. **Tier:** *System audio capture
(Windows)*. **Manual test:** `windows-audio-tiers.md` §2.

The process-loopback tier is probed by **attempting the activation**, never by a build number:
`ActivateAudioInterfaceAsync` exports on builds that do not support the activation *type*, so its
presence proves nothing. Availability is Windows 10 build **20348** and up.

On a retail **Windows 10 19045** host, `probe --audio` must log

```
audio: process loopback unavailable (<error>); endpoint loopback is the only tier
```

and audio must work **exactly as before** — the refusal is expected and must be invisible to the
user. Record the exact error text and HRESULT; that is the thing nobody has ever seen.

Then, on a machine where tier 2 *is* available (build ≥ 20348), force the endpoint tier to fail by
opening the default endpoint in **exclusive mode** from another application, and confirm

```
audio: endpoint loopback failed (<error>); falling back to process loopback
```

and that audio recovers. That fallback ordering is deliberate: endpoint loopback is tried first
because it is what every supported Windows has, and process loopback is *not* measurably faster
(both ride the same shared engine — measured equal on 26200: wake p99 9.44 vs 10.49 ms, capture →
packet p50 5.77 vs 6.29 ms). What tier 2 buys is robustness: endpoint independence across default-
device changes, the only option on an exclusive-mode endpoint, structural self-exclusion, and a
format we choose (48 kHz float32 stereo, so it never takes the resampler).

While here, verify the two claims the module documents as measured and the two it documents as *not*
needed: no silent render companion on tier 2 (there is no endpoint to park), and no
`AUDCLNT_BUFFERFLAGS_SILENT` (the synthetic device delivers real zeros, so `FLAG_SILENT` never
reaches the client on this tier). Confirm both on your hardware and record the packet rate.

### 3.14 — L-13. Audio format conversion tiers 2 and 3

**Files:** `windows_utils/audio/format.rs`, `loopback.rs`. **Tier:** *Audio format conversion
(Windows)*.

Three tiers: (1) the endpoint is already 48 kHz f32 stereo and the mix format is taken verbatim;
(2) the engine converts, via `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | SRC_DEFAULT_QUALITY`; (3) the
engine refuses and we capture the endpoint's own format, then downmix (ITU BS.775) and int→float
convert in `format.rs`. **Tiers 2 and 3 were unreachable in production until now.**

Remember: `SRC_DEFAULT_QUALITY` **reads backwards from its meaning** — it selects the *expensive*
converter, which has group delay. Verify what it actually costs on your hardware rather than
trusting the name.

Set each of these as the default output and run `probe --audio --duration 30`:

| Endpoint | What it exercises |
|---|---|
| 44.1 kHz stereo (a USB DAC, or an HDMI sink forced to 44.1) | sample-rate conversion, tier 2 |
| 24-bit / 32-bit integer interface | `SampleKind::I24In3` / `I32` in tier 3 |
| 5.1 or 7.1 HDMI sink | the BS.775 downmix in `downmix_frame` (6ch and 8ch arms) |
| mono endpoint if you can find one | the 1ch arm |
| 96 kHz or 192 kHz endpoint | the far end of the resampler |

For each, record the negotiated format line, which tier was taken, the wake histogram, capture →
packet percentiles, the `capture_ns` skew, and **listen to it**. The downmix arms in `format.rs` are
unit-tested for arithmetic but have never processed real multichannel audio; a channel-order mistake
is silent in a unit test and obvious in a speaker.

Force tier 3 by making the engine refuse `AUTOCONVERTPCM` — on some drivers an exclusive-mode
holder or an unusual channel mask does it. The expected line is

```
audio: engine refused AUTOCONVERTPCM (<error>); capturing the endpoint's own format and converting in userspace
```

Tier 3 costs CPU, not latency: measure the CPU delta and confirm the latency percentiles do not
move.

### 3.15 — L-14. The "no default output device" branch

**Files:** `windows_utils/audio/device.rs`, `windows_utils/compatibility.rs`. **Tier:** *System
audio (Windows)*.

Disable every render endpoint (Device Manager → disable each audio output; or unplug everything on a
machine with no internal speaker). `doctor --json` must report `audio_backend: "none"` and
`check_system_requirements` must carry an `optional` entry:

```
System Audio Capture — No default audio output device, so there is nothing to capture.
```

The point is that this is visible **before** the user tries the per-device Audio toggle, rather than
failing silently at stream time. Confirm a session still starts and streams video with the toggle
off, and that turning the toggle on produces a clear message rather than a hang or a black screen.

Then re-enable an endpoint **while the host is running** and confirm the `IMMNotificationClient`
path picks it up without a restart. `device.rs`'s callback runs on a COM thread we do not own, so it
only posts to the capture thread over `crossbeam-channel` — it must never block and never take the
capture lock. Verify that under a rapid sequence of enable/disable/default-change events (a script
toggling the default output every 500 ms for a minute is a good stress test) the counters
`dev_changes` and `discontinuity` move, `starvations` stays 0, and nothing deadlocks.

### 3.16 — L-15. `capture_ns` and the play-cursor clamp, with a real audio client

**Files:** `windows_utils/audio/loopback.rs`, `streamer/audio/mod.rs` (read-only). **Rows:** 19, 67.

`qpcpos` is documented as when "the audio endpoint device read the device position", but on a
**loopback** stream the endpoint is a *render* device and that position is its **play cursor** — a
time in the future. The client sizes its jitter buffer from `enqueue − capture_ns`, so a future
capture time is surplus audio delay. The fix anchors `capture_ns` to `min(qpcpos, now − read_duration)`
and counts every clamp in `ts_clamped`.

Dev-box readings, for comparison: before, `capture → subscriber` recorded **0 samples** (every span
negative), skew min −20.36 / mean −14.65 / max +5.61 ms with **1596 of 1601 packets stamped ahead of
the host clock**; after, `capture → subscriber` 9.44 / 9.44 / 9.44–10.49 / 10.9 ms and skew min
−3.04 / mean +7.69 / max +10.9 ms with ≤ 1 of 1601 ahead. `ts_clamped` fires on **every drain** on
that box.

What has never been done is verifying it **with a client that actually has system audio on.** Do
that:

1. Join a phone, enable system audio for that device.
2. Play real content on the extended display (a video with clear transients — speech plus
   percussion).
3. From the client page's console read `SEAudio.getSyncInfo()`: the commanded `targetMs`, the
   measured offset, `underruns`, and the transport (`shared` / `port` / `postMessage`).
4. On the host read `probe --audio`'s skew line and the `status --json` audio counters.
5. **Measure A/V offset by camera**: film the host screen and the client, with a clap or a
   percussive transient visible in both. Report the offset in milliseconds, signed.

Then re-run the whole thing at the low engine period from 3.12, because the clamp's `read_duration`
term scales with the period and the interaction has never been measured.

### 3.17 — L-16. `OPUS_SET_PREDICTION_DISABLED` and constrained VBR's packet-size distribution

**Files:** `streamer/audio/encoder.rs` and `opus_sys.rs` are **Tier C** — read only. The
*measurement* is yours; the code change is a finding plus a ledger row unless it can be made
entirely through `OpusEncoderConfig`'s existing field, which is a config default rather than encoder
code. **Row:** 73. **Manual test:** `windows-audio-tiers.md` §3, §4.

The encoder runs `RESTRICTED_LOWDELAY` (CELT-only) with 5 ms frames. `OPUS_SET_VBR_CONSTRAINT` went
0 → 1 on the argument that unconstrained VBR spikes packet size on transients, which becomes arrival
jitter, which the client answers by growing its buffer. The setting is **confirmed in force by
reading it back** (`vbr=1 vbr_constraint=1`), but the *distribution* is not: an idle host encodes
3-byte silence frames, so the dev box cannot see the tail.

1. With **real audio playing** (percussion, applause, speech), `probe --audio --duration 60`.
   Record `payload avg` and `max`, and the full packet-size histogram.
2. Compare against a build with the constraint back off. Expect the maximum to come down toward the
   nominal ~118 bytes while the average barely moves. If it does not, the hypothesis was wrong and
   that is a result — record it with numbers.
3. `OPUS_SET_PREDICTION_DISABLED` defaults **off**. Prediction-disabled makes every frame nearly
   independent, which is what a zero-retransmit unordered transport wants, but it costs bitrate at
   fixed quality. Flip it, A/B against the current build over a real Wi-Fi link with **2 % and 5 %
   induced loss** (`clumsy`), and watch `SEAudio.getSyncInfo()` for `underruns` and the commanded
   `targetMs`. Keep it only if the loss-robustness gain is audible and the clean-link quality loss
   is not. This is a listening decision, which is exactly why it was not landed blind.
4. Then a 10-minute client session on real Wi-Fi, watching whether `getSyncInfo().targetMs` settles
   lower.

### 3.18 — L-18. The DPR ladder on a pre-26100 build

**Files:** `windows_utils/virtual_display.rs` (Tier B), `windows_utils/streamer/capture.rs`
(Tier A), `streamer/server.rs` and `streamer/platform.rs` (Tier C — read).

The IDD virtual display **only accepts pre-registered resolutions**. The whole DPR ladder is
registered at create time (`dpr_mode_ladder` pre-registers one display mode per ratio in
`platform::display_dpr_ladder()`, plus each transpose so an orientation flip is already registered),
and the effective ratio is snapped down by `platform::snap_display_dpr`. **A live display cannot be
given new modes**, and Windows **resets the DPI scale when the resolution changes**, so the scale
must be re-applied afterwards — `set_display_scale` does that and now waits for a read-back
(five consecutive agreeing reads, 2 s deadline) rather than reporting success on a write.

Verify on this build: join at DPR **1.0, 1.25, 1.5, 2.0** from a client and read back the applied
mode and DPI on the host (`Get-DisplayResolution`, the Settings display page, and the host's own
`[display]` lines). Assert that the resolution multiplier and the DPI percentage are the **same
number** every time — resolution ×d with DPI 100·d %. A mismatch is the "DPR looks inert" bug.

Measure the cost against the 26200 table: in-ladder with resolution unchanged **111 ms**; in-ladder
with a new resolution **308 ms**; a geometry outside the ladder ~**772 ms** (monitor departure 179 +
remove/recreate/reattach 491 + set the new mode 281) plus a desktop-wide window reflow and a capture
session rebuild that no stopwatch sees. Run `report_selected_refresh_rate` and
`report_mode_change_cost` (`--ignored --nocapture`) three times each and report medians.

Watch specifically for the two things that differ on an older WDDM: whether the driver accepts the
full ladder at create time on this Windows version (count the registered modes and compare with
`display_dpr_ladder()`'s length), and whether `DisplayConfigSetDeviceInfo(SET_DPI)` — a **private,
undocumented** DisplayConfig call, `DISPLAYCONFIG_DEVICE_INFO_SET_DPI = -4` — behaves the same. If
the private call fails on this build, that is a blocker-class finding for the whole DPR feature on
Windows 10 and needs a fallback (`SystemParametersInfo`, or the per-monitor scale registry path)
researched and, if it works, built.

Also check the out-of-ladder path's *correctness*, not just its cost: after a remove + recreate the
client must not be evicted and the session must not be torn down (simultaneous displays are uncapped
and no connected device may be evicted to make room).

### 3.19 — L-05. CPU-set confinement on a machine with one efficiency class

**Files:** `windows_utils/streamer/tuning.rs` (Tier B). **Row:** 99.

`performance_core_masks` probes "does this machine report more than one distinct
`EfficiencyClass`" via `GetSystemCpuSetInformation`, and confines capture and encode threads to the
highest class with `SetThreadSelectedCpuSetMasks` — a **soft** restriction, unlike
`SetThreadAffinityMask`, so the scheduler may still migrate rather than deadlock. On the hybrid dev
box the branch is taken and logs

```
[cpu] hybrid CPU detected: confining capture/encode threads to the <n> logical processors of efficiency class 1
```

**The negative branch has never executed.** On a homogeneous CPU the correct behaviour is to take
*no action at all* and say nothing (or say it once, quietly). Verify on such a machine — most
pre-Alder-Lake Intel and most AMD desktop parts qualify — that no confinement is applied, no
`SetThreadSelectedCpuSetMasks failed` line appears, and thread affinity is untouched (check with
Process Explorer → thread → Affinity).

If your hybrid laptop is the only machine available, this item needs the second box. If neither is
homogeneous, say so plainly in the report rather than claiming it.

While here, since the whole point of row 99 is tail latency: on **your** machine, A/B the
confinement (an env-var override in `tuning.rs`, Tier B, temporary) and measure `capture_delivery`
and `encode` p99/max with and without. The row is `VERIFIED` as *taken*, not as *beneficial* —
nobody has ever measured what it buys.

### 3.20 — L-17. Intel Quick Sync on a different generation and a clean install

**Measure-and-report only. Do not edit `windows_utils/streamer/intel/`.** **Rows:** 235, 237.
**Tiers:** *oneVPL dispatcher*, *Quick Sync slice layout*. **Risk:** R-230.

Three questions, all of which need a machine that is not the dev box.

1. **Is `libvpl.dll` present on a real Intel host with only the GPU driver installed?** It is
   deliberately **not bundled** (`resources/` ships nothing; the reasons are in
   `resources/PROVENANCE.md`). Intel's guidance is that Xe (Gen12) and later drivers install the
   dispatcher and pre-Gen12 applications must ship their own. On a **clean Windows install with only
   the Intel GPU driver**, record whether `C:\Windows\System32\libvpl.dll` exists, its version, its
   signature, and what the session log says:

   ```powershell
   Get-Item C:\Windows\System32\libvpl.dll | Select-Object VersionInfo, Length
   cargo run --release --features probe -- probe --encode out_intel.h264 --encoder intel
   ```

   The dispatcher search order is OS path → exe directory → `resources/`, with an `MFXLoad` export
   check so a file with the right name but the wrong contents is skipped rather than failing the
   whole load. Record which candidate resolved, and — if none did — the honest error naming
   everything that was tried. **If Gen12+ hosts do not in fact ship it, that reverses a recorded
   decision** and is the most valuable single reading in this item.

2. **The `MFX_WRN_DEVICE_BUSY` retry path**, which never triggered on the dev box. It changed from
   `sleep(1ms)` (bounded below by the Windows timer resolution — up to ~15.6 ms without a
   `timeBeginPeriod(1)` that nothing sets, so thirty retries was up to ~450 ms of blocked encode
   thread) to spin → yield → 200 µs sleeps under a **50 ms wall-clock budget**, with a line logged
   when a wait is long enough to cost a frame. Force contention: run a second hardware H.264 encode
   (OBS with QSV, or `ffmpeg -c:v h264_qsv`) alongside a ScreenExtend session and watch for the
   line. Record how often it fires, how long the waits are, and whether 50 ms is the right budget.

3. **`MaxSliceSize` under `LowPower = ON` on another generation.** The dev box (Intel UHD,
   `LowPower = ON`) honoured it: 30 frames at 1280×720 gave **151 slices, p50 1191 B, max 11876 B**
   — so the cap is best-effort on the IDR. `ENCODE_Query` echoes `MaxSliceSize` back when it will
   honour it and zeroes or shrinks it otherwise, and the `NumSlice` height heuristic
   (`(height/256).clamp(4,8)`) is the fallback, applied and logged rather than assumed. Repeat on a
   different Intel generation and record the slice count and size distribution
   (`ffprobe -show_frames` plus the log's own read-back).

Deliver: the readings, findings for anything wrong, and ledger/tier row updates. Any code change is
written up, not made.

### 3.21 — Sweep: the WGC handler and the vendored fork

**Files:** `windows_utils/windows_capture/**` (Tier A), `windows_utils/streamer/capture.rs`.

Not a PLAN item, but this territory is yours and the standard applies. With the machine in hand:

- **The frame-arrived handler.** It drains the pool to the newest frame, counting discards into
  `frames_dropped_stale`, then reads `ContentSize`, `Surface`, casts to
  `IDirect3DDxgiInterfaceAccess`, gets the texture and its desc, and — on a content-size change —
  `Recreate`s the pool with the **same** buffer count as `Create` (passing 1 there silently undid
  the capture/encode decoupling for the rest of the session; that was the bug row 96 fixed). Verify
  the size-change path by changing the client's geometry mid-session and confirming the pool depth
  is preserved and `frames_dropped_stale` behaves.
- **The frame-pool depth**, per path: 2 for the inline handler, 3 for the ring. On the dev box
  `frames_dropped_stale` read 5, 0 and 3 against 321–421 delivered frames, so there was no queue to
  shorten. **Re-measure on your machine** — a slower CPU or a busier compositor may put the counter
  somewhere else entirely, and the trade is explicitly documented as re-openable if it climbs into
  the tens per second.
- **`Direct3D11CaptureFramePool::CreateFreeThreaded`**, per §5.5. The rejection rests on
  `capture_delivery` being dominated by the compositor rather than the message pump. **Measure the
  pump hop directly on this machine** (timestamp the `FrameArrived` callback entry against the
  frame's `SystemRelativeTime` and against the DWM signal if you can reach it). If the pump is a
  materially larger fraction here than on the dev box, the rejection does not bind and the change is
  back on the table — with all three of its risks to solve.
- **`FORK.md`.** The fork carries six genuine upstream bug fixes and had ~2,800 lines of dead code
  removed. Confirm `FORK.md` still describes what diverges, and update it if you change anything.
- **`is_supported()` and `wgc_supported()` used to disagree**, and the disagreement was resolved
  the way this guide's §2.4.1 says it should be: the vendored fork's
  `GraphicsCaptureApi::is_supported()` no longer ANDs
  `IsApiContractPresentByMajor("Windows.Foundation.UniversalApiContract", 8)` with
  `GraphicsCaptureSession::IsSupported()` — it now just returns `compatibility.rs::wgc_supported()`,
  because a contract check in front of a real probe can only produce **false negatives**. Before
  that, on a build where the contract check was the binding one, `wgc_features()` returned
  `WgcFeatures::default()` (every flag `-`) while the compatibility report said WGC was supported.
  **Confirm the reconciliation on your builds**: `grep -n 'fn is_supported' -A3
  src-tauri/src/windows_utils/windows_capture/graphics_capture_api.rs` should show the one-line
  form, `capture_features` in telemetry must match §6.2's table for your build, and the report and
  the session must agree. If your tree still has the conjunct, that is the fix to make — it is Tier
  A — and while you are there, record what `UniversalApiContract 8` actually corresponds to on 19041
  and 22631, because nobody in this project has established it and it is the last remaining reason
  the old code was written that way.

---

## 9. Phase 4 — research agenda

Spawn dedicated Opus 5 research agents. Each writes into `docs/research/` (a new file, or an
appendix section clearly attributed to this run) with citations, concrete actionable findings and an
explicit "what we should do about it" section. Prefer primary sources: Microsoft Learn, the Windows
SDK headers, `windows-rs`, and the source of OBS, Sunshine, FFmpeg and Chromium.

1. **DXGI Desktop Duplication on hybrid graphics — the definitive answer.** Why does
   `DuplicateOutput` return `DXGI_ERROR_UNSUPPORTED` on the primary display of an Optimus-class
   machine? Is it the adapter the calling process is bound to, the display's composition owner, the
   driver's WDDM version, or a `D3DKMT`-level policy? What do OBS, Sunshine and spacedesk do about
   it — and do any of them succeed where we fail? Is there a documented way to ask which adapter
   composes a given output (`D3DKMTOpenAdapterFromGdiDisplayName`, the DXGI 1.6 output ordering, or
   the `DISPLAYCONFIG` path's `targetInfo.adapterId`)? A definitive answer would let the app *tell
   the user why* instead of listing adapters that declined.
2. **DXGI duplication and HDR.** `DUP1_FORMATS` offers `R10G10B10A2_UNORM` and `R16G16B16A16_FLOAT`.
   What actually comes back on an HDR display, what does the rest of the pipeline do with it, and
   what do Sunshine and OBS do to tone-map or refuse? This path has never run.
3. **The pre-26100 change-detection problem.** With no `DirtyRegionMode`, what do OBS, Sunshine,
   Chrome Remote Desktop and spacedesk use to avoid re-encoding identical frames on Windows 10?
   Candidates to evaluate: a GPU-side hash/compare pass, `DwmGetCompositionTimingInfo`'s present
   counters, the `IDXGISwapChain` present statistics, or simply trusting the encoder's own
   skip-on-identical behaviour. Bring back numbers, then implement the winner (task 3.1).
4. **`GraphicsCaptureAccess.RequestAccessAsync(Borderless)`** — the API, its consent UX, which
   builds have it (20348+), whether an unpackaged desktop app can use it, and what shipping apps do.
   The border is a user-visible defect on every build from 20348 up that has not granted it.
5. **The WASAPI engine period across driver classes.** Which real Windows audio drivers expose a
   small buffer, and what do they expose? The in-box HD Audio driver documents 128–480 frames; what
   do Realtek's, Intel SST's, USB Audio 2.0's and Bluetooth endpoints do? Is there a class of driver
   where the request succeeds but the endpoint misbehaves? What does the low-latency audio
   documentation say about the power cost, and does anyone publish a measurement?
6. **`netsh wlan hostednetwork`'s actual death.** Which driver versions and which Windows builds
   still support it, and what replaced it in practice? What do the Mobile Hotspot and Wi-Fi Direct
   paths cost in latency and throughput compared with each other and with a real AP? Is
   `NetworkOperatorTetheringManager`'s 6 GHz band reachable on any shipping adapter yet?
7. **Windows 10 scheduling for a capture/encode pipeline.** `SetThreadSelectedCpuSetMasks` is Win 10
   1809+. What is available and worth using on 19041 that 26200 does not need — `MMCSS` task names
   beyond "Pro Audio", `PROCESS_POWER_THROTTLING_EXECUTION_SPEED`,
   `D3DKMTSetProcessSchedulingPriorityClass`, hardware-accelerated GPU scheduling (HAGS) presence,
   MPO? Measure each on this band rather than assuming the 26200 answers carry over.
8. **The IDD driver on an older WDDM.** The bundled driver declares **IddCx 1.2**. Which IddCx
   version does each build in this band ship, what does `IddCxAdapterSetRenderAdapter` (1.4) need,
   and what would a rebuilt driver buy on Windows 10 specifically? `docs/OPTIMIZATIONS.md`'s
   "Needs a driver rebuild, and what it is worth" is the existing analysis — extend it with what is
   true below 26100.

---

## 10. Phase 5 — the hyper-optimisation sweep

After the verifications land, go back through every knob in this territory and measure it. The
standard is §3: every 0.1 ms is in scope, and a speculative idea is resolved by trying it or by a
recorded rejection **with a number**.

**Capture, WGC.** Frame-pool depth per path (re-measure, do not inherit). Pixel format —
`DirectXPixelFormat` is `Bgra8` today; is there a format this build accepts that the encoder wants
directly? `TryGetNextFrame` polling versus the `FrameArrived` event. `MinUpdateInterval` is absent
here, so what paces the pool on this build, and is anything gained by pacing ourselves? The message
pump hop (§3.21).

**Capture, DXGI.** `AcquireNextFrame`'s timeout strategy — the pipeline passes a fixed timeout; is a
shorter one with a spin cheaper, or a longer one that blocks less? The composite path: on the
identity+no-cursor case `frame()` returns the duplication copy directly and never touches
`composite`, which is already optimal; on the incremental path it does one `CopyResource` per
changed rect plus one to erase the old cursor — is a single union rect cheaper than two small ones
when they are close together? `covers_whole_output`'s threshold is currently "spans the whole
output"; is there a better crossover (say, 60 % of area) at which a full `CopyResource` beats
`CopySubresourceRegion`? Measure it with a GPU timestamp query, not a guess.

**The staging ring.** Slot count (3 today), the device-lock contention from the blocking `Map`
(§3.11), and whether the encode thread should have its own device.

**Cursor.** `build_sprite` runs on every `PointerShapeBufferSize > 0` report — how often is that in
practice, and is the sprite ever rebuilt identically? A shape cache keyed on the shape info plus a
hash of the buffer is cheap. Measure how much time `build_sprite` takes and how often it runs during
a drag.

**Audio.** The engine-period sweep (§3.12). The drain loop's accumulator: it is documented as
allocating nothing beyond the reused accumulator plus a per-packet `Bytes` copy — verify that with a
real allocation profiler on this build, because that invariant is the one that keeps the RT thread
honest. The silent companion's pump loop: `event.wait(100)` then `GetCurrentPadding` then
`GetBuffer`/`ReleaseBuffer` for the whole available span — is filling the entire buffer each wake
the right amount of work, or does a smaller fixed chunk reduce jitter? The `IMMNotificationClient`
re-acquire path's wall time (how long is audio actually gone for on a default-device change?).

**Hosted network.** `wait_for_ap_address` polls every 400 ms for up to 8 s; measure the real
distribution of how long a routable IPv4 takes on each tier and tighten both numbers. The Mobile
Hotspot start already retries three times with backoff — measure whether the backoff is the right
shape.

**Compatibility probing.** `run_probes` spawns a thread and joins it, running the WGC probe, the DXGI
probe (which **creates and drops a real duplication**), the audio probe, the encoder enumeration,
the display-backend probe and the hosted-network probe **serially**. On a slow machine that is a
visible startup cost, and `doctor --json` reports per-probe timings. Measure each, then parallelise
the independent ones and cache what is genuinely static (the DXGI probe is not — a display can be
plugged in; the CPU-set probe is). Time-to-first-paint of the desktop UI is part of "feels slow".

**Startup and join time.** Measure display creation, mode registration, TLS handshake, ICE
gathering, encoder init and first keyframe on this band, and compare with the 26200 mode-change
table. Time-to-first-frame is part of the latency the user experiences.

---

## 11. Verification and gates

**Before every commit**, from `src-tauri/`:

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`cargo test` must stay green. Several tests are hardware- or OS-gated and no-op off their platform;
keep that convention and make a skipped test **visibly** skipped rather than silently passing.

**The `#[ignore]`d hardware harnesses in this territory** — run them all, with `--nocapture`, and
paste the output into the report:

```powershell
cargo test --release --lib report_backends                        -- --ignored --nocapture
cargo test --release --lib report_display_backend                 -- --ignored --nocapture
cargo test --release --lib report_display_name_correlation        -- --ignored --nocapture
cargo test --release --lib report_active_display_paths            -- --ignored --nocapture
cargo test --release --lib report_selected_refresh_rate           -- --ignored --nocapture
cargo test --release --lib report_mode_change_cost                -- --ignored --nocapture
cargo test --release --lib report_tethering_capability            -- --ignored --nocapture
cargo test --release --lib report_wlan_hosted_network_capability  -- --ignored --nocapture
cargo test --release --lib scaler                                 -- --nocapture
```

**Add tests.** Every fix you make gets one. The conventions: inline `#[cfg(test)]` modules, several
collected in per-module `test/` subdirectories (`windows_utils/streamer/test/`,
`windows_utils/audio/test/`). Pure logic — rect unions, coordinate transforms, format parsing,
period arithmetic, tier ordering — is unit-testable with no hardware and should be. Hardware paths
get an `#[ignore]`d reporter like the ones above, so the next person with the machine can take the
reading in one command.

**The final production build gate.** After all feature work, and immediately before writing the
report:

```powershell
pnpm tauri build
```

**The one failure you may accept** is the Tauri updater signing-key error — the private key for the
updater manifest is not on this machine and that is expected. **No other failure is acceptable.**
Zero cargo errors, zero TypeScript errors, zero linker errors, zero missing-resource errors, zero
bundling errors. If the build fails any other way, it is not done — go fix it. Do not write the
report over a broken build.

Also run and pass, from the repo root:

```powershell
pnpm lint ; pnpm build ; node scripts/check-legacy-webkit.mjs
node scripts/check-bindings.mjs ; node scripts/check-versions.mjs
```

Then confirm the tree is clean and reviewable: nothing half-finished, no debug scaffolding in
shipping paths, no commented-out experiments, every temporary developer override either kept
deliberately (and documented) or removed.

---

## 12. Deliverables and the final report

### 12.1 What must be in the repo when you finish

- `docs/OPTIMIZATIONS.md` — every row this guide owns moved to `VERIFIED` with numbers, or reverted
  with a reason; new rows for everything you changed; recorded rejections **with numbers** for
  everything you tried and did not keep.
- `docs/CAPABILITY-TIERS.md` — every tier row in §5.2 carrying a real answer for this band.
- `docs/BASELINE.md` — a new, clearly labelled section for this machine class, with Matrices A, B
  and C filled, and the `unmeasurable` DXGI cells replaced by numbers.
- `docs/manual-tests/windows-capture.md` and `windows-audio-tiers.md` — updated with what you
  actually found, including any step whose expected reading was wrong. `core-closeout.md` §3 and §6
  likewise.
- `docs/discovery/windows-legacy-machine.md` — the machine facts and the discovery output.
- `docs/research/` — the Phase 4 reports.
- `findings/` — **empty.** Anything not done is deleted with the reason recorded in the report.
- A local branch `guide/windows-legacy` with small, single-purpose, measured commits. **Nothing
  pushed.**

### 12.2 The report

Write it as the last thing. It is the artifact the user reads first.

1. **Build result, at the top.** Which machines you built on, that `pnpm tauri build` succeeded, and
   that the only error encountered was updater signing.
2. **Machine facts** — exact OS build (from `winver`) and UBR, model, CPU, both GPUs and their
   driver versions, audio device and driver, Wi-Fi adapter and driver, display language, AP model
   and band, client devices and their browser versions. Enough for someone to reproduce.
3. **Per-item status**, one line each, against §8's numbering L-01 … L-21:
   `VERIFIED (numbers) | reverted (reason) | still UNVERIFIED (what was missing)`. **No item may be
   silently absent.**
4. **Before/after numbers** for every change, as p50 / p90 / p99 / max, with the trial count and
   whether the machine was quiet (state the concurrent `rustc` count).
5. **The WGC-vs-DXGI comparison on identical hardware** — its own section. This is the headline
   result of the run and the only place in the project where it exists.
6. **The ledger diff** — the exact rows added or edited in `docs/OPTIMIZATIONS.md` and
   `docs/CAPABILITY-TIERS.md`, and the `docs/BASELINE.md` cells filled.
7. **Findings** — created, resolved-and-deleted, and retagged, by filename.
8. **Comparison against the competitor table** in §5.4, and against the 26200 numbers in §5.3.
9. **Decisions Made Without Consultation** — every judgment call the user might have made
   differently, with your reasoning, so each can be reviewed and reversed. Temporary developer
   overrides you chose to keep belong here.
10. **Side work** — every bug found and fixed, dead code removed, doc corrected, test added, that
    was not in this guide's list. What it was, why it was worth doing, where it lives.
11. **What could not be done and why** — honestly, including negative results, which are results.
    Anything that needed a machine you did not have goes here with the exact reading that would
    close it.
12. **Recommendations requiring the user** — anything in Tier C that your measurements say should
    change, written as a concrete proposal with the number behind it; any UI-visible optimisation
    you declined to make; anything you judged too risky to do unsupervised.
13. **Commits** — branch name and one line per commit. Confirm nothing was pushed.
14. **`findings/` is empty**, and a list of anything deleted without being done, with why.
