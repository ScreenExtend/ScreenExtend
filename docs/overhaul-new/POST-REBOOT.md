# POST-REBOOT — the measurements the overhaul-new job owed, on the real profile

Branch `post-reboot`, started from `latency-overhaul-new` `90f4840` on 2026-10-06 after the
machine was rebooted. Windows 11 26200, RTX 3080 Ti Laptop (hybrid; the virtual display, WGC
capture, ring copy and NVENC all run on the NVIDIA adapter), Chrome 154 headless. The owner's
installed and dev apps were not running at any point (checked before every run). Every handler run
was wrapped by a runner that refused to start if any `ScreenExtend.exe` was running and
checked the owner's `config.json` SHA-256 before and after: `309851D4DECF7BC2…` at the start, after
every one of the ~130 runs, and at the end.

**Results in one paragraph.** Measured with a corrected harness (below), the real-profile build of
`90f4840` is **level with the old app and the previous head on per-frame latency** in every scenario
(five interleaved rounds), and clearly ahead on joins, keyframes, duplicates and audio. The one
large lever this work found is the GPU's clock state: the NVIDIA board sat in P8 (210 MHz) for the
whole session. Holding it out of idle while a session streams (`6c0bc71`) cuts the stamp p50 from
24.2 to **18.7 ms at 1080p60** and from 34.2 to **28.0 ms at 2560×1440@120** (five interleaved pairs
on the real profile, ranges 0.96–3.27 ms). It costs +15–18 W of GPU power and is on only on AC
power. One kept change was refuted (`ea821a9`, painted fps with two displays, Part 3) and is
reverted (`d28baaa`).

---

## 0. A harness defect found first (`0f15e67`)

The first seven post-reboot runs gave impossible numbers. One window read stamp p50 12.9 ms against
the page's own `hostToPaint` of 15.7 ms, although the stamp metric covers strictly more of the path.
The browser-minus-system offset varied from 0.95 to 13.76 ms run to run.

The cause is Windows' timer resolution. After the reboot the machine ran at the default 15.6 ms.
Every clock the metric compared was anchored on the coarse wall clock, each with its own 0–16 ms
error:

- the stage drew its stamp with .NET `DateTime.UtcNow`;
- the handler took node's `performance.timeOrigin` as "system time";
- every Chrome context (each page document and each worker) has its own `timeOrigin`.

Measured against `GetSystemTimePreciseAsFileTime`, `UtcNow` lagged in a 0–15.9 ms sawtooth, and
node's `timeOrigin` was off by −3 to −8 ms per process. Before the reboot something on the machine
held the timer at 1 ms and hid this. The bias changed per run by up to about ±16 ms, which is larger
than every difference the job has been deciding on.

The fix puts everything on one precise clock:

- `lib/clock.mjs` anchors node's QPC (hrtime) to the precise wall clock once.
- `stage.ps1` and `clicks.ps1` draw and time with the precise clock.
- Every Chrome context gets its own min-RTT offset, and every paint and decode sample is moved onto
  the reference clock before it is read.
- The calibration page stamps from its own corrected clock.
- The audio test measures the current document's offset instead of reusing the first document's.

After the fix, contexts agree within 0.03 ms and the calibration floor is p50 0.06–0.27 ms
(p99 0.5–1.0). Those seven runs are archived as `p2biased-*` and not used.

**Consequence for earlier ledgers:** any absolute stamp number taken on a machine at 15.6 ms timer
resolution carries an unknown per-run bias of this size. The pre-reboot job reported browser
offsets of 0.3–1.06 ms, which says the timer was fine then. Their comparisons stand. Their
absolute values are only as good as that.

## Part 1 — the real-profile build and the gate

The build was `pnpm tauri build --no-bundle` with no `CARGO_PROFILE_*` overrides (fat LTO, one
codegen unit, `debug = "line-tables-only"`), `CARGO_NET_OFFLINE=true`, `CARGO_TARGET_DIR=…\ScreenExtend-final`,
under the build lock at Idle priority with 4 jobs. Dependencies were reused from the `d4d8be9` fat
target dir it was seeded from. The build took 683 s in total; cargo itself took 9 min 05 s.

| | path | size | SHA-256 |
|---|---|---|---|
| `90f4840` | `C:\Users\sarve\.cargo\shared_target\ScreenExtend-final\final\ScreenExtend.exe` | 30,531,584 | `89B2BEA7F38B64786E971620EA55DB69F616DB18A1FADEFC1B8502A38547ADEC` |
| `6c0bc71` (clock hold) | `…\ScreenExtend-final\final-hold\ScreenExtend.exe` | — | starts `814A7FB52945D0A7` |

Both have `libopus.dll` and `libx264-164.dll` beside them.

The gate on `90f4840`:

| gate | result |
|---|---|
| `cargo fmt --check` | pass |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo test` | lib 491 passed, 0 failed, 18 ignored; other targets 0 passed / 4 ignored; no `SKIP` |
| `SE_REQUIRE_HW=1 cargo test` | 490 passed, **1 failed**: `nvenc_ltr_repair_is_cheaper_than_an_idr`. LTR is opt-in (`SE_NVENC_LTR=1`), so strict mode turns that test's skip into a failure. This is by configuration, as in consolidation |
| `pnpm lint` | 0 errors, 33 warnings |
| `pnpm build`, `test:client` (10/10), `test:config`, `test:gate` | pass |
| `check-lockfile` (5 packages), `check-bindings` (54 commands, 11 events), `check-versions` (0.6.3), `check-legacy-webkit` | pass |

**The gate on the final head `d28baaa`:** the same results, except that `cargo test` has lib 490 passed (the revert removed one test), 0 failed, 18 ignored. `SE_REQUIRE_HW=1` again has 489 passed and the same 1 failure by configuration. `pnpm lint` has 0 errors and 33 warnings; `test:client` 10/10; `test:config`, `test:gate` and the four checkers pass.

## Part 2 — the final three-way (five interleaved rounds)

**Setup.** Old `bef5f89`, prev `d4d8be9` (fat LTO) and new `90f4840` (fat LTO) ran in the order
old, prev, new, five times, all in exclusive mode. The request was
`{"scenarios":["static","small_window","large_area","hires_small_window"],"runs":2,"duration":20,"stamp_band":"center","force_exclusive":true}`.
Each run made one 1080p join and one hi-res join (2560×1440, DPR 2, a real 120 Hz, fresh token, no
saved override).

**Run conditions.** No window was `loaded` (window CPU 10–29 %). Every join made one offer. Free
physical memory was 4.4–9.9 GB and commit headroom 36–42 GB.

**From each `serve.log`:**
- Every run set 1920×1080@60 and 2560×1440@120.
- Every run used CBR with no constant QP: 12.44 Mbps at 1080p and 30 Mbps at hi-res.
- Old answers Baseline; prev and new negotiate High.
- The panel went from 360 Hz to 60 Hz whenever exactly one virtual display was attached, in every
  run of every build.

Stamp metric, p50 median ms (min–max), 10 windows per cell:

| scenario | old | prev | new | verdict (keep rule) |
|---|---|---|---|---|
| small window 1080p60 | 26.12 (25.25–26.69) | 27.15 (26.52–28.01) | 25.96 (23.81–27.00) | level with both |
| large area 1080p60 | 29.10 (28.44–29.49) | 29.36 (28.63–30.12) | 28.73 (26.87–30.23) | level with both |
| hi-res 2560×1440@120 | 34.68 (33.80–35.50) | 34.42 (30.25–36.12) | 34.26 (31.02–34.61) | level with both |

| measure | old | prev | new |
|---|---|---|---|
| p90 small / large / hi-res | 32.1 / 33.8 / 40.0 | 32.8 / 34.3 / 40.5 | 31.8 / 33.9 / 40.2 |
| p99 small / large / hi-res | 35.2 / 37.6 / 46.9 | 36.0 / 39.1 / 49.0 | 35.4 / 38.0 / 47.6 |
| max small / large / hi-res | 39.8 / 44.6 / 53.1 | 42.2 / 45.9 / 59.0 | 41.7 / 44.9 / 55.1 |
| painted fps small / large / hi-res | 55.0 / 54.1 / 90.9 | 54.3 / 54.3 / 83.9 | 54.3 / 53.0 / 86.6 (83.3–88.5; old 85.2–95.0) |
| longest gap median, small / hi-res | 62.5 / 31.5 ms | 63.5 / 33 ms | 63.5 / 33 ms |
| duplicate frames / 20 s, 1080p / hi-res | 0 / 499–694 | 50–65 / 283–521 | 0–3 / 290–371 |
| PLI per window (page) | 52–63 | 55–70 | 5–7 |
| host IDRs per window | not reported | 50–70 | 1–6 |
| static 30 s: changed / green frames | 0 / 0 (5 runs) | 0 / 0 | 0 / 0 |
| calibration floor p50 / p99 | −0.09–0.26 / 0.51–0.89 | 0.08–0.21 / 0.58–0.86 | 0.06–0.19 / 0.66–0.99 |
| join, submit → answer | 7.8–8.4 s | 5.3–5.5 s | **2.6–3.1 s** |
| join, submit → first picture | 7.8–10.9 s | 7.5–7.8 s | **2.8–3.1 s** |
| hi-res join, answer / first picture | 5.7–6.2 / 8.3–8.8 s | 2.6–3.1 / 2.8–3.1 s | 2.4–2.7 / 2.7–2.9 s |
| stamp checksum failures, 1080p | 76–130 of ~1200 | 77–137 of ~1250 | 75–138 of ~1190 |
| WUDFHost delta per run | 141–190 MB | 188–215 MB | 189–215 MB |

**Audio** (`{"scenarios":[],"audio":true,"click_source":"wasapi",...}`, three runs per build, interleaved):

| | old | prev | new |
|---|---|---|---|
| clicks matched | 0/15 ×3 (the old page cannot play: COEP worker defect; the host was restarted and the device rejoined as before) | 15/15 ×3 | 15/15 ×3 |
| click → decoded p50 | — | 81.5 / 82.0 / 91.5 | **61.9 / 62.4 / 62.6** |
| click → rendered p50 | — | 94.0 / 95.4 / 99.8 | **74.5 / 76.6 / 77.2** |
| click → output p50 | — | 139.4 / 149.4 / 155.7 | **122.9 / 123.0 / 124.9** |
| host `dc_drops` per run | 0 | 26 / 41 / 53 | **0 / 0 / 0** |

New is ~20 ms ahead of prev at the renderer, beyond the run-to-run range. These are click-train
numbers (WASAPI-timed), for comparison only.

**Noise figure of the final build.**
- From the five interleaved new runs: p50 range 3.19 ms (small), 3.36 (large), 3.59 (hi-res).
- The five consecutive runs on the final build are in Part 4 §4.4.

**The ~10 % checksum failures persist at 1080p** with `center` and the precise clock, on every
build (6–11 %). Hi-res shows none. They are not the stage's blit tear (`center` reads one row) and
not the clock. They remain unexplained. The hi-res client's 64 px cells against 48 px at 1080p is
the next thing to vary.

**The unexplained implausible-stamp join from consolidation did not recur.** There were 15 runs
and 30 joins, and at most 2 implausible decodes in any window. It is plausibly the same clock
defect (an offset 16 ms off cannot make a stamp 5 s old, though, so it stays unexplained).

## Part 3 — confirmations

Each item gives the request, the runs, the numbers and the verdict. "x" is the thin switch build
(`ScreenExtend-x\xbin`, both arms in the same binary; thin LTO, said where used). Its patch is
`scratchpad/x-switches.patch` and is never committed. It carries G03's switches, ported by hand
because `docs/overhaul-new/G03-experiment-switches.patch` no longer applies: its PliBackoff hunks
are in the product now. It also carries `SE_X_*` switches, all readable from a switch file between
renegotiations. "Arms" are A/B arms flipped inside one join by renegotiating in place (`arms`
request, `0f15e67`): no display per arm, no join-to-join variance.

**1. G01 `ea821a9` (WGC minimum update interval ⅔ period), A/B against its parent's flat 4 ms.**

- One display, arms on x. Verdict **level**.
  - 1080p: D 25.58 vs W4 26.16 ms.
  - Hi-res at 60 Hz: D 32.16 vs W4 31.92 ms.
  - Hi-res at a real 120 Hz (`p4-arms-hires120`, 4 windows each): D 33.36 (range 1.44) vs W4 33.80
    (0.40); fps 88.5 vs 86.8.
- Two displays (`two_clients`, 3 interleaved pairs of fresh joins, x):

  | | ⅔ period (head) | flat 4 ms (parent) |
  |---|---|---|
  | B (60 Hz) baseline p50 | 20.14 / 23.33 / 20.74 | 22.25 / 26.26 / 23.53 |
  | B painted fps | **49.6 / 48.9 / 46.5** | **58.0 / 55.5 / 54.9** |
  | B during A's edits, fps | 47.2 / 46.5 / 47.9 | 54.0 / 55.3 / 56.3 |
  | B frames encoded max fps | 53–54 | 62–63 |
  | B `frames_skipped_cadence` per session | 718–741 | 2940–2998 |
  | A (120 Hz) after B joined | 21.8 / 21.8 / 22.5 ms, 92–95 fps | 21.7 / 21.9 / 21.8 ms, 91–94 fps |

  - **p50: level.** Head medians are 2.8 ms lower, inside the larger range of 4.0 ms.
  - **Painted fps for the 60 Hz session: worse by ~8 fps**, with ranges that do not overlap
    (46.5–49.6 vs 54.9–58.0).
  - The floor does what it claimed: four times fewer wasted WGC deliveries. In the state it was
    made for, though, it also drops frames the session needed.
  - The same pattern holds on the final build (`p3-two`: B 48.2 fps).
  - A re-run with the GPU clock hold in both arms (`p5-*`, §4.6) decides whether this survives the
    hold.
  - Verdict: **refuted on fps in the two-display case.** The action is in §4.6.

**2. G03.**

- **PLI backoff at hi-res 120 Hz** (arms `NP` = `SE_G03_NO_PLI_BACKOFF`): **level**, 33.14 vs
  D 33.36 ms. PLIs per window were 5 in both arms, because G05's keyframe hand-back already removed
  the storm. The backoff is a defect fix for pages without that hand-back.
- **`07ec295` audio SACK allowance** (3 interleaved pairs on x, `SE_X_SACK_MS=0` = parent):
  **confirmed**.
  - `dc_drops` per run: **0 / 0 / 0** with the allowance, **142 / 272 / 274** without.
  - Click → rendered p50 is level (64.2–64.5 vs 64.7–66.5 ms).
  - All 15/15 clicks heard in both arms. The drops are silent in this test, because the click is
    redundantly encoded and NetEQ conceals.
- **D-05 after the fix** (`p3-d05-r1/r2`, final build): **confirmed twice**. For each encoder-only
  edit:
  - one join, one display, the same ids;
  - one new peer connection, and `closing the transport this renegotiation replaced`;
  - `live transport sessions: 1`;
  - picture back in 0.55–1.19 s.
- **Ablations**, 1080p small window, arms on x, 4 windows each:

  | arm | p50 med (range) | d vs D | verdict |
  |---|---|---|---|
  | D (shipped) | 25.58 (3.92) | — | — |
  | pacer Light on SameNetwork | 26.57 (3.73) | +0.99 | level |
  | pacer Full | 25.60 (1.32) | +0.02 | level |
  | stock `H264Payloader` | 26.11 (4.39) | +0.53 | level |
  | no bitrate driver (BWE off) | 27.03 (0.56) | +1.45 | level (not better) |
  | Baseline profile | 26.34 (2.05) | +0.76 | level; at hi-res 120 Hz +0.61, fps 85.4 vs 88.5 |
  | NVENC preset P2 | 26.49 (0.79) | +0.91 | level (P1 is already the fastest) |
  | sync NVENC (`SE_NVENC_NO_ASYNC`) | 26.09 (0.57) | +0.51 | level; hi-res 120 +0.01 |
  | no PLI backoff | 26.31 (1.60) | +0.73 | level |

  Nothing beats the shipped configuration, so nothing changes.

**3. G04.**
- **Third A/B round** (before/after the G04 merge, `f47f088` vs `023014b`): **not run**. It needs two
  more builds and 42 joins, and it measures join time, which the owner has said does not matter.
  The three-way above shows the merged join is 2.6–3.1 s against 5.3–5.5 s.
- **G04b `7584269` lost-answer continuation, through the real page** (x, `SE_X_LOSE_FIRST_ANSWER=1`
  holds the first answer 12 s): **confirmed**.
  - One display was created.
  - The page offered twice. The host logged `join retried attempt … whose answer never arrived;
    keeping its token`, `continues join …, whose answer was lost` and `renegotiating in place`.
  - First picture came at 10.4 s, then p50 26.0 ms and 54.5 fps.

**4. G05.**
- **Watchdog demotion live:** **0 demotions in 40 joins** (`p3-g05-joins-r1/r2`, final build,
  `slow_answer {n: 20, hold_s: 8}`). The base rate was 2/56.
  - Every join was on video path 1 after 8 s, with 2 peer connections: the page's warm-up throwaway
    plus the real one.
  - 0 joins with more than one offer, and 0 "falling back to <video>" lines.
  - Answers took 1.8–2.2 s.
  - Together with G05's 0/23 this is 0/63. That is consistent with the fix, though a 3.6 % base rate
    over 63 joins is not overwhelming (P(0 | 3.6 %) ≈ 0.1).
- **Legacy-transport half of the keyframe hand-back** (`createEncodedStreams` tier): **confirmed**.
  3 PLIs per 10 s window, the same as the default tier, against ~30 before the fix. p50 19.4 ms on
  path 1.
- **`<video>` vs WebCodecs**, 3 runs each (final build, clock hold on): **settled**. WebCodecs is
  18.2 / 19.1 / 19.1 ms; `<video>` (no WebCodecs) is 36.1 / 36.4 / 35.8 ms. WebCodecs is ~17 ms
  faster at equal fps.

**5. G06, on the final build.**
- `audio_paths`, all six tiers: **all 15/15**.

  | tier | click → rendered p50 | path / transport |
  |---|---|---|
  | default | 70.1 ms | webcodecs / shared |
  | no SAB | 64.5 | webcodecs / port |
  | no AudioDecoder | 115.7 | netEQ / shared |
  | **plain HTTP** | **116.1** | netEQ / postMessage |
  | worker fails | 105.6 | netEQ / shared |
  | stale output | 64.1 | webcodecs / shared |

  - **Plain-HTTP audio works end to end.** The handler joined over HTTP this time: the HTTPS upsell
    was dismissed by its own button twice. No handler fix was needed, and the earlier failure did
    not reproduce.
- `mic_prompt`, its three cases: **as designed, no defect**.
  - Speaker access granted: no card, no `getUserMedia`, 3 labelled outputs. The host holds
    `supported: true`, 3 outputs.
  - Microphone granted: the same.
  - Nothing granted: the card appears (skipped), `getUserMedia` reads denied, default only. The
    host holds `supported: true`, 1 output.
  - This also confirms `0ad9b7d` audio discovery through real joins in its cases: the discovery map
    reaches `status --json`.
- `stale_output` and `worker_fails`: in `audio_paths` above.
- The click metric on the final build is in Part 2.

**6. G07 / `90f4840`, the join-failure restore** (`join_failures`, final build, plus x for the
500). Page state was read over CDP; no event was dispatched.

| case | offers | settled | after the failure |
|---|---|---|---|
| wrong OTP (401) | 1 | 4.3 s | form shown, Connect enabled, error "Invalid session ID or OTP.", `locked` false, `isLive` false, `streamInputArmed` false, input not suspended, no `beforeunload` handler, wake lock not held (sentinel none, NoSleep off), no fullscreen or pointer lock, 0 open peer connections (2 built), **0 requests to the host in the 8 s after**, AudioContext suspended, 0 displays |
| unknown session id (401) | 1 | 0.2 s | the same |
| host stopped mid-join | 2 (one retry) | 8.1 s | the same, error "Failed to fetch" |
| host 500 (`SE_X_FAIL_JOIN`, x) | 2 (one retry, then stop) | 0.2 s | the same, error "WHEP POST failed: 500 …" |

- **Confirmed live in all four cases.**
  - The handler's first "form back" criterion was wrong: `#stage` is laid out before any join, so
    it is not a failure signal. It was fixed in `8f817f8`; every other criterion passed as written.
