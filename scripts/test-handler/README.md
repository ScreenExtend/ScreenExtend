# scripts/test-handler

Test handler for ScreenExtend: it starts one build's `serve` host, joins it from a headless Chrome and prints the measurements as one JSON document. Use it for every end-to-end measurement of a build, so results from different builds come from one method.

## Requirements

- Windows with Windows PowerShell 5.1, which runs the `.ps1` helpers.
- Node 22 or newer, for the global `WebSocket` and `fetch`. There are no npm dependencies.
- Google Chrome in its default place, or set `CHROME`. Requests that name Edge read `EDGE` the same way.
- `nvidia-smi` for the GPU load figures (optional).
- A host binary that reads ScreenExtend's environment variables: a debug build, or one built with the `bench` feature. A shipped build ignores `SCREENEXTEND_DATA_DIR`, the credential variables and every `SE_*` switch, so it cannot be isolated or steered from here.
- `libopus.dll` and `libx264-164.dll` beside the exe. A plain `cargo build` does not put them there: copy them from `src-tauri/resources/` of the same revision. Without `libopus.dll` system audio cannot load.

## Usage

```text
node scripts/test-handler/handler.mjs --exe <path-to-ScreenExtend.exe> [--request <file.json | inline JSON>]
node scripts/test-handler/handler.mjs --status            # who holds which slot / the measurement lock
node scripts/test-handler/handler.mjs --calibrate-only    # the method's own floor, no host
```

| flag | effect |
|---|---|
| `--exe <path>` | the build to test |
| `--request <file or JSON>` | the request: a JSON file, or inline JSON that starts with `{`. See "Request keys" |
| `--status` | prints which handler holds each slot, a waiting exclusive run and the holder of the measurement lock |
| `--calibrate-only` | measures the floor of the method with one browser and no host |
| `--reap` | removes what a handler that was killed outright left behind. See "Reaping and the parent watch" |
| `--gate-check <n>` | calls the display gate n times (default 2) and changes no display. See "Display gate" |
| `--parent-pid <pid>` | when that process exits, the handler cleans up and exits 1 |
| `--help`, `-h`, or no `--exe` | prints the first 40 lines of this file |

| exit code | meaning |
|---|---|
| 0 | the run completed, whatever the numbers say |
| 1 | the handler itself failed (`errors[]` says why), or `--exe` is missing |
| 2 | the `--exe` path does not exist |
| 3 | the request was refused (`refused` says why) |
| 130 | a signal interrupted the run |

A run starts the build's CLI host (`serve`) and opens a fresh headless Chrome. It runs the standard end-to-end scenarios plus whatever the request asks for, then shuts everything down. It prints one JSON document on stdout, and progress goes to stderr.

### Output files

Bulky artefacts go to `runDir`, which is `%TEMP%\screenextend-test-handler\runs\<time>-slot<n>-<pid>\` unless `SE_TEST_RUN_ROOT` moves it.

| file | content |
|---|---|
| `serve.log` | the host's verbose output. Each line starts with `[epoch-ms]`, the time the line was read |
| `handler.log` | the handler's progress lines |
| `result.json` | the same document as stdout |
| `config.json.before` | a copy of the real `config.json`, taken before the host starts |

Some requests add files of their own. Their sections name them.

### Example request

```json
{ "scenarios": ["static", "small_window"], "duration": 10, "static_duration": 10 }
```

Save it as a file and pass the path to `--request`, or pass the JSON inline.

### No input injection

The handler injects no input unless the request sets `allow_real_input`.

- Page actions are DOM method calls (`.click()`, `requestSubmit()`) through `Runtime.evaluate`.
- The CDP wrapper in `lib/cdp.mjs` refuses every `Input.*` method.
- The injected page script wraps APIs to observe them. It never creates or dispatches an event.
- On-screen motion comes from windows that never take focus and are click-through (`stage.ps1`).

The handler refuses a request, with exit 3, in these cases:

- A key name contains `input`, `mouse`, `keyboard`, `pointer`, `touch`, `cursor`, `keypress`, `clipboard` or `wheel`.
- The text of `custom.js`, `join_probe` or `url_params` creates or dispatches events, or touches `RemoteInput`.
- A `custom.host_commands` entry contains `--control`, `pointer-scope`, `measure-input`, `measure-stream` or `test-startup-gate`.
- A `chrome_flags` entry overrides the slot's debugging port or profile directory.

## What one run does

1. Build probe. The `probeBuild` function reads the exe and runs nothing. It reports whether the binary contains the isolation switch (`SCREENEXTEND_DATA_DIR`), the credential environment variables and control-server telemetry. It also reports the page generation (`window.SEVideo`, `X-Join-Id`) and whether the two DLLs are beside the exe. A missing DLL is a warning in the JSON.
2. Slot. An isolated build takes one of four machine-wide slots. A build without the switch runs in exclusive mode (see "Concurrency and isolation"). A caller that finds all four slots busy waits, and the wait is reported as `queueWaitMs`.
3. Host. The handler first waits for free system commit (`min_commit_gb`), takes the display lock if one is configured, and reaps what a killed handler left. Then it runs `serve --no-cloud --no-qr -v --http-port P --https-port P+1` with the slot's own data directory. Credentials go by environment where the build reads them, else on argv, and `host.credentialsVia` says which. The handler polls `/health` on the LAN address every 25 ms. The `host.startup` object gives `firstOutputMs`, `runningLineMs` ("Host is running") and `healthMs`, all counted from the spawn call.
4. Browser. Chrome starts on a fresh profile with `--headless=new --ignore-certificate-errors --mute-audio --autoplay-policy=no-user-gesture-required --use-fake-device-for-media-stream` and background throttling off. `Emulation.setDeviceMetricsOverride` sets the screen size, viewport and DPR. The handler defines `screen.refreshRate`, which both page generations read first, to set the refresh rate the page reports. Headless `requestAnimationFrame` is unthrottled and would report hundreds of Hz.
5. Clock and calibration (see "The primary metric"), then the standard scenarios, then the special requests.
6. Teardown. The page leaves (the `beforeunload` dialog is accepted) and Chrome closes. The host stops through the `stop` verb where that reaches it, else by PID. The handler then checks the driver for displays this run left, restores `config.json` if it changed and releases the slot. The `cleanup` object in the JSON lists what was left. It must be empty.

## Concurrency and isolation

### Slots

The slot files are `%TEMP%\screenextend-test-handler\slot-{1..4}.json`. Each is created with `O_EXCL` and holds the pid of the handler that owns it. A file whose pid is dead is stale, and whoever finds it removes it.

Slot *n* uses these resources:

| resource | value |
|---|---|
| HTTP port | `18000 + 10n` |
| HTTPS port | `18001 + 10n` |
| Chrome debugging port | `19200 + 10n`, plus an offset per browser |
| browser profile | `profile-slot<n>-*` |
| data directory | `data-slot<n>` |

None of the ports is the default 8080 or 8443. A ScreenExtend tab left open in a browser on the machine therefore does not rejoin a test host.

### Measurement lock

By default `measure.lock` is held for the whole live-display part of a test. That part runs from the clock check and calibration, through the first join, every timed window and every special request, to the last leave. Host start and host stop take the lock too, because each can add or remove a virtual display.

Browser start, slot setup and teardown overlap with other runs.

The lock is that wide because of what two or more attached virtual displays did to this build's streams. They stopped carrying new frames, showed another display, or slowed down (see "Observations about the app"). A window measured while another slot's display exists cannot be trusted.

With the wide lock, only one test has a display at a time. Setting `"overlap_displays": true` narrows the lock to the timed windows only. That is faster, and the numbers are contaminated.

Every window is appended to `measure-log.jsonl` (pid, slot, label, start, end) and listed in `measurementWindows`. The `acceptance.mjs concurrent` mode checks them for overlap.

### The isolation switch

`SCREENEXTEND_DATA_DIR` is implemented in `src-tauri/src/data_dir.rs`. It moves the lock file, the control socket file, `config.json` and the TLS cache to the slot's directory. Every CLI verb run with the same variable reaches that host.

On Windows the switch also puts the host in shared-driver mode. The reasons and the rules:

- The virtual display driver's monitor list is machine-wide.
- The driver attaches a list only when the ids form one run starting at 0 or 1. A gap detaches every display.
- Isolated hosts therefore allocate dense ids under a lock in `%TEMP%\screenextend-shared-driver\`.
- A host removes a departing monitor only when that keeps the run intact. A released id in the middle stays attached and idle until the ids above it go. The last host out removes all.

### Exclusive mode

Exclusive mode is for a build without the switch, or for a request with `"force_exclusive": true`. The handler does this:

1. It writes `exclusive.json`, which stops new slots being handed out.
2. It waits for every slot to be free and holds all four.
3. It runs on slot 1's ports with the real data directory. Version 0.6.3 takes `--http-port` and `--https-port` too.
4. It backs up `%APPDATA%\app.screenextend.desktop\config.json` and, if the run changed it, restores it byte for byte. The `ownerConfig` object holds the SHA-256 before and after. This check runs in every mode.

A host without a reachable `stop` verb is stopped by PID, and a PID stop skips the host's own display removal. In exclusive mode the handler then removes the monitors this run added, through the driver's pipe (`lib/driver.mjs`). It reports them in `cleanup.driverRemoved`.

## The primary metric: clock stamps, measured from outside the build

Numbers marked "on this machine" come from the machine described under "Acceptance runs".

### Host side

The `stage.ps1` script draws a window at the top left of this test's virtual display. The window is borderless, click-through, never activated and topmost. It holds 38 cells, and a cell is the display width / 40, at least 8 px and at most 64 px.

| cells | content |
|---|---|
| 4 | guard `W B W B` |
| 24 | the host's system clock in milliseconds, most significant bit first, white = 1 |
| 8 | check bits: the three data bytes summed, XOR `0x5A` |
| 2 | end guard `B W` |

The stamp is drawn into an off-screen bitmap and blitted in one call about every millisecond.

The handler finds the display through the host's own `status --json`. It takes the join's driver id and matches it against the monitor device path `…UID<256+id>#…`. Windows does not reassign that path, while a `\\.\DISPLAYn` name is a slot that Windows does reassign.

