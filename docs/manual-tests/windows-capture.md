# Manual test: Windows Graphics Capture on a pre-26100 build

**Why this exists:** the WGC feature tiering is compile-checked only. `GraphicsCaptureSession`'s
`MinUpdateInterval` and `DirtyRegionMode` are **build 26100** (Windows 11 24H2) properties;
`IsBorderRequired` is **build 20348**; `IsCursorCaptureEnabled` is **19041**. Until this change,
requesting any absent property made `GraphicsCaptureApi::new` return an error outright, so
ScreenExtend fell back to DXGI Desktop Duplication on **every Windows 10 host and every Windows 11
before 24H2** — losing the NVENC zero-copy ring and serializing capture against encode. Each
property is now probed on its own with `ApiInformation::IsPropertyPresent` and skipped when
absent. The dev box is Windows 11 build 26200, where all of them are present, so the only thing
that box can prove is that the 26100 path still works. What it cannot test is a build *without*
the 26100 properties, and that is what this script is for.

**What you need**

- A **Windows 10 22H2** machine (build 19045) and/or a **Windows 11 23H2** machine (build 22631).
  Either one alone is worth running; both is better, because they sit on different property tiers.
- A VM is adequate. The question here is "does the session construct and stay on WGC", not a
  latency number. Only step 7 wants real hardware.
- A build of the app on that machine. Steps 5 and 6 additionally need `--features probe`.
- `ffplay` on `PATH` for step 6.
- A phone or tablet on the same network to join with.
- Roughly 45 minutes.

**Record as you go.** Every step says what to paste back. The result updates the *Screen capture
(Windows)* row in `docs/CAPABILITY-TIERS.md`, which today reads
`VERIFIED (WindowsGraphicsCapture on Win11 26200)` and says nothing about older builds. If a step
fails, file what broke rather than patching around it on the test machine.

---

## 0. Machine facts (2 min)

```powershell
[System.Environment]::OSVersion
Get-ComputerInfo | Select-Object WindowsProductName, OsBuildNumber
Get-CimInstance Win32_VideoController | Select-Object Name, DriverVersion
```

Also run `winver` and take the build from the dialog — that is the number the tiering keys on, and
`OSVersion` alone under-reports on some hosts.

Paste all three. Then find your row here; every later step compares against it.

| Host | Build | Expected feature flags |
|---|---|---|
| Windows 10 22H2 | 19045 | `+cursor -border -secondary-windows -min-update-interval -dirty-region +free-threaded-pool`, `(pre-26100)` |
| Windows 11 21H2 / 22H2 / 23H2 | 22000 / 22621 / 22631 | `+cursor +border +secondary-windows -min-update-interval -dirty-region +free-threaded-pool`, `(pre-26100)` |
| Windows 11 24H2+ | 26100+ | every flag `+`, `(build 26100+)` |

Which property arrives when, from `WgcFeatures`:

| Flag | Property | Build |
|---|---|---|
| `cursor` | `IsCursorCaptureEnabled` | 19041 (Windows 10 2004) |
| `border` | `IsBorderRequired` | 20348 |
| `secondary-windows` | `IncludeSecondaryWindows` | 22000 (Windows 11 21H2) |
| `min-update-interval` | `MinUpdateInterval` | 26100 (Windows 11 24H2) |
| `dirty-region` | `DirtyRegionMode` | 26100 (Windows 11 24H2) |
| `free-threaded-pool` | `Direct3D11CaptureFramePool::CreateFreeThreaded` | 17763 (Windows 10 1809) |

`+free-threaded-pool` should read `+` on every machine in the first table. A `-` there is a
finding; report it.

---

## 1. WGC is selected at all (5 min)

This is the whole point of the change. Start the headless host and keep its output:

```powershell
cargo run --release -- serve -v | Tee-Object -FilePath wgc-run.log
```

Join from the phone at the printed URL, let the picture come up, then in a second shell:

