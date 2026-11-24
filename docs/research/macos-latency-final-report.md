# macOS latency and stability campaign: final report

Written 2026-11-24. It covers two days of work on the macOS host: what was researched, tested, found
and changed, what the evidence shows, what it does not, and what is left. The per-change ledger is
`docs/OPTIMIZATIONS.md`; the raw data and the working notes are in
`src-tauri/target/bench-results/` (local, not committed; `ANALYSIS-NOTES.md` is the index). The code is on
the branch `new-agent-work`.

The binary this report describes is **r11** (`src-tauri/target/bench/ScreenExtend-bench-r11`, SHA-256
`241d8969…28b445`, universal): the normal app plus a `bench` subcommand.

## 1. Summary

| | Before this work | Now |
|---|---|---|
| Frames the encoder discarded on a recorded window drag (M1) | 1,163 of 1,359 | 0 in every run, 0 of 36,000 in a 10-minute run |
| Encode, 1080p60, M1, idle (p50 / p99) | 8.1 / 9.3 ms | 6.4 / 7.0 ms |
| Encode, 1080p60, Intel Mac on 10.15 (p50 / p95) | 23.2 / 23.8 ms | 10.8–11.1 / 12.0 ms (10-minute run, 0 dropped) |
| Real browser, Intel host, heavy 1080p60 motion (capture → paint p50 / p99) | 43.6 / 63.1 ms with hold push off | 31.7 / 57.4 ms |
| Sockets a host leaves open per join | 1, forever | 0 |
| Host picture size and position when a client's size differs from what macOS remembers (M1) | wrong size, or a display overlapping the user's screen | right at 720p, 1080p, 1440p, 800x600 and 4K |
| Pathological content at pinned quality (every pixel changing) | 371 Mbps, the stream collapsed | bounded, 0 decode errors |
| Periodic keyframes to a healthy browser | every 10 s | every 60 s |
| A client page that stopped after a network hiccup or a big keyframe | dropped frames, requested more keyframes | keeps going |
| Release build on Apple silicon | permissions could never be granted | bundle signed as a whole |

Every encoder, capture and tuning tier is chosen at run time from what the machine reports; none is
chosen by macOS version. The same binary runs on macOS 26.7 (M1) and 10.15.8 (Intel).

**What is not proven is as important as the table.** The short list, with detail in section 10:
Windows was never run (only type-checked); Apple silicon other than one M1, and macOS 11–15, were not
tested; 1440p live measurement on the M1 gives no valid run in the bench; join time on this Mac got
slower through the day and I could not separate code from machine state; some display failures seen
in r7–r8 were never explained; and a user report (the built-in display changing resolution) exposed a
side effect of our own, now guarded.

## 2. How it was measured

Two machines, both on AC power with Low Power Mode off: an **M1** (MacBookAir10,1, macOS 26.7.1) and an
**Intel** MacBookPro10,2 (i5-3210M, macOS 10.15.8, the Quick Sync encoder `h264.gva`). They share a
Wi-Fi access point.

- **The recording:** 1,359 frames of a real fast window drag at 1920x1080, taken at the encoder's
  input with no processing (4.2 GB, private, local only). It reproduces the drop problem exactly and
  is replayed in real time.
- **`ScreenExtend bench`:** every run in its own process under a watchdog; encoder recipes as data;
  stages `lab` (one encoder session), `pipe` (the production encode loop), `live` (a real virtual
  display, real capture, real WHEP, a decoding client), `churn` (repeated joins).
- **Real browsers** (`scripts/e2e-headless/`): headless Chrome against a real `serve` host on either
  Mac, through the shipped page, including a second client machine with an older browser (Chrome 128),
  network stalls induced on the wire, slowed workers, and software decode.
- **Independent checks:** three independent read-only code reviews (two early, one of r4–r7); a watchdog program that logs every change of
  the built-in display's mode; a second probe process next to the host for display state.