For a build that reports nothing, the display is the one screen that appeared with the join. The bundled driver gives every monitor the EDID name `VirtuDisplay+`, so names cannot be used.

### Client side

The hook is in `lib/paint-hook.mjs`. The debugger's auto-attach installs it in every worker the page starts, and the page gets it too. It wraps `drawImage` on 2D canvas contexts, which is how both page generations paint a decoded `VideoFrame` in their transform worker.

When the page paints a frame, the hook takes the time of the paint call and clones the frame. It crops the stamp strip with `createImageBitmap` and decodes it asynchronously, so the paint is not delayed. On the `<video>` path the page samples the element on `requestVideoFrameCallback` (`expectedDisplayTime`).

The metric is `latency = paint time - stamp - clock offset`. It covers host draw → DWM compose → capture → encode → send → receive → decode → paint call. It works for any build, without the page's own counters.

### Reported per window

| field | meaning |
|---|---|
| `latencyMs` | `{n, min, p50, p90, p99, max, mean}` |
| `newStamps`, `paintedFps` | distinct stamps, and distinct stamps per second |
| `duplicateFrames` | painted frames that repeat the previous stamp |
| `skippedStamps` | stamp increments over 1.5 times the median |
| `longestGapMs`, `gapsOver100ms` | the longest time between two new stamps (a freeze), and the number of gaps over 100 ms |
| `decodeErrors` | counts by reason. `guard`: no stamp in the frame. `checksum`: a frame caught between two stamps, or blurred by the encoder. `implausible`: decoded, but older than 5 s or in the future, and dropped |
| `stage.via` | how the display was found |

### Clocks and the floor of the method

- One reference clock (`lib/clock.mjs`). Every time is compared on the precise system clock (`GetSystemTimePreciseAsFileTime`). Node's QPC is anchored to it once, and the stage and click scripts draw and time with it. Every Chrome context (each page document, each worker) gets its own min-RTT offset, and `Client.drainStamps` moves the samples onto the reference clock. The coarse clocks this replaced are .NET `UtcNow` and the `timeOrigin` of node and Chrome. At Windows' default 15.6 ms timer resolution they disagreed by up to about 16 ms per process, which biased the metric per run.
- Clock offset (`clock.browserMinusSystemMs`). This is browser `performance.timeOrigin + now()` minus the system clock, from the minimum-RTT sample of 25 `Runtime.evaluate` round trips. It is subtracted from every latency. On this machine it is 0.3-0.5 ms.
- Calibration floor (`calibration`). A local page with no stream draws the same stamp from its own clock into a canvas, wraps it in a `VideoFrame` and paints it with `drawImage` every 16 ms. The same hook reads it. Measured on this machine: **p50 0.6-0.8 ms, p99 1.2-1.4 ms**, which is readback error plus the 1 ms stamp quantum. That is the floor of the method.
- What the number includes that the page's own `hostToPaint` does not: the time from the stage's draw to the compositor picking it up and capture starting. On this machine the stamp p50 is about 10-12 ms above the page's `hostToPaint` p50 for the same window (for example 33 against 22 ms). The difference is the same for every build, so comparisons hold. Absolute numbers are "host draw → client paint call".
- What it excludes: the client's compositor after the paint call (about one frame). The page's own counter excludes that too. A screencast-based reader that includes it exists (`collectStampsScreencast`) and is not the default. On this machine the screencast ran at 4-36 frames/s against a 60-70 fps stream and slowed the page: its `hostToPaint` rose from about 22 to about 48 ms. It measured the probe, not the build.

## Standard scenarios

These run unless `scenarios` narrows them.

| name | client | content | measures |
|---|---|---|---|
| `static` | `client` (default 1920×1080 CSS px, DPR 1, 60 Hz) | nothing drawn: the bare desktop for `static_duration` (30 s) | pixel stability. Every painted frame is compared on a 64×36 grid: `framesWithChange`, `greenFrames` (frames in which a sample newly turned green-dominant, as a green decode artefact does), and the page and WebRTC frame rates (idle cadence) |
| `small_window` | `client` | stamp plus a 48 px window sliding across the display | the stamp metric, `runs` times |
| `large_area` | `client` | stamp plus about 80 % of the display repainted with shifted noise every 16 ms | the stamp metric under sustained full motion |
| `stamp_only` | `client` | the stamp alone | the stamp metric with nothing else moving (not in the default list) |
| `hires_small_window` | `hires_client` (default 1280×720 CSS px at DPR 2 = a 2560×1440 screen, 120 Hz) | stamp plus moving window | the high-resolution, high-refresh client |
| `hires_large_area` | `hires_client` | stamp plus noise | the same client under full motion (not in the default list) |

Each timed window is `warmup` s of motion followed by `duration` s measured. During the window the handler samples machine load every second. The `load` object holds whole-machine CPU %, and GPU, encoder and decoder % and clock from `nvidia-smi`.

The handler also records what the build reports about itself before and after the window (`secondaryBefore`, `secondaryAfter`). The `pageCounters` object summarises that: `SEVideo.getStats()`, `SEAudio.getSyncInfo()`, WebRTC inbound video and audio, data channels and the candidate pair.

Setting `runs: N` repeats the window N times on the same join. The `aggregate` object gives the median, min, max and range of p50, p90, p99, max, painted fps and longest gap.

Every join reports these fields in `joins[]`:

| field | meaning |
|---|---|
| `answerMs` | submit → the response to the `/whep` POST, from the browser's network log |
| `firstPictureMs` | submit → the first painted frame carrying a stamp. A stage starts at the submit, waits for the join's display to attach and draws the moment it does |
| `firstStampAgeMs` | how old that first stamp was |
| `offers` | how many POSTs the page sent. More than one means a slow or failed answer made the page offer again, and `retried` says so |
| `whepRequests` | every `/whep` request with its timing |
| `modals`, `pageError` | the modals the handler dismissed, and the page's error text |

The top-level `pageConsole` lists the page's warnings and errors.

## Request keys

The request is one JSON object. Unknown keys are ignored and named in `warnings`.

### Keys that shape the run