```powershell
Select-String -Path wgc-run.log -Pattern 'pipeline: starting|wgc: feature set|falling back to DXGI'
```

**PASS** is these two lines, in this order:

```
pipeline: starting live monitor capture (WGC): display=..., name=..., gpu=..., native=WxH, encode=WxH, downscale=..., fps=..., bitrate_bps=...
wgc: feature set +cursor +border +secondary-windows +min-update-interval +dirty-region +free-threaded-pool (build 26100+); optional properties this build lacks are skipped, not fatal
```

with the flags and the parenthetical matching your row from step 0 — on a pre-26100 host the same
line must read `-min-update-interval -dirty-region` and `(pre-26100)`, and on Windows 10 also
`-border -secondary-windows`.

**FAIL** is either of these appearing:

```
WGC capture failed for <device>: <error>; falling back to DXGI Desktop Duplication
pipeline: starting DXGI duplication capture: device=..., native=..., ...
```

That is the exact regression this change was meant to remove. Paste the `<error>` text verbatim —
it names which property or call refused.

| Symptom | Meaning |
|---|---|
| Fallback line, error names a property | A property is still being requested unconditionally instead of behind `IsPropertyPresent`. |
| Fallback line, error from `IsSupported` | WGC is genuinely off on this host — Server core, some VM graphics stacks. Note the GPU from step 0; this is not a tiering bug. |
| Neither `pipeline: starting` line appears | The session never started. Look further up the log; this is a join or virtual-display failure, not a capture one. |
| Both lines appear, in that order | WGC started and then died mid-session. Note how long it survived. |

Keep `wgc-run.log`. Steps 2, 3 and 4 read the same file.

---

## 2. The feature summary matches the build (3 min)

```powershell
Select-String -Path wgc-run.log -Pattern 'wgc: feature set' | Select-Object -First 1
```

Compare each flag against the table in step 0. Every `-` must be a property your build genuinely
lacks, and every property your build has must be a `+`.

| Reading | Meaning |
|---|---|
| A `-` for a property your build should have | The probe is too strict — a capability the host offers is being left on the table. Say which flag. |
| A `+` for a property your build should not have | `IsPropertyPresent` returned true for something absent, or the summary is not derived from the same probe the session uses. Worse than the first case; report it immediately. |
| `(build 26100+)` on a 19045 or 22631 host | The tier name and the property probe disagree. Report it. |

One caveat on `border`: the pipeline asks for `DrawBorderSettings::WithoutBorder`, but suppressing
the yellow capture border also needs the user to have granted
`GraphicsCaptureAccess.RequestAccessAsync(Borderless)`. Without that consent the setter still
succeeds and the border stays. So `+border` in the summary means the property exists, not that the
border is gone — if you see a border on the client, that is this, not a probe failure.

---

## 3. The frame pool did not regress (5 min)

With the session still up, drive the extended display hard for two or three minutes — scroll a long
page, drag a window around, play a video full-screen. `serve -v` prints one telemetry line per
session every 5 seconds:

```powershell
Select-String -Path wgc-run.log -Pattern 'telemetry: capture=' | Select-Object -Last 6
```

The line has the shape:

```
telemetry: capture=wgc[<feature summary>] encoder=... frames=... <stage>=p50/p90/p99/max ... | <degradation counters>
```

The counter to watch is **`frames_dropped_stale`** — frames the pool discarded while draining to
the newest one.

- **Zero, or a small count that stops growing once the load stops:** correct. The drain is doing
  its job, which is to hand the encoder the newest frame instead of a queue of old ones.
- **A large count that keeps climbing under sustained load:** the encoder is behind the capture and
  the pool is absorbing the difference. Note the rate — count divided by seconds — and the `encode`
  stage p99 from the same line; the two should tell the same story.