1,418 recorded benchmark runs (plus the agents' scratch runs) and 215 browser-session files; not all are valid. A run is excluded when the pacing
thread was late, a watchdog fired, capture delivered nothing, or (later) the client decoded nothing.

## 3. What ships in the encoder

Rate control is a **tier chosen per session** from what the encoder reports, every one read back:

| Tier | Where it is used | How quality is held |
|---|---|---|
| `BaseFrameQp` | M1, pinned quality | per-frame constant QP; rate control off, so nothing is dropped for budget |
| `ClampCeiling` / `LowLatencyBitrate` | only when forced in tests | QP limits with a ceiling / a bitrate stand-in |
| `StandardBitrate` | Intel, pinned quality | an explicit bitrate (19.9 Mbps for quality 15 at 1080p60) |
| `AdaptiveLowLatency` / `AdaptiveStandard` | adaptive quality | a target bitrate from the congestion controller |

Also shipped:
- **Hold push** (Intel): the Quick Sync encoder releases frame N only when N+1 arrives; the loop
  learns this and resubmits the picture. It suspends itself when the encoder or the client cannot take
  twice the frame rate, and resumes after a back-off. Cost: about 44 % more bitrate and twice the
  frames on the wire while it runs.
- **A bitrate ceiling for pinned quality** (`BaseFrameQp` only), so content that cannot be compressed
  cannot blow up the stream: ceiling 24–80 Mbps by size, quality steps up fast and back down slowly.
  Pure noise at 1080p60 still runs near 80 Mbps at the worst quality the encoder has.
- **A drop guard** and a fallback to the standard encoder, unchanged from r5.
- **macOS tuning:** foreground task role while streaming, the existing real-time policies, and a
  performance hold that spins one thread while frames arrive, on only with efficiency cores, AC power,
  Low Power Mode off and no thermal pressure. It costs about 0.3–1.9 W (few measurements).

## 4. What else was fixed

- **The client page's stall test.** "No decoder output for 66 ms" was counted while nothing was
  arriving, so a network hiccup or a 1–2 MB keyframe made the page drop frames and ask for another
  keyframe, which caused the next hiccup. In real browsers: 49 keyframes and 78 dropped frames in 20 s
  before, 7 and 0 after. A dead decoder is still caught in 0.13–0.8 s at any frame rate.
- **Frame times and audio sync:** repeated frames were stamped with the time of the picture they
  repeat, which pushed audio delay toward 400 ms on a still screen. Now stamped when they leave the
  encoder, and timestamps always increase.
- **A per-join leak in the host (pre-existing, June and September):** every join left one ICE socket
  open forever; about 1–3 MB and one task each. Found by a real-browser descriptor test, fixed with a
  regression test that fails without the fix. On real hardware: 30 joins leave 0 sockets and flat
  descriptors (M1, r10; the Intel host is in section 8.7).
- **Display creation on macOS:** a display could come up at a remembered wrong size, at a wrong
  position overlapping the user's screen, or fail to come up at all and fail the join. Now: identity
  per device and size, readings accepted only once they are about the new display, one retry, an
  attach timeline in the log, and bounded waits.
- **Screens guard (from a user report):** our tests made macOS save a wrong mode for the built-in
  display together with one test display, so it flipped between 1680x1050 and 1440x900. Permanent
  configuration writes are gone, and the modes of the user's own screens are remembered before every
  display create, resize or removal and put back if they moved. Proven on the M1 against the very
  saved entry: without the guard the built-in stayed at 1440x900 for the session; with it, back within
  about a second.
- **Permissions and signing, the universal audio library, the first join's quality setting, the
  probe's behaviour, telemetry that counted waiting as encoding, test hooks reachable in release
  builds:** see the ledger.

## 5. What was found and not changed

- **Chrome hands received video to a page on a ~64 Hz beat**, so each frame waits up to 15.6 ms (mean
  about 9) before our code sees it. Measured on two Chrome versions, unchanged by any setting tried; that
  it is Chrome's internal metronome is inference. A DataChannel is not on that beat (round trips of
  0.6 ms), so video over one is the only way around it found, and it is a different design.
- **Two ways to shrink keyframes** (a QP ramp on the M1, `DataRateLimits` on Intel) were measured and
  not adopted: both trade a short stall for visibly lower quality on a setting the user pinned.
- **No property lowers M1 encode time** below its hardware rate; a list of encoder settings that are
  dangerous (some crash the process or drop every frame) is in the ledger.

## 6. Research

A five-angle web research pass (19 sources fetched, 95 claims extracted, 24 confirmed by
three-vote adversarial verification, 1 refuted) covered VideoToolbox property keys and their OS
availability, Apple's low-latency guidance (WWDC 2021 session 10158, "Encoding video for
low-latency conferencing"), the macOS 26 additions, and how Chromium, libwebrtc, FFmpeg, Sunshine
and others configure the encoder. Output: `bench-results/research.json`. What it established, each
then tested here:

- **Frame dropping under low-latency rate control is intended behaviour.** A QP ceiling does not
  switch rate control off; pinned at the cap and out of budget, the encoder drops the frame and
  returns success with a null sample and the `kVTEncodeInfo_FrameDropped` flag. Setting minimum
  QP = maximum QP, which is what the app did, is by Apple's own description a control that
  sacrifices frame rate.
- **There is a real constant-QP path:** the per-frame option `kVTEncodeFrameOptionKey_BaseFrameQP`,
  gated on the read-only `SupportsBaseFrameQP`. It switches rate control off, so nothing is
  dropped for budget.
- **The low-latency encoder has no hard rate cap.** `AverageBitRate` is a soft target,
  `DataRateLimits` is accepted and inert, and `ConstantBitRate` and the macOS 26
  `VariableBitRate` / VBV keys are rejected by it.
- **macOS 26 adds nothing to the low-latency path** beyond a read-only preset dictionary.
- **Long-term references exist** (token-based, macOS 12+), which allow repairing a loss with a
  small P frame instead of a keyframe, but need acknowledgements from the receiver.

What the research could **not** answer, and the tests had to: whether per-frame QP is stable over
a long run, what lowers encode time below 8 ms, how these settings behave on these two machines,
and what AirPlay-class products actually measure. No claim about competitor latency figures
survived verification, so this report makes none.

## 7. What did not work (encoder settings and ideas)

**Encoder settings that are dangerous.** None of these is an error at set time; all were found by
running them. The shipped code sets none of them.

| Encoder | Setting | What happens |
|---|---|---|
| `h264.rtvc` (M1) | `NumberOfSlices` 2 / 4 / 8 | every frame dropped |
| | `FigThreadPriority` 63 | the process crashes |
| | `ReferenceBufferCount` 1 | every frame a keyframe, 678 drops |
| | `RealTime` false | p50 82 ms, p95 282 ms |
| | `EncoderUsage` 0 / 2 / 3 | per-frame QP silently ignored (2.6 Mbps, 40.9 dB) |
| | a 150 Mbps bitrate ceiling with the QP clamp | every frame dropped from the first |
| `ave.avc` (M1) | `EncoderUsage` 1 | every frame dropped |
| | QP limits | stalls and drops |
| | frame reordering left at default | 27 ms |
| `h264.gva` (Intel) | `EncoderUsage` 1 | segmentation fault |
| | `EncoderUsage` 2 / 3 | rejected, and the session then fails every frame |
| | `RealTime` false or unset | 40 ms |

**Things tried that gave nothing.**

- No property lowered encode time on the M1's low-latency encoder: `ExpectedFrameRate` (it only
  changes the declared level), `RequestedMaxEncoderLatency`, sub-frame sections, input queue
  depth, thread priority 31 / 47, QP modulation, entropy mode, profile, LTR on or off, temporal
  layers. `MaximizePowerEfficiency` costs about 2 ms. The standard encoder reports a maximum
  pixel rate of 248,832,000 px/s, which is 8.3 ms for a 1080p frame; the measured 6.4–8 ms is the
  hardware's rate.
- Flushing after every frame: on the M1 it trims the tail but blocks the submitting thread for
  7–13 ms; on Intel it defeats the hold (14 ms) but turns every frame into an 850 KB keyframe
  (408 Mbps). Not used.
- `DataRateLimits` on the low-latency encoder: accepted, no effect.
- HEVC on the M1 (`hevc.rtvc`): 21 % fewer bits at the same quality and the same latency. No
  latency gain and no hardware HEVC encoder on the Intel Mac, so it was not pursued.
- An external process keeping the CPU busy does not hold the M1 in its fast state.
- Software H.264 on either Mac is not a usable fallback at 1080p60 (224 ms on the M1, 577 ms on
  the Intel Mac).
- The two keyframe-size mitigations in section 8.9.

## 8. Results


### 8.1 M1: the drop problem and the tiers

Recorded drag, 1080p60, quality pinned at QP 15:

| Configuration | Dropped | Bitrate | PSNR | SPS level / refs |
|---|---|---|---|---|
| Shipped before (min QP = max QP, no bitrate) | 1,163 of 1,359 | – | – | – |
| Apple's default rate control, no settings | 274 | – | – | – |
| First fix: QP clamp + 300 Mbps ceiling | 0 (1 on the stress clip) | 10.9 Mbps | 51.3 dB | 5.1 / 15 |
| The same with a 150 Mbps ceiling | every frame | – | – | – |
| The same with 100 / 120 / 200 Mbps | 2 | – | – | – |
| **Per-frame QP (`BaseFrameQp`), ships** | **0 in every run** | 10.85 Mbps | 51.26 dB | 4.2 / 4 |
| Standard encoder `ave.avc`, bitrate for quality | 0 | 11.65 Mbps | 50.34 dB | – |
| Adaptive (target bitrate + QP range 15–45) | 0 | 6.3 Mbps | 48.3 dB | 4.2 |

Per-frame QP produces the same bytes as the first fix, declares a normal level (4.2 and 4
reference frames instead of 5.1 and 15), and a mid-stream quality change lands on the exact frame.
The ceiling approach had a cliff nobody could have guessed: 300 Mbps worked, 150 Mbps dropped
every frame from the first.

Quality ladder on the recording (per-frame QP): QP 8 22.9 Mbps / 55.3 dB · 10 18.1 / 54.2 ·
12 13.6 / 53.1 · **15 10.85 / 51.3** · 18 7.5 / 48.4 · 22 5.25 / 45.4 · 26 3.4 / 42.2 ·
30 2.3 / 39.6 · 36 1.2 / 35.5 · 44 0.6 / 30.7. Bitrate roughly halves every 6–7 QP steps.

Resolution, cadence and concurrency (per-frame QP, stress clip, one quiet round without the
performance hold, p50 / p95):

| | Latency | Dropped |
|---|---|---|
| 720p60 | 5.9 / 7.1 | 0 |
| 1080p60 | 8.1 / 9.0 | 0 |
| 1080p120 | 7.5 / 8.7 | 0 of 720 |
| 4K60 | 20.0 / 21.0 | 0 |
| 2 sessions at 1080p60 | 12.3 (p99 45.6) | 0 |
| 3 sessions at 1080p60 | 12.4 (p95 68, p99 98) | 0 |

4K60 costs 20 ms on this machine, more than one frame interval, and three simultaneous 1080p60
sessions saturate the encoder. Both are hardware limits of an M1.


### 8.2 M1: stability

| Run | Frames | Dropped | p50 / p95 / p99 | Over time |
|---|---|---|---|---|
| Production loop, the recording looped for 10 min, QP 15 | 25,821 | 0 | 7.4 / 8.5 / 13.8 | 60 s p50 7.7 → 6.8, no upward drift; memory flat at 256–264 MB |
| Production loop, adaptive tier, stress, 5 min | 17,998 of 18,000 | 0 (2 coalesced at start) | 6.8 / 7.0 / 7.3 | flat 6.7–6.9 |
| Single session, stress, 10 min, QP 15 | 36,000 | 0 | 6.7 / 7.4 / 8.0 | flat 6.6–7.2; memory flat |
| 100 create/destroy cycles | – | – | – | no failures, no per-cycle leak |
| Real browser, 5 min, final build (800×600) | 18,030 painted | 0 | – | 60.1 fps, longest gap between paints 43 ms |

Thermal state stayed nominal throughout. The p99 of 13.8 ms on the recording is its forced
keyframes (258 in that run), not drift.


### 8.3 Intel: tiers, the hold, and hold push

There is no QP control on `h264.gva`, the low-latency key does not resolve on 10.15, and no
shipped configuration dropped a frame in any run there. The tiers selected are `StandardBitrate` (QP 15 →
17.8 Mbps, 46.1 dB; QP 22 → 8.0 Mbps, 39.4 dB) and `AdaptiveStandard` (11.0 Mbps, 42.9 dB),
level 4.2, 2 reference frames.

The one-frame hold, submit → output p50 by cadence, with nothing done about it: 120 fps 13.2 ·
60 fps 23.3 · 30 fps 42.6 · 20 fps 59.4 · 10 fps 110.8. That is the frame interval plus 5–10 ms
every time.

Hold push in its default self-learning mode, production loop (p50 / p95):