| key | default | meaning |
|---|---|---|
| `scenarios` | `static`, `small_window`, `large_area`, `hires_small_window` | which standard scenarios run. Their order is fixed |
| `duration` | 20 | seconds per timed window |
| `static_duration` | 30 | seconds of the `static` window |
| `warmup` | 3 | seconds of motion before a timed window opens |
| `runs` | 1 | repeats of each timed window |
| `client` | `{width: 1920, height: 1080, dpr: 1, refresh: 60, https: true}` | the standard client, in CSS px. A partial object overrides only the keys it names |
| `hires_client` | `{width: 1280, height: 720, dpr: 2, refresh: 120, https: true}` | the high-resolution client |
| `host_env`, `host_args` | `{}`, `[]` | extra environment and extra `serve` arguments for the host |
| `chrome_flags` | `[]` | extra Chrome flags. `--se-unmute-audio` is a pseudo-flag that Chrome never gets: it drops `--mute-audio`, so the client's playback is audible and reaches the host's loopback capture |
| `headed` | false | a visible browser window |
| `calibrate` | true | run the floor calibration first |
| `force_exclusive` | false | exclusive mode even for an isolated build |
| `overlap_displays` | false | narrow the measurement lock to the timed windows (see "Measurement lock") |
| `display_check` | false | before the host starts, stop with exit 1 if a non-primary monitor is attached. The monitor is reported in `displayCheck` and never removed |
| `min_commit_gb` | 3 | before starting, wait until this many GB of system commit are free, checking every 60 s. 0 turns the wait off |
| `load_gate` | `{max_cpu: 75, max_wait_s: 180}` | see "Load gate". `false` turns it off |
| `stamp_band` | `'full'` | `'center'` reads a 2-row band through the stamp cells instead of averaging their height. The band is immune to the stage's blit tearing, which causes the roughly 10 % checksum failures at 1080p. It is not the default yet, because its noise figure has not been measured again |
| `screencast_width` | 480 | width of the screencast reader, which is not the default reader |
| `netlog` | false | each browser writes Chrome's net log to `runDir/netlog-<offset>-<n>.json` |
| `host_log` | `'relay'` | which process reads the host's output. See "Host log reader" |
| `url_params` | `''` | extra query parameters appended to every join URL (`a=1&b=2`), for a page's own debug or experiment switches. The input refusal checks them as it checks `custom.js` |
| `join_probe` | null | an expression evaluated in the page after every join. Its JSON result goes into that join's record as `probe`, for example a timeline the page keeps of its own join |
| `label`, `notes` | none | free text. `label` is also written to the slot file, where `--status` shows it |

### Load gate

Before every timed window, and before the static window, the handler waits for whole-machine CPU to fall below `max_cpu`. CPU is averaged over 2 s, and the wait is bounded by `max_wait_s`. Runs at 85-100 % CPU came out 10-25 ms slower than quiet ones.

Each window then carries:

| field | meaning |
|---|---|
| `loaded` | true when the gate gave up, or when the window's own mean CPU ended above `max_cpu` |
| `loadGate` | `waitedMs`, `cpuAtOpen`, `loaded` |
| `windowCpuMean` | the window's own mean CPU |

**Discard and repeat a window that has `loaded: true`.** The `aggregate.loadedRuns` field counts them.

### Host log reader

With `host_log: 'relay'` a process of its own (`lib/tslog.mjs`) reads the host's output. A stall in the handler then cannot block the host's log writes. Verbose `serve` makes those writes synchronously on whatever thread logs, and the handler's synchronous PowerShell calls stall it for 1.5 to 5 s per join.

With `'pipe'` the handler reads the output itself.

### Special requests

The table is in execution order. Modifier keys are listed beside the request they change.

| key | default | meaning |
|---|---|---|
| `audio` | false | the delay of a click train through system audio, on the common join. See "Audio requests" |
| `settings_change` | false | on the common join, each setting class edited alone, each with a fresh stamp window. See "Edits under a live stream" |
| `settings_only` | null | with `settings_change`: run only the edits named here, by label, to spend fewer display mode changes |
| `logs` | false | on the common join: `logs --lines 50` and `status --json` with a live session, then the whole backlog. The `full` object holds lines, bytes, the U+FFFD count, the `[logbus] dropped` reports, and how many of the last 300 bus lines the host also echoed to `serve.log`. With `{burst: N}` on an isolated build, N control connections with a wrong token make the host log N lines from outside (no input, no display). The backlog is then read again (`burst`), which wraps the 2000-line ring |
| `custom` | null | `{js, host_commands, timeout}`. `js` is an expression evaluated in the client page after the common join. `host_commands` is a list of CLI argument lists, for example `[["status","--json"]]`. `timeout` is in seconds (default 20). Results are in `special.custom` |
| `mic_prompt` | false | the microphone prompt and audio-output discovery, in three fresh browsers. See "Microphone prompt" |
| `two_clients` | false | a second browser joins the same host. Three settings are changed on the first while the second's stamps are compared before and during (`bUntouched`). Then the first client's stamps are read now that two displays exist (`aAfterBJoined`) |
| `leave_rejoin` | false | the tab leaves and rejoins within 2 s (`sameDisplayId`). Then it leaves for good and the display count is watched for 30 s (`removedAfterS` against the configured grace) |
| `slow_answer` | false | `true` or `{n: 5, hold_s}`: repeats the join n times and flags answers over 5 s, a page that stopped waiting, or a second session or display. With `hold_s` the page stays on the stream that long before it leaves, and the handler then reads the page's video path |
| `cpu_burn` | null | `{threads, seconds}`: busy workers at normal priority during the joins of `slow_answer`, inside the lock, to reproduce a lost answer under load |
| `fallback_paths` | false | `true`, or a list of tier labels. One fresh browser per client tier, each with a stamp window of at most 10 s and the path the page ended on. Tier labels: `default`, `no WebCodecs`, `no RTCRtpScriptTransform`, `no script transform and no encoded streams`, `plain HTTP (no secure context, no cross-origin isolation)`, `hardware decode off` |
| `mover_stop` | false | `true` or `{cycles: 10, move_s: 1.2, hold_s: 2, step_ms: 8, speed: 2.5}`: a 48 px window moves in pointer-like steps and stops. Per stop it reports where the client's picture comes to rest against the true position (`staleStops`), and how fast it got there |
| `audio_paths` | false | the audio test once per client audio tier. See "Audio requests" |
| `join_failures` | false | joins that are made to fail, and what the page does afterwards. See "Failed joins" |
| `arms` | null | A/B arms inside one join. See "A/B arms" |
| `auto_rejoin` | false | stops and restarts the host under a live page. Reports a timeline of host joins and page state for up to 30 s |
| `receive_hold` | null | `{rates: [30, 60, 120], browsers: ["chrome", "edge"], modes: ["headless"], seconds: 30, settle_s: 5, large: true, relay: false, client}`: when does the page's transform worker get each encoded frame? See "Receive timeline" |
| `impair` | false | `true` or `{scenarios, baseline_s: 10, impaired_s: 20, recover_s: 15, settle_s: 3, large: true, browser, client}`: network impairment through the loopback relay, on one join. See "Relay and impairment" |
| `slow_client` | false | `true` or `{cases, spin_ms: 25, cpu_rate: 6, baseline_s: 10, slow_s: 15, recover_s: 15, large: true, browser, client}`: a client that cannot keep up. Cases: `worker_spin`, `cpu_throttle`, `hidden`, `software_decode`. The first three are phased windows on one join. `software_decode` is its own browser and join |
| `remote_client` | false | `true` or `{ssh: "user@host", rates: [60, 120], seconds: 20, settle_s: 5, large: true, chrome_path, flags, client}`: the page in a headless Chrome on another machine. See "A client on another machine" |
| `multi_session` | false | `true` or `{max: 4, seconds: 15, client}`: browsers join one at a time, one display creation each. After each join every session gets a simultaneous stamp window, with each display's mode and DWM's composition rate (`displayRates`) |
| `churn` | false | join, leave, rejoin, renegotiate, aborted join and killed browser, with counters after every step. See "Churn" |
| `soak` | false | `true` or `{minutes: 60, period_s: 5, large: false, mover: true, client}`: one session. Every period it takes a stamp window plus host, process and page counters, and appends them to `soak.jsonl` in the run directory. `slopesPerHour` fits every series by least squares with a 95 % interval. `leakSuspects` lists the memory, handle, thread and socket series whose interval is above zero |
| `allow_real_input`, `real_input` | false, false | the real cursor and real injection. See "Real input" |
| `join_reliability` | false | joins in a row, each timed on both sides. See "Join timing" |
| `abandon_join` | false | a tab closed right after its offer. See "A tab closed right after its offer" |
| `pipe_close` | false | after every join has left: closes the host's stdout and stderr pipes as a parent that stops reading does, makes the host log (bad-token connections), and reports whether `logs` and `status` still answer (`controlServerSurvived`). Isolated builds only |
| `loss` | false | reported as **not run**. Use `impair`, which needs no admin rights |

