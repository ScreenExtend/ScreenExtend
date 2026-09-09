# Guide: Windows + AMD — verify, fix and hyper-optimize the AMF H.264 backend

**Slug:** `windows-amd` · **Branch to work on:** `guide/windows-amd` · **Base:** `latency-overhaul`

---

## 0. What this is, and the machine contract

ScreenExtend is a Tauri 2 desktop app that turns any device with a web browser into a wireless
second monitor. It runs on the **host** (the PC being extended). A **client** (phone, tablet,
laptop) joins by opening a URL or scanning a QR code — no client install. The host creates a real
virtual display sized to that client, GPU-encodes it as H.264, and streams it over WebRTC (WHEP
signalling) to a dependency-free browser page the host's own axum server serves. Two frontends
live in the repo: `src/` (the React desktop control UI, runs in the Tauri webview on the host) and
`src-tauri/src/streamer/static/` (the client page — plain HTML/JS/CSS, `include_str!`'d into the
binary, **no bundler, no dependencies**). The Rust core is `src-tauri/src/`, with three
`cfg`-gated sibling platform modules (`windows_utils/`, `macos_utils/`, `linux_utils/`) meant to
expose the same surface. **`linux_utils/` does not compile**, implements only part of the command
set, and several of its stubs report success rather than "unsupported"; Linux is declared out of
scope and no guide covers it.

**This guide owns one thing: the AMD AMF H.264 encoder backend.**

That backend — `src-tauri/src/windows_utils/streamer/amd/`, `amd_sys/`,
`test/amd_layout.rs`, `tools/amf_layout_gen.c`, `resources/amf-headers/`,
`windows_utils/streamer/amd.md`, `docs/manual-tests/amd-amf.md` — is roughly 3,600 lines of Rust
that **has never encoded a single frame.** Neither development machine on this project has an AMD
GPU. The FFI is asserted line by line against numbers a C compiler measured from the real AMD
headers, and the graceful-absence path (no `amfrt64.dll`) is unit-tested — but every behavioural
claim in it is `UNVERIFIED`, and the ledger says so in three separate places. Before it existed,
*every* Radeon and Ryzen-iGPU host on earth fell through to software x264, the slowest path in the
product. The code that is supposed to fix that has never run.

Your job, in order:

1. **Prove the machine qualifies** (§0.3) and take a discovery snapshot of what this hardware
   actually exposes (§6).
2. **Take a baseline** on this machine with the shipped binary, before you change anything (§7).
3. **Audit and functionally verify the whole backend** — session creation, caps, the ULL/LL usage
   tiers, NV12 vs BGRA input, the zero-copy D3D11 texture path, the texture ring, submit/query,
   bitrate update without reset, the repeat-frame path, transient-error classification,
   intra-refresh, LTR, slice mode, VUI colour signalling, the concurrent-session cap, the x264
   fallback — and **fix every defect you find** (§8).
4. **Hyper-optimize it to NVENC parity or better** — every AMF knob, every technique the NVENC
   backend uses, and parallelization across the whole pipeline on this machine (§10), all
   measured against the NVENC/Quick Sync/x264 numbers in `docs/BASELINE.md`.
5. **Flip the ledger rows to `VERIFIED` with numbers**, or revert them with reasons, and report
   (§12).

Everything outside the AMD territory in §2.3 has already been audited, optimized and verified on
this project's two development machines. Do not touch it.

### 0.1 Minimum OS: Windows 10 version 2004 (build 19041). No maximum.

- **Floor 19041** because that is the app's own supported floor: `IsCursorCaptureEnabled` on
  `GraphicsCaptureSession` is a 19041 property, the WGC capture path assumes at least that, and
  the bundled indirect-display driver targets it. Nothing in AMF itself requires anything newer.
- **No ceiling**, deliberately. The AMF question is the GPU and the driver, not the OS: a Windows
  11 25H2 box with a Radeon is a *better* machine for this guide than a Windows 10 one, because it
  removes capture-path variables the `windows-legacy` guide owns. If you happen to be on Windows
  10 or Windows 11 pre-24H2, note the build in your report and expect the DXGI/WGC tiering
  differences described in `docs/CAPABILITY-TIERS.md` — but **do not chase them here**, they
  belong to another guide.

### 0.2 Required hardware

| Requirement | Why |
|---|---|
| An **AMD GPU with a VCN/VCE AVC (H.264) encoder** — any Radeon or Ryzen APU since Raven Ridge / Vega, **except Navi 24** (RX 6400 / RX 6500) | Navi 24 has the video *decoder* but AMD removed the AVC encode block. On Navi 24 `CreateComponent(AMFVideoEncoderVCE_AVC)` failing is **correct behaviour, not a bug** — and you cannot do this guide's work. |
| The **Radeon / Adrenalin driver** installed | `amfrt64.dll` — the AMF runtime — ships with the driver and nowhere else. There is no redistributable. |
| `ffprobe`, `ffplay`, `ffmpeg` on `PATH` | Bitstream conformance, colour tags, SSIM/PSNR A/B. |
| A phone or tablet on the same network, with a modern browser | Every end-to-end item (join, refresh, kick, multi-session). |
| The Rust toolchain, Node ≥ 20, `pnpm` | Building and gating. |
| MSVC Build Tools 2022 (x64) — **optional but strongly preferred** | Only needed if you regenerate the ABI assertions (`tools/amf_layout_build.bat`). `dumpbin` also comes from here and is used by A-02. |

**Two GPUs is a bonus, not a requirement.** If your machine is an **Intel iGPU + Radeon dGPU**
laptop you additionally unlock item A-12 (cross-adapter AMF), which nothing on this project can
otherwise reach. If it is an all-AMD box (Radeon dGPU, or a Ryzen APU with only its iGPU), that is
the *primary* target: display, capture and VCN are one adapter and the same-adapter zero-copy path
is what the backend was written for. Say which you have in the report. If you have access to
**both a pre-RDNA part** (Polaris / Vega / RDNA1 — RX 580, Vega 56, RX 5700) **and an RDNA2+
part**, run the whole guide on both: item A-03 (the ULTRA_LOW_LATENCY → LOW_LATENCY fallback) has
two branches and the pre-RDNA one is the only place the second branch has ever been reachable.

### 0.3 Preflight — prove the machine qualifies before you start

Run all of this and paste the output into your discovery note (§6.2). Any failure here means
**stop and report**, not "work around it".

```powershell
# 0. OS build. Must be >= 19041.
[System.Environment]::OSVersion.Version
Get-ComputerInfo | Select-Object OsName, OsVersion, OsBuildNumber, WindowsVersion

# 1. The GPU and its driver. AdapterCompatibility must name AMD/ATI on at least one row.
Get-CimInstance Win32_VideoController |
  Select-Object Name, DriverVersion, DriverDate, AdapterCompatibility, AdapterRAM

# 2. The AMF runtime. If this errors, there is no AMF on this box: reinstall the Adrenalin driver.
Get-Item C:\Windows\System32\amfrt64.dll | Select-Object Length, LastWriteTime, VersionInfo
# Note: do NOT trust the DLL's ProductVersion resource. A 2025 driver shipped it as
# "1.5.0.0;1.2;CL". AMFQueryVersion (step 4) is the only version that means anything.

# 3. ffmpeg family on PATH
ffprobe -version | Select-Object -First 1
ffplay  -version | Select-Object -First 1
ffmpeg  -version | Select-Object -First 1
```

Then, from the repo:

```powershell
cd src-tauri

# 4. The ABI + loader tests. 13 tests, all must pass. On THIS machine
#    `amf_loader_handles_a_missing_runtime` takes its other branch: the DLL is present, so it
#    asserts AMFQueryVersion returns non-zero. That is the first real signal the FFI works.
cargo test --lib windows_utils::streamer::test::amd_layout -- --nocapture

# 5. Vendor id, straight from DXGI. There must be a non-software adapter with vendor_id 0x1002.
#    The `probe --capture` run prints one `DXGI adapter (index=…, name=…, vendor_id=0x…,
#    is_software=…)` line per adapter on its way in.
cargo run --release --features probe -- probe --capture preflight.png
```

Finally, the single decisive check — does AMF exist, is it hardware, and does it encode:

```powershell
cargo run --release --features probe -- probe --encode out_amd.h264 --encoder amd
```

**The order of those arguments matters.** `--encode` takes the *next* token as its output path
only if that token does not start with `-`; `probe --encode --encoder amd out_amd.h264` silently
writes to the default `out.h264` and ignores the trailing filename. (`docs/manual-tests/amd-amf.md`
gets this wrong today — see defect D-05.)

Expected, in order:

```
AMF runtime version 1.5.x.x
DXGI adapter (index=0, name=…, vendor_id=0x1002, is_software=false)
selected AMD adapter for AMF (index=0, name=…)
AMF AVC caps: accel=hardware(VCN/VCE), max_bitrate=…, streams=…, hw_instances=…, max_profile=…,
              max_level=…, max_refs=…, b_frames=…, pre_analysis=…, query_timeout=…,
              slice_output=…, color_conversion=…, throughput=…/… MB, input=…x……x…,
              bgra_native=Some(…), nv12_native=Some(…)
AMD AMF H.264 encoder initialized (D3D11 / ULTRA_LOW_LATENCY): 1920x1080@60, …
encoded frame=0 (au_bytes=…, total_bytes=…)
…
wrote Annex-B H.264: path=out_amd.h264, frames=300, total_bytes=…
```

| What you see instead | What it means | Do |
|---|---|---|
| `loading amfrt64.dll …` error | No AMF. Error 126 also appears on minimal driver installs and boxes missing the VC++ runtime. | Reinstall the full Adrenalin driver, then retry. If it still fails, **stop** — this machine cannot run the guide. |
| `no AMD adapter found` | The `VendorId == 0x1002` match failed. | Paste every `DXGI adapter` line into the report. If a Radeon is present and still not matched, that is a first-class defect: fix it and record it. |
| `this adapter has no AMF H.264 encoder` | `CreateComponent` refused. **Expected and correct on Navi 24.** | If you are on RX 6400/6500, stop: wrong hardware. Otherwise this is a defect — investigate. |
| `AMF reports acceleration type software(cpu)` | The backend refuses on purpose (x264 is the better software path). | Record it; then find out *why* AMF thinks this adapter has no hardware AVC. |
| It initializes but `probe --encode` produces 0 bytes or hangs 2 s and bails | You have found the first real bug in a backend that has never run. Excellent — that is the job. | Go to §8. |

**The single most valuable line in this entire guide is the `AMF AVC caps:` dump.** It answers,
on hardware, every question the research could only guess at: whether `QueryTimeout` is honoured,
whether slice output exists on this VCN, whether BGRA and NV12 are *native* input formats, how many
hardware instances and streams the part advertises, and how much VCN throughput other applications
have already taken. Capture it verbatim, for every configuration you test.

---

## 1. Model and orchestration directives

**Every subagent you spawn runs on Claude Opus 5.** No exceptions, no "this one is simple". If a
subagent spawns its own, those are Opus 5 too.

**You are running fully autonomously. There is nobody to ask.** The person who will read your
report has no access to this environment while you work. A question asked mid-run is a question
that never gets answered; it just stalls the work.

- **Never ask and wait. Decide.** Where anything in this guide, in `CLAUDE.md`, or in the ledgers
  says "propose" or "ask", that instruction is superseded: make the best call on the evidence, act
  on it, and write it down. Every such call goes in the report under **Decisions Made Without
  Consultation**, so it can be reviewed and reversed.
- **When in doubt, build it.** A finished implementation takes a minute to delete. A deferred idea
  costs a future agent a whole re-research cycle, and by then nobody remembers why it seemed good.
  You are holding the context now. Spend it.
- **Bias toward producing measured evidence.** When a call is close, prefer the option that ends
  in a number.
- **Fix what you find.** A bug, a stale comment, a dangling doc reference, a missing test — inside
  this guide's territory (§2.3), fix it and note it. Do not hoard findings for a list.
- **Parallelize hard.** Spawn Opus 5 subagents for independent work: research, reading, review
  passes, doc updates. Run measurement passes concurrently only where the hardware allows —
  **never two release builds at once, and never two probe runs at once** (they contend for VCN and
  for the virtual display; see §3).
- **Self-unblock.** Missing information is a research task. Missing tooling is an install decision
  you make. A failed approach is data: record it and try the next one.
- **Leave the tree reviewable.** No half-finished refactors, no debug scaffolding in shipping
  paths, no commented-out experiments. Anything incomplete is finished, reverted, or isolated and
  called out.
- **Commit at least every 30 minutes**, small and single-purpose. A performance commit states its
  measured before/after in the message.

Suggested agent partition is in §8.6. Keep concurrency at **≤ 8** working agents and tell every one
of them not to spawn helpers.

---

## 2. Absolute rules

### 2.1 Git

- **Work on a local branch `guide/windows-amd`, branched from `latency-overhaul`.**
- **NEVER `git push`. Never touch `origin`.** Nothing on the remote may change, ever, for any
  reason. The repository owner reviews your branch and pushes it themselves.
- Never `git add -A`. Add your own files by explicit path.
- Every commit message ends with exactly:

  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: <the URL of your own session>
  ```

### 2.2 No UI change

The React desktop control UI (`src/`) and the client page (`src-tauri/src/streamer/static/`) keep
their current look, element structure, layout and behaviour. No redesigned screens, no restyled
components, no added or removed controls. This is a constraint on **appearance**, not on
performance — but nothing in this guide should need to touch either frontend at all. If you find a
change that would improve AMD hosts but is UI-visible, do not make it: write it up in
`docs/OPTIMIZATIONS.md` under "UI-visible recommendations" and in your report.

### 2.3 Scope fence — the only files you may modify

**In scope (yours):**

| Path | Notes |
|---|---|
| `src-tauri/src/windows_utils/streamer/amd/**` | `mod.rs`, `encoder.rs` — the encoder. |
| `src-tauri/src/windows_utils/streamer/amd_sys/**` | `mod.rs`, `types.rs`, `vtbl.rs`, `props.rs` — the FFI. |
| `src-tauri/src/windows_utils/streamer/test/amd_layout.rs` | The ABI + behaviour assertions. New AMD tests go here or in a sibling `test/amd_encoder.rs` you may create (mirror `test/nvenc_encoder.rs`). |
| `src-tauri/tools/amf_layout_gen.c`, `amf_layout_cpp_check.cpp`, `amf_layout_build.bat`, `tools/README.md` | The ABI generator. Never hand-edit the generated bodies in `amd_layout.rs` — regenerate. |
| `src-tauri/resources/amf-headers/**` | Vendored MIT headers. Changing them means re-running the generator and updating `resources/PROVENANCE.md` + `SHA256SUMS`. |
| `src-tauri/src/windows_utils/streamer/amd.md` | The design doc. Keep it truthful. |
| `src-tauri/src/windows_utils/streamer/pipeline.rs` | **Only where the AMF arms live**: `Backend::Amf` / `Backend::AmfCpu` match arms, `try_amf`, `cpu_bridge_format`, `scaler_output_format` / `needs_scaler` *for the AMD variants only*, and any new AMD-specific ring/thread you add. Do not change NVENC, Intel or x264 behaviour. |
| `src-tauri/src/windows_utils/compatibility.rs` | **Only the AMD reporting** in `probe_encoder()` (see D-01). |
| `docs/manual-tests/amd-amf.md` | The manual script — fix it, extend it. |
| `docs/OPTIMIZATIONS.md`, `docs/CAPABILITY-TIERS.md`, `docs/BASELINE.md`, `docs/RISK-REGISTER.md` | Ledgers. Append and edit **only AMD rows**. |
| `findings/` | The work queue. Yours to create and delete. |

**Out of scope (already verified elsewhere; a change here is a merge conflict against work you
cannot see):** `nvidia/**`, `intel/**`, `x264/**`, `capture.rs`, `dxgi/**`, `windows_capture/**`,
`scaler.rs`, `tuning.rs`, `clock.rs`, everything under `src-tauri/src/streamer/` except reading it,
the client page, `src/`, all of `macos_utils/`, `linux_utils/`, `cli.rs`, `lib.rs`, `tauri.conf.json`.

**A named exception that has already been spent.** `serve` now takes
`--encoder <auto\|nvidia\|intel\|amd\|software>` — declared in `tauri.conf.json`'s
`plugins.cli.subcommands.serve.args` with `possibleValues`, read in `cli.rs::run_serve`, stored in
`AppState::encoder_vendor` — so you can force AMF in a live host session on a machine that also has
another adapter. It was added under this exception (D-06) and it is purely additive: absent the
flag, nothing changes, and `--software-encode` remains the alias the desktop UI's GPU toggle sets.
**Do not extend the exception any further.** If you find you need another `cli.rs` or
`tauri.conf.json` change, write the finding and use `probe --live --encoder amd` instead.

### 2.4 Repository invariants that bind this work

From `CLAUDE.md`; these are build breaks or shipped crashes, not style.

1. **Nothing may link against AMF.** `amfrt64.dll` is loaded with `libloading` at runtime and
   exactly two exported symbols are resolved (`AMFInit`, `AMFQueryVersion`); everything else goes
   through COM-like vtables. `dumpbin /imports ScreenExtend.exe | findstr /i amf` must print
   **nothing**. If `amfrt64` ever appears in the import table, the binary refuses to start on every
   machine without a Radeon driver. This is the Windows analogue of the macOS `nm -u` rule, and it
   is item A-02.
2. **Probe capabilities, never version numbers.** Every tier is selected by attempting the real
   capability and falling back at runtime without crashing. `AMFQueryVersion`'s value is logged for
   support and drives nothing. `AMFInit` is called with a deliberately **low** version
   (`AMFINIT_VERSION = AMF_MAKE_FULL_VERSION(1, 4, 30, 0)`, Adrenalin 23.5.2) because AMF's
   contract is that a newer runtime serves an older client and never the reverse.
3. **Real-time threads take no locks and do not allocate.** The video capture callbacks and any
   encode thread you add talk to the async world over `crossbeam-channel` only. If your
   optimization introduces an allocation in a capture callback, it is not an optimization.
4. **`std::sync::Mutex` is the one blocking mutex** for first-party code, and a `MutexGuard` is
   **never** held across an `.await`. Lock, copy, drop, then await. Watch the `if let` / `match`
   scrutinee footgun, where the guard lives to the end of the block. `parking_lot` stays confined
   to the vendored `windows_utils/windows_capture/` fork.
5. **`tprintln!` / `teprintln!`, never `println!` / `eprintln!`,** in the Rust core. They push into
   the 2000-line ring backlog, emit a `LogLine` event to the UI, and only echo to the terminal in
   verbose mode. `ScreenExtend logs --lines 400` reads the backlog.
6. **Never delete a legacy path because it is slow — make it as fast as *it* can be.** A GCN part
   on `LOW_LATENCY` with `LatencyConstrainedVBR` deserves the same obsessive tuning as an RDNA3
   part on ULL. "The new tier is faster" is never a reason to drop the old one.
7. **A new Tauri command goes into all three `*_utils` modules and `collect_commands!`**, and
   `src/lib/bindings.ts` is generated — never hand-edit it. You should not need a new command; if
   you do, that is a scope-fence conversation with yourself, recorded.
8. **A new shell invocation goes into `src-tauri/capabilities/default.json`** with argument
   validators, or it fails at runtime.
9. **Simultaneous displays are uncapped.** `VirtualDisplayController::max_concurrent_displays()`
   returns `None` on Windows. Never evict a connected device to make room — including when you
   discover the VCN session cap in A-11. A refusal must be *reported*, not paid for by dropping
   someone.
10. **No AI-tooling references in comments or docs.** (The old `amd.md` had one; it is gone. Keep
    it that way.)

---

## 3. Norms of research and measurement

- **Research first, and prefer primary sources.** AMD's AMF SDK docs and the vendored headers, the
  GPUOpen AMF issue tracker, and the source of OBS, FFmpeg, Sunshine and Moonlight. **If one
  shipping product does the thing, it is possible.** "Impossible" is only allowed after you have
  searched, found nobody doing it, *and* tried it and measured. Web search and Deep Research are
  available and you are expected to lean on them hard (§9 is the agenda).
- **Measure before and after every change**, as **p50 / p90 / p99 / max** — never a bare mean.
  Jitter is felt more than the average, and a creeping max over a five-minute run is where latency
  bugs hide. Nanosecond stamps come off the shared host clock
  (`crate::streamer::audio::host_now_ns`), which is the same epoch the video RTP timestamps ride.
- **Never trust a flag without a read-back.** Every property you set must be read back and logged;
  every tier that is selected must announce itself. This codebase has been bitten repeatedly:
  `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` reads backwards from its meaning; x264's
  `encoder_open` returning non-null for `X264_CSP_BGRA` means "High 4:4:4 Predictive", not "BGRA
  supported"; and AMF's own `QueryTimeout` returns `AMF_OK`, echoes back from `GetProperty` and
  does nothing when set after `Init()`. Assume nothing.
- **Measurement hygiene.**
  - ≥ 3 trials per cell; report the median trial with its p50/p90/p99/max.
  - Check for competing builds and competing GPU work first. `cargo build` running in another
    worktree invalidates a measurement.
  - **Leave ≥ 15 s between probe runs.** A probe that exits without tearing down leaves its
    virtual display attached and silently corrupts the next run's capture — this bit 4 of 12 runs
    during the original overhaul.
  - Say whether the machine was quiet, and what the power plan / GPU clocks were. Radeon parts
    clock down aggressively; a first-run number on a cold GPU is not a steady-state number.
  - Record the Adrenalin version with every result. AMF behaviour is driver-version-dependent in
    ways that are the entire point of item A-03.
- **Token and time cost are not constraints.** Every optimization that shaves even **0.1 ms** on
  this hardware is in scope and gets *implemented and measured*, even at high implementation cost.
  "Good enough", "diminishing returns", "minor" and "speculative" are not reasons to stop. A
  speculative idea is resolved by trying it and measuring, or by a **recorded rejection with a
  reason and a number** in `docs/OPTIMIZATIONS.md` — never by being skipped.

---

## 4. The territory: repo map, harness, ledgers, gates

### 4.1 The code

```
src-tauri/src/windows_utils/streamer/
  amd/
    mod.rs              the module root: `pub mod encoder; pub mod sps;`
    encoder.rs          THE backend. Read it in full before anything else.
    sps.rs              the SPS VUI rewriter (row 304): finds the SPS in the access unit and in
                        the cached ExtraData, unescapes, writes BT.709 + video_full_range_flag +
                        bitstream_restriction_flag/max_num_reorder_frames=0, re-escapes, splices.
                        Gated on a read-back of the SPS AMF produced. 8 unit tests. See 10.3.
  amd_sys/
    mod.rs              libloading of amfrt64.dll; AMFInit/AMFQueryVersion; the property
                        helpers (set_property / get_property / has_property / release /
                        query_interface); `classify()`, the error taxonomy.
    types.rs            ~610 lines: AMF_RESULT, AMF_SURFACE_FORMAT, the AVC enums,
                        AMFVariantStruct, AMFINIT_VERSION, AMF_SECOND, acceleration types.
    vtbl.rs             ~591 lines: one #[repr(C)] struct per vtable, in exact slot order.
                        SLOT ORDER IS THE ABI.
    props.rs            The wide-string property-name catalogue. These strings ARE the API.
  test/amd_layout.rs    ~1,567 lines, 13 tests. Generated bodies + hand-written behaviour tests.
  pipeline.rs           `Backend::Amf` / `Backend::AmfCpu`, `try_amf`, `build_backend`,
                        `encoder_ladder`, `cpu_bridge_format`,
                        `encode_gpu`/`encode_cpu`/`encode_repeat`, `is_transient_encode_error`,
                        `AsyncEncodeRing` over `RingEncoder::{Nvenc, Amf}` and
                        `RingTextures::{Shared, Local}` (row 302 — it is no longer NVIDIA-only),
                        `spawn_repeater`, `synthetic_pattern_loop`.
  mod.rs                `Vendor::from_pci_id`, `AMD_PCI_VENDOR_ID = 0x1002`, `select_vendor`,
                        `device_vendor`, `report_adapter_pairing`.
  scaler.rs             ID3D11VideoProcessor: scale + BGRA→NV12 in one blt. `with_format`,
                        `ReadbackFormat::{Bgra8,Nv12}`, `scale_into`, `read_back`. READ ONLY.
  nvidia/encoder.rs     THE REFERENCE IMPLEMENTATION. Read it in full. READ ONLY.
src-tauri/tools/        amf_layout_gen.c + amf_layout_build.bat + README.md
src-tauri/resources/amf-headers/   vendored MIT AMF headers (1.5.2.0)
```

Everything else you need to *read* (never modify): `src-tauri/src/streamer/config.rs`
(`EncoderVendor`, `H264Profile`, `Config`), `streamer/telemetry.rs` (the stage histograms),
`streamer/repair.rs` + `keyframe.rs` (the LTR/PLI machinery — see §10.4),
`streamer/webrtc_session.rs` (the hand-rolled H.264 packetizer, which is why an SPS rewrite is
even possible), `windows_utils/streamer/test/h264.rs` (a working Annex-B / SPS / VUI parser you
should reuse in tests).

### 4.2 The measurement harness

`probe` is a developer subcommand intercepted in `cli.rs::dispatch` before the Tauri CLI plugin
sees it; its ~25 tuning flags are parsed by hand in `streamer::config::Config::from_args`. It runs
*inside* the Tauri app, because `--live` and `--whep-selftest` need a real virtual display.

```sh
# BUILD ONCE, MEASURE MANY. --features probe adds the WHEP self-test (off by default because
# its TLS verifier accepts any certificate and must never ship).
cargo build --release --features probe

# The whole host path: real WHEP join over loopback against a real virtual display, per-stage
# telemetry printed on exit. THE headline cell.
ScreenExtend probe --whep-selftest --duration 30 --encoder amd --capture-backend wgc

# Real content through the real capture path, to a file. --workload paints a repeatable
# generated scene into a borderless window covering the captured display, so capture, scaler
# and encoder all run on content you control.
ScreenExtend probe --live out.h264 --duration 30 --encoder amd --workload motion

# Glass-to-glass overlay: a large frame number + elapsed-ms clock painted into the encoded
# frames, for the 240 fps camera method (docs/BASELINE.md "Glass-to-glass procedure").
ScreenExtend probe --live --duration 60 --workload motion --counter --encoder amd

# Encoder smoke: 300 synthetic BGRA frames -> Annex-B. NOTE: this path builds its own AMD
# D3D11 device (Encoder::new), NOT the capture device, and feeds encode_bgra_padded. It does
# NOT exercise the same-adapter zero-copy path or NV12.
ScreenExtend probe --encode out_amd.h264 --encoder amd

# Adaptive-bitrate reconfigure path
ScreenExtend probe --bitrate --encoder amd

# Single-frame capture / DXGI smoke
ScreenExtend probe --capture cap.png
ScreenExtend probe --dxgi dxgi.bmp

# From a running host
ScreenExtend serve -v            # one summary line per session every 5 s
ScreenExtend status --json       # the same counters, per session, from another shell
ScreenExtend doctor --json       # capability report + per-probe timings
ScreenExtend logs --lines 400    # the 2000-line ring backlog
```

**What a correct reading looks like.** `probe --whep-selftest` prints a per-stage table on exit
with these stages, from `streamer::telemetry`: `capture_delivery`, `scale`, `encode_queue`,
`encode`, `packetize`, `socket_write`, `pace_wait`, `capture_to_encode`, `capture_to_socket`. The
one to compare against `docs/BASELINE.md` is **`capture_to_socket`**. `status --json` additionally
carries `encoder` — the backend name string, which for you must read `amd-amf-same-adapter` or
`amd-amf-cpu-bridge` (from `Backend::name()`). If it reads `software-x264`, AMF did not load and
every other number is meaningless.

**Two harness facts you must know before you trust a number:**

- `probe --live` forces `synthetic_pattern = false` and paints the workload onto the real display,
  so it goes through `Pipeline::start` → `build_backend` → your AMF backend. Good.
- `probe --synthetic-pattern` (without `--live`) goes to `synthetic_pattern_loop`, which used to
  **hardcode NVENC** and ignore `--encoder` entirely. It now builds a `Backend` by walking
  `encoder_ladder(vendor)` and encodes through `Backend::encode_cpu(.., width * 4, Bgra8, ..)`
  (row 301, D-03). The fall-through was verified on the NVIDIA box; **the AMD rung actually
  encoding is what you verify.**

### 4.3 Ledgers

- **`docs/OPTIMIZATIONS.md`** — one table row per change attempted:
  `| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |`. Failures are
  recorded on purpose. Status vocabulary:
  - **`VERIFIED`** — executed and measured on the machine.
  - **`COMPILE-CHECKED`** — builds and passes static/layout tests, never executed.
  - **`UNVERIFIED`** — written from research; no runtime signal at all.

  Your job is to move the AMD rows to `VERIFIED` *with numbers*, or to revert them *with reasons*.
  Append new rows with the next free number; do not renumber existing ones.
- **`docs/CAPABILITY-TIERS.md`** — one row per subsystem × tier:
  `| Subsystem | Tier (newest first) | Probe | Era / platform | Expected delta | Status |`. Three
  rows are yours (§5.2).
- **`docs/BASELINE.md`** — the pre-change numbers. It currently has an **AMD** row in
  "What could not be measured" reading *"No AMD GPU on either machine."* You are the person who
  deletes that row and fills in a real matrix.
- **`docs/RISK-REGISTER.md`** — R-011, R-228, R-231 (the AMD half), R-315/R-317 (the `amd.md`
  hygiene items) are yours to close.
- **`docs/manual-tests/amd-amf.md`** — the script. It has 11 sections and several errors (D-05).
  Fix it as you go so it is correct for the *next* Radeon machine.
- **`docs/TELEMETRY.md`** — how to read every counter. Read it; extend it only if you add an
  AMD-specific counter.

### 4.4 `findings/` — the work queue and the crash-recovery point

`findings/` at the repo root is gitignored (through `.git/info/exclude`, never `.gitignore`).
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

**Lifecycle: a file is deleted the moment its finding is resolved** — implemented and verified, or
deliberately rejected with the reasoning recorded in `docs/OPTIMIZATIONS.md` *first*. New
discoveries become new files. Nothing may live only in an agent's context. At the end of this
guide, every finding you created must be gone, or listed in the report with the reason it stayed.

**`findings/` is essentially empty when you start, and that is deliberate** — the queue was worked
down to nothing before this guide was written, and every AMD finding was closed by the change series
recorded as rows 298–304. Read whatever is there once; it is unlikely to be yours. Files under
`findings/_out-of-scope/` (Linux) never are, and `findings/_briefs/` and `findings/_STATUS.md` are
historical orchestration notes rather than a queue. Two AMD findings that comments used to point at
— `major-amd-nv12-input-pending-scaler.md` and
`minor-amd-amf-does-not-write-vui-colour-signalling.md` — were resolved and deleted, and the dangling
pointers were cleaned up with them (D-04).

### 4.5 Gates before every commit

From `src-tauri/`:

```powershell
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

From the repo root, only if `src/` or `src/lib/bindings.ts` changed (it should not):

```powershell
pnpm lint
pnpm build
node scripts/check-legacy-webkit.mjs
node scripts/check-bindings.mjs
node scripts/check-versions.mjs
```

And the AMD-specific gates:

```powershell
cargo test --lib windows_utils::streamer::test::amd_layout
dumpbin /imports .\target\release\ScreenExtend.exe | Select-String -Pattern amf   # must be empty
```

Run the client-page harnesses once before your first commit, so a red result is never attributed to
you later — nothing in this guide touches the client page:

```powershell
pnpm test:client        # scripts/test-client.mjs, seven harnesses
```

Note that builds land in whatever `$CARGO_TARGET_DIR` is set for the tree; if you use a worktree,
give it its own target dir so builds do not contend.

---

## 5. Current state of this area

### 5.1 What has landed, and what it claims
**A note on the `W…` labels you will meet in the ledgers, in the risk register and in this file.**
The engagement that produced this repository's current state ran as numbered parallel workstreams:
`W1` transport · `W3a`/`W3b` Windows capture and encode · `W4`/`W4b` AMD · `W5` macOS capture and
encode · `W6` macOS permissions · `W7` macOS audio · `W8` Windows audio · `W9` Windows platform ·
`W10` input · `W11` the client page · `W12` the desktop UI · `W13a`/`W13b` build, CI and hygiene ·
`W14` AirPlay · `W15` tests. They are **provenance labels only** — they tell you which change series
a row came from so a ledger note reads in context. They are not people, none of them is still
running, nothing is waiting on one, and **every one of them has merged into `latency-overhaul`.**


> **Update, W4b — merged into `latency-overhaul` at `b333707`.** Every defect in §8.2
> (D-01…D-08) and the missing ring in §10.2 have been **implemented**, `COMPILE-CHECKED`, on a
> machine with no AMD GPU. Your job on those items is now **verify**, not implement — the
> verification steps in each are unchanged and still the point. Ledger rows 298–304 record what
> landed; §10.3 (the SPS VUI rewrite) and §10.4 (LTR-driven repair) are built and gated on
> capability read-backs. What is still open here is everything that needs a Radeon: §7's
> baseline, §10.1's remaining rows, §10.6's knob sweep, and A-01…A-13.

The whole backend landed as workstream **W4** of the latency overhaul and is
`docs/OPTIMIZATIONS.md` **row 4**:

> New `Backend::Amf` / `Backend::AmfCpu`: a hand-written AMF FFI (`amd_sys`) plus a
> latency-optimal AVC encoder, inserted into the `Auto` chain between Intel and x264.
> `USAGE = ULTRA_LOW_LATENCY` (with the mandatory `LOW_LATENCY` fallback),
> `QUALITY_PRESET = SPEED`, CBR with peak == target, `VBV = bitrate/fps`, no B-frames, no
> pre-encode/pre-analysis/VBAQ, `IDRPeriod = i32::MAX` with on-demand IDR, one slice,
> `QueryTimeout = 1 ms` set pre-`Init`, one frame in flight.
> **Before:** not measured — no AMD hardware on either dev machine. **After:** not measured.
> **Status: UNVERIFIED.**

The other AMD-touching ledger rows:

| Row | What | Status today |
|---|---|---|
| **4** | The whole backend | **UNVERIFIED** — FFI COMPILE-CHECKED by 13 assertions; no frame ever encoded |
| **108** | `Scaler::with_format` can emit NV12: blt 5→4 µs, readback **1192 → 740 µs**, 3,686,400 → 1,382,400 bytes at 1920×1080→1280×720 | VERIFIED (for the scaler itself) |
| **245** | `encode_nv12_padded` on the AMF CPU bridge; colour range as a function of input format; `OutMatrixCoeff`/`OutColorProfile` set | **UNVERIFIED** |
| **246** | Sub-frame / slice-mode output evaluated and rejected on NVENC (`sliceMode`/`enableSubFrameWrite`), AMF (`OUTPUT_MODE = SLICE`) and x264 (`nalu_process`) | VERIFIED *for the NVENC ceiling*; the AMD arm is an inference — **re-open it here** (item A-13) |
| **278** | The AMF backend stays UNVERIFIED, manual script extended from 9 to 11 sections | UNVERIFIED by nature |
| **279** | NVENC's transient encoder-busy now says "device busy", the phrase `pipeline::is_transient_encode_error` matches on | COMPILE-CHECKED |
| **298** | `compatibility.rs::probe_encoder` classifies by `Vendor::from_pci_id` and its AMD arm runs a real AMF probe (`amd::encoder::probe_availability`) | COMPILE-CHECKED for the AMD arm — **verify** (D-01) |
| **299** | Every optional AMF property goes through `HasProperty` + write + read-back, printed as one `AMF optional properties:` line | COMPILE-CHECKED — **verify** (D-02) |
| **300** | `classify_query_output`: an empty non-null output buffer is a consumed frame, not "nothing yet"; `au_scratch` replaces the per-frame `to_vec()` under the mapping | COMPILE-CHECKED, classification unit-tested — **verify** (D-07) |
| **301** | The synthetic probe walks `encoder_ladder` instead of hardcoding NVENC; `serve --encoder` exists | VERIFIED here for the fall-through and the flag; the AMD rung encoding is COMPILE-CHECKED (D-03, D-06) |
| **302** | `AsyncEncodeRing`: the decoupled ring generalized to AMF (`RingEncoder`/`RingTextures`) | COMPILE-CHECKED for AMD; **NVENC verified unchanged** either side — **verify** (§10.2) |
| **303** | AMF LTR wired to `streamer::repair::RepairState`, gated on `HasProperty` + `max_refs > 1` + the read-back | COMPILE-CHECKED — **verify** (D-08 / §10.4) |
| **304** | `amd/sps.rs` rewrites the SPS VUI (BT.709 + `bitstream_restriction_flag = 1`, `max_num_reorder_frames = 0`), gated on a read-back | COMPILE-CHECKED, 8 unit tests — **verify** (§10.3) |

And three `docs/CAPABILITY-TIERS.md` rows, all **UNVERIFIED**:

1. **H.264 encode (Windows, `EncoderVendor::Auto`)** — tiers: NVENC → Intel Quick Sync →
   **AMD AMF ULL** → **AMD AMF LL** → software x264. Probe: `LoadLibrary("amfrt64.dll")` →
   `AMFInit` → `CreateComponent(AMFVideoEncoderVCE_AVC) == AMF_OK` →
   `GetCaps().GetAccelerationType() ∈ {HARDWARE, GPU}` → `Init()`. Adapter by `VendorId == 0x1002`.
2. **AMF usage tier** — ULL, then LL. Two probes in order: AMD's cheap one (set `Usage`, read
   `RateControlPreanalysisEnable` back), then Sunshine's blunt one (if `Init()` still fails,
   re-apply the whole table under `LOW_LATENCY`, because `USAGE` re-defaults it).
3. **AMF encoder input format** — NV12 (via `scaler.rs`) then BGRA + AMF's internal converter.
   "Currently sitting on tier 2."
4. **CPU-bridge readback format (Windows)** — NV12 for x264 / Quick Sync bridge / **AMF bridge**;
   BGRA for NVENC's bridge. "VERIFIED for x264 and Quick Sync; **UNVERIFIED for AMF**."

### 5.2 The design contract the code claims to implement

`src-tauri/src/windows_utils/streamer/amd.md` is the authoritative design note (read it in full;
§3 is a numbered 32-row property table with the reason for every value). Its north star, which is
yours: **minimum glass-to-glass latency; where a knob trades a millisecond for a little PSNR, spend
the PSNR.** The claims you are verifying:

- `USAGE = ULTRA_LOW_LATENCY` **set first**, because it re-defaults most other rate-control and
  GOP properties. AMD's own encode doc: transcoding mode "needs to accept at least 3 input frames
  before any output is produced" while "in low latency modes output becomes available as soon as
  the first submitted frame is encoded" — three frames, **50 ms at 60 fps**, decided by one
  property. Both existing Rust AMF crates default to `TRANSCODING`.
- ULL has a **mandatory fallback**: it breaks `Init()` outright on pre-RDNA parts from Adrenalin
  23.7.1 onward (AMF issue #410).
- **One frame of VBV** (`VBVBufferSize = bitrate / fps`). Sunshine's `rc_buffer_size =
  bitrate/framerate` and OBS's latency-VBR branch land on the same number independently.
- **No B-frames, no pre-encode, no pre-analysis, no VBAQ**; `QUALITY_PRESET = SPEED` (value **1**
  on AVC — the enum is codec-specific: 10 on HEVC, 100 on AV1).
- **Infinite GOP** (`IDRPeriod = i32::MAX`, *not* 0 — zero's meaning is undocumented for AVC), with
  `ForcePictureType = IDR` + `InsertSPS`/`InsertPPS` per-surface on join/PLI/FIR.
- **`QueryTimeout = 1` ms set *before* `Init()`.** Filed under "dynamic properties" in the header
  but effectively static: set afterwards it returns `AMF_OK`, echoes back from `GetProperty`, and
  does nothing (AMF issue #516). Honoured, `QueryOutput` blocks for a millisecond instead of us
  sleeping — and `Sleep(1)` on Windows is 1–15 ms depending on timer resolution.
- **One frame in flight, one thread.** "Nothing yet" is a NULL out-pointer, not a result code:
  `QueryOutput` can return `AMF_OK` with a null `ppData`, and neither `AMF_REPEAT` nor
  `AMF_NEED_MORE_INPUT` is reliable. Both FFmpeg and OBS test the pointer.
- **`AMF_INPUT_FULL` means the surface was NOT consumed** — re-submit the same pointer after
  draining output.
- **`SHADER_RESOURCE` is mandatory** on the ring textures: a texture lacking the bind flag fails
  with `AMF_DIRECTX_FAILED` at the first `QueryOutput`, not at wrap time.
- **`SetMultithreadProtected(TRUE)`** on the shared device plus an explicit `Flush()` after the
  copy — AMD's own fix for flicker plus *growing* latency on a Desktop-Duplication device (AMF
  issue #92).
- **Teardown order**: `Drain` → `QueryOutput` until `AMF_EOF` → `Terminate` → `Release` on the
  component, then `Terminate` → `Release` on the context; the factory is a DLL-owned singleton and
  `AMFTrace` has no `Release` slot. `libloading::Library` is the **last struct field** so it drops
  last.
- **`AMF_ACCEL_HARDWARE` is 0** and `AMF_ACCEL_NOT_SUPPORTED` is **−1**, so an acceleration check
  cannot be written as a truthiness test.

### 5.3 What was already tried and rejected in this area — do not re-try blindly

These are the Windows capture/encode rejections from `docs/OPTIMIZATIONS.md`, with their numbers.
They were measured on the NVIDIA dev box unless stated.

- **`Direct3D11CaptureFramePool::CreateFreeThreaded`** — rejected on the measurement, not the
  difficulty. `capture_delivery` is p50 0.98 / p90 4.19 / p99 6.82 ms static and p50 6.29 / p90
  9.44 / p99 20.97 ms live; a thread wake is tens of microseconds. Against that it moves the
  MMCSS / `TIME_CRITICAL` / P-core registration onto the wrong thread and loses arrival ordering.
- **Double-buffering the CPU-bridge staging texture** — rejected: the whole readback is
  **1192 µs** BGRA / **740 µs** NV12, and pipelining the map costs a full frame period (16.7 ms at
  60 Hz) to remove at most 1.2 ms of stall.
- **Removing the per-frame `Flush` in the NVENC ring stage** — **kept deliberately**: the
  destination is a cross-device shared resource and the failure mode is a torn frame under load on
  some drivers. The AMF path has the same `Flush` for a documented AMD-specific reason (AMF #92) —
  do not remove it without a measurement *and* a torn-frame soak.
- **Frame-pool depth 3 (ring) / 2 (inline)** — `frames_dropped_stale` reads 5, 0 and 3 against
  321–421 delivered frames, so there is no queue to shorten.
- **`BROADCAST_CAPACITY = 4`** — `frames_dropped_broadcast` was zero in all twelve after-matrix
  runs.
- **High-resolution waitable timers and `ThreadMemoryPriority`** — no-ops on the dev box.
- **Sub-frame / slice-mode output** — rejected on NVENC, AMF and x264 against a measured **~2 ms**
  ceiling: NVENC at 1920×1080, 200 steady frames on a fixed picture, p50 **3642 µs** for
  upload+map+encode and **1980 µs** for map+encode alone, so at most about 2 ms of transmit
  overlap. The cost is not in the encoder: `webrtc_session.rs` receives one complete access unit
  per frame, packetizes it in one `payload()` call, derives one RTP timestamp from
  `frame.capture`, marks the last packet, and runs `Pacer::begin_frame` /
  `FrameTiming::mark_packetize_done` per frame. **The ledger explicitly says to re-open this on AMD
  hardware** — that is item A-13, and the AMD number is the one nobody has.
- **The per-frame changed-region hint** — evaluated and not consumed. Its named consumers were
  x264's CPU colour convert (gone: every CPU bridge now has an NV12 scaler), Intel's
  `mfxExtDirtyRect`, and **AMF's ROI map**. What remains needs the capture side to expose the rect
  plus a coordinate scale into encoder space. Revisit only if a measurement shows the encoder
  spending time on unchanged macroblocks.
- **oneVPL surface import** rejected on the per-stage split; **`nal-hrd=cbr`** rejected;
  **`libvpl.dll`** deliberately not bundled.

### 5.4 The numbers to beat

From `docs/BASELINE.md`, measured on Windows 11 26200 with an RTX 3080 Ti Laptop + Intel UHD
(hybrid), release build with `lto="fat"`, `codegen-units=1`, 30 s per cell. **`selftest` = the
whole host path, static 1280×720@60, real WHEP join over loopback.** Milliseconds, p50/p90/p99/max:

| Encoder | Capture | frames (repeats) | RTP/s | capture delivery | scale | encode queue | encode | packetize | socket write | **capture → socket** |
|---|---|---|---|---|---|---|---|---|---|---|
| NVENC zero-copy ring | WGC | 211 (144) | 58 | 0.98 / 4.19 / 6.82 / 6.95 | 0.11 / 0.13 / 2.62 / 2.74 | 0.00 / 0.02 / 0.26 / 14.81 | 0.00 / 1.18 / 1.44 / 15.09 | 0.04 / 0.06 / 0.11 / 0.12 | 0.16 / 0.25 / 0.43 / 0.59 | **1.57 / 8.39 / 16.78 / 18.00** |
| x264 (software) | WGC | 223 (142) | 84 | 0.98 / 3.93 / 4.72 / 4.99 | — | 0.00 | 2.10 / 2.62 / 5.24 / 24.57 | 0.05 / 0.07 / 0.10 / 0.25 | 0.20 / 0.29 / 0.52 / 0.68 | **2.36 / 5.24 / 23.07 / 24.82** |
| Quick Sync (CPU bridge) | WGC | 219 (140) | 55 | 3.15 / 7.86 / 15.73 / 16.11 | — | 0.00 | 4.19 / 11.53 / 16.78 / 21.65 | 0.04 / 0.06 / 0.15 / 0.36 | 0.15 / 0.26 / 0.43 / 1.36 | **11.53 / 16.78 / 25.17 / 26.05** |

**`live` = real content, 1920×1080@360 target, no transport:**

| Encoder | Capture | frames (repeats) | bytes | capture delivery | scale | encode queue | encode | **capture → encode-out** |
|---|---|---|---|---|---|---|---|---|
| NVENC zero-copy ring | WGC | 168 (119) | 1,518,706 | 6.29 / 9.44 / 20.97 / 22.54 | 0.18 / 0.23 / 0.85 / 0.87 | 0.00 / 0.02 / 0.05 / 0.05 | 0.00 / 5.77 / 5.77 / 10.30 | **12.58 / 15.73 / 27.26 / 28.45** |
| x264 (software) | WGC | 191 (140) | 1,235,186 | 8.39 / 9.44 / 16.78 / 18.60 | — | 0.00 / 0.00 / 5.24 / 11.24 | 7.86 / 11.53 / 20.97 / 24.53 | **20.97 / 23.07 / 37.75 / 40.31** |
| Quick Sync (CPU bridge) | WGC | 172 (141) | 3,108,831 | 9.44 / 10.49 / 37.75 / 39.73 | — | 0.00 / 0.00 / 1.84 / 4.80 | 7.34 / 16.78 / 46.14 / 55.27 | **27.26 / 31.46 / 83.89 / 88.25** |

After the W3a capture work landed, the host-path p90 improved from **8.39 ms to 3.93 ms** and p99
from 16.78 to **6.82 ms** on the NVENC path. **That 3.93 ms p90 / 6.82 ms p99 is your target
band**, on the understanding that your GPU is not an RTX 3080 Ti and your `capture_delivery` will
differ. The comparison that matters is **AMF versus x264 and versus Quick Sync on your own
machine**, measured in the same session, with the NVIDIA numbers as the shape to aim at rather than
a number to hit.

Three readings from that baseline that shape what you are looking for:

1. **The encoder is not the bottleneck; capture delivery is.** On the static path NVENC returns in
   1.18 ms at p90 while the whole host path is 8.39 ms, of which capture delivery is 4.19 ms. If
   your AMF `encode` p90 lands anywhere near 1–3 ms you are in the right band, and the remaining
   work is upstream.
2. **A misconfigured hardware encoder is slower than software.** Quick Sync on the CPU bridge is
   **7× NVENC's p50** and loses to x264 on every percentile. `Backend::AmfCpu` is the same shape.
   If your machine lands on the CPU bridge, find out why — on an all-AMD box it should not happen.
3. **Two thirds of every stream is repeats** (140–144 of ~220 frames static). Frame counts are not
   frame rates, and the repeat path (`encode_repeat`) matters as much as the capture path.

**Competitor numbers** (from `docs/BASELINE.md`; none measured on this hardware, each the best
independent figure available): Parsec 4–8 ms LAN (*vendor*), Steam Link 13–16, **Luna Display 11.3
ms USB / 22.4 ms Wi-Fi** (*vendor*, 50 samples — the same job as ours and the target), Moonlight
20–35 on a good LAN, Apple Sidecar "consistently below 30" wired, **Hopp (browser WebRTC) 98 ms**
after tuning libwebrtc down from 159 — the honest ceiling for a browser client, Miracast 80–100
optimized. ScreenExtend's host half was 8.39 ms p90 pre-W3a and 3.93 ms p90 after, leaving most of
a Luna-class budget for network and client. Your job is to put an AMD host in that same band.

---

## 6. Phase 1 — read first, then find out what this hardware actually is

### 6.1 Read these in full before you change one line

In this order. Delegate the reading to Opus 5 subagents where it parallelizes, but make sure the
knowledge lands somewhere durable (a findings file, or your own notes committed as a discovery doc).

1. `src-tauri/src/windows_utils/streamer/amd.md` — the design contract. ~310 lines.
2. `src-tauri/src/windows_utils/streamer/amd/encoder.rs` — **all 1,813 lines.** The module-level
   comment block is the rationale; every "why" you will need is in it.
3. `src-tauri/src/windows_utils/streamer/amd_sys/{mod,types,vtbl,props}.rs` — the FFI. Pay
   attention to `classify()` in `mod.rs` and to the four pairs of header constants that share a
   single wide string (documented at the top of `props.rs`): `PREENCODE_ENABLE` ==
   `RATE_CONTROL_PREANALYSIS_ENABLE`, `LOWLATENCY_MODE` == `LOW_LATENCY_INTERNAL`,
   `OUTPUT_FULL_RANGE_COLOR` == `FULL_RANGE_COLOR`, `CAP_QUERY_TIMEOUT_SUPPORT` ==
   `CAPS_QUERY_TIMEOUT_SUPPORT`. Setting "both" of a pair configures one property.
4. `src-tauri/src/windows_utils/streamer/nvidia/encoder.rs` — **the reference implementation and
   your quality bar.** Read it in full. §10.1 lists the techniques you are porting.
5. `src-tauri/src/windows_utils/streamer/pipeline.rs` — at minimum `Backend`, `build_backend`,
   `try_amf`, `encode_gpu` / `encode_cpu` / `encode_repeat`, `needs_scaler`,
   `scaler_output_format`, `is_transient_encode_error`, `AsyncNvencRing` + `ring_encode_thread`,
   `spawn_repeater`, `Pipeline::on_frame_arrived`, `synthetic_pattern_loop`.
6. `src-tauri/src/windows_utils/streamer/scaler.rs` — `with_format`, `ReadbackFormat`,
   `scale_into`, `read_back`, and the `CheckVideoProcessorFormat` NV12 probe.
7. `src-tauri/src/windows_utils/streamer/test/amd_layout.rs` — the 13 tests, so you know what is
   already asserted and what is not.
8. `docs/research/amd-amf.md` — 2,433 lines: measured vtable slots, the per-USAGE default table,
   the OBS / FFmpeg / Sunshine comparison, §6 the verification plan, §8 the open questions. This is
   the report the implementation was written from and it is the best single source you have.
9. `docs/manual-tests/amd-amf.md` — the 11-section script (with its errors, D-05).
10. `docs/OPTIMIZATIONS.md` rows 4, 108, 245, 246, 247, 277, 278, 279 and the
    "Windows capture/encode — what was rejected" prose; `docs/CAPABILITY-TIERS.md` the four rows in
    §5.1; `docs/BASELINE.md` in full; `docs/TELEMETRY.md`; `CLAUDE.md`.

### 6.2 Discovery: what does this machine actually expose?

Write the answers to **`docs/discovery/amd-host-<short-machine-name>.md`** and commit it. This is
the artifact that makes your numbers reproducible, and the thing the next AMD machine diffs
against.

Record:

- Everything from the §0.3 preflight, verbatim.
- GPU family and VCN generation (Polaris / Vega / RDNA1 / RDNA2 / RDNA3 / RDNA4; VCN 1.0 … 4.x),
  and how you determined it.
- Adrenalin version, `amfrt64.dll` size + timestamp, and `AMFQueryVersion`'s value formatted.
- The full `AMF AVC caps:` line, and what each field says: `accel`, `max_bitrate`, `streams`
  (`NumOfStreams`), `hw_instances` (`NumOfHwInstances`), `max_profile`, `max_level`, `max_refs`,
  `b_frames`, `pre_analysis`, `query_timeout` (`QueryTimeoutSupport`), `slice_output`
  (`SupportSliceOutput`), `color_conversion`, `throughput` requested/max in MBs,
  the input width/height ranges, `bgra_native`, `nv12_native`.
- Every `AMF optional property not applied:` line from a full startup — that list *is* the answer
  to item A-06 (which of `LowLatencyInternal`, `OutputMode`, `InputQueueSize`,
  `EnableEncoderSmartAccessVideo`, `IntraRefreshMBsNumberPerSlot` exist on this driver and VCN).
- `ScreenExtend doctor --json` in full, and specifically its `encoder` field (which is wrong today
  on AMD — defect D-01).
- The `adapters: capture=… encode=… same_adapter=…` line from a live session, and whether
  `capture is running on a SOFTWARE rasteriser` ever appears.
- The display topology: how many monitors, which adapter composes each, whether the IDD virtual
  display driver is installed (`ScreenExtend drivers install` if not).
- Machine facts for reproducibility: CPU, RAM, power plan, whether it is a laptop on battery or
  mains, and the exact Windows build.

---

## 7. Phase 2 — instrumentation and baseline on THIS hardware

Take the baseline **before** you change any code, with the tree exactly at `latency-overhaul`.
If the preflight in §0.3 already failed to encode a frame, fix the minimum needed to get *one*
frame out (that fix is itself a ledger row), then baseline, then continue.

Build once:

```powershell
cd src-tauri
cargo build --release --features probe
```

Fill this matrix. Three trials per cell, ≥ 15 s between runs, median trial reported as
p50 / p90 / p99 / max. Put it in `docs/BASELINE.md` as a new "Matrix — Windows + AMD" section
(and delete the `| **AMD** | No AMD GPU on either machine. |` row from "What could not be
measured").

**Matrix A — `selftest` (whole host path, static, real WHEP join over loopback)**

```powershell
ScreenExtend probe --whep-selftest --duration 30 --encoder <e> --capture-backend wgc
```

| Encoder | frames (repeats) | RTP/s | capture delivery | scale | encode queue | encode | packetize | socket write | **capture → socket** |
|---|---|---|---|---|---|---|---|---|---|
| `amd` (whatever tier it selects) | | | | | | | | | |
| `software` (x264, same-machine control) | | | | | | | | | |
| `auto` (confirm which backend it picks) | | | | | | | | | |
| `intel`, only if this box has an Intel iGPU | | | | | | | | | |

**Matrix B — `live` (real content through real capture, no transport)**

```powershell
ScreenExtend probe --live out_<e>_<workload>.h264 --duration 30 --encoder <e> --workload <w>
```

Workloads: `static`, `scroll`, `motion`, `drag`, `mixed`. At minimum run `motion` and `static`
for `amd` and `software`. Record frames, bytes, capture delivery, scale, encode queue, encode, and
capture → encode-out; plus the resulting file's `ffprobe` summary.

**Matrix C — geometry sweep** (the encoder is configured for the *display's* refresh, so a 144 Hz
or 360 Hz panel gives the encoder a 6.9 ms or 2.8 ms budget it may not meet — that is why the x264
`live` row above looks worse than its own unit test):

| Geometry | fps | `encode` p50/p90/p99/max | bytes/frame | notes |
|---|---|---|---|---|
| 1280×720 | 60 | | | |
| 1920×1080 | 60 | | | |
| 1920×1080 | display native (say what it is) | | | |
| 2560×1440 | 60 | | | |
| 3840×2160 | 60 | if the caps' `max_throughput` allows | | |

**Matrix D — a real client.** Join from the phone, and take `ScreenExtend status --json` after
30 s and after 5 minutes. Record `encoder`, every stage histogram, and every `Degradation` counter
(`frames_dropped_stale`, `frames_dropped_ring`, `frames_dropped_broadcast`,
`encoder_transient_errors`, `capture_fallback`, …). **Watch the `max`, not the average** — a
creeping max over five minutes is a VBV or buffering regression.

**Matrix E — glass-to-glass by camera**, per `docs/BASELINE.md` "Glass-to-glass procedure":
`ScreenExtend probe --live --duration 60 --workload motion --counter --encoder amd`, film host and
client in one shot at 240 fps, ≥ 30 samples, report p50/p90/p99 with the camera model and both
refresh rates (240 fps quantises to 4.17 ms — state that as the method's floor). Do the same with
`--encoder software` for a same-machine control. Note the caveat recorded in that procedure: the
`--counter` overlay is painted by the workload window, so add the measured `capture_delivery` p50
for the backend under test when comparing like with like.

---

## 8. Phase 3 — the work

Each item below is: **what to verify** → **what to fix or optimize** → **how to measure** → **what
"done" looks like**. "Done" always means a ledger row moved to `VERIFIED` with numbers, or reverted
with a reason and a number.

Items **A-01 … A-13** are the inventory this guide owns. Items **D-01 … D-08** are defects found by
reading the shipped code while writing this guide.

**Read the D-items this way.** Each opens with a quoted banner recording that the fix **landed** and
what it did; the prose below the banner is the **original defect description**, kept because it is
the argument for why the fix is shaped the way it is and because you need it to know what a
*regression* would look like. So where a D-item says "Fix: …", read "this is what was done — confirm
it did what it claims, on hardware that can tell." Every one of them was written and gated on a
machine with **no AMD GPU**, which is exactly why they are all still in your list.

### 8.1 The inventory

#### A-01 — The whole backend encodes a frame, for the first time

**Verify:** `AMFInit` succeeds; `CreateComponent(AMFVideoEncoderVCE_AVC)` succeeds;
`GetCaps().GetAccelerationType() ∈ {AMF_ACCEL_HARDWARE (0), AMF_ACCEL_GPU (1)}`; `Init()`
succeeds; an actual Annex-B bitstream comes out; and `ffprobe` says it is what we asked for.

**Files:** `amd/encoder.rs` (`Encoder::build`, `probe_caps`, `configure_and_init`,
`init_component`, `submit_slot`, `submit_surface`, `query_once`), `amd_sys/**`, `pipeline.rs`
(`try_amf`).

**Measure:**

```powershell
ScreenExtend probe --encode out_amd.h264 --encoder amd
ffprobe -v error -select_streams v:0 -show_entries stream=codec_name,profile,level,has_b_frames,color_range,color_space,color_primaries,color_transfer -of default=nw=1 out_amd.h264
ffprobe -v error -select_streams v:0 -show_entries frame=pict_type -of csv=p=0 out_amd.h264 | Sort-Object -Unique
ffprobe -v error -show_packets -of csv out_amd.h264 | Select-Object -First 3
ffplay out_amd.h264
```

**Assert:** `codec_name=h264`; `has_b_frames=0`; `pict_type` values only `I` and `P`, **never
`B`**; the first frame is `I` and there is **exactly one** in a 300-frame probe (infinite GOP —
more than one means `IDRPeriod` did not take, which is also item A-07); the first access unit
carries SPS and PPS in band; `ffplay` shows the moving cyan box on a scrolling gradient, not
garbage. Chroma read as luma looks like a green or magenta cast over the whole frame.

**Done:** `docs/OPTIMIZATIONS.md` row 4 flips from `UNVERIFIED` to `VERIFIED` with the before/after
cells filled from §7 Matrix A, and the `CAPABILITY-TIERS` "H.264 encode (Windows)" row's AMD tiers
say `VERIFIED` naming the tier this machine selected. Risk-register R-011 and R-228 close.

#### A-02 — Nothing is linked against AMF

**Verify:** `dumpbin /imports .\target\release\ScreenExtend.exe | Select-String -Pattern amf`
prints **nothing**, on a full release build.

**Also:** run the same check on the bundled artifact from `pnpm tauri build`
(`src-tauri/target/release/bundle/`), not only the raw exe. And confirm the negative case still
works: temporarily rename `C:\Windows\System32\amfrt64.dll` (or test on a machine without it) and
confirm `Encoder::new` returns a clean `Err` naming the DLL, `build_backend`'s `Auto` chain falls
through to x264, and the log says why — `amf_loader_handles_a_missing_runtime` covers the loader
half but not the pipeline half.

**Done:** the check is recorded in the report and added to `docs/manual-tests/amd-amf.md` §1 as a
gate, with the exact command that worked on your toolchain.

#### A-03 — The usage tier: `ULTRA_LOW_LATENCY` vs `LOW_LATENCY`

**Neither branch has ever executed.** The code probes cheaply first
(`Encoder::probe_ultra_low_latency`: set `Usage = ULL`, then `GetProperty("RateControlPreanalysisEnable")`
— a failed read means the usage did not take), then falls back blunt (if `Init()` still fails,
re-apply the *whole* property table under `LOW_LATENCY` and retry, because `USAGE` re-defaults it).

**Verify** exactly one of these three shapes appears in the log:

| Tier | Evidence | Meaning |
|---|---|---|
| **AMF ULL** | `AMD AMF H.264 encoder initialized (D3D11 / ULTRA_LOW_LATENCY)` and **no** `falling back` line | the good path |
| **AMF LL** | `AMF: ULTRA_LOW_LATENCY usage rejected by this driver/GPU; falling back to LOW_LATENCY` **or** `AMF Init() failed under ULTRA_LOW_LATENCY (…); retrying with LOW_LATENCY`, then `initialized (D3D11 / LOW_LATENCY)` | the AMF #410 workaround fired — expected on pre-RDNA with Adrenalin ≥ 23.7.1. Note **which** of the two messages appeared: the first means the cheap probe caught it, the second means only `Init()` did. |
| **x264** | `AMD AMF unavailable (…); falling back to software x264` then `SOFTWARE x264 CPU path` | no AMF encode at all |

Also watch for `AMF: Usage read back as … after setting …`, and read `rc=` in the `initialized`
line. **On GCN parts (RX 580, Vega) `RateControlMethod` is reported not to be overridable after
ULL at all** — you keep ULL's `LatencyConstrainedVBR` whatever you write. With a one-frame VBV that
is an acceptable outcome: **record it, do not "fix" it.** Silently believing you are in CBR when
you are not is what makes bitrate control look broken.

**Force each branch:** there is no flag. To exercise the fallback deliberately, temporarily make
`probe_ultra_low_latency` return `false` and re-run; you should get the `rejected by this
driver/GPU` line and a working `LOW_LATENCY` encode. Revert the edit afterwards.

**Measure:** run §7 Matrix A under both tiers and record the delta. If LL is what your hardware
gets, spend real effort making LL as fast as it can be (invariant §2.4.6) — that means confirming
which of the ULL-only defaults the explicit table actually restores.

**Done:** the `AMF usage tier` row in `CAPABILITY-TIERS.md` flips to `VERIFIED` naming the branch,
the GPU family and the Adrenalin version, with both tiers' numbers if you can produce both.

#### A-04 — Encoder input format: NV12 vs BGRA + AMF's internal converter

This is probably the **largest single win available on this hardware**, and today it is not taken
on the GPU path at all.

**The facts in the code.** `PREFERRED_INPUT_FORMAT = DXGI_FORMAT_B8G8R8A8_UNORM`. On the
`Backend::Amf` GPU path, `needs_scaler` only builds a scaler when the geometry differs, and
`scaler_output_format` returns `ReadbackFormat::Bgra8` for every non-CPU-bridge backend — so the
ring is BGRA whether or not a scaler exists, and `build_ring` logs
`AMF input is BGRA rather than NV12: AMF's internal colour converter is active (accel=…)`. Only
the **CPU bridge** ever asks for NV12 (`Backend::cpu_bridge_format()` → `Nv12` for `AmfCpu`), which
then triggers `retune_input_format` on the first frame.

**Why it matters.** AMD state that packed-RGB input silently invokes an **uncontrolled** internal
GPU colour converter that competes with the foreground application, and OBS refuses BGRA on its
texture path for exactly this reason. `GetInputCaps → GetFormatAt(i, &fmt, &native)` says which
formats this VCN considers *native*; `CAP_COLOR_CONVERSION` says whether the internal convert is
even hardware-accelerated. Both are already read and logged as `bgra_native` / `nv12_native` /
`color_conversion` — **read them and act on what they say.**

**Verify:** the caps dump's `bgra_native` and `nv12_native` values; that `retune_input_format`
works at all (`AMF re-initializing for a DXGI_FORMAT_NV12 input (was DXGI_FORMAT_B8G8R8A8_UNORM)`,
then **no** subsequent `AMF input is BGRA` line, 300 clean frames, and the IDR count is **2 not 1**
because the retune sets `idr_requested`).

**Fix / optimize:** teach the **GPU** path to take NV12. `Scaler::with_format(..., ReadbackFormat::Nv12)`
already exists and is VERIFIED (ledger row 108): the video processor does the scale *and* the
BT.709 conversion in one `VideoProcessorBlt` it was running anyway. Two sub-cases:

- **Scaling case** (client size ≠ native): make `scaler_output_format` return `Nv12` for
  `Backend::Amf`, so the blt lands straight in an NV12 ring slot. One line, plus letting
  `retune_input_format` fire once.
- **No-scale case** (client size == native): today there is no scaler at all and `encode_texture`
  does a `CopyResource`, which demands identical formats. To get NV12 you need a convert-only blt —
  extend `needs_scaler`'s `convert` term to cover `Backend::Amf` when the ring wants NV12. Measure
  whether the identity-blt-plus-convert beats AMF's internal converter; ledger row 108 measured an
  identity-blt NV12 readback at **2394 → 1170 µs** on the CPU-bridge side, which is the shape of
  the saving but not the same measurement.

Both must fall back cleanly: `Scaler::with_format` already refuses NV12 for odd dimensions or an
unsupporting video processor and returns BGRA with a log line, so the request can never fail a
session. If `nv12_native` comes back `Some(false)` on your part, that is a result — record it and
measure anyway.

**Measure:** an A/B of `encode` and `capture_to_socket` p50/p90/p99/max over §7 Matrix A and B,
BGRA ring vs NV12 ring, on both the GPU path and the CPU bridge; plus bytes/frame and an
`ffprobe` colour-tag comparison; plus GPU utilisation if you can read it (the internal converter's
cost is supposed to show up as contention with the foreground app, so also run a foreground 3D or
video workload and compare).

**Done:** the `AMF encoder input format` tier row flips to `VERIFIED` and says which tier this
machine ends up on and why; `CPU-bridge readback format (Windows)`'s AMF half flips too; ledger
row 245 gets numbers.

#### A-05 — The AMF CPU bridge

**Verify:** `Backend::AmfCpu` works end to end: `encode_nv12_padded` (and `encode_bgra_padded`),
the colour range as a function of the input format, and `OutMatrixCoeff` / `OutColorProfile` /
`OutColorPrimaries` / `OutColorTransferChar` / `FullRangeColor` as actually set.

The colour rule in the code: `full_range = self.surface_format != AMF_SURFACE_NV12`. Packed BGRA
off the capture is **full range**; NV12 out of the D3D11 video processor is BT.709 **limited**.
Signalling full range over limited-range samples is the washed-out picture this rule exists to
avoid, so a mismatch between the `AMF input is …` / `AMF re-initializing for a …` log line and
`ffprobe`'s `color_range` is a **real bug, not cosmetic**.

**Force the bridge:** on an all-AMD box `Backend::Amf` is chosen because the capture device is
AMD. To reach the bridge, either run on a hybrid machine (A-12), or temporarily make
`Encoder::new_on_device_with_ltr`'s vendor check fail. `probe --encode` is *already* the bridge
shape (`Encoder::new` builds its own AMD device and calls `encode_bgra_padded`), but it does not go
through the pipeline.

**Measure:** `encode` percentiles bridge vs same-adapter, at the same geometry; and the
`ffprobe` colour tags for each.

**Done:** ledger row 245 flips with numbers; the manual test §11's expectations are corrected to
match what actually happened.

#### A-06 — Which probe-first properties actually exist

Five properties are written with `set_optional_i64` / `set_optional_bool`, meaning a failure is a
log line rather than an error: `LowLatencyInternal`, `OutputMode`, `InputQueueSize`,
`EnableEncoderSmartAccessVideo`, `IntraRefreshMBsNumberPerSlot`. Every failure prints
`AMF optional property not applied: …`.

**Verify:** collect the complete list of those lines on this driver + VCN, and cross-check against
`HasProperty`. **Note that `amd_sys::has_property` is written, documented in `amd.md` §1 as the
gating mechanism, and never called from anywhere.** That is defect D-02.

**Fix:** use it. Probe each optional property with `HasProperty` first, log a single consolidated
line naming which knobs this runtime has, and only then attempt the write — so the caps report and
the write result agree instead of being discovered separately. Also settle §8 open question 1 of
the research report: **is `InputQueueSize` writable, and is it static or dynamic?** It is listed in
the header's "dynamic properties" block, but a queue depth is the kind of thing allocated at
`Init` — and `QueryTimeout`, also listed dynamic, turned out to be static. If `1` is honoured,
`AMF_INPUT_FULL` becomes a meaningful backpressure signal instead of a 16-frame cushion; if it is
not, we simply never submit ahead (which one-in-flight already guarantees).

**Done:** the property table in `amd.md` §3 is annotated with what this hardware actually has, and
a new ledger row records the `HasProperty` gating with the list.

#### A-07 — Does `IDRPeriod = 0` mean "never" or "every frame"?

The code ships `IDRPeriod = i32::MAX` precisely because zero's meaning is undocumented for AVC
(Sunshine ships `gop_size = INT_MAX`; FFmpeg maps `gop_size` straight onto the property).

**Verify:** temporarily set it to 0, encode 300 frames, and count `pict_type=I`:

```powershell
ffprobe -v error -select_streams v:0 -show_entries frame=pict_type -of csv=p=0 out.h264 |
  Where-Object { $_ -eq 'I' } | Measure-Object | Select-Object Count
```

One `I` = "never"; 300 = "every frame"; anything else is a third behaviour worth recording. Then
put `i32::MAX` back regardless — the answer is documentation, not a change.

**Done:** answered in `amd.md` §3 row 19 and in a ledger note, with the driver version, because the
answer may be driver-dependent.

#### A-08 — Does `QUERY_TIMEOUT` blocking beat sleep-polling, and by how much?

The encoder sets `QueryTimeout = 1` before `Init()`, reads it back
(`init_component`: `self.query_timeout_supported = echoed != 0`), logs a line if the caps and the
read-back disagree, and **only sleeps when the read-back says the driver did not take it**
(`POLL_SLEEP = 1 ms`). FFmpeg's exact recipe: trust the read, not the write.

**Verify:** which branch this driver takes (`query_timeout=1ms-blocking` vs `poll+sleep` in the
`initialized` line), and whether the caps' `QueryTimeoutSupport` agrees with the read-back.

**Measure:** A/B the encode-thread wait, p50/p90/p99/max, by forcing `query_timeout_supported =
false` for one run. Remember why this matters: `Sleep(1)` on Windows is 1–15 ms depending on the
process timer resolution, and the pipeline raises timer resolution on some threads
(`tuning::tune_timer_thread`) but not necessarily the one you are on. Also try a **larger**
`QueryTimeout` (2, 4, 8 ms) — a blocking wait that covers a whole frame period removes the poll
entirely, at the cost of latency on the teardown path. Measure, do not assume.

**Done:** a ledger row with the A/B numbers, and `amd.md` §3 row 32 annotated.

#### A-09 — Encode latency and, above all, the tail

**Target:** the NVENC band — single-digit ms at 1080p, low teens at 4K — with the max under about
2× the average. **Watch the max over a five-minute run.** A creeping max is a VBV or buffering
regression and is where latency bugs hide.

**Measure:**

```powershell
ScreenExtend probe --whep-selftest --duration 30 --encoder amd
ScreenExtend probe --live --duration 300 --encoder amd --workload motion
# and, with a real client joined:
ScreenExtend serve -v          # then, from another shell, every 60 s:
ScreenExtend status --json
```

Compare against `docs/BASELINE.md`'s NVENC row: capture→socket **1.57 / 8.39 / 16.78 / 18.00 ms**,
and post-W3a p90 **3.93** / p99 **6.82**.

**A note on the instrument.** `docs/manual-tests/amd-amf.md` §5 tells you to watch for
`encode-path latency: path=amd-amf-same-adapter, avg_ms=…, max_ms=…`. **That line only exists on
the DXGI capture arm** (`path=dxgi+…`); the WGC path never prints it. Use the telemetry histograms
(`status --json`, and the probe's own summary on exit) instead — they are p50/p90/p99/max rather
than avg/max, which is what this engagement wants anyway. Fix the manual test (D-05).

Also confirm `encoder` reads `amd-amf-same-adapter`. If it reads `amd-amf-cpu-bridge` on an all-AMD
box, that is a bug — paste the `adapters: capture=… encode=… same_adapter=…` line.

And confirm this line **never** appears:
`AMF SubmitInput returned AMF_INPUT_FULL under one-frame-in-flight discipline`. If it does,
one-in-flight discipline is broken somewhere upstream.

**Done:** §7's matrices are in `docs/BASELINE.md` with an "after" column, and the report carries the
AMD-vs-NVENC-vs-x264 comparison.

#### A-10 — What ULL's free intra refresh costs

The AMF encode doc's per-`USAGE` defaults set `IntraRefreshMBsNumberPerSlot = 255` for both
`ULTRA_LOW_LATENCY` and `LOW_LATENCY`, and 0 for everything else. At 1080p (8,160 MBs) that is a
~32-frame wave, about **0.53 s at 60 fps**. `apply_properties` writes `0` **only** when
`!cfg.intra_refresh`, so by default an AMD host ships a permanent rolling refresh **on** while
NVENC ships it **off** (`Config::intra_refresh` defaults false) — an inconsistency nobody chose and
nobody has costed. A rolling refresh raises the per-frame bit cost against a fixed CBR budget, and
ours is fixed (`VBVBufferSize = bitrate/fps`, peak == target), so the cost is paid in picture
quality, not bytes.

**Measure:**

```powershell
ScreenExtend probe --encode out_amd_refresh.h264   --encoder amd --intra-refresh on
ScreenExtend probe --encode out_amd_norefresh.h264 --encoder amd --intra-refresh off
ffprobe -v error -show_entries format=size -of csv=p=0 out_amd_refresh.h264
ffprobe -v error -show_entries format=size -of csv=p=0 out_amd_norefresh.h264
ffmpeg -i out_amd_norefresh.h264 -i out_amd_refresh.h264 -lavfi "[0][1]ssim;[0][1]psnr" -f null -
```

(Put the path **immediately after** `--encode`, per §0.3.)

**Decide:** under ~2 % SSIM delta is noise and the ULL default stays. A 10–15 % quality cost is
resilience the transport may already be providing through NACK/RTX and the keyframe gate, and then
the refresh should be tied to `Config::intra_refresh` the way NVENC's is — which is a two-line
change in `apply_properties`. Also watch the picture for the banding the CLI help warns about: a
refresh wave can be visible as bands repainting the image.

**Done:** ledger row with the SSIM/PSNR delta and the file sizes, the decision, and the reason.

#### A-11 — Adaptive bitrate, on-demand IDR, leaks and restarts, and the concurrent-session cap

**Bitrate.** `Encoder::set_bitrate` writes `TargetBitrate`, `PeakBitrate` and `VBVBufferSize`
dynamically — no `ReInit`, no `Flush`. AMD warn "you cannot change it too often", which is why
`streamer::bitrate::BitrateController` stays the only writer.

```powershell
ScreenExtend probe --bitrate --encoder amd
```

The byte rate must follow the stepped target within a few frames, with **no resolution change** and
**no IDR storm**. In a live session the log shows `adapting bitrate: … -> … bps` then
`AMF bitrate reconfigured (bitrate_bps=…)`. **Read the values back** after each step: on GCN parts
the rate-control method may not be overridable at all, and `VBVBufferSize` may be static on some
parts. Add the read-back to `set_bitrate` if it is not there and log a single line when a write
does not stick.

**On-demand IDR.** Force a keyframe (reload the client page, or briefly drop its network so the
browser sends a PLI) and confirm **exactly one** IDR results, not a burst. Cross-check
`Encoder::last_output_was_idr()` (which reads AMF's own `OutputDataType`, where only `_IDR` marks a
stream-startable unit — a plain `_I` does not) against the pipeline's Annex-B scan.

**Leaks and restarts.** Join and leave 20 times in a row under `serve -v`. In Task Manager
(Details → add the *Handles* and *GPU memory* columns) both must return to baseline. This is really
a test of `Drain` → `Terminate` → `Release` ordering and of `libloading::Library` being the last
dropped field. Also check the log has **no** stray AMF text outside `logbus`: `silence_amf_trace`
disables AMF's console writer and sets `SetGlobalLevel(AMF_TRACE_ERROR)` at init, and if AMF
messages appear interleaved with ours that did not work.

**The concurrent-session cap — the item with the sharpest teeth.** The caps dump already reports
`NumOfStreams` and `NumOfHwInstances`. Open sessions (join from more devices, or run several
`probe --whep-selftest` hosts on different ports) until one is refused, and record: how many
succeeded, what the refusal *looks like*, and whether the already-connected sessions were affected.
The NVENC analogue measured **8** sessions on the RTX 3080 Ti with the 9th refused and the other
eight unaffected.

**The likely bug.** There is no "busy" or "out of sessions" code in `AMF_RESULT` at all — the
enum runs `AMF_OK`(0) … `AMF_VULKAN_FAILED`(45) with no such member. So session exhaustion will
surface as `AMF_OUT_OF_MEMORY`(6), `AMF_FAIL`(1) or `AMF_NOT_SUPPORTED`(10) from
`CreateComponent` or `Init` — and today `CreateComponent`'s failure is wrapped with the context
**"this adapter has no AMF H.264 encoder"**, which is exactly wrong for the ninth session, while
`AMF_OUT_OF_MEMORY` is classified `Recoverable` ("rebuild the session"), which invites a rebuild
loop. Fix both: identify session exhaustion, say so in the log with the caps numbers next to it,
and let the session fall through to x264 rather than dying or looping. **Never evict a connected
device to make room** (invariant §2.4.9).

**Done:** ledger rows for the bitrate path, the IDR path and the session cap; the observed cap
number recorded next to NVENC's 8 in `docs/manual-tests/amd-amf.md` §7/§8 and in
`CAPABILITY-TIERS.md`.

#### A-12 — Cross-adapter AMD (needs an Intel-iGPU + Radeon-dGPU laptop)

Only runnable on a hybrid machine. If you do not have one, say so plainly and move on.

**Verify:** which adapter `D3D11CreateDevice` lands on for capture; whether `Backend::Amf` refuses
with `AMD AMF requires an AMD D3D11 device, but the capture device is not AMD` and `try_amf` falls
to `Backend::AmfCpu` with `AMD AMF CPU-bridge path`; that 150 frames arrive clean; and what
`--encoder auto` picks (on that machine, same-adapter Quick Sync is the *correct* choice, which is
why AMF sits after Intel in the `Auto` chain).

**Read:** the `adapters: capture=… encode=… same_adapter=false` line, and the follow-on line about
IddCx 1.2 and cross-adapter composition.

**Measure:** the encode-latency delta bridge vs same-adapter; if you can also run the same guide on
an all-AMD box, that pair of numbers is the most useful thing this item can produce.

**Consider (and measure before deciding):** a real cross-adapter AMF path — a
`SHARED_NTHANDLE | SHARED_KEYEDMUTEX` texture opened on the AMD device, the way
`build_zero_copy` does it for NVENC — instead of the CPU round trip. The Quick Sync baseline says
what the round trip costs: capture→socket p50 **11.53 ms against NVENC's 1.57**, and 3.15 ms of
`capture_delivery` against 0.98. If the shared-texture path works on AMD, it is worth roughly that
gap. `create_input_texture` is where the ring textures are made; note `SHADER_RESOURCE` is
mandatory and `CreateSurfaceFromDX11Native` does not migrate or re-flag a texture.

**Done:** the AMD half of R-231 closes; the manual test §9 gets real numbers.

#### A-13 — Re-check the two AMD rejections on real hardware

Both were rejected against NVENC measurements, not AMD ones, and the ledger says to re-open them
here.

**(a) `OUTPUT_MODE = SLICE` sub-frame output.** `SupportSliceOutput` is already in the caps dump.
If this VCN has it, measure the actual sub-frame output latency directly: encode a frame, and
time the first `QueryOutput` that returns bytes against the last. The NVENC ceiling was ~2 ms
(p50 3642 µs upload+map+encode vs 1980 µs map+encode). If AMD's number is materially larger — say
a 1080p frame's slices arrive spread over 5–8 ms — the case reopens, and then the cost is on the
transport side: `webrtc_session.rs` assumes one complete access unit per frame for the RTP
timestamp, the marker bit, `access_unit_has_sps`, the pacer and the client's capture-time
recovery. **That transport change is out of your fence.** What is in your fence is producing the
number and writing it into `docs/OPTIMIZATIONS.md` row 246's notes so the decision is made on
evidence. If the number is large, file a finding for the transport owner and say so in the report.

**(b) Transient encoder-busy.** `pipeline::is_transient_encode_error` decides whether to shed the
frame or tear the session down by matching the substring **"device busy"** in the formatted error.
`Encoder::amf_error` appends `" [transient: device busy]"` when `classify()` says `Transient` —
but `classify()` returns `Transient` only for `AMF_OK`, `AMF_REPEAT`, `AMF_INPUT_FULL`,
`AMF_NEED_MORE_INPUT` and `AMF_EOF`, **every one of which is handled before it could reach
`amf_error`.** So the AMF transient path is, today, dead code. Under real load, find out what AMF
actually returns when the encoder cannot accept work (another app encoding, several of our own
sessions, a foreground game), and make the classification match reality. Then confirm the phrase
reaches the log:

```powershell
ScreenExtend logs --lines 400 | Select-String "device busy"
```

and that `status --json`'s `encoder_transient_errors` counter moves while sessions survive.

**Done:** row 246's AMD arm and row 279's AMD analogue both get real numbers or a recorded
"this hardware never produced the condition, here is what we tried".

### 8.2 Defects found by reading the code — fix these

These are not in the original inventory. They are real, they are in your territory, and they were
found while writing this guide by reading the shipped source.

#### D-01 — `compatibility.rs` still tells AMD users they have no hardware encoder

> **Landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify.** `probe_encoder` now classifies by
> `Vendor::from_pci_id` (with `select_vendor` only for an id we do not know) and its AMD arm
> calls `amd::encoder::probe_availability`, which loads the runtime, takes a context on the AMD
> adapter, creates the AVC component and reads `GetCaps` — reporting `amf`, `amf (cpu bridge)`
> or `x264 (AMF unavailable: <reason>)`. Ledger row 298. The verification below is unchanged.

`windows_utils/compatibility.rs::probe_encoder()` classifies adapters with **`select_vendor(&name)`**
— the *description-string* fallback — and then reports:

```rust
} else if has_amd {
    // AMD encode is a stub; those hosts run the CPU encoder.
    "x264 (no AMD hardware encoder)"
```

Both halves are wrong now. AMD encode is not a stub, and the project's own rule is
`Vendor::from_pci_id` (`VendorId == 0x1002`), because description strings are localized,
OEM-rebranded, and WARP's contains no vendor name at all while reporting Microsoft's `0x1414`. The
string is surfaced through `CompatibilityReport.encoder_backend`, `doctor --json` and the desktop
UI's compatibility view, so an AMD user is currently *told* the product cannot use their GPU.

**Fix:** switch `probe_encoder` to `Vendor::from_pci_id(desc.VendorId)` (keeping `select_vendor`
only where no id is available), and report `"amf"` for AMD — ideally matching the tier the pipeline
would actually pick. Keep the change confined to the AMD reporting; do not restructure the probe.
Verify with `ScreenExtend doctor --json` before and after, and by eye in the desktop UI's
compatibility card (**appearance unchanged** — only the string it displays changes).

#### D-02 — `amd_sys::has_property` is documented as the gating mechanism and never called

> **Landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify.** Every optional property goes through
> `set_gated_*`: `HasProperty`, the write, a read-back, all recorded and printed as one
> `AMF optional properties:` line. `HasProperty` is *reported*, not obeyed — the write happens
> either way, because a refused optional write costs nothing on AMF and nobody here can confirm
> what AMF's implementation counts as "has"; an entry reading `unknown-but-set:` is the answer to
> that question and `docs/manual-tests/amd-amf.md` §2 asks for it. Ledger row 299.

`amd.md` §1 says "Optional features are gated by `HasProperty` and by `SetProperty`'s return code",
and `amd_sys/mod.rs` provides `has_property` with a doc comment explaining it is "the cheap 'does
this build of the runtime know the property at all' probe, and the reason `GetPropertyInfo` is
never called". Nothing calls it. Fold this into A-06: use it, log one consolidated capability line,
and make the doc true.

#### D-03 — `synthetic_pattern_loop` hardcodes NVENC and ignores `--encoder`

> **Landed VERIFIED (fall-through) at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`).** The loop builds a `Backend` by walking
> W15's `encoder_ladder(vendor)` and encodes through `Backend::encode_cpu(.., width * 4, Bgra8,
> ..)`, which is byte-for-byte what the NVENC arm did. Confirmed on the NVIDIA box:
> `probe --bitrate --encoder amd` now prints `synthetic encode: AMD AMF unavailable (no AMD
> adapter found …); trying software x264` and `encoder=software-x264`. The AMD rung actually
> encoding is what you verify. Ledger row 301.

`pipeline.rs::synthetic_pattern_loop` calls `Encoder::new(config)` — the **NVENC** encoder — and
logs `encode thread: failed to create NVENC encoder (…); pipeline stopped` on failure. So
`probe --synthetic-pattern` (and any `--workload` run that does *not* also pass `--live`) cannot
use AMF, and on an AMD-only machine simply stops. `probe --live` is unaffected because it forces
`synthetic_pattern = false` and paints the workload onto the display instead.

**Fix:** give the synthetic loop a vendor arm the same way `build_backend` has one — adding an AMF
arm to a vendor dispatch is squarely "pipeline.rs where the AMF arms live". Do not change what the
NVIDIA arm does. Then re-check that `probe --live --workload motion --counter --encoder amd` (the
glass-to-glass recipe) still goes through capture, since that is the run §7 Matrix E depends on.

#### D-04 — Dangling `findings/` references in shipped comments and docs

> **Landed at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — nothing to verify.** Both pointers are inlined against the ledger
> rows that resolved them (245 for the NV12 input; the colour question became `amd.md` §3a and
> the rewriter in §10.3). `grep -rn 'findings/' src-tauri/src/windows_utils/` is empty, and
> `docs/CAPABILITY-TIERS.md`'s AMF-input row was carrying the same dead pointer and no longer is.

`amd/encoder.rs`'s `PREFERRED_INPUT_FORMAT` doc comment used to point at a
`findings/major-amd-nv12-input-pending-scaler.md`, and `amd.md` §2 and §3 row 26 at that file and at
a `findings/minor-amd-amf-does-not-write-vui-colour-signalling.md`. **Neither file existed any
more** — both findings had been resolved and deleted, which is the correct lifecycle, but the
pointers stayed, and `findings/` is gitignored, so they were references to something a reader could
never see. The substance is now inlined against the ledger rows that resolved them (245 for the NV12
input; the colour question became `amd.md` §3a and the rewriter in §10.3), and
`grep -rn 'findings/' src-tauri/src/windows_utils/` is empty.

**What you do:** re-run that grep after every change you make, and sweep `amd.md` and
`amd/encoder.rs` for any pointer you add. R-315 and R-317 in the risk register are the older version
of this same problem and close with it. **No comment or document in this repository may point at a
file the reader cannot open** — that includes a `findings/` path, because the directory is
gitignored.

#### D-05 — `docs/manual-tests/amd-amf.md` has commands that cannot work

> **Landed VERIFIED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`).** All four fixed, and every command in the file was run on
> the NVIDIA dev box: §2 and §10 are `probe --encode <path> --encoder amd …` with the path
> immediately after the mode flag (proved: two A/B runs write two different files and `out.h264`
> is never created), §5's `serve -v --encoder amd` works because `serve` has the flag and now
> points at the telemetry summary / `status --json` instead of the DXGI-only `encode-path
> latency:` line, and §7 is `probe --bitrate`. The file also gained the new
> `AMF optional properties:` line in §2, BT.709 expectations in §4, and a §12 for the ring and
> LTR. Re-run it as written on the Radeon box; nothing in it should be a usage error.

Four of them, and this is the script the *next* Radeon machine will follow:

1. §2: `cargo run --release -- --probe-encode --encoder amd out_amd.h264`. `cli::dispatch` only
   intercepts the harness when `args[0] == "probe"`; a leading `--probe-encode` goes to `route()`
   and is rejected as an unknown subcommand. **And** `--probe-encode` immediately followed by
   `--encoder` takes the default path `out.h264`, so the trailing filename is ignored.
   Correct form: `cargo run --release --features probe -- probe --encode out_amd.h264 --encoder amd`.
2. §5: `cargo run --release -- serve -v --encoder amd`. `serve` has no `--encoder` argument in
   `tauri.conf.json`'s `plugins.cli.subcommands.serve.args`, and `tauri-plugin-cli` rejects any
   argument it was not told about. On an all-AMD box `serve -v` alone is correct (the `Auto` chain
   reaches AMF because NVENC and Quick Sync are absent). On a hybrid box you need either the named
   exception in §2.3 or `probe --live --encoder amd`.
3. §5: the `encode-path latency:` line it tells you to watch only exists on the DXGI arm. Point it
   at `status --json` / the probe summary instead.
4. §10: `ScreenExtend probe --probe-encode --encoder amd --intra-refresh on out_amd_refresh.h264`
   — same path-swallowing bug, so **both** runs would write to `out.h264` and the comparison would
   be a file against itself.

**Fix all four**, re-run the script end to end as written, and only then call it verified.

#### D-06 — `serve` cannot select an encoder

> **Landed VERIFIED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`).** `serve --encoder auto|nvidia|intel|amd|software`, declared
> in `tauri.conf.json` with `possibleValues`, parsed in `run_serve`, stored in a new
> `AppState::encoder_vendor` that `sync_streamers` and `register_cloud_session` read.
> `--software-encode` stays as the alias and still wins, because it is what the desktop UI's GPU
> toggle sets. No Tauri command, so `bindings.ts` is unchanged. Confirmed here:
> `serve -v --encoder amd` prints `Encoder: amd` and starts; `--encoder nonsense` is rejected by
> the plugin. Ledger row 301.

Covered by the named exception in §2.3, which is now spent. Nothing to implement — confirm
`serve -v --encoder amd` selects AMF on your box and that `--encoder nonsense` is refused by the
plugin, then use the flag for every live-host measurement in §7 Matrix D.

#### D-07 — `query_once` treats a zero-length output buffer as "nothing yet"

> **Landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify.** Resolved in the first direction: a
> non-null `AMFData` with an empty buffer is a **consumed** output, logged once and returned as an
> empty access unit the pipeline drops, rather than polled for two seconds and then fatal. The
> classification is a pure `classify_query_output(data_null, buffer_empty, result)` with a unit
> test, because it cannot be reached on hardware this project has. What you verify is that the
> `AMF QueryOutput returned an output buffer of zero bytes` line never appears in §5's
> five-minute run — if it does, that is the real answer to the open question. Ledger row 300.

In `Encoder::query_once`, if AMF hands back a real `AMFData` whose buffer is null or size 0, the
function returns `Ok(None)` and the caller keeps polling until `SUBMIT_DEADLINE` (2 s) and then
bails with "encoder stalled". If any driver ever emits an empty buffer legitimately, the symptom is
a 2-second hang followed by a torn-down session rather than a dropped frame. Decide on the
evidence: either treat an empty non-null buffer as a **consumed, empty** output (log once, return
an empty AU, let the pipeline skip it), or leave it and document why. Either way, this is an audit
item to resolve rather than leave open.

#### D-08 — LTR is plumbed and unreachable

> **Landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify.** `build_amf_ring` requests
> `MAX_LTR_FRAMES_SUPPORTED`; `open` gates it on `HasProperty(MaxOfLTRFrames)` **and**
> `caps.max_reference_frames > 1`; `configure_and_init` believes the property read-back rather
> than the request; `mark_ltr` / `use_ltr` mirror the NVENC API so `ring_encode_thread` drives
> both engines with one `RepairState` protocol. `invalidate_reference` reports `Unsupported` on
> AMF (it disallows LTRs outside the forced bitfield instead), which the thread already handles.
> LTR stays 0 on the inline path — it is mutually exclusive with ULL's rolling refresh and only
> the ring reads receipts. `docs/manual-tests/amd-amf.md` §12 is the measurement. Ledger row 303.

`Encoder::new_on_device_with_ltr` exists, `MAX_LTR_FRAMES_SUPPORTED = 2`, `mark_next_frame_as_ltr`,
`force_ltr_reference`, `last_marked_ltr` and `last_referenced_ltr` are all written — and
`pipeline.rs::try_amf` calls `new_on_device`, which passes **0**. Nothing can turn it on. See
§10.4: the WebRTC-side machinery the comments say does not exist (`streamer/repair.rs`,
`RepairState::acknowledge` / `take_action` / `set_ltr_slots`) has since landed and is wired into
the NVENC ring. This is now implementable.

### 8.3 Suggested parallel workstreams

Partition for Opus 5 subagents; adjust freely.

| Stream | Owns | Depends on |
|---|---|---|
| **W-A** Bring-up & conformance | §0.3 preflight, §6.2 discovery, A-01, A-02, A-07, D-05 | nothing — do this first, alone |
| **W-B** Tiering & properties | A-03, A-06, A-08, D-02, and the property read-back audit | W-A |
| **W-C** Input format & colour | A-04, A-05, §10.3 (the SPS VUI rewrite), D-04 | W-A |
| **W-D** Latency, bitrate, sessions | A-09, A-10, A-11, A-13(b), D-07 | W-A; needs exclusive use of the GPU while measuring |
| **W-E** Parallelization | §10.2 (the AMF ring), D-03 | W-A, and merges after W-D's baseline is taken |
| **W-F** Research | §9, entirely off-machine | nothing |
| **W-G** Hybrid | A-12, A-13(a) | only if the hardware exists |

**Serialize every measurement.** Two agents cannot measure at once: they contend for VCN, for the
virtual display, and for the CPU. Have exactly one measurement agent at a time and let the others
read, research and write.

---

## 9. Phase 4 — the research agenda

Run these as Deep Research / web-search tasks in parallel with the machine work (workstream W-F).
The rule from §3 applies: **if one shipping product does the thing, it is possible.** Bring back
citations, not impressions, and file anything actionable as a finding.

### 9.1 The three reference implementations — read their source

1. **OBS Studio**, `plugins/obs-ffmpeg/texture-amf.cpp` (and its older `obs-amd-encoder` history).
   Questions: exactly which property table does it apply and in what order; how does it decide
   `USAGE`; what does it do about BGRA on the texture path (it is documented to refuse it — find
   the commit and the reason); how does it handle `AMFSurfaceObserver` and surface lifetime; what
   does its preset-compatibility check do with `GetCaps` before `Init`; and what does it set that
   we do not.
2. **FFmpeg**, `libavcodec/amfenc.c` + `amfenc_h264.c` + `libavutil/hwcontext_amf.c`. Questions: how
   does it decide "nothing yet" from `QueryOutput`; how does it map `gop_size` onto `IDRPeriod`;
   what is its `query_timeout` handling and does it read it back; does it rely on AMF copying
   input-surface properties onto the output buffer (its `av_frame_ref<N>` mechanism does — is that
   documented anywhere, or is it folklore we should not copy?); what does it do on
   `AMF_INPUT_FULL`.
3. **Sunshine** (the Moonlight host), its AMF encoder path. Questions: the "blunt" ULL probe (it
   re-applies the whole property table and retries `Init`) — is that still what it ships; what is
   its `async_depth`; what does it set for VBV and rate control; how does it handle multiple
   simultaneous clients on one VCN; does it use LTR or intra-refresh on AMD, and with what
   parameters.

Where **three independent projects set the same property to the same value, that is your answer.**
Where they diverge, that divergence is a measurement you should run here.

### 9.2 AMD's own sources

- The **AMF SDK documentation** for `AMFVideoEncoderVCE_AVC`: the full property table, the
  per-`USAGE` default table (Annex A of the encode doc — the source of the claim that ULL sets
  `RateControl = LatencyConstrainedVBR`, `VBV = 735 kb`, `EnforceHRD = true`,
  `LowLatencyInternal = true`, `IntraRefresh = 255 MBs/slot`, `PreAnalysis = false`,
  `QualityPreset = SPEED`, `IDRPeriod = 240`, `QueryTimeout = 0`), and the encode-latency section
  that says transcoding needs three input frames before any output.
- The **GPUOpen AMF issue tracker**. Specifically confirm the current status of: **#410** (ULL
  breaks `Init()` on pre-RDNA from Adrenalin 23.7.1), **#516** (`QueryTimeout` is effectively
  static), **#92** (Desktop Duplication flicker and growing latency; the `SetMultithreadProtected`
  + `Flush` fix), **#502** (AMD's two-thread model, and that one thread works), and **#398 / #277 /
  #540** (`OutColorProfile`, `OutColorPrimaries`, `OutColorTransferChar` and `OutMatrixCoeff` never
  reach the bitstream on any codec; only `FullRangeColor` works). For each: is it still open, has
  the driver you are running fixed it, and does your machine reproduce it?
- The **AMF version → Adrenalin driver mapping**, so a support report can convert
  `AMFQueryVersion` into a driver.
- **VCN generation capability tables**: which generations have B-frames (VCN 1.0–2.x, RDNA1
  included, do not), which have `SupportSliceOutput`, how many `NumOfHwInstances` each part
  advertises, and where AMD removed the AVC encoder entirely (Navi 24).

### 9.3 Open questions carried over from `docs/research/amd-amf.md` §8

Answer each on this hardware, or record why you could not:

1. Is `InputQueueSize` writable, and static or dynamic? (→ A-06)
2. Does `LOWLATENCY_MODE` ("POC mode 2") change the bitstream in a way a browser's WebCodecs H.264
   decoder cares about? POC type 2 constrains picture-order-count signalling for no-reordering
   streams and *should* be strictly easier to decode — but it is a bitstream-level change and the
   client page must work on old mobile WebKit as well as Chrome. Verify with a real client on at
   least two engines.
3. How do we get correct colour signalling out of AMF at all? (→ §10.3)
4. Does AMF really copy input-surface properties onto the output buffer? FFmpeg's whole frame
   lifetime depends on it and OBS depends on it for its `PTS` property, and it is nowhere
   documented. If it holds, it is a cheaper alternative to an `AMFSurfaceObserver`. Test it
   directly: set a custom property on the input surface and read it off the output buffer.
5. Slice-mode output (→ A-13a).
6. What does the ULL default intra-refresh cost (→ A-10).
7. Does the `AMFSize`-vs-`AMFRate` signedness distinction bite anywhere? Both are 8 bytes of two
   32-bit ints and only the variant type tag discriminates, so a binding that reuses one struct for
   both compiles and silently passes the wrong `AMF_VARIANT_TYPE`. The layout test catches sizes,
   not semantics — **review every `set_var` call site by hand** (`FrameSize` uses
   `AMFVariantStruct::size`, `FrameRate` uses `::rate`; confirm the tags).

### 9.4 Competitive and adjacent

- How do **Parsec, Sunshine/Moonlight and Steam Link** configure AMD encoders specifically, and
  what do their users report about AMD-vs-NVIDIA latency on the same link? The competitor table in
  §5.4 is the target; find out whether their AMD numbers differ from their NVIDIA numbers, and by
  how much.
- **SmartAccess Video**: what does `EnableEncoderSmartAccessVideo` actually do, on which
  iGPU+dGPU AMD pairings, and is splitting one low-latency stream across VCN instances ever a win?
  The code sets it `false` on the reasoning that it parallelises *throughput*, not latency. Confirm
  or refute with a source.
- **Pre-analysis / VBAQ / HMQB**: is there any published measurement of what they cost in *latency*
  (as opposed to quality) on VCN? We turn all three off; find out whether anyone has measured the
  trade.
- **AV1 and HEVC on VCN**: not in scope to implement, but record what it would take. Every
  identifier is name-mangled per codec (`AMFVideoEncoderHW_HEVC`, `AMFVideoEncoderHW_AV1` — note
  the wire strings do **not** match the macro names) and the enum values are renumbered, so each
  codec needs its own Rust enums and property module. The gate would be
  `CreateComponent(<id>) == AMF_OK` plus `GetAccelerationType() == AMF_ACCEL_HARDWARE`, never a
  version or a GPU name. The blocker is the client: nothing in the browser matrix decodes AV1 or
  HEVC reliably enough to negotiate today.

---

## 10. Phase 5 — the hyper-optimization sweep

Only after A-01…A-13 have run at least once and §7's baseline exists. The standard from §3 applies
in full: **every 0.1 ms is in scope**, and a speculative idea is resolved by trying it and
measuring, or by a recorded rejection with a number.

### 10.1 NVENC parity — the concrete technique list

`nvidia/encoder.rs` is the reference implementation and the quality bar. These are the techniques
it uses, each with its AMF equivalent and its current status in the AMD backend. Work through every
row.

| # | NVENC technique (where) | AMF equivalent | Status in `amd/encoder.rs` |
|---|---|---|---|
| 1 | **Zero-copy D3D11 texture input** — `nvEncRegisterResource` with `NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX`, `NV_ENC_BUFFER_FORMAT_ARGB`, on a `SHARED_NTHANDLE \| SHARED_KEYEDMUTEX` texture | `AMFContext::CreateSurfaceFromDX11Native` (slot 49) — a pointer *wrap*, not a copy, given `InitDX11(capture_device)` | **Done**, same-adapter only. Cross-adapter is A-12. |
| 2 | **Caps gating for every optional feature** — `EncoderCaps::query` reads 19 `NV_ENC_CAPS` and *every* optional field in `initialize()` is gated on one, because asking for an unsupported feature costs the whole vendor path (`build_backend` cannot tell "no intra refresh" from "no NVENC") | `AMFComponent::GetCaps` before `Init` + `HasProperty` | **Done** (W4b, COMPILE-CHECKED): every optional property goes through `HasProperty` + write + read-back, reported as one line. → verify on hardware (D-02) |
| 3 | **Required-entry-point check** — `require_functions()` verifies all 13 hot-path function pointers before anything opens, so a partially-populated driver table fails cleanly instead of panicking on a detached thread | Only two exports exist, but the vtables are unchecked at runtime | **Consider**: a post-`Init` sanity check (caps read back, `ExtraData` non-empty, first frame produced within N ms) that fails the backend cleanly rather than mid-stream. |
| 4 | **Async submit with completion events** — `enableEncodeAsync`, `OUTPUT_BUFFERS = 4` bitstream buffers + one auto-reset event each, `WaitForSingleObject(2000 ms)` turns a wedged encoder into a named error, `LockBitstream` with `doNotWait` never blocks. Measured a *throughput* wash on Ampere; kept for the **bounded wait**. | `QueryTimeout = 1 ms` makes `QueryOutput` block for us; AMD's own model is two threads (submit + query) | **Partial.** One thread, `SUBMIT_DEADLINE = 2 s` hang detector, `POLL_SLEEP = 1 ms` fallback. → A-08, §10.2 |
| 5 | **The `AsyncNvencRing` decoupling ring** (`pipeline.rs`) — K=3 shared textures, `crossbeam_channel` free/ready queues, a dedicated `nvenc-ring-encode` thread, `PoisonedSlots` quarantine, a `stage_timeout_ms` of two frame periods clamped to 5–50 ms, and `FRAME_POOL_BUFFERS_RING = 3` vs `_INLINE = 2` | nothing yet | **Done** (W4b, COMPILE-CHECKED): `AsyncEncodeRing` over `RingEncoder`/`RingTextures`, K=3, `RingTextures::Local` for AMF. NVENC measured unchanged either side. → verify + measure (§10.2) |
| 6 | **Bitrate update without reset** — `apply_pending_bitrate_atomic` reconfigures rate control with no teardown and no `Flush` | dynamic `TargetBitrate` / `PeakBitrate` / `VBVBufferSize` writes applied at the next `SubmitInput` | **Done**, unverified. → A-11 |
| 7 | **Repeat-frame path** — `encode_repeat` re-encodes the held slot for a static desktop, driven by `spawn_repeater` / the ring's keepalive, and the ring hands the encoder the shared `idr_request` atomic so a PLI on a still desktop still produces an IDR | `Encoder::encode_repeat` re-submits the staged texture: zero copies, one encode | **Done**, unverified. Confirm the IDR-on-static-desktop case specifically — it was a *blocker* on NVENC (`blocker-nvenc-ring-ignores-idr-on-static-desktop`) and only 2 IDRs appeared in 30 s before it was fixed. |
| 8 | **Transient-error classification** — `NV_ENC_ERR_ENCODER_BUSY` formats as "device busy", the substring `is_transient_encode_error` matches; up to `MAX_TRANSIENT_ENCODE_DROPS = 60` frames are shed rather than killing the session, counted in `encoder_transient_errors` | `classify()` + the `" [transient: device busy]"` hint | **Present but dead.** → A-13(b) |
| 9 | **Reused output scratch** — `au_scratch: Vec<u8>`, grown only, so the copy out of the driver's write-combined mapping never allocates while the bitstream lock is held | `au_scratch`, grown only, copied under the mapping and turned into a `Vec` after the release | **Done** (W4b, COMPILE-CHECKED) |
| 10 | **Full VUI** — `videoSignalTypePresentFlag`, `videoFullRangeFlag = 1`, `colourDescriptionPresentFlag`, BT.709 primaries/transfer/matrix, and **`bitstreamRestrictionFlag = 1`** so the SPS says `max_num_reorder_frames = 0`. Without it a decoder infers reorder depth from the level and holds min(16, MaxDpbMbs/MBs) frames before showing the first — **33–267 ms at 60 fps** | AMF is reported never to write any of `OutColorProfile` / `OutColorPrimaries` / `OutColorTransferChar` / `OutMatrixCoeff` into an AVC bitstream; only `FullRangeColor` works | **Done** (W4b, COMPILE-CHECKED): `amd/sps.rs` rewrites the SPS VUI, gated on a read-back; 8 unit tests. → verify (§10.3) |
| 11 | **Multiple reference frames + LTR** — `maxNumRefFrames = 4` when `multiple_ref_frames`, `numRefL0 = 4`, `enableLTR` with `ltrNumFrames = min(caps, 2)`, `mark_ltr` / `use_ltr` / `invalidate_reference`, wired to `streamer::repair::RepairState` in `ring_encode_thread` | `MaxOfLTRFrames` (0..=2), `MarkCurrentWithLTRIndex`, `ForceLTRReferenceBitfield`, `MarkedLTRIndex`, `ReferencedLTRIndexBitfield`; mutually exclusive with intra-refresh | **Done** (W4b, COMPILE-CHECKED): enabled on the ring, gated on `HasProperty` + `max_refs > 1` + the read-back, wired to `RepairState`. → verify + measure (§10.4) |
| 12 | **`lowDelayKeyFrameScale = 2`** — with a one-frame VBV, an IDR would otherwise have to blow the buffer or be quantised hard; this is the documented allowance for a keyframe to spend two frame budgets | no direct analogue; `MaxAUSize` is the nearest knob and is **never set** | **Open.** → §10.6 |
| 13 | **`zeroReorderDelay`, `enableLookahead = 0`, `enableAQ = 0`, `enableTemporalAQ = 0`, `multiPass = DISABLED`, `frameIntervalP = 1`, `gopLength = INFINITE`, `repeatSPSPPS`, no filler, no AUD, no SEI** | `BPicturesPattern = 0`, `RateControlPreanalysisEnable = DISABLED`, `EnablePreAnalysis = false`, `EnableVBAQ = false`, `HighMotionQualityBoostEnable = false`, `IDRPeriod = i32::MAX`, `HeaderInsertionSpacing = 0`, `FillerDataEnable = false`, `InsertAUD = false`, per-surface `InsertSPS`/`InsertPPS` | **Done**, all unverified. Read every one back. |
| 14 | **`raise_d3d11_gpu_priority(encoder.device())`** on the encoder device, and `tuning::tune_current_thread()` on the encode thread | same helpers, same call sites | **Partial**: `build_backend` calls `raise_d3d11_gpu_priority` on `backend.device()`, so AMF gets it. There is no AMD encode *thread* to tune yet. → §10.2 |
| 15 | **`nvenc_layout.rs`** — the FFI is asserted against numbers a C compiler measured, because a wrong offset is silent memory corruption | `test/amd_layout.rs`, 13 tests, regenerated by `tools/amf_layout_gen.c` | **Done.** Keep it done: never hand-edit the generated bodies, and treat a diff after a header bump as a required review item. |

### 10.2 Parallelization: give AMF the decoupled ring

> **Shape A landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify, and measure.** `AsyncNvencRing`
> is now `AsyncEncodeRing` over `RingEncoder::{Nvenc, Amf}` and `RingTextures::{Shared, Local}`;
> AMF's slots are the encoder's own textures on the capture device, so there is no shared handle,
> no keyed mutex and no way to poison a slot. K=3, `FRAME_POOL_BUFFERS_RING = 3`,
> `tune_current_thread()` on the encode thread, the keepalive consuming the shared `idr_request`,
> and the post-stage `Flush` are all as on the NVENC side. Selected for `--encoder amd`, and for
> `auto` only when the capture device is itself AMD. **NVENC was measured either side to prove no
> regression**: `probe --whep-selftest --duration 30`, median of 3 on a quiet box,
> `capture_to_socket` 2.88 / 5.24 / 11.53 / 12.08 ms before against 2.36 / 5.24 / 9.44 / 12.65 ms
> after, `encoder=nvenc-zero-copy-ring` unchanged. Ledger row 302. **Shape B (AMD's two-thread
> model, depth 2) is untried and still yours** — as is every number below, and the K sweep.

**The problem, precisely.** In `Pipeline::on_frame_arrived`, when `self.ring` is `None` — which it
always is on AMD, because `try_ring` is `matches!(vendor, EncoderVendor::Nvidia | EncoderVendor::Auto)`
and `build_zero_copy_ring` needs NVENC — the code locks `EncodeCore` and calls
`encode_captured` **inside the WGC `TypedEventHandler` callback**. So on an AMD host: capture
delivery, the optional scaler blt, the `CopyResource` into the ring slot, the `Flush`, the
`CreateSurfaceFromDX11Native` wrap, `SubmitInput`, the blocking `QueryOutput`, the `to_vec()` of
the access unit, and the broadcast **all happen on one thread, serially, in the callback that WGC
is waiting on.** Capture and encode do not overlap at all.

That is exactly what `AsyncNvencRing` exists to fix on the NVIDIA side, and it is why the NVENC
`selftest` row shows `encode` p50 **0.00 ms** — the ring decouples submit from encode so the
measured span is only the submit call.

**The work.** Build the AMD equivalent. Two shapes, and you should measure both rather than
assuming:

- **Shape A — an `AsyncAmfRing`, modelled directly on `AsyncNvencRing`.** N ring textures (start at
  the existing `NUM_IN_FLIGHT = 2`, then try 3 and 4), a `crossbeam_channel::bounded` free-slot
  queue and a ready queue, a dedicated `amf-ring-encode` thread running
  `tuning::tune_current_thread()`, a `PoisonedSlots` quarantine, and a `stage_timeout_ms` of two
  frame periods clamped to 5–50 ms so a GPU hiccup does not block the capture callback for a full
  second. The capture side then does only: blt/copy into a free slot, `Flush`, send `ReadyMsg`.
  Note that AMD's ring textures live on the **capture device**, so — unlike the NVENC cross-adapter
  case — there is no keyed mutex to acquire and no shared handle; the slot ownership is entirely
  yours to manage. Keep `FRAME_POOL_BUFFERS_RING = 3` for this path if it wins, `_INLINE = 2`
  otherwise.
- **Shape B — AMD's own two-thread model** (submit thread + query thread, AMF #502), with the
  strict one-in-flight discipline relaxed to two. **Be careful here**: the design note is explicit
  that submitting ahead grows wall-clock latency by (queue depth × frame time) while throughput
  still looks perfect — "the classic way an encoder integration measures fine and feels awful". If
  you try depth 2, measure `capture_to_socket`, not frames per second, and be prepared to revert.

**Also in scope for this stream:**

- **Thread affinity and priority.** `tuning::tune_current_thread()` (MMCSS / `TIME_CRITICAL` /
  P-core CPU-set confinement) is what the NVENC ring thread gets. Apply it, read back what it
  actually set, and measure with and without. Note the recorded rejection:
  `CreateFreeThreaded` was rejected partly *because* it moves this registration onto the wrong
  thread — do not repeat that mistake by tuning the wrong thread here.
- **The `Flush` after the stage copy.** It is there for a documented AMD reason (AMF #92: flicker
  *and growing latency* on a Desktop-Duplication device). Measure its cost, but treat removing it
  as requiring a torn-frame soak, not just a faster number.
- **Repeat/keepalive on the ring.** The NVENC ring's idle path re-encodes the held slot from inside
  the encode thread and consumes the shared `idr_request` atomic there, because `stage()` is the
  only other consumer and it does not run on a static desktop. Replicate that exactly, or you will
  reintroduce the NVENC blocker (2 IDRs in 30 s, a joining client with nothing to decode from).
- **Broadcast and channel sizing.** `BROADCAST_CAPACITY = 4`; `frames_dropped_broadcast` was zero
  in all twelve NVIDIA after-runs. Confirm it stays zero on AMD before touching it.

**Measure:** §7 Matrix A and B before and after, plus `frames_dropped_stale`,
`frames_dropped_ring`, `frames_dropped_broadcast` and `encoder_transient_errors`, plus the
capture-callback duration itself (you can infer it from `capture_delivery` widening under load).
The headline number is `capture_to_socket` p90 and p99.

### 10.3 Colour signalling: patch the SPS VUI ourselves

> **Landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify.** `amd/sps.rs` does exactly what this
> section specifies: find the SPS in the access unit and in `ExtraData`, unescape, copy the prefix
> **bit for bit**, write BT.709 primaries/transfer/matrix, `video_full_range_flag` from the actual
> input format, `bitstream_restriction_flag = 1` and `max_num_reorder_frames = 0`, re-escape,
> splice. Gated on a read-back of the SPS AMF produced, so it stands down the day #398 is fixed;
> per-frame cost on a P frame is one NAL-header read. Eight unit tests read the output back with
> `test/h264.rs::parse_sps`. **The "verify first" step below is still the one that matters** —
> record which of the two log lines this driver produces, and whether an AMD and an NVENC stream
> now look the same in the client page. Ledger row 304.

**The problem.** AMF is reported (AMF #398, #277, #540, still open on driver 24.8.1) never to write
`OutColorProfile`, `OutColorPrimaries`, `OutColorTransferChar` or `OutMatrixCoeff` into an AVC
bitstream. Only `FullRangeColor` reaches the stream. The encoder sets all five anyway in case a
future driver honours them. Meanwhile the NVENC path writes a **full** VUI including
`bitstreamRestrictionFlag = 1`, which is what tells a decoder `max_num_reorder_frames = 0`.

So an AMD host and an NVIDIA host produce **differently signalled streams from identical input** —
and the AMD one may be telling browsers to buffer frames we never reorder.

**Verify first.** With `ffprobe -show_entries stream=color_range,color_space,color_primaries,color_transfer`,
and with the SPS parser that already exists in `windows_utils/streamer/test/h264.rs`
(`parse_sps` → `Sps { vui_present, bitstream_restriction, max_num_reorder_frames, … }`). Record for
this driver: which of the five properties reached the bitstream, and whether
`bitstream_restriction` is set at all. Then render an AMD stream and an NVENC stream of the same
content in the client page on the **same** device and say whether they look different.

**Then fix it, in your own territory.** We packetize H.264 by hand
(`webrtc_session.rs` uses a raw `TrackLocalStaticRTP`), so the SPS is ours to rewrite — and the
cleanest place is inside `amd/encoder.rs`, on the AU as it leaves `query_once` and on the cached
`extradata`, so nothing outside the AMD backend changes. Write an SPS NAL rewriter that:

- finds the SPS NAL in the access unit (and in `ExtraData`),
- unescapes the emulation-prevention bytes, parses to the VUI, sets `video_signal_type_present_flag`
  / `colour_description_present_flag` / primaries / transfer / matrix = BT.709 and
  `video_full_range_flag` to match the actual input range, and sets
  `bitstream_restriction_flag = 1` with `max_num_reorder_frames = 0` and `max_dec_frame_buffering`
  consistent with the DPB we actually use,
- re-escapes and splices the NAL back,
- and does it **once per distinct SPS**, caching the rewritten bytes, so the per-frame cost is a
  memcmp rather than a bitstream parse.

Gate it on a read-back: if a future driver does write the VUI correctly, detect that (parse the
SPS AMF produced) and skip the rewrite. Unit-test it against `test/h264.rs`'s parser, in both
directions, including a stream with emulation-prevention bytes inside the VUI.

**Measure:** client-side decode latency and time-to-first-paint on at least two browser engines,
before and after; plus a visual A/B against an NVENC stream. If the browser was already inferring
BT.709 for HD and libwebrtc was already rewriting the SPS on the receive path — which is what
happens on the RTP transport today — then the win is insurance rather than latency, and you should
say so with the measurement that shows it. It stops being insurance the moment video leaves RTP.

### 10.4 LTR-driven repair — the blocker is gone

> **Landed COMPILE-CHECKED at `9efbd7f` (W4b, merged into `latency-overhaul` at `b333707`) — verify, and measure.** See D-08 above for what
> was wired. The decisions this section asks for were made and recorded: LTR is enabled only on
> the ring (the only path that reads receipts), only when `HasProperty(MaxOfLTRFrames)` and
> `caps.max_reference_frames > 1` and the read-back is non-zero; `MaxNumRefFrames` is still never
> set; and `IntraRefreshMBsNumberPerSlot = 0` is written when LTR is on, so the read-back shows
> the exclusivity instead of leaving a 255 nobody honours. **The measurements are still open**:
> IDRs per minute under induced loss, bytes per repair frame, `keyframes_avoided`, and A-10's
> SSIM number as the other half of the choice.

`amd/encoder.rs` says LTR stays off because acting on it "needs the WebRTC session to tell the
encoder which frames the client acknowledged". **That machinery now exists**:
`src-tauri/src/streamer/repair.rs` provides `RepairState` with `set_ltr_slots`, `note_encoded`,
`note_marked`, `acknowledge(capture_ns)`, `note_lost`, `request_repair`, `take_action`,
`take_lost`, and the counters `repairs` / `invalidations` / `keyframes_avoided`. The NVENC ring
thread already consumes it: it calls `repair.set_ltr_slots(encoder.ltr_slots())` at startup, drains
`take_lost` into `invalidate_reference` before each submit, and predicts from an acknowledged LTR
instead of paying for an IDR.

**Do the AMD half**, gated on caps:

- Turn LTR on only when `MaxOfLTRFrames` is writable and `caps.max_reference_frames > 1`. Remember
  it is **mutually exclusive with intra-refresh** ("With user control of LTR, B-pictures and
  Intra-refresh features are not supported"), so this is a *choice* between two loss-recovery
  strategies, not an addition — and A-10's SSIM number is one input to that choice.
- Note `MaxNumRefFrames` is deliberately never set today, precisely because forcing 1 reference
  would disable intra-refresh. Under LTR the reasoning inverts: you want more than one reference.
  Decide on the evidence and record it.
- Wire `mark_next_frame_as_ltr` / `force_ltr_reference` / `last_marked_ltr` /
  `last_referenced_ltr` into the same `RepairState` protocol the NVENC ring uses, in your new AMF
  ring thread (§10.2). AMF has no `invalidate_reference` analogue — it *disallows* LTRs outside the
  forced bitfield from current and future reference, which is a different mechanism achieving a
  similar end. Work out the mapping and document it.
- If `MaxOfLTRFrames` is not available on this VCN, that is a result: record it and leave
  intra-refresh as the resilience mechanism.

**Measure:** IDRs per minute with and without repair, under induced loss; bytes per repair frame vs
bytes per IDR; and `keyframes_avoided`. The NVENC-side reference measurement was a repair frame at
**23,698 B against a 28,423 B IDR** on a pessimistic synthetic fixture — a smaller margin than you
might expect, which is why the number that matters is *how many IDRs a lossy minute stops
producing*, not the per-frame size.

### 10.5 Allocation and copy audit

Walk the whole AMD frame path and remove every avoidable allocation and copy. Known items:

- **`query_once` allocates a `Vec<u8>` per frame** (`slice::from_raw_parts(native, size).to_vec()`).
  NVENC solved this with `au_scratch`: a retained buffer, grown only, so the copy out of the
  driver's mapping never allocates. Do the same, and return a slice/`Bytes` rather than a fresh
  `Vec` where the pipeline allows it. Note the pipeline currently takes `Result<Vec<u8>>` from
  `encode_gpu` / `encode_cpu` / `encode_repeat` and immediately does `Bytes::from(au)` — measure
  whether wrapping the scratch in `Bytes` (which can take ownership of a `Vec`) once per frame is
  cheaper end to end.
- **`submit_slot` clones the `ID3D11Texture2D`** (a COM AddRef/Release pair per frame) twice: once
  in the caller and once again inside `submit_slot`. Cheap, but it is on the hot path.
- **`carry: Vec<u8>` in `submit_surface`** is allocated per frame even though it stays empty under
  one-in-flight discipline. `Vec::new()` does not allocate, so this one is free — verify rather
  than assume, and leave it if so.
- **`sequence_params()` clones the extradata** on every call. Check who calls it and how often.
- **`set_var` builds an `AMFVariantStruct` per property write.** That is 30-odd writes at init and
  1–4 per frame (the per-surface `InsertAUD`, and `ForcePictureType`/`InsertSPS`/`InsertPPS` on
  IDRs). `InsertAUD = false` is written on **every single surface** — check whether it is sticky on
  the component instead, which would remove three property calls per frame.
- **`texture_format(src)` calls `GetDesc` per frame** on the no-scaler path. Cache the format and
  invalidate on a size change.
- Profile the whole thing. If you have Windows Performance Analyzer or Superluminal available, use
  it; otherwise instrument with `host_now_ns` spans and report percentiles.

### 10.6 The remaining AMF knobs — every one, measured

These properties exist in `amd_sys/props.rs` and are **never written**. Each is a hypothesis with a
measurement attached. Try them, keep what wins, and record a rejection with a number for what does
not.

| Property | Hypothesis | How to decide |
|---|---|---|
| `MaxAUSize` | The nearest AMF analogue to NVENC's `lowDelayKeyFrameScale`: capping the access unit means no single frame can head-of-line-block the link. With `VBV = bitrate/fps` and peak == target, an IDR must already be squeezed hard — measure whether an explicit cap helps the tail or just wrecks keyframe quality. | `capture_to_socket` p99/max, and IDR bytes, at several caps (2×, 3×, 4× the per-frame budget) and unset |
| `RateControlSkipFrameEnable` | Frame skipping prevents VBV overflow on a sudden scene change. For a screen stream it may also produce visible stutter. | bytes/frame variance, dropped-frame count, and the client's own paint interval |
| `InitialVBVBufferFullness` | Left at the default 64 (= 100 %) deliberately; 0 means *empty*, the worst case. Confirm the default read-back is really 64, and try 32 to see whether a half-full buffer starts faster. | time-to-first-frame, and the first 30 frames' sizes |
| `MinQP` / `MaxQP` | Only `MinQP` is set today, and only when a quality QP is configured — `Config::quality_qp`, which the probe harness sets from `--qp <1..51>` and the shipped path from the per-device quality slider; it reaches the backend as `EncoderConfig::min_qp` and is applied only when it is `> 1`. A `MaxQP` ceiling bounds how ugly a hard frame is allowed to get, which under CBR is how you trade a bitrate spike for quality. | SSIM on `motion`, bytes/frame p99 |
| `LTRMode` | Present in the catalogue, unused. Read the header's meaning and decide whether it matters once §10.4 turns LTR on. | — |
| `ProfileLevel` | Deliberately unset (browsers are lenient, and pinning one low enough for every device would cap resolution). Confirm what AMF derives, and that it is not *lower* than the geometry needs — a too-low level in the SPS can make a strict decoder refuse. | parse the SPS level and compare with the geometry |
| `QualityPreset` | `SPEED` (1) is set. Measure `BALANCED` (5) and `QUALITY` (0) anyway: on a modern VCN the encode-time delta may be small enough that the quality is free, and the ledger wants the number either way. **Remember the value is codec-specific** — 1 is SPEED on AVC only. | `encode` percentiles and SSIM at fixed bitrate |
| `EnableEncoderSmartAccessVideo` | Set `false`. If this machine is an AMD iGPU + AMD dGPU pairing, try `true` and measure — this is the only hardware where the question is answerable. | `encode` percentiles, and whether it initialises at all |
| `HighMotionQualityBoostEnable` | Set `false`. Confirm the property exists here and that turning it on really costs latency rather than being free on this VCN. | `encode` percentiles on `motion` |
| `IntraRefreshMBsNumberPerSlot` | ULL's 255 is the default. Sweep it (0, 64, 128, 255) — a slower wave costs fewer bits per frame and recovers more slowly. | bytes/frame, SSIM, and time-to-recover after an induced loss burst |
| `SlicesPerFrame` | 1 today. Only worth more than 1 alongside `OUTPUT_MODE = SLICE` — but *measure* whether multiple slices alone change encode time on VCN, since some hardware parallelises across slices internally. | `encode` percentiles at 1, 2, 4 slices |
| `DeBlockingFilter` | `true` — free on the encoder, better picture. Confirm it is actually free here. | `encode` percentiles on/off |
| `CABACEnable` | CABAC except on Baseline. Worth 5–15 % bitrate at equal quality on screen content, at some decode cost on weak clients. Confirm the profile negotiation actually reaches High on your clients. | bytes/frame at fixed SSIM |

Also sweep the **structural** parameters: `NUM_IN_FLIGHT` (2 → 1, 3, 4), `SUBMIT_DEADLINE`,
`POLL_SLEEP`, and `QueryTimeout` (§A-08).

### 10.7 The Auto chain and the fallback

`build_backend`'s `Auto` order is NVENC → Intel → **AMF** → x264, with AMF after Intel deliberately
("an Intel-iGPU + Radeon-dGPU laptop should still prefer the same-adapter Quick Sync path, and an
all-AMD box never reaches the Intel branch because its capture device is not Intel"). Verify that
reasoning holds on your hardware:

- On an all-AMD box, confirm `Auto` reaches AMF and does not waste time failing Intel first (log
  the elapsed time of each failed attempt — a slow failing probe is startup latency the user pays).
- On a hybrid box, confirm `Auto` picks Quick Sync and that this is genuinely the faster choice by
  measuring both.
- Confirm `EncoderVendor::Amd` explicitly requested falls through to x264 cleanly with a named
  reason when AMF is unavailable, and that the message a user would see is accurate.
- Confirm `capture_adapter_preference` (`windows_utils/streamer/mod.rs`) puts the capture device on
  the AMD adapter for `EncoderVendor::Amd` (`GpuPreference::Vendor(AMD_PCI_VENDOR_ID)`) and that
  `Auto`'s `HighPerformance` lands somewhere sensible on your machine — read the
  `adapters: capture=… encode=… same_adapter=…` line and say.

---

## 11. Verification and gates

### 11.1 Every commit

From `src-tauri/`: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
Plus `cargo test --lib windows_utils::streamer::test::amd_layout` explicitly, and the `dumpbin`
import check after any release build.

### 11.2 The ABI, if you touch the headers or the FFI

Never hand-edit the generated bodies in `test/amd_layout.rs`. Regenerate:

```bat
cd src-tauri\tools
amf_layout_build.bat
```

which enters the x64 environment via `vcvars64.bat`, builds `amf_layout_gen.c` in **C mode**
(`cl /TC`, so the `#else /* !__cplusplus */` branch we bind is what gets measured) plus
`amf_layout_cpp_check.cpp` in C++ mode (`/TP`), and writes `amf_layout_generated.txt` and
`amf_layout_cpp_check.txt`. Paste the sections over the corresponding `abi_*_matches_amf_headers`
bodies, in order. **MSVC, not MinGW** — `amfrt64.dll` is MSVC-built and MinGW does not model the
MSVC C++ ABI. Any struct whose C and C++ builds disagree must be modelled from the **C++** side;
today exactly one does (`AMFPropertyInfo`, 112 bytes in C with `name` at 0, 120 in C++ with `name`
at 8, because of a virtual destructor) and `amd_sys` sidesteps it by never calling
`GetPropertyInfo`. A diff in the numbers after a header bump is a **required review item**.

Remember what the generator cannot check: whether the *driver* agrees. Only running the encoder
does that — which is the whole point of this guide. Consider adding a
`test/amd_encoder.rs` mirroring `test/nvenc_encoder.rs`: a hardware-gated functional test that
encodes a handful of frames, asserts the NAL structure and the caps, and reports a visible
**SKIP** on a machine without AMF. The convention is `crate::test_support::skip(what, reason)` /
`skip_because(what, reason)` — re-exported into this module as
`windows_utils::streamer::test::skip` — which prints `SKIP <what>: <reason>` on stderr, and
**`SE_REQUIRE_HW=1` turns a skip into a failure. Set it on your machine**, because a Radeon box is
exactly where an AMD test silently not running is most misleading. Gate with
`#[ignore = "<reason>"]` so the harness prints the reason rather than a bare `ignored`.

### 11.3 Manual checks that remain

`docs/manual-tests/amd-amf.md`, all 11 sections, **as corrected**. Run it end to end once at the
very end, on the final binary, and record every answer under "What to report back". Anything it
asks for that you cannot produce is a line in the report, not a blank.

### 11.4 The final production build gate

After all work is complete and immediately before writing the report:

```powershell
pnpm tauri build
```

**The one failure you may accept is the Tauri updater signing key error** — the private key for the
updater manifest is not on this machine and that is expected. Zero `cargo` errors, zero TypeScript
errors, zero linker errors, zero missing-resource errors, zero bundling errors. If anything else
fails, it is not done: go fix it. Do not write the report over a broken build.

Also run and pass, from the root: `pnpm lint`, `pnpm build`,
`node scripts/check-legacy-webkit.mjs`, `node scripts/check-bindings.mjs`,
`node scripts/check-versions.mjs`.

Then run the `dumpbin` import check one final time against the **bundled** binary, confirm the tree
is clean and reviewable, confirm `findings/` contains nothing of yours, and only then write the
report.

(There is no macOS work in this guide, so the macOS overlay build and `scripts/check-macos-dyld.sh`
do not apply. Say so explicitly in the report rather than leaving it ambiguous.)

---

## 12. Deliverables and the final report

### 12.1 Deliverables

1. **Code** on branch `guide/windows-amd`, committed in small single-purpose commits, each
   performance commit stating its measured before/after, each message carrying the trailer from
   §2.1. **Nothing pushed.**
2. **`docs/OPTIMIZATIONS.md`** — every AMD row moved to `VERIFIED` with numbers or reverted with a
   reason; new rows for everything you changed, including the rejections.
3. **`docs/CAPABILITY-TIERS.md`** — the four AMD-touching rows updated with the tier this machine
   selected and why.
4. **`docs/BASELINE.md`** — a new "Matrix — Windows + AMD" section with §7's tables filled, the
   `| **AMD** | No AMD GPU on either machine. |` row deleted from "What could not be measured", and
   the glass-to-glass numbers with the camera model and both refresh rates.
5. **`docs/discovery/amd-host-<machine>.md`** — the hardware snapshot from §6.2.
6. **`docs/manual-tests/amd-amf.md`** — corrected, extended, and re-run end to end.
7. **`docs/RISK-REGISTER.md`** — R-011, R-228, the AMD half of R-231, and R-315/R-317 closed or
   updated.
8. **`findings/`** — every file you created deleted (resolved), or listed in the report with the
   reason it survives.
9. **`src-tauri/src/windows_utils/streamer/amd.md`** — truthful. Its "Status: `UNVERIFIED`" banner
   is the single most satisfying line in this engagement to change.

### 12.2 The report

Write it as your final message **and** as `docs/reports/windows-amd-<date>.md`. Sections, in order:

1. **Build result, at the top.** That `pnpm tauri build` succeeded and that the only error was
   updater signing (or that there was none).
2. **Machine facts** — exact Windows build, model, CPU, GPU (family and VCN generation), Adrenalin
   version, `amfrt64.dll` size/date, `AMFQueryVersion` value, monitors and which adapter composes
   them, whether the IDD driver was installed, browser and OS versions of every client used, AP
   model and band, power plan, laptop-on-mains-or-battery. Enough that someone can reproduce it.
3. **The `AMF AVC caps:` line, verbatim**, plus every `AMF optional property not applied:` line.
4. **Per-item status**, one line each, against §8's numbered inventory (A-01…A-13, D-01…D-08):
   `VERIFIED (numbers) | reverted (reason) | still UNVERIFIED (what was missing)`. **No item may be
   silently absent.**
5. **Before/after numbers** for every change, as p50 / p90 / p99 / max, with the trial count and
   whether the machine was quiet.
6. **The comparison table**: AMF vs x264 vs (if present) Quick Sync **on this machine**, against
   `docs/BASELINE.md`'s NVENC row and against the competitor numbers in §5.4 (Luna Display 22.4 ms
   Wi-Fi as the target, Hopp 98 ms as the browser-WebRTC ceiling). State plainly whether an AMD
   host now sits in the same band as an NVIDIA one, and if not, where the gap is.
7. **Decisions Made Without Consultation** — every call you made that a person might have made
   differently, with the reasoning, so it can be reviewed and reversed. Include the §2.3 named
   exception if you took it.
8. **Verification status** — what is `VERIFIED`, what remains `COMPILE-CHECKED` or `UNVERIFIED`,
   and for each of the latter, the hardware or OS it needs and the manual-test section that closes
   it. Be specific: "needs a pre-RDNA part on Adrenalin ≥ 23.7.1" is useful; "needs more testing"
   is not.
9. **What did not work, with numbers.** Negative results are results. Every knob you tried and
   rejected, with the measurement that rejected it.
10. **The ledger diff** — the exact rows added or edited in `docs/OPTIMIZATIONS.md` and
    `docs/CAPABILITY-TIERS.md`, and the `docs/BASELINE.md` cells filled.
11. **Findings** — created, resolved-and-deleted, and retagged, by filename.
12. **Recommendations requiring the user** — anything you could not do inside the scope fence
    (a transport-side change for slice mode, a UI-visible improvement, a driver rebuild), each with
    its measured upside.
13. **Commits** — branch name and one line per commit. **Nothing pushed.**

Write down every number you produced, including the ones that disappointed you.