| Content | Without | With |
|---|---|---|
| 1080p60 stress, 10-minute run, keyframe forced every 7 s | 23.1 / 23.7 (max 206) | **11.1 / 12.3**, p99 17.0, max 39.6 |
| 1080p60 scroll | 22.9 / 23.4 | 10.6 p50 |
| 720p60 | 19.6 / 20.1 | 7.8 p50 (9.0 p95 once learned) |
| 30 fps | 42.4 / 43.6 | 16.3 p50 (18.4 p95 once learned) |
| 10 fps | 109.9 / 115.0 | 24.6 / 34.7; flat over a 5-minute run |
| 120 fps | 13.5 | unchanged: the encoder's work is already ≥ the interval |

In that 10-minute run with the final build 35,865 of 36,000 frames were pushed, none were
dropped, the mode changed exactly once (learning took the first ~2.2 s), and the timestamp
correction fired 5 times in 72,000 frames. Short runs show a p95 near the un-pushed value
only because the ~2 s of learning are inside them.

Other Intel findings: under 4 busy threads the Mac goes from 32.3 ms to 20.4 ms with hold push;
no tuning switch changed anything there; software H.264 is unusable (577 ms per 1080p frame,
187 ms at 720p); quality saturates near 20 Mbps on the stress clip (4 Mbps 29.1 dB · 8 Mbps 38.1 ·
12 Mbps 42.5 · 20 Mbps 46.2 · 50 Mbps 47.8).


### 8.4 Tuning on the M1

The M1 has two latency states for an identical configuration with nothing else running: about
6.2–6.5 ms and about 7.9–8.2 ms. It sits in the slow one after minutes of idleness and in the
fast one while other work keeps the performance cores at speed. An external spinner process did
not move it; a spinner inside the streaming process at user-interactive priority does.

Production loop, 1080p60, 4 repeats each (p50 / p95 / p99):

| | Idle, 60 fps | Idle, recording | Idle, 10 fps (p50) | 8 busy threads |
|---|---|---|---|---|
| Foreground role off, no hold | 8.0 / 8.8 / 9.2 | 8.2 / 9.2 / 15.1 | 9.4 | 9.8 / 16.7 / 20.5 |
| Foreground role on, no hold | 8.1 / 8.9 / 9.3 | 8.2 / 9.3 / 15.3 | 9.4 | 6.2 / 6.5 / 6.8 |
| + hold while a frame is encoding | 6.5 / 6.8 / 7.1 | 6.6 / 7.8 / 14.0 | 8.8 | 6.3 / 6.5 / 6.8 |
| + hold always spinning | 6.5 / 6.8 / 7.0 | 6.5 / 7.0 / 10.9 | 6.6 | 6.2 / 6.5 / 8.1 |
| **Final build defaults** (role on, hold with a 200 ms linger) | **6.4 / 6.7 / 7.0** | **6.5 / 6.9 / 10.8** | **6.6** | **6.2 / 6.5 / 9.7** |

On the standard encoder (the drop guard's fallback) the hold is worth more: 10.5 / 13.3 →
7.4 / 8.3. The encode-only hold loses most of its gain on sparse updates such as typing, which
is why the shipped variant lingers. Under load the hold buys nothing (the cores are already at
speed) and its spinning thread costs a little at the tail: p99 9.7 ms with it against 6.8 ms
with the foreground role alone.

Switches that measured as no help, idle or loaded: a tier-0 task QoS, a shorter time-constraint
period on the encode thread, a time constraint on the drain thread, `caffeinate`. Switches that
measured as harmful when removed: the encode thread's time constraint (9.7 / 18.1 / 22.5 under
load without it) and the drain thread's precedence (9.4 / 14.8 / 20.1).


### 8.5 Whole workflow, in-process receiver (bench `live`, M1, real capture)

Real virtual display, ScreenCaptureKit, the production encoder and transport, a decoding client on
the same Mac; AC power. Milliseconds. The encoder is about 70 % of capture → decoded.