The `audio`, `settings_change`, `logs` and `custom` requests share the common join of the standard scenarios. The `pipe_close` and `loss` requests run after the live-display part.

### Audio requests

The `audio` request enables system audio for the device with `devices set <token|ip> --audio on`. It then plays a click train: `clicks.ps1` plays 4 ms bursts of 2 kHz every 700 ms for 12 s, timed by winmm's played position. An analyser spliced in before the destination detects each onset at the client.

| field | meaning |
|---|---|
| `delayToRenderMs` | click → onset at the analyser |
| `delayToOutputMs` | the same plus the browser's output latency, from `getOutputTimestamp` |
| `delayToDecodeMs`, `decodeToRenderMs` | the same delay split at the decode worker |
| `matched`, `unmatchedOnsets` | clicks paired with an onset, and onsets with no click. Other sounds on the machine are captured too |
| `clickPeak`, `silenceBefore`, `silenceAfter` | onset peaks, and the level before and after the train |
| `path`, `transport`, `syncInfo` | the page's audio path, from `SEAudio.getSyncInfo()` |
| `dataChannels`, `audioTrack` | the data channels, or the Opus track |
| `fallbackHints` | console lines that explain a fallback |

**The absolute delay includes winmm's own buffering ahead of the audio engine.** The host time is when winmm reports the sample consumed, which is earlier than the engine mixes it, so the delay reads high: about 140 ms to render on this machine. It is the same for every build, so compare, do not quote.

A build that applies the override only to a new connection (version 0.6.3 says so) is left and rejoined first.

Every handler client runs on the host machine, and a page may keep its own playback muted for that reason. The handler unmutes such a page through `SEAudio.setMuted(false)` before the train, and reports `pageMutedBeforeTest` and `pageUnmutedByHandler`.

| key | default | meaning |
|---|---|---|
| `click_source` | `'winmm'` | `'wasapi'` plays the train with `clicks-wasapi.ps1`, which times each click by the audio engine's play position |
| `audio_keep_page_mute` | false | do not unmute a page that muted itself before the click train |
| `audio_paths` | false | `true` runs every client audio tier, each in a fresh browser with the full audio test. A list names tiers by key |

| `audio_paths` tier | client |
|---|---|
| `default` | isolated page, `SharedArrayBuffer` ring |
| `no_sab` | no `SharedArrayBuffer` (private ring) |
| `no_audio_decoder` | no `AudioDecoder` (standard Opus track from the start) |
| `plain_http` | plain HTTP (no secure context) |
| `worker_fails` | the decode worker fails to load after the fast path was claimed |
| `stale_output` | a saved output id that this client does not have |

### Edits under a live stream

The `settings_change` request edits each setting class alone, after the common join. Each edit gets a fresh stamp window.

| label | edit |
|---|---|
| `audio on`, `audio off` | `devices set --audio on`, then `off` |
| `video quality` | `--video-quality 30` |
| `video scale` | `--video-scale 75` |
| `refresh rate` | `--refresh-rate 120` |
| `DPR (native-dpr off)` | `--native-dpr off` |
| `orientation portrait`, `orientation landscape` | `--orientation Portrait`, then `Landscape` |
| `display scale` | `--scale 150` |
| `name` | `config set devices.<i>.name`, because `devices set` has no name flag |
| `adaptive quality` | `--adaptive-quality on` |

Each edit reports:

- live joins, join id, displays and display id, before and after;
- device connected and disconnected lines in the host log;
- new peer connections and new `/whep` requests on the page;
- the largest stamp gap after the edit, and `pictureRecoveredMs`.

`defectSeen` is true when any edit leaves more than one join or more than one display.

### Microphone prompt

The `mic_prompt` request opens three fresh browsers, one display creation each.

| case label | permissions granted |
|---|---|
| `speaker access granted (no microphone)` | `speakerSelection`, `wakeLockScreen` |
| `microphone granted` | `audioCapture`, `wakeLockScreen` |
| `nothing granted` | none |
| `plain HTTP, nothing granted` | none, over the LAN address with no secure context. Runs only with `http: true` or when named in `cases` |

Each case reports:

- whether `getUserMedia` asked for audio, and the Permissions-API queries;
- whether the page's microphone card appeared (the handler dismisses it with *Skip*);
- the outputs `enumerateDevices` lists, with whether the id is empty and the label;
- the page's `SEAudio.getOutputPlan()` and its `se.outputs.*` storage;
- submit → first `/whep` POST, and submit → `/audio-outputs` POST;
- `selectAudioOutput` and `setSinkId` support;
- `defect`, when a prompt or card appears although outputs were already labelled.

As an object, the request takes these options:

| option | effect |
|---|---|
| `cases` | the case labels to run |
| `audio: true` | turns system audio on per device, then reads the page's audio path, context state, sink and 4 s of DataChannel or track traffic |
| `rejoin: true` | the nothing-granted browser joins again: one more display |
| `http: true` | adds the plain-HTTP case: one more display |

Every case also reports `hostAudioOutputs`, which is what the host holds for that join. It comes from `status --json`, on builds that report `telemetry.joins[].audioOutputs` (`supported`, `outputs`, `labelled`, `discovery`). It also comes from the line `serve` logs per post: `[audio-outputs] join <id>: supported=… outputs=N (kinds) labelled=… current=…`.

A value of `supported=false` there is what makes the device sheet say "can't switch speakers". The handler flags it as a `defect`.

### Failed joins

The `join_failures` request is `true`, a list of cases, or `{cases, stop_after_ms: 600, watch_ms: 8000}`. The default cases are the first three.

| case | what makes the join fail |
|---|---|
| `wrong_otp` | the join URL carries a wrong OTP |
| `unknown_session` | the join URL carries a session id the host does not have |
| `host_stopped` | the host is stopped `stop_after_ms` after the offer is on the wire, and restarted afterwards |
| any other name | nothing. The join is a normal one, for example against a host built to fail it |

After each failure the handler reads the page's own state: the form, Connect, the wake lock sentinel, NoSleep, the `beforeunload` handler, the input gate and open peer connections. It also records every request the page sends for `watch_ms`, and it dispatches nothing. The `pass` object holds one boolean per check.

### A/B arms

The `arms` request is `{arms: {name: {SWITCH: value}}, order: [names], scenario, qualities: [q1, q2], settle_s, set_args}`. It runs A/B arms inside one join.

1. Before each window the handler writes the arm's switches to `x-switches.txt` in the run directory, one `SWITCH=value` per line.
2. The host gets that path as `SE_X_SWITCH_FILE`. A build that does not read the variable ignores the switches.
3. The device renegotiates in place with `devices set --video-quality`, alternating between the two `qualities`.

Use a balanced order, for example forward then reversed. The `set_args` list is added to every edit. Pass `["--refresh-rate","120"]` for the hi-res client, because `devices set` on a device with no saved row writes `refreshRate` 60.

### Churn

The `churn` request is `true` or `{kinds, cycles: 1, client, reuse_client, set_args, require_picture}`. Each kind costs one display creation per cycle (`costPerCycle`). By default all five kinds run.

| kind | what happens |
|---|---|
| `join_leave` | a join, then a leave |
| `rejoin` | the page goes away and the same device joins again 2 s later, then leaves |
| `reconfig` | five `devices set` edits on a live join (video quality, refresh rate, video scale), then a leave |
| `abort` | the tab closes as soon as its `POST /whep` is on the wire. The handler watches what the host keeps, and for how long |
| `kill` | the browser process is killed with no leave |

After every step the handler records displays, joins and `live_transport_sessions`. It records the host's private bytes, handles, threads, UDP and TCP sockets and GPU memory, and the driver host's memory and threads.

At the start and after each leave it also records the host's handles by object type and its GPU memory per adapter (`handles.ps1`). `perCycleSlopes` fits every counter against the cycle index with a 95 % interval.

The run stops once the driver host is above 5 GB or 500 threads. A join that never paints records `neverConnected` (see "A join that never connects").

For a leak verdict, run `{kinds: ["join_leave"], cycles: 30}` twice: once as is, and once with `host_args: ["--encoder", "intel"]`.

| option | effect |
|---|---|
| `reuse_client: true` | one browser, and so one device token, for every cycle except `abort` and `kill` |
| `set_args` | arguments passed once to `devices set`, after the first join |
| `require_picture: false` | count a join for leak accounting even when nothing painted |

### Real input