- `efef3a1` (relay query rejoined to the path) **cannot be tested here**: it needs the live relay,
  which would register a public session for the owner.

**7. G02.**
- **x264 path** (`--encoder software`, final build): works, with no colour or decode artefact
  detectable by the pixel checks.
  - Static 30 s: 0 changed, 0 green.
  - Small window p50 26.0 / 27.9 ms, 53.4 fps.
  - The BT.709 matrix against the VUI was not judged by eye.
- **Async NVENC** (arms): level. 1080p 26.09 vs 25.58; hi-res 120 33.37 vs 33.36. As shipped.
- **Adaptive-quality floor** (`settings_change` "adaptive quality", final build): the edit
  renegotiates in place (1 join, 1 display) and the encoder reopens as **CBR** at the same
  resolution. The QP floor's value is not logged, so the floor itself is **unverified**.
- **The 1440p encode-stage question:** answered. **The 8.4 ms is not keyed-mutex wait**:
  - With x's per-phase timing, the ring encode at 1440p splits into acquire 0.07 ms, submit
    0.12 ms, **completion wait 7.3–9.3 ms**, lock and copy-out 0.03 ms.
  - At 1080p the wait is 5.73 ms.
  - The wait is NVENC hardware time at idle clocks: video clock 555–627 MHz, P5/P8.
  - With the GPU held out of idle it falls to ~2.6 (1080p) and ~3.5 ms (1440p). See Part 4.