Note `frames_dropped_ring`, `frames_repeated` and `idrs_sent` from the same line too. Zero for all
three is normal on an idle-to-moderate session; `frames_repeated` rising while nothing moves on the
extended display is expected. A counter missing from the line is zero — the summary only prints
non-zero counters.

---

## 4. The adapter pairing (5 min)

Logged once per session:

```powershell
Select-String -Path wgc-run.log -Pattern 'adapters:|SOFTWARE rasteriser|live capture ready|zero-copy path unavailable'
```

The first line is:

```
adapters: capture=<name> (vendor 0xXXXX, LUID 0x...) encode=<name> (vendor 0xXXXX, LUID 0x...) same_adapter=true|false
```

**`same_adapter=true` is the goal.** It means the captured texture never leaves the GPU that
produced it, which is the precondition for the zero-copy path.

`same_adapter=false` means every frame takes a CPU round trip — GPU to system memory to the other
GPU. On a hybrid laptop that can be legitimate (capture on the iGPU, encode on the dGPU), but it is
always a cost. The line that confirms the consequence is:

```
NVENC zero-copy path unavailable (...); falling back to CPU bridge (higher latency)
```

after which the path line names a bridge rather than the ring. You may also see
`adapters: capture=<name> encode=cpu (software encoder)` — that is the x264 case, and step 4's
question does not apply to it.

On a VM, watch for either software-rasteriser warning:

```
dxgi: output <name> is composed by a SOFTWARE rasteriser -- ...
```

That is the VM's virtual GPU, not a bug. Note it, because it explains any latency number from
step 7 and makes step 3's counters uninteresting.

Then the path selection line:

```
pipeline: live capture ready -- NVENC ZERO-COPY RING (K=3) decoupled path (WxH@fps)
```

One of these must appear. Paste whichever you got:

| Path line | What it means here |
|---|---|
| `NVENC ZERO-COPY RING (K=3) decoupled path` | The best outcome, and the one the DXGI fallback was costing pre-26100 hosts. |
| `NVENC ZERO-COPY cross-adapter GPU path` | Two GPUs, still no CPU copy. |
| `INTEL Quick Sync same-adapter path` | Fine. Intel host. |
| `AMD AMF same-adapter GPU path` | Fine. The encoder itself is `docs/manual-tests/amd-amf.md`, not this script. |
| `NVENC CPU-bridge fallback` | Works, but every frame round-trips through system memory. Pair it with the `same_adapter=` value. |
| `SOFTWARE x264 CPU path` | No hardware encoder was usable. Expected on most VMs. |

---

## 5. Forced DXGI still works (5 min)

The fallback has to keep working — this change makes it rarer, not obsolete. DXGI is otherwise only
reached by WGC failing, so the probe harness is the only way to exercise it deliberately:

```powershell
cd src-tauri
cargo run --release --features probe -- probe --capture-backend dxgi --whep-selftest --duration 30
```

Expect:

```
pipeline: starting DXGI duplication capture: device=..., native=..., ...
```

then the self-test running its full 30 seconds and exiting having proved RTP flowed, followed by
its per-stage telemetry summary. Paste the summary line.

| Symptom | Meaning |
|---|---|
| `error: --whep-selftest is not in this build` | Rebuild with `--features probe`. The self-test carries a TLS verifier that accepts any certificate, so it is not in a normal build. |
| Self-test exits early, no RTP | The DXGI path is broken. Paste everything after the `pipeline: starting DXGI` line. |
| The WGC line appears instead | `--capture-backend dxgi` was not honoured. |

Run the same command with `--capture-backend wgc` for a same-machine comparison; the two summaries
side by side are what step 7 wants anyway.

---

## 5b. What DXGI does on hybrid graphics, and how loudly it says so (10 min)

The DXGI fallback is not available everywhere, and the point of this step is that a machine where
it is unavailable must **say so** rather than negotiate a session and show a black screen.

Three things changed and each has a log line that proves it:

1. `IDXGIOutput5::DuplicateOutput1` is tried before `IDXGIOutput1::DuplicateOutput`, probed by
   `QueryInterface` rather than by a build number. It takes an explicit format list and is the only
   one that can duplicate a display in HDR mode.
2. Every adapter that enumerates the display is tried, not just the first. On a hybrid laptop the
   same display can be listed by more than one adapter and only the one that composes it can
   duplicate it.
3. A duplication that is re-established without ever producing a frame now fails the capture after
   40 attempts instead of looping forever, and the "re-established" line is rate-limited.

### The healthy shape

```powershell
cd src-tauri
cargo run --release --features probe -- probe --capture-backend dxgi --whep-selftest --duration 30
```

```
dxgi: duplication ready on \.\DISPLAYn via DuplicateOutput1 on <adapter> (vendor 0x____, LUID 0x____): WxH, rotation=..., logical=WxH
pipeline: live capture ready -- DXGI duplication (DuplicateOutput1) -> <path>
```

`via DuplicateOutput` instead of `DuplicateOutput1` is fine and expected on Windows 8.1-era stacks;
it only means the newer interface was absent or declined. Note which one you got — that is the
answer this step exists to collect.

### The unavailable shape

On this project's dev box (RTX 3080 Ti Laptop + Intel UHD, hybrid), the primary display is
enumerated **only** by the NVIDIA adapter and that adapter refuses duplication through either
entry point:

```
probe failed: no adapter can duplicate \.\DISPLAY1 (NVIDIA GeForce RTX 3080 Ti Laptop GPU (vendor 0x10DE, LUID 0x17D9A): Error { code: HRESULT(0x887A0004), ... })
```

`0x887A0004` is `DXGI_ERROR_UNSUPPORTED`. That machine is WGC-only and that is a legitimate
outcome — but the message must name every adapter it tried and why each declined. A bare HRESULT
with no adapter named is a regression; report it.

The compatibility report agrees, because its `dxgi` probe now creates a duplication and drops it
rather than asking whether a DXGI factory exists:

```powershell
cargo run --release -- doctor --json
```

`capture` should read `WindowsGraphicsCapture` on any machine where WGC works. In the log from the
same run, exactly one of these two lines must appear:

```
dxgi: duplication fallback available via DuplicateOutput1
dxgi: duplication fallback is NOT available on this machine (...); Windows.Graphics.Capture is the only capture path here
```

| Symptom | Meaning |
|---|---|
| Neither line | The probe did not run. Report it — the report is back to guessing. |
| "NOT available" on a machine where step 5 succeeded | The probe and the live path disagree; paste both. |
| "available" on a machine where step 5 failed | Same, in the other direction, and worse. |

### The spin that must not happen

Against a freshly created virtual display, watch for:

```
dxgi: duplication re-established (1 since the last frame)
dxgi: duplication re-established (10 since the last frame)
...
dxgi capture: duplication was re-established 41 times without ever delivering a frame; this output cannot be captured with DXGI Desktop Duplication
```

The count rising to 10, 20, 30 and then the capture stopping with that message is **correct
behaviour**, not a bug: the alternative is an endless `ACCESS_LOST` -> re-duplicate loop that
delivers nothing while the client sits on a negotiated session showing black. What is a bug is
either an unbounded run of those lines (the cap is not working) or a line printed for every single
re-establishment (the rate limit is not working — and every log line is an IPC event to the UI).

A count that climbs a little and then resets to nothing as frames start flowing is the normal
settling of a virtual display; ignore it.

### The staging ring

The DXGI path no longer encodes on the capture thread. Under load, the per-60-frame line now
carries the frame budget:

```
encode-path latency: path=dxgi+<backend>, avg_ms=..., max_ms=..., frames=..., budget_ms=...
```

`avg_ms` above `budget_ms` means the encoder cannot keep up at the requested rate; pair it with
`frames_dropped_ring` from the telemetry line in step 3, which is where the DXGI ring's drops are
counted too.