The caller must set both `allow_real_input: true` and `real_input`. That pair is the only way past the input refusal.

The request is `real_input: {cases: ["cursor_sweep", "remote_control"], seconds: 10, moves: 200, client}`.

| case | what it does |
|---|---|
| `cursor_sweep` | moves the real cursor over the test's virtual display with `real-cursor.ps1`, and compares painted fps and latency against a still cursor. The script refuses the primary display, aborts if anything else moves the cursor, and puts the cursor back |
| `remote_control` | turns remote control on for the test device (pointer scope `display`) and sends pointer moves into the page through CDP. Each move is paired with the host's `SE_INPUT_TRACE` line (`dispatchToInjectMs`, `hostQueueUs`, `hostInjectUs`) |

Both cases move the pointer only. Neither clicks or presses a key.

## A join that never connects

Six requests retry a join that painted nothing, once, from a fresh navigation: `receive_hold`, `impair`, `slow_client`, `remote_client`, `soak` and `real_input`. Before the retry they record `neverConnected`, which is also collected in the top-level `neverConnected` list. The `churn` request records it too, without a retry.

| field | content |
|---|---|
| `ice` | the page's peer connections: connection, ICE and gathering state, local and remote candidates, candidate pair states |
| `patches` | every trickle `PATCH` with its status and timing |
| `hostAtFailure` | the host's live joins and monitors at that moment |
| `displayKeptAfterPageGoneS` | how long the host kept the display after the page went away, watched for up to 60 s |

## Join timing (`join_reliability`)

The request is `true` or `{joins: 40, threshold_ms: 5000, race: false, pause_between_s: 0, cpu_burn: {threads, seconds}, browser, client}`. It runs joins in a row, or two racing with `race`, and times each on both sides.

A join fails when its answer is slower than `threshold_ms`, when the page offered more than once, or when nothing painted. `pass` is false when any join failed.

### Where the times come from

- Browser side, from the network log: when the `POST /whep` was sent, when its response arrived, and whether it failed.
- Host side, from `serve.log` through `lib/joinlog.mjs`: for each join request of that device, the time from the request line to the first line of each stage.

The host stages, in order: `displayLockWait` (the lock wait the host itself reports), `displayCreated`, `displayAttached`, `modeSettled`, `pipelineStart`, `adapterSelected`, `encoderCaps`, `encoderReady`, `gpuClockHold`, `captureReady`, `answer`, `iceSelected`.

Two more values come from the host log. `hostReported` holds the host's own `join stages` line. `longestSilenceMs` is the longest stretch with no host log line before the answer, with the line it followed.

Log times are when the line was read (`[epoch-ms]`), so with `host_log: 'pipe'` a stalled handler delays them. `handlerLoopMaxLagMs` says how long this process's event loop was stalled during the join.

### Per join

| field | meaning |
|---|---|
| `sentToHostLogMs` | POST sent → the host logging the request |
| `hostAnswerToBrowserMs` | the host logging the answer → the browser receiving it |
| `shape` | for a failed join, one of: the POST never appeared in the host log; the host logged the request but no answer; the host answered late; the host answered but the browser got it late; other |
| `stage`, `restaged` | where the stamp stage drew and how it found the display, and whether it had to be started again |
| `hookBeforeFirstPicture` | guard and stamp reads before the first stamp |
| `stageTicks` | once a second from the stage: the stamps drawn, the monitor its window is on (by device path), and whether the window was cloaked or visible |
| `stageVerifiedAt` | the first second in which the stage drew on the join's monitor |
| `monitorChanges` | every non-primary monitor's handle, device-path tail and rectangle. An independent `stage.ps1 -MonitorOnly` sampler reads them every 250 ms and starts before the submit. A sample is kept only when the list changed |
| `displayMessages` | the `WM_DISPLAYCHANGE`, `WM_DEVICECHANGE`, `WM_DPICHANGED` and `WM_SETTINGCHANGE` messages the stage window and the sampler received. `from` names the process that received each one |
| `post` | per POST: `sentMs`, `answeredMs`, `failedMs` and Chrome's timing |
| `console` | what the page and its workers logged during the join |

The monitor sampler owns a hidden top-level window, pumped every 10 ms. `displayMessages` therefore includes broadcasts sent before the stamp stage has found its display.

The `stages` and `hostStages` objects give the distribution of each value. `firstJoinVsLater` separates the first join after host start.

### Capture and response timing

These values come from the host log and the network log alone. They are reported even for a join that never paints a stamp.

| field | meaning |
|---|---|
| `capture.tries` | capture starts |
| `capture.staleRetries` | capture items that went stale before they could be opened |
| `capture.itemTries` | the tries the host itself counted before it got a capture item |
| `capture.openedMs` | when the host logged that the capture opened, in ms after the join request |
| `capture.hostFirstFrameMs`, `capture.firstFrameBackend` | from the host's "first frame of ... from <backend>" line, or `first_frame=` on its join stages line |
| `capture.firstFrameReopens` | reopen lines in the log |
| `capture.telemetryFirstFrameReopens`, `capture.telemetryDarkRebuilds` | the counters on the telemetry line |
| `capture.answeredAsIs`, `capture.reopens`, `capture.reopened` | whether the host answered with the capture as it was, the reopen times, and whether any reopen happened |
| `scale` | the host's scale line: the effective DPI before the write, at the first read after it, and when the written value showed |
| `answerToResponseMs`, `hostRequestToResponseMs` | from the host's "join response returned" line, per request |
| `handlerResumedMs` | from the host's "the handler resumed N ms after the join finished" line, which the host prints only for 20 ms or more |
| `browserRequestToResponseMs` | per POST, from send to response |
| `firstPictureAfterStageMs` | the first picture measured from `stageVerifiedAt`, so a late stage does not count against the host |
| `maxMonitors` | the most of our monitors seen at once during the join |
| `attempts` | what the host logged about a second offer of the same attempt, a superseded attempt, a join closed without a client, and display removal |
| `encoderSessionsMax` | with `encoder_sessions: true`: the most NVENC sessions on the machine during the join |

### More keys of the request

| key | effect |
|---|---|
| `screenshot_slow_s: N` | takes one screenshot N s into each join that has not finished by then. The file is `slow-<join>-<client>.jpg` in the run directory. It may show the desktop, so look at it and then delete it |
| `encoder_sessions: true` | samples the NVENC session count every 200 ms with `nvidia-smi`. All samples go to `encoder-sessions.json` |
| `host_patterns: {<key>: "<regex>"}` | replaces any of the `CAPTURE_PATTERNS` in `lib/joinlog.mjs`, for a host build whose wording differs |
| `fresh_device: true` | clears the page's local storage and cookies before every join, so each join is a first-time device with no token. `'first'` clears before the first join only |
| `alternate_device: true` | odd joins are a new device. Even joins return as the device before them |
| `lose_answer: {mode, ms, after_first}` | makes the page lose the response to the first `POST /whep` of each page load. See "Losing an answer" |
| `offer_strip: [fields]` | removes those fields from the JSON body of every `POST /whep` |
| `during_wait: {action, after_answer_ms: 300, every, value}` | one action once the answer is in and before the picture. `action` is `reload`, `close` or `refresh_rate` |
| `rejoin_gaps: [seconds, ...]` | after join i the page reloads the join URL, which sends the leave. The same device submits again that many seconds later |
| `deep: true` | the host's counters and the page's WebRTC counters every second, and Chrome's GPU process before and after |
| `remote`, `remote_chrome_path`, `remote_flags` | the page runs in Chrome on another machine. `remote: true` takes the target from `SE_TEST_REMOTE_CLIENT` |
| `pause_reader_ms`, `pause_reader_after_ms` | with `host_log: 'pipe'`: stops reading the host's output for that long, starting 500 ms into the join by default |
| `hang_probe: true` | runs `hangprobe.ps1` for the whole request. See "Hang probe" |
| `space_until_shell_quiet` | `true` or `{quiet_s: 5, cap_s: 60}`. With `hang_probe`, the next join starts only once Explorer has answered every sweep for `quiet_s` |
| `stop_on_fail: true` | stops after the first join that fails for a reason other than the threshold |
| `monitor_samples: false` | turns the monitor sampler off |

### Losing an answer

With `lose_answer` the request reaches the host and the host answers. The page never sees the response.