**8. G08.**
- **The two telemetry fixes through the handler:** **confirmed** (`p3-d05` runs, `status --json`
  on the final build). `capture_to_encode.count` is 607–620 for the ~10 s session, about the frames
  captured (it was 1,087 of ~9,900 before `3120abf`), and `capture_delivery` is absent.
- **Static idle cost with instruments off:** one arm ran (0 changed, 0 green, 5.5 fps idle). The
  other five runs were dropped when free physical memory fell under 2 GB. **Unverified**; it was
  low value after G08's own result.

**9. `settings_change` across every class, plus `two_clients`, `leave_rejoin`, `auto_rejoin` and
`logs`, on the final build.**

| edit | joins | displays | same ids | device rows +/− | new PCs | largest gap | picture back |
|---|---|---|---|---|---|---|---|
| audio on / off | 1→1 | 1→1 | yes | 0/0 | 0 | 62–63 ms | — |
| video quality | 1→1 | 1→1 | yes | 0/0 | 1 | 536 ms | 0.88 s |
| video scale | 1→1 | 1→1 | yes | 0/0 | 1 | 504 ms | 0.59 s |
| refresh rate 120 | 1→1 | 1→1 | yes | 0/0 | 1 | 653 ms | 1.51 s |
| DPR (native off) | 1→1 | 1→1 | yes | 0/0 | 1 | 374 ms | 1.04 s |
| orientation portrait / landscape | 1→1 | 1→1 | yes | 0/0 | 0 | 32 ms | — |
| display scale | 1→1 | 1→1 | yes | 0/0 | 0 | 77 ms | — |
| name | 1→1 | 1→1 | yes | 0/0 | 0 | 33 ms | — |
| adaptive quality | 1→1 | 1→1 | yes | 0/0 | 1 | 398 ms | 1.29 s |

