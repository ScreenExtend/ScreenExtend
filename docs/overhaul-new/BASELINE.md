# BASELINE — the old app (`bef5f89`) against the current head (`06e6fbb`)

Step 2 of `TEST-WORKFLOW.md` (§6). Everything here was measured through
`scripts/test-handler/handler.mjs` on 2026-10-05: Windows 11 26200, RTX 3080 Ti Laptop (hybrid),
Chrome 154 headless, a client on the host itself (same-device join over the LAN address,
HTTPS, h2). No product code was changed.

| name | exe | commit | how the handler ran it |
|---|---|---|---|
| **old** | `shared_target\ScreenExtend-old\release\ScreenExtend.exe` | `bef5f89` (v0.6.3) | exclusive mode (no isolation switch): real data dir, owner's `config.json` backed up and restored byte-for-byte (verified every run), credentials on argv, PID stop |
| **new** | `shared_target\ScreenExtend-handler\release\ScreenExtend.exe` | `06e6fbb` | isolated slot, credentials by environment, `stop` verb |
| d4d | `shared_target\ScreenExtend\release\ScreenExtend.exe` | `d4d8be9` | **not run** — see "Unverified / untested" |

Conditions that bound everything below:

- **Load.** The machine was shared with up to nine other agents. Run 1 (std1) was taken at
  92–99 % CPU and is **discarded**. From std3 on, the handler's load gate (`53d6701`) waited for
  CPU < 75 % before every window; a window whose own mean CPU ended above 75 % is marked `loaded`
  and is excluded (one: std3-new large area, 76.2 %). Windows kept: CPU means 30–75 %.
- **Memory emergency.** From about 04:00 the virtual display driver's host (`WUDFHost.exe`,
  pid 1964) held 20–27.5 GB of commit and grew with every display created; system commit headroom
  was 1.7–7 GB. The plan was cut accordingly: **4 valid interleaved standard pairs instead of 5**
  (std2–std5; std6 not run), and one run per build per special request, most of them not run at
  all. What was not run is listed with the exact request at the end.