| setting | effect |
|---|---|
| `mode: 'drop'` | fails the fetch after `ms` (default 300) the way a reset connection does. The page offers again at once |
| `mode: 'hold'` | never resolves the fetch, so the page's own 10 s timeout fires. This is the default |
| `after_first: true` | lets join 1 through, so the later joins are a returning device |

Each record's `lost` says what the page saw and when the real response arrived.

### Hang probe

Every 250 ms `hangprobe.ps1` sends `WM_NULL` to every top-level window on the desktop, in every process. It uses `SendMessageTimeoutW(..., SMTO_ABORTIFHUNG, 250)`. It records each window that does not answer in time, or that takes 100 ms or longer: process, pid, thread, class, title, visible and `IsHungAppWindow`.

Once a second it lists the host's threads that own a window, message-only windows included. It sends nothing else, and it moves and focuses nothing.

Each join's `hang` record holds:

- the refused period from the host log, from the first "not capturable yet" line to the item or the give-up;
- every window that did not answer inside that period, with how long it stayed unresponsive and when it answered again. `alreadyBefore` marks a window that was unresponsive before the join;
- the host's window-owning threads before, during and after the period.

All sweeps go to `hang-probe.json`.

## A tab closed right after its offer (`abandon_join`)

`abandon_join: {joins: 2, close_after_ms: 50, watch_s: 90}` opens a fresh browser per join and submits. It closes the tab `close_after_ms` after the `POST /whep` went out, then watches for `watch_s`.

It reports:

- when the display appeared and when the last of ours was gone, from the monitor sampler;
- the host's lines about the join: closed without a client, removal, grace;
- the most monitors seen.

A last ordinary join checks that the host still serves. `health_join: false` skips it.

## Receive timeline (`receive_hold`)

Every worker the page starts also gets `lib/rx-hook.mjs`, which wraps `ReadableStreamDefaultReader.read`. Both transform tiers in `transform-worker.js` take frames with `reader.read()`. For every encoded video frame the hook records:

- the RTP timestamp;
- the instant the read resolved;
- `getMetadata().receiveTime`, the last packet's arrival (Chrome 135 and later);
- the host capture time the worker derives from the RTP stamp.

Nothing on the page changes. There is one join per browser, and each rate is an in-place renegotiation (`devices set --refresh-rate`, one mode change).

`modes: ["headless", "headed"]` runs each browser both ways. A headed browser starts off every screen and is then moved onto the join's own virtual display.

### Clocks

- `read` and `receiveTime` are on the same worker clock, so `receive_to_read_ms` needs no offset.
- Worker times move onto the precise system clock through that worker's own min-RTT offset (`workerClockMinusSystemMs`, `workerOffsetRttMs`). The error is up to half that RTT.
- Relay times are on the system clock already.
- The host capture time comes through the page's PING/PONG offset to the host clock.

### Per window

| field | meaning |
|---|---|
| `sourceFps`, `sourceIntervalMs` | from the RTP stamps, which are on the host's 90 kHz clock |
| `readIntervalMs` | the read-to-read intervals, with a histogram |
| `pairs` | reads under 1 ms after the previous one: two frames handed over by one release |
| `releaseGroups` | reads within 1 ms grouped as one release: count per second and sizes |
| `phaseLock` | the mean resultant length R of the read and receive instants folded onto 15.625 ms, the display periods and the source period, with a Rayleigh Z and a 16-bin phase histogram. R near 1 means locked. R near 1/sqrt(n) means not locked |
| `bestPeriod` | the period between 12 and 20 ms with the largest R |
| `perSecondLock15_625` | the lock at 15.625 ms, per second |
| `receive_to_read_ms` | receive → read, with a 1 ms histogram |
| `tickModel` | a 64 Hz clock with its phase taken from the reads: the hold it predicts for each frame, and how far each read lands from the tick it predicts |
| `relay_to_read_ms`, `relay_to_receive_ms` | with `relay: true` the media also crosses the relay. These compare with the relay's own send of each frame's last packet |

Raw rows go to `rx-<browser>-<hz>-<time>.json` in the run directory.

### Reading a 15.625 ms grid

Chromium releases received video frames to an encoded transform on a 64 Hz metronome (`rtc_encoded_video_stream_transformer.cc`, `timer_based_tick_provider.h`). Two other things produce a 15.625 ms grid on Windows:

1. The default system timer period. `timerResolutionBefore` and `timerResolutionAfter` record `NtQueryTimerResolution`.
2. Windows ignoring the timer requests of a process with no visible window, which a headless browser is.

A comb at exactly 15.625 ms with the timer at 1 ms rules out the first. Headless runs cannot rule out the second.

Two things are outside what a page can measure: the browser's compositor after the paint call, and the network stack before `receiveTime`.

## Relay and impairment (`impair`)

`lib/relay.mjs` runs a UDP relay on a worker thread, bound to the host's LAN address. CDP Fetch rewrites the host candidates in the WHEP answer to the relay's ports, and strips the browser's candidates from the offer and from every trickle `PATCH`. The only pair that can form therefore goes through the relay.

`relayVerdict.viaRelay` checks the browser's selected pair against the relay's ports, and requires media counted at the relay. HTTPS signalling stays direct.

### What the relay does

Per direction, in this order:

1. Stall: hold everything, then release in arrival order.
2. Loss: Bernoulli, or Gilbert-Elliott bursts.
3. Delay with uniform jitter. Order is kept unless `reorder` is set.
4. Rate cap: a token bucket serving a FIFO drop-tail queue limited in bytes (default 100 ms at the rate).

A `schedule` changes a direction's settings mid-window. The relay keeps:

- a row per second: packets in and out, drops by cause, held packets, queue depth;
- totals;
- the RTP sequence gaps it saw arriving (`streams[].seqGaps`), which is loss before the relay;
- its own lateness (`lateness_ms.wake`): when its scheduler meant to run against when it ran.

### Scenarios

`impair.scenarios` takes names, or `{name, label, down, up, schedule}` objects.

| name | impairment |
|---|---|
| `stall200` | a 200 ms stall every 3 s, host to browser |
| `loss1` | 1 % random loss, host to browser |
| `loss5` | 5 % random loss, host to browser |
| `burst` | Gilbert-Elliott burst loss, host to browser |
| `loss5_up` | 5 % random loss, browser to host |
| `delay_jitter` | 30 ms delay plus or minus 10 ms, both ways |
| `cap5` | 5 Mbit/s cap, host to browser |
| `cap1_5` | 1.5 Mbit/s cap, host to browser |
| `cap_change` | a cap of 8, then 1.5, then 8 Mbit/s, host to browser |

The default list is `stall200`, `loss1`, `loss5`, `cap5`, `cap1_5`, `cap_change`.

### What a scenario reports

Each scenario is one continuous stamp window in three phases: baseline, impaired, recovery. Per phase:

- the stamp metric;
- the page and WebRTC counter deltas: keyframes requested, PLI, NACK, packets lost;
- the host's counters, from `status --json` once a second: bitrate series, encoded fps, QP, keyframe requests and suppressed ones, IDRs sent, NACKs, bitrate cuts.

| field | rule |
|---|---|
| `recovery.recoveredMs` | from the end of the impairment to the first of four consecutive healthy 250 ms bins. A healthy bin paints at least 80 % of the baseline frame rate, with a p50 within max(10 ms, 30 %) of the baseline's |
| `recovery.backToFloor` | the p50 of the last 5 s is within max(5 ms, 15 %) of the baseline's |

A scenario is invalid (`valid: false`, reasons in `invalid`) in four cases:

- the baseline painted under 20 fps;
- the relay did nothing to the traffic;
- the relay's scheduler was more than 2 ms late at p99;
- the machine was above the load gate.

## A client on another machine (`remote_client`)

The handler starts a headless Chrome on the target over SSH, with `BatchMode`, so a key must already work. The browser gets a fresh profile in one new directory under the target's `/tmp`. The handler forwards the debugging port over SSH and drives the browser like a local one.

Nothing else on that machine is read or written. The directory is removed at the end, and `closed.remote` reports anything left running.

The `ssh` target may come from `SE_TEST_REMOTE_CLIENT`. Without either, the request is reported as not run. The default `chrome_path` is the macOS one, `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`.

The run records:

- the page console;
- a 2 s timeline of the browser's WebRTC counters next to the host's;
- `stall` spans where packets kept arriving but nothing was painted, or where nothing arrived.

Firefox and Safari cannot be driven this way without changing settings on that machine. Safari needs remote automation enabled. Firefox would need a WebDriver BiDi client, which the handler does not have.