- **`settings_change`:** `defectSeen` false. The WUDFHost delta for the whole run was 94.7 MB:
  one create plus the refresh and DPR mode changes.
- **`logs`:** pass. 1,080 backlog lines, 118 KB, 0 U+FFFD, no drop reports.
- **`two_clients`:** `bUntouched` true. B 15.6 ms p50 (48.2 fps); A after B 19.8 ms, 95.8 fps.
  B's fps carries the item-1 finding.
- **`leave_rejoin`:** a rejoin within 2 s reused the same display id. After the final leave the
  display was gone within 1 s.
- **`auto_rejoin`:** **the page did not rejoin within 30 s** of a host stop and restart. It is not
  a regression of `90f4840`.
  - The host's `stop` verb now sends `BYE_HOST_EXIT`, and the page takes the kick path ("The host
    stopped sharing this screen.", the join released with `/leave`, no reconnect). That is the
    design since `ab37665`.
  - `kickDevice` released the join before `90f4840` too.
  - The handler README's earlier "rejoined within 12 s" was a host stop the page did not hear as a
    bye.
  - Whether a page should rejoin a restarted host is a question for the owner (§7).

**A new defect found by the measurements (not fixed):** `ScreenExtend devices set <key> …` on a
device with no saved row writes a row with **`refreshRate` 60** (`cli.rs` `devices set` default). A
save makes every value the user's, so any CLI edit of a 120 Hz client pins it at 60 fps.
- It showed up as the hi-res arms first running 2560×1440@60.
- The desktop UI writes the row from its `DeviceJoin` payload and is not affected, by reading.
- Proposed fix: seed a new row's refresh rate from the live join's reported rate (the host knows it)
  instead of 60.
- The handler's arms now pass `--refresh-rate 120` for the hi-res client (`2bbce85`).

## Part 4 — latency itself

### 4.1 The budget of one frame (stamp metric, p50, ms)

Sources: host stage telemetry (`telemetry:` lines, truthful since G08) and x's per-phase NVENC
timing, the page's own `hostToPaint`, `decode` and `arrivalToPaint`, and the stamp metric. 1080p60
small window and 1440p120 small window, final build with the hold off and on.

