# scripts/test-handler — every ScreenExtend measurement goes through here

```
node scripts/test-handler/handler.mjs --exe <path-to-ScreenExtend.exe> [--request <file.json | inline JSON>]
node scripts/test-handler/handler.mjs --status            # who holds which slot / the measurement lock
node scripts/test-handler/handler.mjs --calibrate-only    # the method's own floor, no host
```

It starts that build's CLI host (`serve`), opens a fresh headless Chrome, runs the standard
end-to-end tests plus whatever the request asks for, shuts everything down, and prints **one JSON
document on stdout** (progress goes to stderr). Exit 0 = the harness ran to completion, whatever
the numbers say; 1 = the harness itself failed (`errors[]` says why); 2 = bad arguments; 3 = the
request was refused (`refused` says why). Bulky artefacts go to `runDir`
(`%TEMP%\screenextend-test-handler\runs\<time>-slot<n>-<pid>\`): `serve.log` (the host, verbose),
`handler.log`, `result.json` (the same document), `config.json.before`.

Needs Node 22+ (global `WebSocket`/`fetch`), Google Chrome in its default place (`$env:CHROME`
overrides), Windows PowerShell 5.1, `nvidia-smi` for GPU load (optional). No npm dependencies.

Nothing here injects input. Page actions are DOM method calls (`.click()`, `requestSubmit()`)
through `Runtime.evaluate`; the CDP wrapper refuses every `Input.*` method; the injected page
script only wraps APIs to observe them and never creates or dispatches an event; on-screen motion
comes from windows that never take focus and are click-through (`stage.ps1`). A request asking for
input (keys like `input`/`mouse`/`keyboard`/`pointer`/`touch`/`cursor`, or a `custom.js` that
creates/dispatches events or touches `RemoteInput`, or host commands touching `--control` /
`--pointer-scope` / the input scripts) is refused with exit 3.

## What one run does

1. **Build probe** (`probeBuild`, reads the exe, runs nothing): does it contain the isolation switch
   (`SCREENEXTEND_DATA_DIR`), credential environment variables, control-server telemetry, which
   page generation (`window.SEVideo`, `X-Join-Id`); are `libopus.dll` / `libx264-164.dll` beside it.
   A missing DLL is a warning in the JSON: **copy them from `src-tauri/resources/` of that revision**
   (a plain `cargo build` does not put them there; system audio cannot load without `libopus.dll`).
2. **Slot.** Isolated builds take one of four machine-wide slots; builds without the switch run in
   exclusive mode (below). A caller that finds all four busy waits (`queueWaitMs`).
3. **Host.** `serve --no-cloud --no-qr -v --http-port P --https-port P+1` with the slot's own data
   directory. Credentials go by environment where the build reads them (`credentialsVia`), else on
   argv (the old app has no other way; reported). Waits for `/health` on the LAN address.
4. **Browser.** Fresh profile, `--headless=new --ignore-certificate-errors --mute-audio
   --autoplay-policy=no-user-gesture-required --use-fake-device-for-media-stream`, background
   throttling off. Screen size, viewport and DPR are set with `Emulation.setDeviceMetricsOverride`;
   the refresh rate the page reports is set by defining `screen.refreshRate` (both page generations
   read it first), because headless `requestAnimationFrame` is unthrottled and would report
   hundreds of Hz.
5. **Clock and calibration** (below), then the standard scenarios, then the special requests.
6. **Teardown.** Leave (the `beforeunload` dialog is accepted), close Chrome, stop the host (the
   `stop` verb where it reaches the host, else by PID), check the driver for displays this run
   left, restore `config.json` if it changed, release the slot. `cleanup` in the JSON lists what
   was left (it must be empty).

## Concurrency and isolation

- **Slots**: `%TEMP%\screenextend-test-handler\slot-{1..4}.json`, created with `O_EXCL`, holding
  the owner pid; a file whose pid is dead is stale and is removed by whoever finds it. Slot *n*
  uses HTTP `18000+10n`, HTTPS `18001+10n`, Chrome debugging `19200+10n(+offset)`, profile
  `profile-slot<n>-*`, data dir `data-slot<n>`. None of these are the default 8080/8443, so a
  ScreenExtend tab left open in the owner's browser does not rejoin a test host.
- **Measurement lock** (`measure.lock`): by default held for the whole *live-display* part of a
  test — from the clock check and calibration through the first join, every timed window, every
  special request, to the last leave — plus host start and host stop (anything that adds or
  removes a virtual display). Browser start, slot setup and teardown overlap. Why so wide: with two
  or more virtual displays attached at once, this build's streams were seen to stop carrying new
  frames, show another display, or slow down (see "Findings"), so a window measured while another
  slot's display exists is not trustworthy. With the lock that wide, only one test ever has a
  display. `"overlap_displays": true` narrows the lock to the timed windows only (faster, and
  contaminated). Every window is appended to `measure-log.jsonl` (pid, slot, label, start, end)
  and listed in `measurementWindows`; `acceptance.mjs concurrent` checks them for overlap.
- **The isolation switch** (`SCREENEXTEND_DATA_DIR`, first commit of this branch; `CLAUDE.md`
  documents it): the lock file, control socket file, `config.json` and TLS cache move to the slot's
  directory, and every CLI verb run with the same variable reaches that host. On Windows it also
  puts the host in *shared-driver mode*: the virtual display driver's monitor list is machine-wide,
  it attaches a list only when the ids form one run starting at 0 or 1, and a gap detaches every
  display — so isolated hosts allocate dense ids under a lock in `%TEMP%\screenextend-shared-driver\`
  and remove a departing monitor only when that keeps the run intact (a released id in the middle
  stays attached, idle, until the ones above go; the last host out removes all).
- **Exclusive mode** (a build without the switch, or `"force_exclusive": true`): announces itself
  (`exclusive.json`, which stops new slots being handed out), waits for every slot to be free,
  holds all four, runs on slot 1's ports (the old app takes `--http-port/--https-port` too) with the
  real data directory, backs up `%APPDATA%\app.screenextend.desktop\config.json` and restores it
  byte-for-byte (verified by SHA-256 in `ownerConfig`). A host without a reachable `stop` verb is
  stopped by PID; a PID stop skips the host's own display removal, so the handler removes the
  monitors this run added through the driver's pipe (`lib/driver.mjs`, exclusive mode only) and
  reports it in `cleanup.driverRemoved`.

## The primary metric: clock stamps, measured from outside the build

**Host side.** `stage.ps1` draws a borderless, click-through, never-activated, topmost window at
the top-left of **this test's** virtual display: 38 cells (cell = display width / 40, at most 64
px) holding guard `W B W B`, the host's system clock in milliseconds (24 bits, white = 1), an
8-bit check, end guard `B W`. It is drawn into an off-screen bitmap and blitted in one call about
every millisecond. The display is named by the host's own `status --json` (the join's driver id,
matched against the monitor device path `…UID<256+id>#…`, which Windows does not reassign; a
`\\.\DISPLAYn` name is a slot it does) or, for builds that report nothing, as the one screen that
appeared with the join. The bundled driver gives every monitor the EDID name `VirtuDisplay+`, so
names cannot be used.