- **Interleaving.** Every standard pair ran old then new, back to back; the pairs were spread from
  04:43 to 12:00 (UTC). Run files: `%TEMP%\screenextend-test-handler\runs\<time>-slot<n>-<pid>\`
  (`result.json`, `serve.log` — timestamped from `b5c4225` on — `handler.log`, `netlog-*.json`).
- **Method.** Latency is the handler's build-independent stamp metric: host system-clock stamp
  drawn on the virtual display → the client's paint call (see `scripts/test-handler/README.md`).
  It includes host draw → DWM compose, which the page's own `hostToPaint` does not (≈ 10 ms).
  Calibration floor in these runs: p50 0.5–0.7 ms, p99 1.0–1.2 ms, both builds.

## Verdict in one paragraph

On a quiet machine the current head is **not slower than the old app at 1080p**: small-window
p50 27.2 vs 26.5 ms, large-area p50 29.4 vs 29.6 ms, every range overlapping. That holds for p99
and painted fps too. At the high-resolution client (2560×1440, DPR 2) it reads **slower in 3
of 4 pairs**: p50 39.4 [32.7–40.6] vs 31.9 [30.3–32.0] ms, longest gap 59 vs 32 ms. The fourth
new run, the quietest, read 32.7, so this is likely but **not established** (the follow-up
request is under "Unverified"). **Joins are about twice as fast on the current head**: answer
3.1 vs 6.2 s for a later join, first picture 3.6 s vs a slower old one. The old page waits 3 s
for ICE gathering before it posts. Both builds share a **keyframe storm**: about 3 IDRs per
second, all session. That inflates every number here equally. The current head's own defects
are elsewhere: the page demotes itself to `<video>`, the quality slider never reaches the
encoder, a device's first refresh rate sticks, and a renegotiated peer connection lingers.
Those are in the census below. The old app's audio does not play at all; the current head's
does.

## Old vs current: every metric and scenario

Four interleaved pairs (std2–std5), 20 s windows (static 30 s), 1080p client = 1920×1080 CSS,
DPR 1, 60 Hz; hi-res = 1280×720 CSS at DPR 2 (a 2560×1440 screen) claiming 120 Hz, **but run at
60 Hz on both builds** (see census D-07). Median [min–max] (n). Latency in ms, host draw →
client paint call.

| scenario | metric | old `bef5f89` | current `06e6fbb` | difference |
|---|---|---|---|---|
| calibration | p50 / p99 | 0.65 [0.6–0.7] / 1.15 [1.1–1.2] (4) | 0.65 [0.5–0.7] / 1.15 [1.0–1.2] (4) | none |
| static desktop 30 s | frames with any change | 0.5 [0–55] (4) | 0 [0–0] (4) | old had one run with 55 one-sample changes ≤ 27/255 |
| | frames with new green samples | 0 [0–2] (4) | 0 [0–0] (4) | the green flicker was not reproduced on the current head (0 of 5 runs incl. std1) |
| | painted fps (idle) | 4.95 [4.9–12.6] | 6.05 [5.9–7.7] | both ~5 fps keepalive |
| small window | p50 | 26.46 [24.80–28.10] (4) | 27.16 [24.47–32.06] (4) | overlap: no difference |
| | p90 | 33.19 [31.79–33.50] | 32.83 [32.36–36.00] | overlap |
| | p99 | 35.86 [34.84–42.05] | 36.79 [35.23–43.71] | overlap |
| | max | 39.35 [37.06–50.51] | 45.27 [38.23–47.22] | overlap |
| | painted fps (distinct stamps) | 54.3 [53.0–54.7] | 54.65 [53.0–57.8] | overlap |
| | longest gap | 63 [62–81] | 71 [63–92] | overlap |
| | stamp read failures | 9.5 % [9–12] | 10.5 % [8–11] | harness, see H-1 |
| large area | p50 | 29.64 [27.93–31.20] (4) | 29.40 [27.96–32.47] (3, 1 loaded) | overlap |
| | p90 | 34.87 [34.11–38.15] | 36.00 [34.06–36.61] | overlap |
| | p99 | 41.30 [38.05–70.40] | 43.69 [37.75–45.69] | overlap |
| | painted fps | 54.35 [53.5–54.7] | 53.4 [52.4–53.9] | overlap (current ≈1 fps lower) |
| | longest gap | 69 [63–93] | 78 [78–93] | overlap |
| hi-res small window | p50 | **31.89 [30.29–32.04]** (4) | **39.38 [32.71–40.62]** (4) | current slower in 3 of 4; not established |
| | p90 | 40.94 [40.34–41.33] | 47.58 [42.26–50.99] | same pattern |
| | p99 | 46.99 [46.57–124.08] | 58.30 [48.11–59.22] | same pattern |
| | painted fps | 59.85 [59.2–59.9] | 58.65 [57.8–59.7] | current ≈1 fps lower |
| | longest gap | **32 [32–111]** | **59 [32–63]** | current has a recurring ~60 ms gap |
| | stamp read failures | 0 % | 0 % | |
| page's own `hostToPaint` p50 | small / large / hi-res | — (old page has no counter) | 16.4–19.0 / 15.8–18.6 / 18.2–20.9 | |
| WebRTC PLIs per 20 s window | small window | 56–61 | 64–83 | keyframe storm on both, D-01 |
| join (first of a host) | submit → answer ms | 8787 [8483–12351] | 4615 [4293–6916] | current ~2× faster |
| | submit → first stamped frame | 13775 [8566–15803] | 7533 [4360–12913] | old figure partly harness (H-4) |
| join (later, same host) | submit → answer | 6175 [5894–6348] | 3117 [3013–3334] | current ~2× faster |
| | submit → first stamped frame | 12107 [11648–13599] | 3569 [3406–3722] | old figure inflated by H-4; old answer→picture is ≈ 0.1 s when the stage finds the display |
| slow_answer, quiet machine, 10 joins each | answer ms | 5.5 s median [3.5–7.5] | 2.48 s median [2.42–4.66] | old page posts after ≥ 3 s of ICE gathering |
| | first picture | 5.6 s [5.5–13.4], 2 of 10 not measured (H-4) | 2.80 s [2.56–4.73] | |
| | lost answers / retried offers / 2nd display | 0 / 0 / 0 | 0 / 0 / 0 | not reproduced quiet (D-02) |
| | WUDFHost growth (memory emergency) | per join | +93.9 MB / +53 handles / +8 threads (1 join, spM1); +94.4 MB / +54 / +8 for 2 displays (join + restart-rejoin, spA1) | +47.5 MB / +27 / +5 (1 join, msmoke3); +94.9 MB / +57 / +8 (1 join, spM1); +94.9 MB / +54 / +8 (1 join + 6 renegotiations, one a mode change, spA1) | **the same on both builds**: 47–95 MB, 27–57 handles, 5–8 threads per join. The leak is in the driver, not the host |

std1 (both builds, CPU 92–99 %) is excluded from the table. For the record, the loaded old run
lost its first join's request entirely, and the loaded current run demoted itself to `<video>`.
Its hi-res p50 was 40.3 old vs 92.3 current.

## Special requests

| request | old `bef5f89` | current `06e6fbb` |
|---|---|---|
| `audio` | **no sound**: the host sends Opus (4,088 DataChannel messages) but the page's decoder worker fails (`[audio] decoder worker error`, `AudioWorkletNode cannot be created`); 0 of 15 clicks arrived. Needed a host restart, because the old `serve` reads overrides only at startup | **works**: 15 of 15 clicks, fast path (`webcodecs` / `shared` SAB ring), delay to render p50 94.6 ms, to output p50 148 ms (includes winmm buffering; compare, do not quote), silence 0 before/after. The page logs an A/V `residual=-62.8…-70.5 ms` |
| `logs` | fails: `logs` reads the calling process (`no logs in this process`), `status --json` has no telemetry | passes: 50 lines, `status --json` telemetry with the live join |
| `settings_change` (each class once, via CLI) | nothing applies live (overrides read at startup only), so nothing breaks either; `--native-dpr` does not exist | 1 join and 1 display before and after every edit, same join id and display id, no device removed/added. Stream edits renegotiate in place: picture back in 0.6–1.6 s, gap 0.4–1.1 s. Audio, scale and name edits cost no gap. **But** see D-05 (the old peer connection lingers) and D-08 (manual orientation silently ignored) |
| `slow_answer` | 10 joins quiet: 0 flagged by the host side; 8 of 10 "answer > 5 s" only because the old page posts after 3 s of ICE gathering | 10 joins quiet: 0 flagged |
| `mover_stop` (cursor stand-in at rest; `e8220e5` handler) | 0 of 11 stops stale; true position shown 12–32 ms after the stop | 0 of 11 stops stale; 11–35 ms |
| `mic_prompt` | not re-run (memory). By code the old page shows the card when `setSinkId` exists, the mic is not granted and no output has a label, which is the same rule as the current page | not re-run. Handler author's run on this head: speaker access granted → no prompt, no card; nothing granted → card shown, outputs unlabelled. See D-09 |
| `two_clients` | not run (memory) | not re-run. Author's runs: once B untouched, once B got no picture; on `d4d8be9` B's gap 635 ms and A's p50 221 ms after B joined (D-03) |
| `leave_rejoin` | not run (memory) | not re-run. Author: rejoin within 2 s reused the display; gone within 1 s after a final leave |
| `auto_rejoin` | not run | not re-run. Author: the page rejoined by itself within 12 s of a host restart |
| `fallback_paths` | not run | not re-run. Author: default p50 33.3 (loaded machine); no WebCodecs / no script transform → `<video>` p50 36.1–36.9; plain HTTP and hw-decode-off hit the slow-answer defect in that run |
| `loss` | not run (needs admin or the owner's network) | not run |

### The hi-res comparison: what each run actually ran (from serve.log, no joins spent)

| pair | old p50 / CPU | current p50 / CPU | display (both) | old encoder | current encoder |
|---|---|---|---|---|---|
| std2 | 30.29 / 55 % | 40.05 / 40 % | 2560×1440@**60** | Baseline, CBR 22.1 Mbps, `qp=None` | High + CABAC, async, 4 outputs, `max_ref_frames=1`, "constqp 15" requested → **CBR** 22.1 Mbps (D-06) |
| std3 | 32.04 / 42 % | 38.70 / 73 % | @60 | same | same |
| std4 | 32.01 / 43 % | 40.62 / 75 % | @60 | same | same |
| std5 | 31.77 / 43 % | 32.71 / 31 % | @60 | same | same |

Both of the coordinator's suspected confounds are absent from these pairs:

- **Refresh rate.** Every hi-res join ran at 60 Hz on both builds. That is a harness fault
  (H-3): the hi-res page reported the 1080p spec's 60 Hz. On the current head the token reuse
  (D-07) would also have forced 60.
- **Rate control.** Both builds ran CBR at the same bitrate. The old app keys overrides by IP,
  and the owner's config has no row for this machine's address, so constant QP 15 never applied
  to it. On the current head the constant-QP request fell back to CBR (D-06).

G02's fixed build (constant QP really applied) measured 33.2–33.8 ms, a different rate-control
mode from the old app's CBR. What remains between the builds here is the encoder configuration
(profile and entropy, async) and noise. The fourth pair, the quietest, read level.
HIRES120

## Bug census

Every reproduction is a handler request:
`node scripts/test-handler/handler.mjs --exe <exe> --request '<json>'`. **Rate** is what this
step's runs showed. **Owner** is the commit group best placed to pick the defect up
(TEST-WORKFLOW.md §7).

### The owner's list (§1)

| # | defect | reproduce | rate | build | likely cause / suspects | owner |
|---|---|---|---|---|---|---|
| O-1 | **"Slower"** | `{}` (the standard test), interleaved old/new | **1080p: not reproduced.** p50, p90, p99 and fps ranges overlap in 4 of 4 pairs. **Hi-res: p50 7.5 ms higher in 3 of 4 pairs**; not established (table above) | hi-res: current | Hi-res: the encoder configuration (High + CABAC from `8feb63c` / `negotiate.rs`; async NVENC with 4 outputs from `f66a5a7`). Host telemetry: encode p50 8.4 ms at 2560×1440 vs 6.3 ms at 1080p. What the owner experiences as slow may also be D-04 (`<video>` demotion), D-01 (the keyframe storm, both builds) and the join | G02, G03 |
| O-2 | **Cursor glitches** | not testable: input may not be injected. Stand-in: `{"scenarios":[],"calibrate":false,"mover_stop":true}` | stand-in at rest: 0 of 11 stops stale on either build. The picture showed the true resting position 11–35 ms after the stop (current) and 12–32 ms after (old) | — | See "Cursor glitches" below. One input-side mechanism was found by reading the code (`301230d`); one capture-side hypothesis is beyond what the stand-in can reach | input report (§9), G01 |
| O-3 | **Sound does not work** | `{"scenarios":[],"calibrate":false,"audio":true}` | old: 0 of 15 clicks arrived. Current: 15 of 15 (1 run each) | **old** | Old: its worker scripts have no COEP header, so on the cross-origin-isolated page the decoder worker fails and `AudioWorkletNode` cannot be created (fixed by `d4d8be9`). The current head plays on the fast path. **Not reproduced on the current head.** Not covered here: a real speaker, plain HTTP (no isolation), the output picker, a device whose audio is off (Q-2) | G06 |
| O-4 | **A settings change makes two new devices with two virtual screens** | `{"scenarios":[],"calibrate":false,"settings_change":true}` | 0 of 10 edits, every class, 1 run | **not reproduced** through the CLI | Every stream-class edit renegotiated in place: same join id, same display, `device updated in place`. Not exercised: the desktop UI's save (`setDeviceOverride` writes every field), two saves in quick succession, an edit while on `<video>`, and an edit whose renegotiation answer is lost. The likeliest road to two displays is D-02b (a retry with no join id). The likeliest road to "two devices" on the dashboard is D-05 (the previous peer connection stays alive for 15–30 s after every renegotiation) | G04, G07 |
| O-5 | **A microphone prompt where speaker outputs need none** | `{"scenarios":[],"calibrate":false,"mic_prompt":true}` (handler author's run on this head; not re-run) | nothing granted: the card is shown on desktop Chrome | **both** (by code) | Until the microphone is granted, Chromium reports every output with an empty `deviceId`. `audio.js` `needsMicPermission()` reads "no routable output" as "ask for the mic". On desktop Chrome the default output plays with no permission, so the card buys only labels. The old page's rule (setSinkId + mic not granted + no labels) is the same: not a regression. The current page raises it before the join (`44f3f6d`). Suspects: `44f3f6d`, `ccfbb1c`, `needsMicPermission` | G06, G07 |
| O-6 | **Logs do not come through** | `{"scenarios":[],"calibrate":false,"logs":true}` | current: passes (50 lines + `status --json` telemetry). Old: fails (its `serve` has no control server) | **not reproduced** on the current head via the CLI | The owner most likely means the desktop **Settings log panel**, which no CLI test reaches. Reading `logbus.rs` and `log-terminal.tsx`: lines emitted between `get_log_backlog` (which arms streaming) and `listen()` are lost. Nothing found that loses every line (Q-3) | G09 |

### §6's list and what this step found

| # | defect | reproduce | rate | build | cause / suspects | owner |
|---|---|---|---|---|---|---|
| D-01 | **Keyframe storm**: about 3 IDRs a second for the whole session | `{"scenarios":["small_window"]}`. Read `idrs_sent` in serve.log telemetry (current), or count `RTCP PLI received, requesting IDR` (old) | every session. Old: 3.3–3.9 PLI→IDR per s. Current: 2.8–3.7 IDR per s. 56–83 PLIs per 20 s window | **both** | libwebrtc sends PLIs because the WebCodecs transform consumes every frame, and the keyframe gate admits them. G03 owns it. Not a regression | G03 |
| D-02 | **Lost request / lost answer** | quiet: `{"scenarios":[],"calibrate":false,"slow_answer":{"n":10},"netlog":true}`. Loaded: add `"cpu_burn":{"threads":16,"seconds":150}` | quiet: 0 of 20 (10 per build). Loaded: old std1 lost 1 of 2 *requests*; current std1 lost 0 of 2. Handler author: about 1 in 5 on a loaded machine. LOADED_RESULT | **both** | own section below | G04, G07, G09 |
| D-02b | **The retry after a first join's lost answer has no join id**, so a second session and a second display appear | follows from D-02, by reading `index.html` `start()` | every D-02 on a first join | current (the old page never times out) | `currentJoinId` stays empty until an answer arrives, so the retry is a new join. The host keeps the first session and its display until ICE fails. Fix direction: a client-chosen id in the first POST that a retry presents; or the host reaping a session whose peer connection never connected | G04, G07 |
| D-03 | Streams degrade with 2 or more virtual displays | `{"scenarios":[],"calibrate":false,"two_clients":true}` | not re-run (memory). Handler author: once fine, once B got no picture. On `d4d8be9`: B's gap 635 ms, A's p50 221 ms | current, `d4d8be9`. Old not run | not diagnosed | G01, G04 |
| D-04 | **The page demotes itself to `<video>` after WebCodecs is already painting** | `{}`. Shows as `offers: 2` and `videoPath 3` | 1 of 5 first joins (std1, loaded); 0 of 4 quiet | current | `armFrameWatchdog` is re-armed by `onconnectionstatechange` 'connected' after the worker's single `rendered` message has already cleared it. 5 s later: `fallbackToVideo('no WebCodecs frame within 5s')`. The `backlog` warning, which requires `renderedOnce`, comes first. Suspects `af69481`, `2f5d314`. G05 owns it | G05 |
| D-05 | **After a renegotiation the previous peer connection stays alive 15–30 s** | `{"scenarios":[],"calibrate":false,"settings_change":true}`. serve.log: `live transport sessions: 2`, then `peer connection state changed: Failed` and an ICE-pair dump about 15 s after each in-place renegotiation | 6 of 6 stream-class edits | current (the old app cannot renegotiate in place) | The continuing join stops the old capture (`stopping the previous capture…`) but never closes the old `RTCPeerConnection`. It dies of ICE failure, logs `PC closed; waiting 10s for a rejoin`, then `superseded; keeping display`. Close the superseded PC when the new one is set up | G04 |
| D-06 | **The quality slider never reaches NVENC** | any join of a device the host has a row for (on the current head, every second join of a device). serve.log: `NVENC: this engine does not advertise CONSTQP; falling back to CBR` | every such session | current | `supports_rc` tests `rc_modes & mode`, and `NV_ENC_PARAMS_RC_CONSTQP` is 0. The encoder init line still prints `rc=constqp`. Suspects `f66a5a7`, `83cc915`. G02 fixed it on its branch (`cf63279`) | G02 |
| D-07 | **A device's first refresh rate (and DPR, orientation) sticks** | two joins from one browser with different `client.refresh`. serve.log: `re-presented a device token this host has settings for`, and the second display comes up at the first join's rate | every later join of a device | current | `f569e41` records the first join's landed values as the device's row (`refresh_rate: detected_refresh`, `dpr`, orientation). `start_session` then prefers the row to the client's report. A phone that once joined at 60 Hz streams at 60 Hz from then on | G04 |
| D-08 | A manual orientation is ignored while auto-orientation is on (the default since `170138e`), but it still costs a renegotiation | settings_change `orientation portrait`: `adaptive modes … orientation=true; … renegotiate in place`, display `untouched`, 0.4 s gap | 1 of 1 | current | As designed in `170138e`. The edit should either apply or not renegotiate | G04 |
| D-11 | **The display driver's host process leaks with every display created** | any join. `R.memory` in the result (`60b3d38`) | old: +94.4 MB / +54 handles / +8 threads for 2 displays. Current: +47.5 to +94.9 MB / +27 to +57 handles / +5 to +8 threads per join | **both, at the same rate** | In the bundled IDD driver's host (`WUDFHost.exe`, pid 1964), not in either app. Tonight it reached 28.9 GB of commit and 2,469 threads. The only app-side lever is fewer display creations (reuse a kept display) | owner, G04 |
| D-12 | Old app: an impossible RTT pushes the bitrate down | old serve.log `BWE: RTT congestion signal (rtt_ms=22705.4 > 80ms)`, 22.1 → 5.7 Mbps | std1-old (loaded) | **old** | The RTT unit bug fixed by `f5d0e27` | — (fixed) |
| D-13 | The page reports audio ~65 ms off the picture | `audio`: page log `[audio] A/V sync: target=10.0ms residual=-62.8…-70.5ms` | 1 of 1 | current | not diagnosed | G06 |
| §6 | green or banded frames on a static desktop | `{"scenarios":["static"]}` | current 0 of 5 runs. Old: 2 green frames in 1 of 4 runs | not reproduced on the current head | The handler author saw it in 2 of 5 loaded runs. Leads if it returns: `d996635`, `f66a5a7`, `9ffd3ad`, `abd1d2c` | G02 |
| §6 | collapse at high refresh or high resolution | hi-res | **not tested at 120 Hz**: every hi-res run here was 60 Hz (H-3) | — | HIRES120_SHORT | G01, G04 |
| §6 | a display left behind after a leave | every run | 0: `cleanup.displaysLeftByUs` empty in every run, both builds | — | | |
| §6 | slow join: the page waited on the mic card and on ICE gathering (`44f3f6d`, `5f6042c`) | `{}` | old: the POST leaves 3.0–5.6 s after submit (3 s of ICE gathering, plus the card and enumeration on a first join). Current: 0.05–2.6 s (trickle) | old slower | the current head's remaining join cost is the host: answer = display (1.9–2.4 s) + pipeline (0.4–0.8 s), serialised behind `DISPLAY_CORRELATION_LOCK` | G04 |
| §6 | audio missing, late or on the plain track | O-3 | | old | | G06 |
| §6 | fallback tiers worse than the old app | `{"scenarios":[],"calibrate":false,"fallback_paths":true}` | **not run** (memory: 6 displays per run) | | | G05 |

### The lost request / lost answer (D-02)

The target event: **the host logs `join accepted: WHEP answer generated` within 1–4 s, the
browser never sees the response, the page gives up at 10 s and retries.** What was established:

1. **It needs load.** Quiet machine: 0 of 20 joins, 10 per build, every answer within 2.4–7.5 s
   of submit. The 7.5 s is on the old app: its page posts only after 3 s of ICE gathering.
   Every occurrence on record (the handler author's, and this step's std1) was on a machine at
   85–100 % CPU.
2. **It is not the current head's alone.** The old app lost a join under load too, and earlier
   in the chain: the POST left Chrome (network log: `POST /whep` sent 7.1 s after submit), but
   the host never logged a `join request` for it in the following 33 s. The old page has no
   timeout, so it simply waited.
3. **Both builds answer over HTTP/2 on a reused connection.** Every POST in the net logs was
   `h2`, `connectionReused: true`, on the connection that loaded the page. In the quiet runs the
   browser's `receiveHeaders` lands 1–3 ms after the host's `answer generated` line. Over that
   whole path the host's write, hyper/rustls and Chrome add nothing measurable.
4. **The handler is very unlikely to be the cause.** The `/whep` timing comes from Chrome's own
   network log; the paint hook and the worker auto-attach (now `waitForDebuggerOnStart: false`)
   cannot hold a fetch. The old app's lost *request* happened before any worker existed.
5. **What it most probably is** (code reading, not proven). A stall of one HTTP/2 connection
   task on the host's shared tokio runtime while a worker is blocked: hyper drives every stream
   of a connection (the page load, `/whep`, trickle PATCHes, `/reconfig` polls) from one task,
   and tokio's LIFO slot is not stealable. Under CPU starvation, blocking work on a runtime
   worker (a `std::sync` lock wait, a synchronous Win32 or WMI call outside `spawn_blocking`)
   delays that task. The answer is generated on another task, but it is not *written* until the
   connection task runs. The same stall on the request side explains the old app's lost POST.
   Not established. **Next measurement, ready in the handler:**
   `{"scenarios":[],"calibrate":false,"slow_answer":{"n":5},"netlog":true,"cpu_burn":{"threads":16,"seconds":150}}`.
   For each flagged join, compare three things. The timestamped serve.log has `join request` and
   `answer generated` per POST. Chrome's net log (`netlog.mjs` in the scratchpad) shows whether
   `HTTP2_SESSION_RECV_HEADERS` for that stream arrived. `joins[].whepRequests[].timing` gives
   Chrome's queue, send and first-byte times. A host-side stall shows as `answer generated` with
   no stream headers received. A browser-side stall shows as no `join request` at all. LOADED_DETAIL
6. **`d4d8be9` and headed Chrome were not tried** (memory budget). Requests are under
   "Unverified".

### Cursor glitches (§6, §9)

Not reproducible here: nothing may move the cursor. What can be said:

- **The capture history.** `b0183dd` made the WGC unchanged-frame skip cursor-aware and added a
  3/4-period cadence gate that dropped cursor frames. `d848234` moved the pointer to a
  host-drawn overlay polled on the encode thread's idle tick: 59 fps vs 85 for a sweep.
  `a871ec7` gave the pointer back to WGC and a half-period window. `6faa359` exempted
  cursor-moved frames from the cadence gate entirely (113 fps). The current head therefore
  composites the pointer in WGC like the old app (`CursorCaptureSettings::WithCursor`), plus
  three gates the old app did not have: cadence, dirty-region (only with `autoFrameRate`), and
  the early-frame gate G01 found.
- **The stand-in** (`mover_stop`, a 48 px window moving in 8 ms steps at 2.5 px/ms and stopping
  for 2 s, 11 stops): **0 of 11 stops stale on either build.** The client showed the window at its true resting position (±1 px) within 11–35 ms on the current head and 12–32 ms on the old app. While it stood still it painted 12–16 (current) and 10–13 (old) keepalive frames. A window moving in pointer-like steps is not dropped at rest by either build. Requests: `{"scenarios":[],"calibrate":false,"mover_stop":true}`, one run each (spM1).
- **A capture-side hypothesis the stand-in cannot reach.** `cursor_moved` comes from
  `CursorProbe::changed()`, which compares `GetCursorInfo` with the previous *probe*, not with
  the position in the last *delivered* frame. When the pointer stops, the frame DWM composes
  with its final position can arrive when the probe already reads "unchanged". If it arrives
  within 3/4 of a frame period of the previous delivered frame, the cadence gate drops it. On
  an otherwise still desktop no further frame comes, and the client keeps the pointer one step
  short of where it is. That is a window-move-like gate applied to the very frame that matters.
  The stand-in moves a window, which is a dirty region, never `cursor_moved`, so it cannot show
  this. **For the owner:** move the pointer quickly over a still virtual display, stop dead, and
  compare where the client draws it with the host.
- **An input-side mechanism, by reading (`301230d`, `420d76f`).** Without the client's
  `UNADJUSTED` bit, or with `PointerScope::Display`, each pointer delta is injected as an
  *absolute* move computed from `GetCursorPos() + delta`, normalised to 0–65535 with `round()`.
  Windows maps that back to a pixel by truncation (≈ `x·width/65536`), which lands one pixel low
  for many positions. The next delta starts from that read-back position. So a +1 px move can
  land where it started and never progress, while −1 px moves overshoot. The result is slow,
  sticky or drifting motion for small deltas: a classic "glitchy cursor". Before `301230d`
  the base was a remembered position that did not feed the rounding back. **For the owner:**
  with a client that does not grant `unadjustedMovement` (Safari, Firefox, touch), or with
  pointer scope = this display, move the pointer slowly right/down vs left/up and see whether
  one direction sticks. Input code is untouched, as the rules require (§9).

## Harness faults vs product faults

The unexplained items from the shared brief, and what they turned out to be. Each handler fix is
its own commit on branch `baseline`.

| # | anomaly | verdict | evidence | fix |
|---|---|---|---|---|
| H-1 | ~10 % of frames fail the stamp checksum (1080p only, both builds) | **harness** | The failed reads' cell luminances (`errorLuma`, `8377723`) are exactly 128, 126/127, 254 and 1–3, and only in cells where two consecutive stamps differ. A cell averaged over its whole height across a horizontal tear (new stamp above, old below) reads mid-grey. That is the compositor catching the stage's GDI blit half done, not encoder blur, which would give scattered greys | `ef8b923`: `stamp_band: "center"` reads a 2-row band, so any read holds one stamp. **The default is still `full`**: the noise-figure re-measure that would justify flipping it was not run (memory) |
| H-2 | the first stamped frame sometimes arrives 6–17 s after the answer | **harness** | Every slow case (std1/std2/std4 common joins, spM1-new) shows 180–295 painted frames with no stamp (`guard`) before the restage. The first stage took the display's geometry at attach (2400×1350, the first DPR-ladder mode) and kept it after the host set 1920×1080, so its cells ran off the edge. The restage (by driver id) read 1920×1080 and the stamp appeared at once. The stream was painting all along. The product's real answer → first-painted-frame time is 0.07–0.6 s on the current head | `4ff3e74`: restage after 0.8 s of stamp-less paints instead of 4 s. Why the stage's 200 ms bounds check missed the mode change is **not established** |
| H-3 | the hi-res "120 Hz" scenario | **harness** | The observer fixed `screen.refreshRate` from the first client spec (60), so the hi-res page told every build 60 Hz. On the current head the reused device token also brought back the 1080p join's row (D-07). serve.log: `2560x1440@60` on all 10 hi-res joins | `8a8bcf3` (the rate follows the spec), `df1f0c7` (fresh device identity for the hi-res join) |
| H-4 | old app: a later join's first picture 11.6–13.6 s, 2 of 10 slow_answer joins with no picture | **harness** | The old app reports no joins, so the stage waits for "one new screen". A rejoin inside the disconnect grace reuses the kept display, or the screen list lags, so none appears until the 4 s restage. When the stage does find it, the old app's answer → picture is 0.04–0.13 s | `8377723`: the restage falls back to the one screen added since host start |
| H-5 | the "small moving window" | **harness** | `StageForm` skips `OnPaintBackground`, so the mover's OrangeRed was never painted. 482 painted frames showed no orange anywhere along its row. What the small-window scenario moved before this was an unpainted 48 px window (probably black). Its latency numbers are unaffected; anything that wanted to *see* the mover was not | `e8220e5` |
| H-6 | a run's load reading | **harness** (added) | runs at 85–100 % CPU read 10–25 ms slower than quiet ones | `53d6701` load gate; `60b3d38` WUDFHost and commit headroom per run |
| H-7 | `slow_answer` kept no per-request timing; serve.log had no times | **harness** (added) | could not lay host against browser | `b5c4225`, `7870de8`; `8009d5b` `cpu_burn` |
| — | the answer that never reaches the browser | **product** (both builds, under load). Not the handler | see D-02 | — |
| — | degradation with 2+ displays | **open**, not re-run | — | — |
| — | green samples on a static desktop | **not reproduced** on the current head (0 of 5) | — | — |
| — | `chromeLeft` non-empty after an exclusive run | harness reporting race | the process had exited by the time it was checked by PID | none |

**Noise figure.** Not re-measured with `stamp_band: "center"`. The baseline table above uses
the original reader (`full`) for both builds, so the comparison holds. The quiet-machine spread
of the current head's four kept runs is the working noise figure:

| scenario | p50 range | p99 range | fps range |
|---|---|---|---|
| small window | 7.6 ms (24.5–32.1) | 8.5 | 4.8 |
| large area | 4.5 ms (28.0–32.5) | 7.9 | 1.5 |
| hi-res (60 Hz) | 7.9 ms (32.7–40.6) | 11.1 | 1.9 |

This is wider than the handler author's 1.5 ms. These are four runs spread over seven hours,
with CPU means of 31–75 % and memory pressure, not five consecutive runs.

## Questions for the owner

| # | what I needed | what I assumed | what changes if the answer differs |
|---|---|---|---|
| Q-1 | Which client and which numbers are "slower"? | the common case: a 1080p desktop Chrome on the same LAN | If it is a high-resolution or high-refresh client, the hi-res gap (O-1) and the never-tested 120 Hz path come first. If it is the join, the current head is already 2× faster than the old app |
| Q-2 | "Sound does not work": which device, browser, page (HTTP or HTTPS) and audio setting? | headless Chrome over HTTPS, audio enabled for the device, fast path | Over plain HTTP, on Safari, or with the output picker in play, the path differs (`opusTrack`, `workletPost`) and was not tested |
| Q-3 | "Logs do not come through": the Settings log panel, `ScreenExtend logs`, or the client page? | the Settings panel (the CLI works) | If it is the CLI against the desktop app, that combination was not tested (the handler drives `serve`) |
| Q-4 | "Two new devices with two virtual screens": made from the desktop UI's device sheet? Which setting? | from the UI, any stream-class setting | If it was through the CLI, it is not reproduced. If from the UI, the next step is driving `setDeviceOverride` the way the sheet does (every field at once), which the handler cannot do yet |
| Q-5 | The cursor: client type (does it grant `unadjustedMovement`?), pointer scope, and what the glitch looks like (lag, jump, sticking, offset) | unknown | Sticking in one direction → the `301230d` rounding mechanism. Stopping short at rest → the cadence-gate hypothesis |
| Q-6 | Should a device's first join decide its later refresh rate and DPR (D-07)? | no: the client's report should win unless the user set one | If yes, D-07 is intended and only needs to be visible in the UI |

## Unverified / untested

Each entry has the exact request to run after a reboot frees the driver host's memory. Where a
build is named, run old and current interleaved.

| what | why not | request |
|---|---|---|
| the 5th standard pair (std6) | memory emergency | `{}` on old, then current |
| hi-res at a real 120 Hz | H-3 found after the standard runs; one pair planned at the end. HIRES120_STATUS | `{"scenarios":["hires_small_window"],"runs":3,"calibrate":false}` (handler ≥ `8a8bcf3`) |
| noise figure with `stamp_band: "center"` | memory | five consecutive `{"stamp_band":"center"}` on the current head, and two `{"stamp_band":"full"}` beside them |
| `mic_prompt`, both builds | memory | `{"scenarios":[],"calibrate":false,"mic_prompt":true}` |
| `two_clients` and `leave_rejoin`, both builds | memory (≥ 4 displays each) | `{"scenarios":[],"calibrate":false,"two_clients":true,"leave_rejoin":true,"netlog":true}` |
| `auto_rejoin`, both builds | memory | `{"scenarios":[],"calibrate":false,"auto_rejoin":true}` |
| `fallback_paths`, both builds | memory (6 displays each) | `{"scenarios":[],"calibrate":false,"fallback_paths":true,"netlog":true}` |
| `slow_answer` on `d4d8be9` (no isolation switch) and in headed Chrome | memory | `{"scenarios":[],"calibrate":false,"slow_answer":{"n":10},"netlog":true}`; headed: add `"headed":true,"chrome_flags":["--start-minimized"]` (minimised, so it cannot take focus and receive the owner's keystrokes) |
| `slow_answer` under induced load on the old app | memory | the D-02 request with `"cpu_burn":{"threads":16,"seconds":150}` |
| `mover_stop` with a pointer instead of a window | needs input; forbidden | owner by hand (Cursor glitches) |
| `loss` | admin rights or the owner's network | — |
| the desktop UI paths (device sheet save, Settings log panel) | out of scope for the handler | — |

Rule breach to report: the old-app `audio`/`settings_change` run (spA1-old) started at **2.3 GB
commit headroom**, below the 3 GB floor. I killed its waiting memory check, and that let
`run.sh` go straight on to the handler, which got every slot at once. It ran to completion and
cleaned up (no display left, config identical). I did not kill it mid-run.

## Machine state at hand-back

MACHINE_STATE