| stage | 1080p60, hold off | 1080p60, hold **on** | 1440p120, hold off | 1440p120, hold **on** |
|---|---|---|---|---|
| stage draw → DWM compose → WGC callback → copy submitted (stamp − `hostToPaint`) | ~8.4 | ~7.4 | ~12.7 | ~10.9 |
| copy submit ("scale" stage) | 0.16 | 0.16 | 0.21 | 0.19 |
| encode queue | 0.03 | 0.04 | 1.7–2.1 (p90 8.4) | 0.03 |
| NVENC: acquire / submit / **hardware wait** / copy-out | 0.07 / 0.12 / **5.73** / 0.03 | ~ / ~ / **~2.6** / ~ | 0.07 / 0.12 / **7.3–9.3** / 0.03 | ~ / ~ / **~3.5** / ~ |
| packetize + socket write | 0.25 | 0.21 | 0.41 | 0.36 |
| host capture → socket (telemetry) | 5.8 | 3.15 | 10.5 | 4.2 |
| socket → arrival at the client's transform worker (h2p − host − a2p) | ~9.2 | ~7.3 | ~10.1 | ~12.0 |
| arrival → decode → paint call | 0.82 | 0.82 | ~0.9 | ~0.9 |
| **stamp metric** | **24.2** | **18.7** | **34.2** | **28.0** |
| calibration floor (readback) | 0.1–0.2 | 0.1–0.2 | | |

The largest items:

1. **GPU idle clocks.** They were behind most of the encode stage and part of the compose/copy
   stage. Fixed (§4.2).
2. **The composition wait.** Up to one 60 Hz interval between the stage's draw and DWM picking it
   up, ~7.4 ms left at 1080p.
3. **"Socket → worker arrival", 7–11 ms on a loopback link, unexplained.**
   - It covers webrtc-rs's internal send path after `write_rtp` returns (interceptors, SRTP, the
     ICE/DTLS conn), the loopback, and Chrome's receive pipeline up to the encoded transform.
   - Without a packet capture (admin) it cannot be split further. It is the next thing to
     instrument: a host-side stamp at the real `send_to`, and the page's receive time per frame.

### 4.2 Kept: hold the GPU out of idle while a session streams (`6c0bc71`)

**The finding.** The NVIDIA board sat in P8 for the whole 1080p60 session: 210 MHz graphics,
555 MHz video, 18 W. NVENC's completion wait was 5.7 ms a frame. Nothing a process may set for
itself moves the driver's governor. The per-application "prefer maximum performance" setting is a
persistent write to the driver's profile database, which is the owner's to make (§7).

**Dose map, found first.** Arms on x, 1080p, 4 windows each. The busy thread copies between two
2048² textures on a device of its own.

| copies / tick | P-state | graphics / video MHz | p50 (range) | d vs none | board W |
|---|---|---|---|---|---|
| none | P8 | 210 / 555 | 26.52 (1.23) | — | 18.4 |
| 1 / 1 ms | P8 | 323 / 555 | 26.49 (0.73) | −0.0 | 23.7 |
| 2 / 1 ms | P5 | 452 / 555 | 23.40 (3.68) | −3.1 | 24.6 |
| 4 / 2 ms | P5 | 460 / 555 | 23.04–23.45 | −2.5 to −2.8 | 24.5 |
| 4 / 1 ms | P5 | 860 / 740 | 21.69 (0.51) | −4.8 | 26.9 |
| **8 / 1 ms** | **P3** | **1243 / 1082** | **19.19 (0.44)** | **−7.3** | **34.2** |
| 16 / 2 ms | P3/P2 | 1498 / 1316 | 18.73 (0.51) | −7.8 | 43.4 |
| 16 / 1 ms | P0/P2 | 1892 / 1658 | 18.05 (0.62) | −8.2 | ~88 |

- The knee is eight copies a millisecond. At hi-res 120 Hz: 4/2 ms gave −3.0 and 16/1 ms gave −6.3
  (93.8 W).
- The first 48-window arms run had already shown it: the two other arms' windows that happened to
  catch the GPU in P0/P5, from load outside the test, read 22.6–23.1 ms against 25–27 ms in P8.

**Implemented.** `windows_utils/streamer/nvidia/clock_hold.rs`, held by each live NVENC ring
session (`AsyncEncodeRing`) and stopped with the last one.
- On AC power only (`GetSystemPowerStatus`).
- `SE_GPU_CLOCK_HOLD=0` turns it off; `=1` forces it.
- Not used by AMF, Quick Sync, x264 or the classic NVENC path.
- Not measured on any other GPU.

**Confirmed on the real profile.** Same binary, `SE_GPU_CLOCK_HOLD=0` vs default, five interleaved
pairs of fresh joins (`p4-hold-r*`):

| scenario | off p50 (range) | **on** p50 (range) | p99 off → on | fps off → on | page `hostToPaint` off → on | board W off → on |
|---|---|---|---|---|---|---|
| small window 1080p60 | 24.20 (3.27) | **18.70** (1.81) | 34.9 → 30.7 | 54.7 → 54.8 | 15.8 → 11.3 | 18.2 → 33.7 |
| hi-res 2560×1440@120 | 34.15 (0.96) | **28.03** (0.99) | 46.7 → 38.2 | 86.0 → 87.3 | 21.5 → 17.1 | 24.4 → 42.0 |