**Client side** (`lib/paint-hook.mjs`). Every worker the page starts gets a hook through the
debugger's auto-attach, and so does the page. It wraps `drawImage` on 2D canvas contexts: when the
page paints a decoded `VideoFrame` — both page generations paint that way in their transform
worker — the hook takes the time of the paint call, clones the frame, crops the stamp strip with
`createImageBitmap` and decodes it asynchronously, so the paint is not delayed. On the `<video>`
path the page samples the element on `requestVideoFrameCallback` (`expectedDisplayTime`). **latency
= paint time − stamp − clock offset**: host draw → DWM compose → capture → encode → send →
receive → decode → paint call, for any build, without the page's own counters.

Reported per window: `latencyMs` {n, min, p50, p90, p99, max, mean}, `newStamps`, `paintedFps`
(distinct stamps per second), `duplicateFrames`, `skippedStamps` (stamp increments over 1.5×
the median), `longestGapMs` (freeze), `gapsOver100ms`, `decodeErrors` by reason (`guard`: no
stamp in the frame; `checksum`: a frame caught between two stamps or blurred by the encoder;
`implausible`: decoded but older than 5 s or in the future, dropped), and `stage.via` (how the
display was found).

- **Clock offset** (`clock.browserMinusSystemMs`): browser `performance.timeOrigin + now()` minus
  the system clock, from the minimum-RTT of 25 `Runtime.evaluate` round trips; subtracted from
  every latency. 0.3–0.5 ms on this machine.