## Page overrides (`client-override.mjs`)

`client-override.mjs` serves local copies of client page files in place of the ones the host embeds. A page change can then be tried against a host binary built before it. Load the module ahead of the handler:

```text
node --import ./scripts/test-handler/client-override.mjs scripts/test-handler/handler.mjs --exe ... --request ...
```

| variable | meaning |
|---|---|
| `SE_TEST_CLIENT_OVERRIDE` | `NAME=PATH[;NAME=PATH...]`, for example `transform-worker.js=C:\x\transform-worker.js`. A `PATH` of `host` keeps the host's own file |
| `SE_TEST_SLOW_MODE` | `paint`, `decode` or `starve`, or a comma list taken in turn by successive `worker_spin` cases. It sets what the case does with its `spin_ms` |
| `SE_TEST_HIDDEN_MODE` | `freeze`: the `hidden` case also freezes the page's lifecycle, and thaws it on restore |
| `SE_TEST_NO_SCRIPT_TRANSFORM` | `1`: the page takes the legacy `createEncodedStreams` transport |
| `SE_TEST_WORKER_LOG` | a `.jsonl` file. Every worker's counters and per-frame rows (read, submit, output and paint times) are appended once a second |
| `SE_TEST_WINDOW_SCRIPT` | with `SE_TEST_WORKER_LOG`: what each stamp window in turn does to the worker, comma separated. Entries are `none`, `paint:<ms>`, `decode:<ms>`, `starve:<ms>` and `freeze:<ms>` |

| slow mode | effect |
|---|---|
| `paint` | every `VideoFrame` `drawImage` busy-waits `spin_ms` |
| `decode` | the decoder takes at most one chunk per `spin_ms` |
| `starve` | other work takes the thread: `spin_ms` busy, 2 ms free, repeated |

Only the response body changes, because the worker needs the same COEP and CORP headers as the page. The module dispatches no input.

Under this module `SE_TEST_DISPLAY_GATE` has another reading. It names a script, which the module runs with `bash` before every join, leave and refresh-rate edit. `SE_TEST_DISPLAY_GATE_FILE` then names the timestamp file.

## Environment

| variable | meaning |
|---|---|
| `SE_TEST_DISPLAY_LOCK` | a directory. The handler creates it before its host starts and removes it after cleanup, with `owner.txt` inside. `mkdir` makes the take atomic. Other tools on the machine that create virtual displays take the same directory. While another process holds it, the handler retries every 20 s. Unset: only the handler's own measurement lock applies |
| `SE_TEST_DISPLAY_LOCK_WAIT_S` | how long to wait for the display lock, in seconds (default 1800) |
| `SE_TEST_DISPLAY_GATE` | a file holding the epoch ms of the last virtual display add, removal or mode change by any process on the machine. See "Display gate" |
| `SE_TEST_DISPLAY_GATE_MS` | lengthens the 60 s spacing of the display gate. It cannot shorten it |
| `SE_TEST_RUN_ROOT` | where run directories and browser profiles go (default `%TEMP%\screenextend-test-handler`). Soak series and net logs grow, so point this at a large drive for long runs |
| `SE_TEST_REMOTE_CLIENT` | `user@host` for `remote_client` |
| `SE_TEST_SSH` | the ssh client (default `C:/Windows/System32/OpenSSH/ssh.exe`) |
| `CHROME` | the Chrome executable (default `C:/Program Files/Google/Chrome/Application/chrome.exe`) |
| `EDGE` | the Edge executable (default `C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe`) |

### Display gate

Every display event the handler causes waits until 60 s after the time in the gate file. The handler then writes the new time there, and adds a line to `DISPLAY.events.log` beside it. The run records every event in `displayEvents`.

These events are gated:

- every join submit;
- every navigation or close of a page that holds a join;
- every `devices set` that changes resolution, refresh rate, DPR, orientation or scale;
- a host stop or restart with a display attached;
- a killed browser or a closed tab in `churn`;
- every monitor removal in cleanup and in `--reap`.

While nothing of ours is attached, the handler releases the display lock for the wait. `--gate-check N` exercises the gate alone.

### Reaping and the parent watch

`--reap` removes what a handler that was killed outright left behind: its browsers, its `serve`, its virtual monitors when no live host owns any, and a display lock whose holder is gone. A run does the same before it starts its host.

`--parent-pid <pid>` makes the handler watch that process. When it exits, the handler cleans up the host, browsers, displays and display lock, and exits 1. A caller that runs under a watchdog passes its own pid, so a watchdog kill leaves nothing behind.

## Old builds (exclusive mode)

The handler has run against two builds without the isolation switch: version 0.6.3, and the last development build before the switch (the pre-switch build).

This is what version 0.6.3 lacks and what the handler does instead. All of it is reported in the JSON.

| missing in 0.6.3 | the handler instead |
|---|---|
| `SCREENEXTEND_DATA_DIR` | exclusive mode, the real data directory, `config.json` backed up and restored (byte-identical, checked) |
| `SCREENEXTEND_SESSION_ID` / `SCREENEXTEND_OTP` | credentials on argv (`--session-id`, `--otp`). `host.credentialsVia` says so |
| control server under `serve` (`stop`, `status --json` telemetry, `logs`) | a PID stop (`cleanup.hosts[].method`), then a check of the driver for leftover monitors. None were left, because the page's leave had already removed the display. `special.logs.pass` is false, because the `logs` verb reads the calling process. The device is addressed by IP |
| `window.SEVideo`, join ids, trickle ICE | nothing needed. The paint hook, the network log and the screen diff are build-independent. `pageCounters` is empty |
| applying a device override to a live session | the audio test leaves and rejoins after `devices set <ip> --audio on` |

Seen on version 0.6.3 through the handler:

- Its `serve` reads device overrides only at startup. The audio test therefore restarts that host (PID stop, same credentials and ports) and rejoins.
- The host then sends audio, but the page cannot play it: `[audio] decoder worker error` and `AudioWorkletNode cannot be created`. The worker scripts carry no COEP header. The pre-switch build fixes that.
- The build directory had no DLLs until `libopus.dll` and `libx264-164.dll` were copied beside the exe. The handler warns when they are missing.

## Acceptance runs

All runs are from 2026-10-05 on Windows 11 26200 with an RTX 3080 Ti Laptop and Chrome 154 headless. **The machine was busy throughout**: other processes were compiling, and whole-machine CPU was 82-90 % during every timed window (`load.cpuPct` in each run). Treat the absolute numbers as this machine under that load.

| build | what it is |
|---|---|
| isolated build | the development code of 2026-10-04 plus the isolation switch |
| pre-switch build | the same development code without the switch |
| version 0.6.3 | the released app |

`acceptance.mjs` produced every figure below. It left its summaries in `%TEMP%\screenextend-test-handler\acceptance-*.json`.

### 1. Four simultaneous invocations, a fifth queues

The command was `acceptance.mjs concurrent --n 5`, with runs started 1.5 s apart and this request:

```json
{"scenarios":["static","small_window"],"duration":10,"static_duration":10}
```

- Slots 1-4 were taken at once (queue wait 1-3 ms). The fifth run waited **169 s** for a slot.
- All five exited 0 with `ok: true`.
- There were 15 measurement windows across the five runs, and 0 overlaps.
- Every run's `cleanup` was empty: no display, host or Chrome left. The real `config.json` was identical after each.
- Small-window p50 per run: 34.33, 34.42, 33.40, 33.56 and 34.05 ms. Painted fps was 53.1-55.3.

Two earlier attempts had the lock covering only the timed windows. They completed and serialised too, but their numbers were not valid: with 2-4 virtual displays attached, slots measured no stamps at all, frozen frames, or p50 ≈ 100 ms. That is why the lock now covers the whole live-display part (see "Measurement lock").

### 2. Exclusive mode against builds without the switch

- The pre-switch build ran `small_window`, `audio` and `two_clients`.
- Version 0.6.3 ran `static`, `small_window`, `audio` and `logs`, then `audio` again with the restart.
- All of them ran to completion.
- The SHA-256 of `config.json` was `7245d85df57bfe17…` before and after each. The audio step writes the file, so it was restored byte for byte and verified.
- The pre-switch build stopped through its `stop` verb, and version 0.6.3 by PID. The driver showed no leftover display after either.
- Credentials went by environment to the pre-switch build, and on argv to version 0.6.3, which has no other way.