- The gain is beyond the larger range in both scenarios, and p99 and fps are better. **Kept.**
- At 1440p the encode queue (p90 8.4 ms) and the ring drops disappear, and delivered fps rises from
  100 to 110.
- The cost is +15–18 W for as long as a session streams. That is why it is AC-only, and it is a
  question for the owner (§7).

### 4.3 Tried and not kept (1080p unless said; arms on x, 4 windows each, both arms in one binary)

| candidate | result | why |
|---|---|---|
| 120 Hz virtual display for a 60 Hz client (`SE_X_COMPOSE_HZ=120`) | 26.57 (0.66) vs D 26.21 (2.11) | level, hold off. Re-run with the hold in §4.6 |
| WGC `MinUpdateInterval` at the full stream period | 1080p 26.01 vs 25.58 level; **1440p120 35.22 vs 33.36, fps 72 vs 88: worse** | at 120 Hz it under-delivers, and the lighter load also let the GPU fall to P8 |
| NVENC preset P2 (one step slower; P1 is the fastest) | 1080p +0.91, hi-res 60 −0.26 | level |
| pacer Light / Full on the same machine | +0.99 / +0.02 | level |
| stock packetizer, BWE off, Baseline, no PLI backoff, sync NVENC | +0.5 to +1.5 | level (§3 item 2) |

### 4.4 Noise figure of the final build (five consecutive runs)

These are five consecutive standard runs (`p6-noise-*`) of the final build `d28baaa`: clock hold
on, `ea821a9` reverted. Each run was exclusive, `stamp_band: "center"`, no window loaded, and every
join made one offer. Free memory was 5.8–7.8 GB.

| scenario | p50 median (min–max), **range** | p90 | p99 median (min–max) | painted fps median (min–max) | longest gap median |
|---|---|---|---|---|---|
| small window 1080p60 | 19.06 (18.40–19.69), **1.29** | 27.2 | 31.0 (30.0–32.1) | 53.9 (52.3–55.3) | 64 ms |
| large area 1080p60 | 21.41 (20.14–22.55), **2.41** | 29.9 | 33.7 (33.5–34.5) | 53.1 (52.5–54.4) | 64 ms |
| hi-res 2560×1440@120 | 28.67 (27.64–31.28), **3.64** | 33.3 | 40.5 (36.7–43.4) | 87.6 (81.5–90.7) | 32 ms |

- Static desktop: 0 changed and 0 green frames in all five runs.
- Calibration p50 0.04–0.30 ms.
- Joins answered in 2.3–3.4 s, with the first picture at 2.6–3.4 s.
- Checksum failures at 1080p: 92–152 of ~1200 per window.
- Against the three-way's `90f4840` (no hold): 1080p is 6.9 ms lower, large area 7.3 ms lower and
  hi-res 5.6 ms lower, all beyond either build's range.

### 4.5 Panel refresh

The internal panel's mode was recorded read-only (`EnumDisplaySettings`, every 2 s) through every
run. It was 360 Hz with no virtual display, **60 Hz whenever exactly one virtual display was
attached** (every run, every build), and back to 360 Hz afterwards. Every comparison above was
therefore taken at the same panel state. Whether a 360 Hz panel with one virtual display changes
latency could not be tested: the host may not write a display it did not create.

### 4.6 G01 with the GPU clock hold, and the composition candidate with the hold