- **Calibration floor** (`calibration`): a local page with no stream draws the same stamp from its
  own `Date.now()` into a canvas, wraps it in a `VideoFrame`, paints it with `drawImage` every 16 ms,
  and the same hook reads it. Measured on this machine: **p50 0.6–0.8 ms, p99 1.2–1.4 ms** (readback
  error plus the 1 ms stamp quantum). That is the method's floor.
- **What the number includes that the page's own `hostToPaint` does not**: the time from the
  stage's draw to the compositor picking it up and capture starting. On this machine the stamp
  p50 sits about 10–12 ms above the page's `hostToPaint` p50 for the same window (e.g. 33 vs 22 ms).
  It is the same for every build, so comparisons hold; absolute numbers are "host draw → client
  paint call".
- **What it excludes**: the client's compositor after the paint call (about one frame), as the
  page's own counter does too. A screencast-based reader that includes it is kept
  (`collectStampsScreencast`) but is not the default: on this machine the screencast ran at 4–36
  frames/s against a 60–70 fps stream and itself slowed the page (its `hostToPaint` rose from ~22
  to ~48 ms), so it measured the probe, not the build.

## Standard scenarios (unless `scenarios` narrows them)

| name | client | content | measures |
|---|---|---|---|
| `static` | `client` (default 1920×1080 CSS, DPR 1, 60 Hz) | nothing drawn; the bare desktop, `static_duration` (30 s) | pixel stability: every frame compared on a 64×36 grid; `framesWithChange`, `greenFrames` (samples newly green-dominant: the UX-GREEN artefact), page/WebRTC frame rates (idle cadence) |
| `small_window` | `client` | stamp + a 48 px window sliding across the display | the stamp metric, `runs` times |
| `large_area` | `client` | stamp + 80 % of the display repainted with shifted noise every 16 ms | the stamp metric under sustained full motion |
| `hires_small_window` | `hires_client` (default 1280×720 CSS at DPR 2 = a 2560×1440 screen, 120 Hz) | stamp + moving window | the high-resolution, high-refresh client |
| `hires_large_area` | `hires_client` | stamp + noise | (not in the default list) |

Each timed window: `warmup` s of motion, then `duration` s measured, with machine load sampled
every second (`load`: whole-machine CPU %, GPU/encoder/decoder % and clock from `nvidia-smi`),
plus whatever the build reports about itself before and after (`secondaryBefore/After`,
summarised in `pageCounters`: `SEVideo.getStats()`, `SEAudio.getSyncInfo()`, WebRTC inbound
video/audio, data channels, candidate pair). `runs: N` repeats the window N times on the same
join; `aggregate` gives median/min/max/range of p50, p90, p99, max, painted fps and longest gap.
Every join reports, in `joins[]`: `answerMs` (submit → the `/whep` POST's response, from the
browser's network log), `firstPictureMs` (submit → the first painted frame carrying a stamp: a stage
is started at the submit, waits for the join's display to attach and draws the moment it does;
`firstStampAgeMs` says how old that first stamp was), `offers` (how many POSTs the page sent; more
than one means it gave up on an answer and retried), `whepRequests`, the modals it dismissed and
the page's error text.
`pageConsole` lists the page's warnings and errors.

## Request keys