| Content | Encode p50 / p99 | Capture → decoded p50 / p95 / p99 |
|---|---|---|
| 1080p60 stress (3 runs) | 6.5 / 9.4 | 8.9 / 10.2 / 10.9 |
| scroll | 6.3 / 7.0 | 8.5 / 8.8 / 8.9 |
| the recording | 6.3 / 9.7 | 9.0 / 11.0 / 11.9 |
| 720p60 | 2.9 / 3.5 | 4.1 / 4.7 / 5.1 |
| 1440p60 | 9.4 / 10.5 | 13.2 / 15.2 / 16.5 |
| 4K60 (642 of 720 frames captured: past the encoder's rate) | 16.8 / 18.9 | 34.1 / 41.4 / 43.9 |
| 5-minute run | 6.3 / 6.8 | 8.8 / 10.0 / 10.9, every captured frame decoded |

Quoted from r7 (same encode path as r11). Later r8–r10 live runs at 1080p agreed; 1440p live in r10 gave no
valid run (the benchmark's decoder and image stamp, not the host), so 1440p here is r7's.

### 8.6 Whole workflow, real browser

Headless Chrome against a real `serve` host, real screen capture of a moving window, default settings, a
new device each run. Capture → paint is the page's own measure (its clock offset was checked against the
host's, error 0.00–0.23 ms on one machine). Chrome on the M1 adds about 9 ms inside the browser (section 5).

**M1 host, Chrome on the M1** (r10/r11; 0 dropped frames, 0 decode errors, 0 keyframe requests in every row):

| Run | Painted | Capture → paint p50 / p90 / p99 |
|---|---|---|
| 1080p window drag | 57.2 fps | 20.0 / 26.7 / 28.5 |
| 1080p scroll | 57.5 fps | 16.2 / 23.9 / 26.1 |
| 1080p noise (every pixel changes) | 56.8 fps, 82.8 Mbps, quality at its floor | 25.7 / 30.2 / 35.4 |
| 1080p drag with a 150 ms network stall every 3 s | 57.5 fps | 24.4 / 30.9 / 149 |
| 1440p drag | 58.6 fps | 26.7 / 31.8 / 33.3 |
| 720p, 800x600 (two runs each) | 56.6–57.7 fps | 14.0–15.1 / 20.5–21.7 / 21.7–24.1 |
| 4K drag (two runs; both FAIL the harness's 90 %-of-60 rule) | 51.5 and 51.0 fps (the encoder's limit) | 48.6 / 57.1 / 61.8 |
| 1080p drag, 5 minutes (marked FAIL, see below) | 59.4 fps (17,835 frames, 0 dropped, longest gap 186 ms) | 19.5 / 26.1 / 27.6, flat over the run |
| 15 idle-then-join cycles, 20 rapid joins (a cargo check ran for most of them), 8 sizes in one host | 43 of 43 attached first try | attach 337–434 ms |

The 5-minute run is marked FAIL by the harness because the window-moving helper was killed by its own timer
before it printed its result, so how many pictures it presented is not known. The host encoded 17,874 new
frames and repeated 29, so the picture did keep changing. I ran it after the scripted copy was lost to a
double start; it is one run.

The host's memory and sockets over 30 joins: see 8.7.

**Intel host, Chrome on the M1** (r11, 1080p, hold push engaged where marked):

| Run | Painted | Capture → paint p50 / p90 / p99 |
|---|---|---|
| window drag * | 101 fps (pushes) | 31.7 / 39.6 / 57.4 |
| noise | 101 fps (pushes) | 35.9 / 48.4 / 68.1 |
| scroll | 106 fps | 29.6 / 36.6 / 55.4 |
| drag with 150 ms stalls (10 induced) | 105 fps | 33.6 / 48.3 / 157 |
| drag, 5 minutes | 114 fps, 22 keyframes | 42.4 / 56.2 / 69.2; **worse than r7 (33.1 / 40.5 / 60.3), unexplained** |
| a slow client (8 ms per frame) | pushes suspend by themselves | 57.1 (74.9 with them forced on in r7; 56.0 with them off) |
| software decode client * | 101 fps | 44.5 / 59.5 / 77.5 |
| Chrome 128 on the same Intel Mac | 57.7 fps | 46.2 / 66.2 / 79.2; pushes suspended by the encoder guard |

\* Marked FAIL by a harness bug (no capture stage existed at its first status read); the frame rates were
recomputed from the per-second series and meet the rule. The harness is fixed; these two were not rerun.

The 5-minute run held 32–36 ms until about 235 s, stepped to 40 ms at 240 s and ended at 43–45 ms. Capture →
socket did not change, so the extra is after the socket or in the page's clock estimate. The reported round
trip rose too (5.5 → 13 ms), but at about 252 s, after the step, so the network is not shown to be the
cause. One run; longest gap 430 ms (r7 294), 22 keyframes (r7 18). Idle p99 was also higher (161 against
92 ms) and painted rates about 8 % lower than r7, one run each.

With hold push off this host shows about 43 ms p50 and 56 fps on the same drag (r7).

**Chrome 128 (a 2012 laptop) as a client of the M1 host:** 1080p drag 57.8 fps for 30 s, capture → paint
31.5 / 37.7 / 42.7. In a 2-minute run it froze after about 73 s and fell back to `<video>`, and noise froze
for 3.9 s. Packets kept arriving with no loss and the page's decoder was idle: the frames stopped reaching
the page, upstream of our code. Same with the old page. Cause unknown; never seen with Chrome 158.

### 8.7 Displays, sockets and joins

- **Joins:** the bench and browser joined and left hundreds of times on both Macs with no failures, wrong
  sizes or overlaps on r8–r11 (r6 failed 1 of 20 on the Intel Mac; r7 is below). Churn on r9, r10 and r11:
  160 of 160 on each Mac. Idle-then-join: 18 of 18 on each Mac (M1 on r10, then 15 of 15 in a browser on r11).
  One join in the Intel churn took 4.1 s (a mode switch that does not return on 10.15, bounded).
- **Sockets:** r7 left one UDP socket per join (30 after 30 joins, never closed) on both Macs. Now: M1 0
  (r10 browser test and r11 churn), descriptors flat; Intel host 1 at every checkpoint (r11), descriptors
  flat at 20–22. Memory over 30
  joins: M1 65 → 92 MB; Intel 17 → 83 MB, about 0.8 MB per join, smaller than r7's 2.8 but not explained.
- **Wrong size and position (M1, r5–r6):** fixed at every size tested, 4K included.
- **Display failures:** in r7, 2 of 9 local runs failed to bring the display online. The display was up
  while the host's view was frozen. The only proposed fix (a running event loop) was tested and not shown to
  help; it is off. Since then 0 of 480 churn
  joins on the M1 (r9–r11), 72 in a windowless test and the browser joins failed. At r7's rate that is good
  evidence, not proof, since r7's failures came in clusters. **Unexplained.**
- **Resolution of the built-in display (M1):** flipped by our own tests (section 9); the guard was
  proven against the saved entry (1440x900 for the whole session without it, back in about a second with
  it) and has not had to act in 160 further joins.

### 8.8 Join time (not streaming)

On the M1 a join took about 0.64 s in r8 and 1.0–1.4 s late in the campaign with the code essentially
unchanged; the increase is in starting the virtual display and the capture, and grew all day (first
joins 0.6 → 1.9 s). On the Intel Mac it was 0.72 s throughout except one build (r9). I could not separate
the code from machine state; a restart of the M1 was not tried.

### 8.9 Keyframe size: two mitigations measured and not adopted

| | Keyframe (stress clip) | With the mitigation | Its cost |
|---|---|---|---|
| M1: code the keyframe at a higher QP, step back over five frames | 1.58 MB | six frames of ~330 KB; encode p99 33 → 13–16 ms | PSNR 26.7 dB on the keyframe: a soft pulse of ~80 ms at every keyframe |
| Intel: `DataRateLimits` | 1.7–2.1 MB | ~245–270 KB | PSNR 23.5 dB on the keyframe, recovering slowly (−5 dB on average with a keyframe every 2 s); the keyframe takes 61–63 ms to encode instead of 37 |

Both trade a stall on worst-case content for visibly lower quality on a setting the user pinned.
With the client no longer multiplying keyframes, neither was adopted. On ordinary content the
question does not arise: the recording's keyframes are at most 441 KB and a scrolling page's are
27–41 KB.



## 9. Mistakes and corrections in this work

Kept because they change how much to trust the rest:
- The first diagnosis (capture, then a 360 Hz panel) was wrong; the encoder was discarding frames.
- A ceiling fix regressed the Intel Mac; hold push's first version wasted half its pushes and fell out
  of its fast mode; a client guard in r9 suspended hold push for healthy clients; each was caught by a
  test or a review and fixed before shipping.
- I reported "no leak" from thread counts while file descriptors were growing; corrected the same hour.
- This report's first draft had errors I caught by rereading the data: it presented the 5-minute M1 run as clean,
  blamed the Intel 5-minute slowdown on the network, and quoted a Windows check made on r9 as if on the final tree.
- I called a display cause "found" (a missing event loop) and later "our code"; both went further than
  the evidence and were withdrawn. The stuck-display cluster seen on r7 remains **unexplained**.
- Two benchmark sets were over loopback or had no network stall actually induced; those numbers were
  withdrawn. The performance hold's power cost was first given too precisely.
- The built-in display flipped for hours because I did not act on an early note about it, and I created
  displays on a Mac in use. The user had to report it.

## 10. Not verified

- **Windows: nothing was run.** The final tree type-checks for `x86_64-pc-windows-msvc` in both
  configurations (stubbed C/resource steps, nothing linked). Clippy on current stable fails there on `chunks_exact` in
  `windows_utils` files this work did not touch. Shared changes (session, leak fix, first-join
  quality, keyframe interval, telemetry, client page) have never run there.
- **The release app** built here is arm64 only and unpublished (`src-tauri/target/release/bundle/macos/`):
  it passes `scripts/check-macos-signature.sh` (signed as a whole, hardened runtime, the bundled library
  signed). The universal CI build and the updater archive (no signing key on this Mac) were not produced, and
  the app window was not opened and the app was not installed. `doctor` run from a terminal reported the
  permissions as granted, but that is the terminal's grant, not the app's.
- **Other hardware and systems:** no Apple silicon but one M1 was used; no macOS 11–15 machine; only one
  Intel Mac; the performance hold's power cost is from a few measurements (0.3–1.9 W).
- **The desktop app:** every host test used `serve` or the benchmark. Joins, displays and the event-loop
  behaviour inside the real app were not tested. Neither was a human click-through of the new permission
  flow, nor a notarized build (no Developer ID exists).
- **Stuck display view:** the r7 failures (2 of 9) have no established cause and no proven fix; they
  have not recurred in r10–r11. A retry inside the same process cannot fix that case.
- **1440p live in the benchmark:** 0 valid of 6 runs in r10 (a missing picture stamp and a decoder that
  does not return, in the benchmark, not the host). The 1440p browser runs passed.
- **Join time on the M1** grew through the day; cause (code or machine state) not separated.
- **Client push guard** ran once each way in a real browser (Intel host) and worked; no slow real device
  has been used.
- **Bitrate ceiling** stays above its target on pure noise at 1080p (about 80 Mbps against 49.8, at the
  worst quality the encoder has) and is untested at 4K in a browser.
- **Chrome 128 (2012 laptop) as a client** freezes after about a minute in some runs; cause unknown, not
  our page; a real device of that age was not tested.
- **Memory growth per join on the Intel host:** about 0.8 MB, unexplained.
- **A host on macOS 10.15** switching to 800x600 right after 1080p can take up to about 4 s to join.
- **A client that aborts its join mid-request** can leave a virtual display up until it retries (found by
  reading; not fixed).
- **One unrelated unit test** fails before and after (`airplay::ax::tests::the_menu_bundle_yields_localized_titles`).
- **The screens guard** was proven once against one saved entry; it cannot act in a process whose view of
  the displays is stuck, and a saved entry from another source would still flicker for up to about a second.


## 11. Where things are

| | |
|---|---|
| Encoder tiers, drop guard | `src-tauri/src/macos_utils/streamer/encoder.rs`, `vt_recipe.rs` |
| Hold push and its guards, bitrate ceiling, frame times | `pipeline.rs` in the same directory |
| Tuning | `tune.rs`, `perf_hold.rs`, `qos.rs`, `tuning.rs` |
| Virtual display, screens guard | `src-tauri/src/macos_utils/virtual_display.rs` |
| Host session and leak fix, keyframe interval, telemetry, join quality | `src-tauri/src/streamer/webrtc_session.rs`, `keyframe.rs`, `telemetry.rs`, `server.rs` |
| Leak tests | `src-tauri/src/streamer/test/session_leak.rs` |
| Client page | `src-tauri/src/streamer/static/transform-worker.js`, `scripts/test-client-transform.mjs` |
| Benchmark and browser harness | `src-tauri/src/bench/`, `scripts/build-bench-macos.sh`, `scripts/e2e-headless/` |