---

## 6. Cursor correctness on both backends (5 min)

Each backend composites the cursor differently, and the DXGI path does it incrementally, so it
needs looking at on both.

WGC first — with the live session from step 1 still up, move the mouse across the extended display
slowly, then fast, then along each of the four edges, then in and out of the corners.

Assert, on the **client's** screen:

- The cursor is visible at all.
- It leaves **no trail** — no stale cursor image left behind at a previous position.
- There is exactly **one** cursor, not a duplicate a few pixels off.
- It does not stick, tear or vanish when it crosses an edge or leaves the display entirely.

Then the DXGI side. `serve` has no backend flag, so record a live capture through the probe and
watch it back:

```powershell
cd src-tauri
cargo run --release --features probe -- probe --live --capture-backend dxgi --duration 60 dxgi_cursor.h264
ffplay dxgi_cursor.h264
```

Move the cursor the same way during those 60 seconds, then check the recording for the same four
things. Trails and duplicates are the two shapes an incremental composite fails in; if you see
either, say where on the display it happened and how fast you were moving.

---

## 7. Latency, if the machine is worth measuring on (10 min)

Skip this on a VM with a software rasteriser — the numbers mean nothing there.

```powershell
cargo run --release -- serve -v | Tee-Object -FilePath wgc-latency.log
```

Join, leave it up for **five minutes** under ordinary use, then take the last telemetry line:

```powershell
Select-String -Path wgc-latency.log -Pattern 'telemetry: capture=' | Select-Object -Last 1
```

Record p50/p90/p99 for four stages: **`capture_delivery`**, **`encode`**, **`capture_to_encode`**
and **`capture_to_socket`**.

**The interesting comparison is this machine before and after the change, not this machine against
the dev box.** A pre-26100 host was on DXGI before and is on WGC now; the delta between those two
on the same hardware is the whole result. If you have a build from before the change, run it for
five minutes the same way and paste both lines. If you do not, `probe --capture-backend dxgi` from
step 5 against `--capture-backend wgc` is a usable stand-in — shorter runs, same machine.

Watch the p99 and max, not the p50. A p50 that stays flat while the p99 creeps upward over five
minutes is a queueing problem, and that is where these bugs live.

---

## 8. Nothing regressed at 26100 (5 min, only if you also have a 24H2 box)

Run steps 1 and 4 unchanged on a build 26100+ machine.

```powershell
cargo run --release -- serve -v | Tee-Object -FilePath wgc-26100.log
Select-String -Path wgc-26100.log -Pattern 'wgc: feature set|adapters:|live capture ready|falling back to DXGI'
```

Two assertions:

- The feature summary reads **every flag `+`** and ends `(build 26100+)`. A `-` on a 24H2 host
  means the probe now rejects a property that used to be requested successfully.
- The path line is still the ring — `NVENC ZERO-COPY RING (K=3) decoupled path` on an NVIDIA box,
  or whatever step 4's table says for yours. The point is that it is the same path as before the
  change.

The DXGI fallback line must not appear anywhere in the log.

---

## What to report back

1. The `wgc: feature set ...` line, verbatim, per machine.
2. The `adapters: ...` line, verbatim.
3. The `pipeline: live capture ready -- ...` line — which path you got.
4. The OS build from `winver`, and the product name from step 0.
5. The last `telemetry: capture=...` line of the run, whole.
5b. From step 5b: the `dxgi: duplication ready ... via <api> on <adapter>` line (or the
   `no adapter can duplicate ...` failure), and whichever of the two `dxgi: duplication fallback
   ...` lines the compatibility probe printed.
6. Anything you did not expect — especially a `falling back to DXGI Desktop Duplication` line on
   any host, a flag that disagrees with the step 0 table, or a `frames_dropped_stale` count that
   kept climbing after the load stopped.