**`ea821a9` again, with the GPU clock hold in both arms** (`p5-g01-2c-hold-*`). This was three
interleaved pairs of `two_clients` on x with `SE_X_GPU_BUSY=8,1` (the hold's dose) in both arms:

| | ⅔ period (head) | flat 4 ms (parent) |
|---|---|---|
| B (60 Hz) baseline p50 | 17.18 / 17.98 / 17.26 | 16.83 / 16.15 / 16.20 |
| **B painted fps** | **49.1 / 48.5 / 46.5** | **56.5 / 56.7 / 54.7** |
| A (120 Hz) after B, p50 / fps | 19.66–19.95 / 91.7–92.9 | 19.34–20.04 / 89.4–93.1 |

- The finding holds with the hold. With two displays, the ⅔ floor costs the 60 Hz session 6–10
  painted fps, with no ranges overlapping, and its latency is no better: the parent's medians are
  lower, by 1.0 ms against a 0.8 ms range.
- **`ea821a9` is reverted on `post-reboot`** (the revert commit, after `6c0bc71`):
  - `WGC_MIN_UPDATE_INTERVAL` is a flat 4 ms again.
  - The function and its test are gone.
  - `CLAUDE.md` and `OPTIMIZATIONS.md` G01-MINUPDATE say why.
- The cost of the revert is what `ea821a9` set out to save: about 4× more WGC deliveries
  (`frames_skipped_cadence` 2,950 vs 720 per session) for a 60 Hz session sharing the machine with
  a second display. That is CPU and GPU copy work, not latency.

**The composition candidates with the hold** (`p5-arms-hold-1080`, arms on x, all with
`SE_X_GPU_BUSY=8,1`, 4 windows each, 1080p):

| arm | p50 med (range) | d | fps |
|---|---|---|---|
| D | 19.17 (0.53) | — | 54.4 |
| 120 Hz virtual display for the 60 Hz client | 18.74 (0.97) | −0.43 | 54.3 |
| WGC floor at the full period | 19.07 (0.78) | −0.10 | 52.2 |
| WGC flat 4 ms | 19.33 (0.82) | +0.15 | 54.35 |

All are level. A 120 Hz compose cadence does not buy back the composition wait measurably here.
That wait is most of the ~7.4 ms "draw → capture" term in §4.1, which stays the largest structural
item. Nothing more was kept.

## 5. WUDFHost (the display driver's host process) and the machine

| boundary | largest WUDFHost commit | threads |
|---|---|---|
| start (after the reboot) | 9 MB | 19 |
| after Part 1 | 9 MB | 19 |
| after the seven discarded runs and the two clock tests | 1,420 MB | 142 |
| after Part 2 | 4,875 MB | 435 |
| after the arms runs (Part 4, first half) | 5,350 MB | 475 |
| after the clock-hold A/B | 7,459 MB | — |
| after Part 3 | 12,268 MB | 1,061 |
| after the follow-ups (G01 with the hold, composition arms) and the final build's five noise runs | **14,752 MB** | — |

- Per run: 141–215 MB for a standard run (two joins), ~47 MB per 1080p join, 940 MB for 20 joins.
- Free physical memory fell from 12.7 GB to under 2 GB as the driver host grew, and commit headroom
  from 46 to ~25 GB. Five of G08's six static runs were dropped for that reason.
- Display creations and mode changes used: about 225, counting each join, each two-client second
  join, each fallback or audio tier browser, and each mode-changing edit.

## 6. Commits on `post-reboot`

| commit | what |
|---|---|
| `0f15e67` | test-handler: one precise reference clock (the post-reboot bias fix); `join_failures` and `arms` requests |
| `2bbce85` | test-handler: GPU video clock, power and P-state in the load sample; `slow_answer.hold_s`; a fallback tier filter; arms `set_args`; the "adaptive quality" edit |
| `8f817f8` | test-handler: `join_failures` judges "form back" against the pre-join page |
| `6c0bc71` | **perf(encode-win): the GPU clock hold** (+ `CLAUDE.md`, `OPTIMIZATIONS.md` POSTREBOOT-GPUCLOCK) |
| `d28baaa` | **Revert `ea821a9`** (WGC floor ⅔ period), refuted on painted fps with two displays (+ `CLAUDE.md`, `OPTIMIZATIONS.md` G01-MINUPDATE) |
| (this commit) | `POST-REBOOT.md`; `REPORT.md` (verdict, the five-round table, statuses, the four later changes, incidents, questions); `FUTURE-TEST-WORKFLOW.md` (head, binaries, §6.7 rewritten) |

The experiment switches (`SE_X_*`, `SE_G03_*`) live only in the scratch patch `x-switches.patch`
(`C:\Users\sarve\AppData\Local\Temp\claude\…\scratchpad\x-switches.patch`) and the binary
`C:\Users\sarve\.cargo\shared_target\ScreenExtend-x\xbin\ScreenExtend.exe`. They are never
committed. Run data: `%TEMP%\screenextend-test-handler\runs\2026-10-06T*`; the per-run summaries
and the runner scripts are in the same scratch directory under `results\`.

## 7. Questions for the owner

1. **The GPU clock hold costs +15–18 W while streaming, and buys 5.5–6 ms per frame.**
   - It is on by default on AC power and off on battery (`SE_GPU_CLOCK_HOLD`).
   - **Assumed:** latency first, as you said.
   - **If not:** set the default to off, or have the desktop UI expose it (a UI change, not made
     here).
   - **Alternative:** the persistent driver setting "Power management mode: Prefer maximum
     performance" for `ScreenExtend.exe` (NVIDIA Control Panel, or NVAPI DRS as Sunshine does)
     would hold the clocks without the copy load. It is a system change this job may not make. If
     you set it yourself, the hold could be turned off. That trade was not measured.
2. **`ea821a9` costs a 60 Hz session ~8 painted fps when two displays are attached**, at level
   latency, against 4× fewer wasted captures.
   - **Done:** reverted by the keep rule (`d28baaa`).
   - **If you prefer fewer GPU copies to the frames:** re-apply `ea821a9`.
3. **Should a page rejoin by itself after the host was stopped and restarted** (today it shows "The
   host stopped sharing this screen." and waits for Connect)?
   - **Assumed:** the current design (`ab37665`).
4. **The CLI's `devices set` pins a 120 Hz device at 60 Hz** when no row was saved before. Fix it as
   proposed (seed the refresh rate from the live join)?
   - **Assumed:** yes. Not done here; it is CLI-only.

## 8. Unverified / untested (and why)

- **G04's third round:** two builds and 42 joins for join time, which you said does not matter.
  The request is in `G04.md`.
- **G08's static idle cost with instruments off:** dropped for memory (one of six runs).
- **The adaptive-quality QP floor's value:** not logged by the host.
- **The x264 colour matrix against the VUI:** needs eyes.
- **The relay (`efef3a1`, `8e709bd`):** needs the live relay.
- **The 10 % checksum failures at 1080p:** unexplained.
- **The 7–11 ms between the host's socket write and the client's transform:** unexplained, needs
  a packet capture or a host stamp at `send_to`.
- **The GPU clock hold on any GPU but this one, on a desktop, or on battery (forced):** not
  measured.
- **Input, macOS, AMD, Intel and the desktop UI:** untouched, as before.