| key | default | meaning |
|---|---|---|
| `scenarios` | the four above | subset/order of scenario names |
| `duration`, `static_duration`, `warmup`, `runs` | 20, 30, 3, 1 | seconds; `runs` repeats each timed window |
| `client`, `hires_client` | see above | `{width, height, dpr, refresh, https}` (CSS px) |
| `host_env`, `host_args` | `{}`, `[]` | extra environment / `serve` arguments for the host |
| `chrome_flags`, `headed` | `[]`, false | extra Chrome flags; a visible window |
| `calibrate` | true | run the floor calibration first |
| `force_exclusive` | false | exclusive mode even for an isolated build |
| `audio` | false | enable system audio for the device (`devices set <token|ip> --audio on`), play a click train (`clicks.ps1`: 4 ms 2 kHz bursts every 700 ms, timed by winmm's played position) and detect each onset at the client through an analyser spliced in before the destination. Reports `delayToRenderMs` / `delayToOutputMs` (adds the browser's output latency via `getOutputTimestamp`), `matched` / `unmatchedOnsets` (other sounds on the machine are captured too), `clickPeak`, `silenceBefore/After`, `path`, `transport`, `syncInfo`, data channels or the Opus track, and console lines that explain a fallback. **The absolute delay includes winmm's own buffering ahead of the audio engine** (the host time is when winmm reports the sample consumed, which is earlier than the engine mixes it), so it reads high (≈140 ms to render on this machine); it is the same for every build, so compare, do not quote. Builds that apply the override only to a new connection (the old app says so) are left and rejoined first |
| `mic_prompt` | false | two fresh browsers: speaker access granted without the microphone, and nothing granted. Reports whether `getUserMedia` asked for audio, the Permissions-API queries, whether the page's microphone card appeared (it is dismissed with *Skip*), output labels from `enumerateDevices`, `selectAudioOutput`/`setSinkId` support, and `defect` when a prompt or card appears although outputs were already labelled |
| `settings_change` | false | after the common join, each setting class alone (audio on/off, quality, video scale, refresh, DPR via `--native-dpr off`, orientation both ways, display scale, name via `config set devices.<i>.name`), each with a fresh stamp window: live joins, join id, displays and display id before/after, device connected/disconnected lines in the host log, new peer connections / `/whep` requests on the page, largest stamp gap after the edit and `pictureRecoveredMs`; `defectSeen` = more than one join or display after any edit |
| `two_clients` | false | a second browser joins the same host; three settings are changed on the first; the second's stamps before vs during (`bUntouched`); then the first client's stamps now that two displays exist (`aAfterBJoined`) |
| `leave_rejoin` | false | tab leaves and rejoins within 2 s (same display id?); then leaves for good and the display count is watched for 30 s (`removedAfterS` vs the configured grace) |
| `slow_answer` | false (`{n:5}` when true) | repeats the join n times, flags answers over 5 s, a page that gave up, or a second session/display |
| `auto_rejoin` | false | stops and restarts the host under a live page; timeline of host joins and page state |
| `logs` | false | `logs --lines 50` and `status --json` with a live session |
| `fallback_paths` | false | one fresh browser per client tier — default, no WebCodecs, no `RTCRtpScriptTransform`, neither script transform nor encoded streams, plain HTTP (no secure context / isolation), hardware decode off — each with a 10 s stamp window and the path the page ended on |
| `loss` | false | reported as **not run**: inducing loss needs admin rights or the owner's network |
| `screencast_width` | 480 | width of the (non-default) screencast reader |
| `overlap_displays` | false | narrow the measurement lock to the timed windows (see Concurrency) |
| `load_gate` | `{max_cpu: 75, max_wait_s: 180}` | before every timed window (and the static window) wait, bounded, for whole-machine CPU averaged over 2 s to fall below `max_cpu`. Each window then carries `loaded` (true when the gate gave up or the window's own mean CPU ended above `max_cpu`; **discard and repeat such a window**), `loadGate` (`waitedMs`, `cpuAtOpen`, `loaded`) and `windowCpuMean`; `aggregate.loadedRuns` counts them. `false` turns the gate off. Why: runs at 85–100 % CPU came out 10–25 ms slower than quiet ones |
| `netlog` | false | each browser writes Chrome's net log to `runDir/netlog-<offset>-<n>.json` |
| `stamp_band` | `'full'` | `'center'` reads a 2-row band through the stamp cells instead of averaging their height: immune to the stage's blit tearing, which is the ~10 % checksum failures at 1080p. Not yet the default (noise figure not re-measured) |
| `mover_stop` | false | `true` or `{cycles, move_s, hold_s, step_ms, speed}`: a 48 px window moves in pointer-like steps and stops; per stop, where the client's picture comes to rest vs the true position (`staleStops`), and how fast it got there |
| `cpu_burn` | null | `{threads, seconds}`: busy workers at normal priority during `slow_answer`'s joins (inside the lock), to reproduce the lost answer under load |
| `custom` | null | `{ js: "<expression evaluated in the client page after the common join>", host_commands: [["status","--json"], ...], timeout: 20 }`; results in `special.custom` |

Adding a request: write an `async function` in `handler.mjs` that returns a plain object, call it
from `main()` behind its key, add the key to `DEFAULTS`. Use `underLock()` for anything timed or
anything that creates/removes a display, `startStage()` for on-screen content, `cl.collectStamps()`
for the metric, `hostJoins()` / `ourMonitors()` / `serveLogCounts()` for host state.

## Old builds (exclusive mode)

Run against `bef5f89` (v0.6.3, `C:\Users\sarve\.cargo\shared_target\ScreenExtend-old\release\ScreenExtend.exe`)
and against the pre-switch head `d4d8be9` (`…\shared_target\ScreenExtend\release\ScreenExtend.exe`).
What `bef5f89` lacks and what the handler does instead (all reported in the JSON):

| missing at `bef5f89` | the handler instead |
|---|---|
| `SCREENEXTEND_DATA_DIR` | exclusive mode, real data dir, `config.json` backed up and restored (byte-identical, checked) |
| `SCREENEXTEND_SESSION_ID` / `SCREENEXTEND_OTP` | credentials on argv (`--session-id`, `--otp`); `host.credentialsVia` says so |
| control server under `serve` (`stop`, `status --json` telemetry, `logs`) | PID stop (`cleanup.hosts[].method`), then a check of the driver for leftover monitors (none were left: the page's leave had already removed the display); `special.logs.pass` is false (the `logs` verb reads the calling process); device addressed by IP |
| `window.SEVideo`, join ids, trickle ICE | nothing needed: the paint hook, the network log and the screen diff are build-independent; `pageCounters` is empty |
| applying a device override to a live session | the audio test leaves and rejoins after `devices set <ip> --audio on` |

Seen on `bef5f89` through the handler: its `serve` reads device overrides only at startup, so the
audio test restarts that host (PID stop, same credentials and ports) and rejoins; the host then
sends audio, but the page cannot play it (`[audio] decoder worker error`, `AudioWorkletNode cannot
be created`: the worker scripts carry no COEP header, fixed at `d4d8be9`). DLLs: the
build directory had none until the orchestrator copied `libopus.dll` / `libx264-164.dll` beside the
exe; the handler warns when they are missing.

## Acceptance (TEST-WORKFLOW.md §5.7)

All on 2026-10-05, Windows 11 26200, RTX 3080 Ti Laptop, Chrome 154 headless. **The machine was
busy throughout** (other agents compiling): whole-machine CPU 82–90 % during every timed window
(`load.cpuPct` in each run). Treat the absolute numbers as this machine under that load. Builds:
`ScreenExtend-handler` = `6004f60` + the isolation commit (the current head plus the switch); the
pre-switch head `d4d8be9`; the old app `bef5f89`. `acceptance.mjs` produced every figure below and
left its summary in `%TEMP%\screenextend-test-handler\acceptance-*.json`.

**1. Four simultaneous invocations, a fifth queues** (`acceptance.mjs concurrent --n 5`, request
`{"scenarios":["static","small_window"],"duration":10,"static_duration":10}`, started 1.5 s apart):
slots 1–4 taken at once (queue wait 1–3 ms), the fifth waited **169 s** for a slot; all five exited
0 with `ok: true`; 15 measurement windows across the five runs, **0 overlaps**; every run's
`cleanup` empty (no display, host or Chrome left), the owner's `config.json` identical after each.
Small-window p50 per run: 34.33, 34.42, 33.40, 33.56, 34.05 ms; painted fps 53.1–55.3.

Two earlier attempts with the lock covering only the timed windows completed and serialised too,
but their numbers were not valid: with 2–4 virtual displays attached, slots measured no stamps at
all, frozen frames, or p50 ≈ 100 ms. That is why the lock now covers the whole live-display part
(see "Findings", and "Concurrency" above).

**2. Exclusive mode against builds without the switch** — pre-switch head `d4d8be9`
(`small_window` + `audio` + `two_clients`) and old app `bef5f89` (`static` + `small_window` +
`audio` + `logs`, then `audio` again with the restart): all ran to completion; `config.json`
SHA-256 `7245d85df57bfe17…` before and after each (the audio step writes it; it was restored
byte-for-byte and verified); `d4d8be9` stopped by its `stop` verb, `bef5f89` by PID (the driver
showed no leftover display after either). Credentials went by environment to `d4d8be9` and on
argv to `bef5f89` (it has no other way).

**3. Five consecutive standard runs on the current head** (`acceptance.mjs noise --runs 5`, the
default request: calibration, static 30 s, small window / large area / hi-res small window 20 s
each). **This spread is the noise figure.** Latency is host draw → client paint call (see "The
primary metric").

| scenario | metric | median | min | max | range |
|---|---|---|---|---|---|
| small window (1920×1080, DPR 1, 60 Hz) | p50 ms | 33.34 | 33.24 | 34.72 | **1.48** |
| | p90 ms | 44.86 | 41.38 | 48.99 | 7.61 |
| | p99 ms | 63.45 | 56.69 | 105.58 | 48.89 |
| | max ms | 87.84 | 75.54 | 162.57 | 87.03 |
| | painted fps (distinct stamps) | 53.6 | 52.5 | 54.4 | 1.9 |
| | longest gap ms | 76 | 70 | 88 | 18 |
| large area (same client) | p50 ms | 37.07 | 35.98 | 38.29 | **2.31** |
| | p90 ms | 50.20 | 46.55 | 50.51 | 3.96 |
| | p99 ms | 69.69 | 58.22 | 78.08 | 19.86 |
| | max ms | 99.32 | 66.75 | 288.48 | 221.73 |
| | painted fps | 50.5 | 50.3 | 51.6 | 1.3 |
| | longest gap ms | 76 | 66 | 114 | 48 |
| hi-res small window (2560×1440 screen, DPR 2, 120 Hz) | p50 ms | 50.10 | 47.41 | 55.31 | **7.90** |
| | p90 ms | 76.25 | 68.65 | 115.86 | 47.21 |
| | p99 ms | 114.98 | 93.41 | 178.41 | 85.00 |
| | max ms | 138.13 | 101.23 | 194.64 | 93.41 |
| | painted fps | 56.1 | 55.8 | 56.6 | 0.8 |
| | longest gap ms | 64 | 58 | 72 | 14 |
| join (10 joins) | submit → answer ms | 3454 | 3165 | 6085 | 2920 |
| | submit → first painted frame ms | 4377 | 3640 | 22645 | 19005 |

- **Calibration floor** across the five runs: p50 0.5–0.7 ms, p99 1.1–1.2 ms, max ≤ 1.6 ms.
  Clock offset (browser − system) 0.31–1.06 ms, subtracted.
- **The page's own counters in the same windows** (`SEVideo.getStats()`): `hostToPaint` p50
  22.1–24.7 ms (small window), 22.5–25.0 (large area), 28.6–32.9 (hi-res); painted 61–67 fps;
  `videoPath` 1 in every window.
- Join answer times are bimodal: the first join of a host (display created) 5.5–6.1 s, a later one
  3.2–3.5 s. Three of the five first joins showed the first stamped frame 6.6–16.7 s after the
  answer; the join-stage was restarted in each (`restagedFor`). Not established whether the delay
  was the stream's or the stage's (the display may still have been changing mode); the stage now
  follows mode changes and the join records `hookBeforeFirstPicture`, but the noise runs predate
  both. Treat `firstPictureMs` outliers with care until re-measured.
- Static desktop, 30 s: 2 of 5 runs had 36 changed frames with 6 frames carrying a single newly
  green-dominant grid sample; 3 of 5 had none. Not explained.
- p99/max ranges are wide (tens of ms): on this loaded machine, compare p50 (range 1.5–2.3 ms on the
  1080p scenarios, 7.9 ms hi-res) and treat p99 differences under ~50 ms as noise.

**4. Nothing left running.** After every run above: `cleanup.displaysLeftByUs`, `hostProcessesLeft`
and `chromeLeft` empty; the driver reported `[]`; no `ScreenExtend.exe` in the process list.

## Special requests: what was run here

All implemented requests were exercised at least once against the current head (and `audio` against
all three builds). Highlights:

| request | result on the current head |
|---|---|
| `audio` | fast path (`webcodecs`/`shared`); 14 of 15 clicks matched, delay to render p50 142 ms, to output p50 192 ms (includes winmm buffering, see the table above); silence 0 before and after |
| `mic_prompt` | speaker access granted: no microphone request, no card, outputs labelled (Chrome's fake devices); nothing granted: the card appears, outputs unlabelled; `selectAudioOutput` absent, `setSinkId` present; no defect flagged headless |
| `settings_change` | every class: 1 join and 1 display before and after, same join id and display id, no device removed or added; stream-class edits renegotiate in place (1 new peer connection, 2 `/whep` requests) with the picture back in 1.0–2.5 s; audio/scale/name edits 0 gap |
| `two_clients` | varied run to run: once `bUntouched` true (B's longest gap 76 ms before and during A's edits); once B got no picture; on `d4d8be9` (exclusive) B's gap rose to 635 ms and A's p50 to 221 ms after B joined |
| `leave_rejoin` | rejoin within 2 s reused the same display id; after a final leave the display was gone within 1 s |
| `slow_answer` (n=3) | 0 flagged in that run; but see Findings |
| `auto_rejoin` | after a host restart the page rejoined by itself within 12 s (page showed "The host stopped sharing this screen" meanwhile) |
| `logs` | `logs --lines 50` returned 50 lines and `status --json` telemetry while live (`bef5f89`: not available) |
| `fallback_paths` | default p50 33.3 ms (path 1); no WebCodecs / no script transform / neither: path 3 (`<video>`), p50 36.1–36.9 ms; plain HTTP and hardware-decode-off hit the slow-answer defect in that run |
| `custom` | page expression and host commands returned |
| `loss` | **not run** (needs admin rights or the owner's network) |

## Findings about the app (not the handler), for the baseline step

- **The answer that never arrives.** In roughly one join in five on this machine, the host logs
  `join accepted: WHEP answer generated` within 1–4 s, yet the browser's network log shows no
  response to that `POST /whep` before the page's 10 s timeout ("The host did not answer in
  time"); the page then retries, the host opens a **second session and a second display**, and
  the client is demoted to `<video>`. Seen on the current head in single-client runs and in
  `two_clients`. Unexplained. `joins[].offers > 1` and `retried` flag it in every result.
- **Two or more virtual displays make streams unreliable.** With 2–4 displays attached (two
  clients on one host, or several isolated hosts), measured streams stopped showing new stamps,
  showed frozen frames, or ran at ~100–220 ms p50. Seen with the shared-driver mode and, in the
  two-client case, on the unmodified `d4d8be9` too. Unexplained; the handler avoids it by default.
- **`bef5f89` audio**: after the override is applied (host restart + rejoin) the host sends 4,172
  audio packets, but the page cannot play them: `[audio] decoder worker error` and `AudioWorkletNode
  cannot be created` (its worker scripts lack the COEP header a cross-origin-isolated page needs,
  fixed at `d4d8be9`).
- The stamp's painted fps (≈53) is below the page's own count (61–67) because about 10 % of
  painted frames fail the stamp check (frames caught between two stamps or blurred by the encoder).
  It is reported, and it is the same method for every build.

## Files

| file | role |
|---|---|
| `handler.mjs` | the entry point: request, slots, host, browser, scenarios, special requests, cleanup, the JSON |
| `acceptance.mjs` | `concurrent` / `noise` / `exclusive` runs of the handler, with the overlap check and the spread |
| `lib/slots.mjs` | slot files, exclusive mode, the measurement lock, stale-owner recovery |
| `lib/host.mjs` | build probe, `serve` start, CLI calls, stop (verb or PID) |
| `lib/cdp.mjs` | Chrome launch and the debugging-protocol client (refuses `Input.*`) |
| `lib/client.mjs` | one browser: observer + paint hook injection, join timing, stamp and static collection, `stampMetrics` |
| `lib/paint-hook.mjs` | the `drawImage` / `requestVideoFrameCallback` readback that produces the primary metric |
| `lib/inject.mjs` | the observer page script (microphone, peer connections, audio analyser and onset detector), geometry and secondary-stats expressions |
| `lib/driver.mjs` | read the virtual display driver's monitor list over its pipe; remove monitors (exclusive mode only) |
| `lib/load.mjs` | CPU and `nvidia-smi` sampling |
| `lib/metrics.mjs` | percentiles and spreads |
| `lib/png.mjs`, `lib/stamp.mjs` | PNG decoding and stamp decoding for the screencast reader (not the default) |
| `stage.ps1` | the stamp, moving window and noise area on one display (never focused, click-through) |
| `clicks.ps1` | the timed click train for the audio test |