### 3. Five consecutive standard runs on the isolated build

The command was `acceptance.mjs noise --runs 5` with the default request: calibration, static for 30 s, then small window, large area and hi-res small window for 20 s each. **This spread is the noise figure.** Latency is host draw → client paint call (see "The primary metric").

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

- Calibration floor across the five runs: p50 0.5-0.7 ms, p99 1.1-1.2 ms, max ≤ 1.6 ms. The clock offset (browser minus system) was 0.31-1.06 ms, and it is subtracted.
- The page's own counters in the same windows, from `SEVideo.getStats()`: `hostToPaint` p50 was 22.1-24.7 ms (small window), 22.5-25.0 (large area) and 28.6-32.9 (hi-res). The page painted 61-67 fps. `videoPath` was 1 in every window.
- Join answer times are bimodal. The first join of a host, which creates the display, took 5.5-6.1 s. A later join took 3.2-3.5 s.
- Three of the five first joins showed the first stamped frame 6.6-16.7 s after the answer. The join stage was restarted in each (`restagedFor`). It is not established whether the delay was the stream's or the stage's, because the display may still have been changing mode. The stage now follows mode changes and the join records `hookBeforeFirstPicture`, but the noise runs predate both. Treat `firstPictureMs` outliers with care until they are measured again.
- Static desktop, 30 s: 2 of 5 runs had 36 changed frames, with 6 frames carrying a single newly green-dominant grid sample. The other 3 had none. This is not explained.
- The p99 and max ranges are wide (tens of ms). On this loaded machine, compare p50, whose range is 1.5-2.3 ms on the 1080p scenarios and 7.9 ms on hi-res. Treat p99 differences under about 50 ms as noise.

### 4. Nothing left running

After every run above, `cleanup.displaysLeftByUs`, `hostProcessesLeft` and `chromeLeft` were empty. The driver reported `[]`, and no `ScreenExtend.exe` was in the process list.

## Special requests: results from 2026-10-05

Every request that existed at that date ran at least once against the isolated build. The `audio` request ran against all three builds.

| request | result on the isolated build |
|---|---|
| `audio` | fast path (`webcodecs`/`shared`). 14 of 15 clicks matched. Delay to render p50 142 ms, to output p50 192 ms. Both include winmm buffering (see "Audio requests"). Silence 0 before and after |
| `mic_prompt` | speaker access granted: no microphone request, no card, outputs labelled (Chrome's fake devices). Nothing granted: the card appears and outputs are unlabelled. `selectAudioOutput` absent, `setSinkId` present. No defect flagged headless |
| `settings_change` | every class: 1 join and 1 display before and after, same join id and display id, no device removed or added. Stream-class edits renegotiate in place (1 new peer connection, 2 `/whep` requests) with the picture back in 1.0-2.5 s. Audio, scale and name edits: 0 gap |
| `two_clients` | varied run to run. Once `bUntouched` was true (B's longest gap 76 ms before and during A's edits). Once B got no picture. On the pre-switch build (exclusive) B's gap rose to 635 ms and A's p50 to 221 ms after B joined |
| `leave_rejoin` | the rejoin within 2 s reused the same display id. After a final leave the display was gone within 1 s |
| `slow_answer` (n=3) | 0 flagged in that run. See "Observations about the app" |
| `auto_rejoin` | after a host restart the page rejoined by itself within 12 s. Meanwhile it showed "The host stopped sharing this screen" |
| `logs` | `logs --lines 50` returned 50 lines, and `status --json` returned telemetry while live. Neither is available on version 0.6.3 |
| `fallback_paths` | default p50 33.3 ms (path 1). No WebCodecs, no script transform, or neither: path 3 (`<video>`), p50 36.1-36.9 ms. Plain HTTP and hardware-decode-off hit the slow-answer defect in that run |
| `custom` | the page expression and the host commands returned |
| `loss` | **not run**. It needs admin rights or a change to the machine's network |

## Observations about the app

These concern the app, not the handler.

- The answer that never arrives. In roughly one join in five on this machine, the host logs `join accepted: WHEP answer generated` within 1-4 s. Yet the browser's network log shows no response to that `POST /whep` before the page's 10 s timeout ("The host did not answer in time"). The page then retries, the host opens a **second session and a second display**, and the client is demoted to `<video>`. This was seen on the isolated build, in single-client runs and in `two_clients`. It is unexplained. `joins[].offers > 1` and `retried` flag it in every result.
- Two or more virtual displays make streams unreliable. With 2-4 displays attached (two clients on one host, or several isolated hosts), measured streams stopped showing new stamps, showed frozen frames, or ran at about 100-220 ms p50. This was seen with the shared-driver mode and, in the two-client case, on the unmodified pre-switch build too. It is unexplained. The handler avoids it by default.
- Audio on version 0.6.3. After the override is applied (host restart plus rejoin) the host sends 4,172 audio packets, but the page cannot play them. The page logs `[audio] decoder worker error` and `AudioWorkletNode cannot be created`. Its worker scripts lack the COEP header that a cross-origin-isolated page needs. The pre-switch build fixes that.
- The stamp's painted fps (about 53) is below the page's own count (61-67). About 10 % of painted frames fail the stamp check: frames caught between two stamps, or blurred by the encoder. The figure is reported, and the method is the same for every build.

## Files

| file | role |
|---|---|
| `handler.mjs` | the entry point: request, slots, host, browser, scenarios, special requests, cleanup, the JSON |
| `acceptance.mjs` | `concurrent`, `noise` and `exclusive` runs of the handler, with the overlap check and the spread |
| `client-override.mjs` | serves local copies of client page files in place of the host's (see "Page overrides") |
| `lib/slots.mjs` | slot files, exclusive mode, the measurement lock, removal of stale files |
| `lib/host.mjs` | build probe, `serve` start, CLI calls, stop (verb or PID) |
| `lib/cdp.mjs` | Chrome launch and the debugging-protocol client, which refuses `Input.*` |
| `lib/client.mjs` | one browser: observer and paint hook injection, join timing, stamp and static collection, `stampMetrics` |
| `lib/clock.mjs` | the reference clock: node's QPC anchored to the precise system clock |
| `lib/paint-hook.mjs` | the `drawImage` and `requestVideoFrameCallback` readback that produces the primary metric |
| `lib/inject.mjs` | the observer page script (microphone, peer connections, audio analyser and onset detector), and the geometry and secondary-stats expressions |
| `lib/driver.mjs` | reads the virtual display driver's monitor list over its pipe, and removes monitors (exclusive-mode cleanup and reaping only) |
| `lib/load.mjs` | CPU and `nvidia-smi` sampling, the load gate, memory snapshots |
| `lib/metrics.mjs` | percentiles and spreads |
| `lib/png.mjs`, `lib/stamp.mjs` | PNG decoding and stamp decoding for the screencast reader (not the default) |
| `lib/rx-hook.mjs` | the worker hook and the analysis behind `receive_hold` |
| `lib/relay.mjs` | the UDP relay with impairment behind `impair` |
| `lib/remote.mjs` | a headless Chrome on another machine over SSH |
| `lib/joinlog.mjs` | the host log parsed into join stages |
| `lib/tslog.mjs` | runs the host with its output read by a process of its own (`host_log: 'relay'`) |
| `lib/proc.mjs` | host and driver process counters, and the slope fit for `soak` and `churn` |
| `lib/reap.mjs` | removes what a killed handler left: browsers, hosts, monitors, the display lock |
| `stage.ps1` | the stamp, the moving window and the noise area on one display (never focused, click-through), and the monitor sampler |
| `clicks.ps1` | the click train for the audio test, timed by winmm |
| `clicks-wasapi.ps1` | the same train, timed by the audio engine's play position (`click_source: 'wasapi'`) |
| `handles.ps1` | one process's handles by object type and its GPU memory per adapter, unelevated |
| `hangprobe.ps1` | which top-level windows answer `WM_NULL`, for `hang_probe` |
| `real-cursor.ps1` | the real-cursor sweep for `real_input` |

## Development: adding a request

1. Write an `async function` in `handler.mjs` that returns a plain object.
2. Call it from `main()` behind its key.
3. Add the key to `DEFAULTS`.

| helper | use it for |
|---|---|
| `underLock()` | anything timed, and anything that creates or removes a display |
| `startStage()` | on-screen content |
| `cl.collectStamps()` | the metric |
| `hostJoins()`, `ourMonitors()`, `serveLogCounts()` | host state |
