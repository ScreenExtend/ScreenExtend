# Baseline measurements

Numbers captured **before** any optimization, so every later claim has something to reference.
All latencies are per-stage timestamps off the shared host clock (`streamer::audio::host_now_ns`)
unless marked as camera-measured glass-to-glass. Distributions are p50 / p90 / p99 / max — never
a bare mean, because jitter is felt more than the average. `docs/TELEMETRY.md` defines every
stage and how it is read back.

## How these were produced

Release build at commit `ddf9d76` — the profile that ships (`lto = "fat"`, `codegen-units = 1`)
plus `--features probe`, which adds the WHEP self-test. Each cell is one whole process, 30 s:

```sh
ScreenExtend probe --whep-selftest --duration 30 --encoder <e> --capture-backend <c>
ScreenExtend probe --live out.h264 --duration 30 --monitor 0 --encoder <e> --capture-backend <c>
```

Two families, because neither alone covers the path:

| Family | Capture | Content | Transport | What it measures |
|---|---|---|---|---|
| `selftest` | real, of a virtual display created for the run, 1280x720@60 | an empty extended desktop — **static** | real: WHEP join, SRTP over loopback | the whole host path, capture through socket write |
| `live` | real, of the primary display, 1920x1080@360 | whatever was on screen | none — frames go to a file | capture and encode under real content and a real resolution |

Read them together. `selftest` has the transport but almost no content; `live` has content and
a 1080p360 target but stops at the encoder. Neither has a browser client, so no cell here
includes decode or paint (see [What could not be measured](#what-could-not-be-measured)).

## Machines

| Role | Machine | Notes |
|------|---------|-------|
| Windows host | Windows 11 build 26200 (25H2), NVIDIA RTX 3080 Ti Laptop GPU (driver 580.97) + Intel UHD Graphics, hybrid; primary 1920x1080@360 | IDD virtual display driver installed and working; NVENC, Quick Sync and x264 all exercised |
| macOS host (dev box) | macOS 10.15.8 Catalina, Intel, Command Line Tools only, reached over SSH | CGDisplayStream + VideoToolbox only — SCK, Process Tap and VT low-latency do not exist on this OS |
| macOS host (modern) | not available during this run | **needs modern Mac** — every 12.3+ / 13+ / 14.2+ tier is unmeasured: ScreenCaptureKit capture, `EnableLowLatencyRateControl`, `EnableLTR`, the Process Tap audio path, and whether `displayTime` / the SCK PTS are composite times there or scheduled ones as on Catalina. The script to run is `docs/manual-tests/macos-capture-encode.md`, which lists the log lines and the numbers to record, section by section. |

## Matrix — Windows, `selftest` (full host path, static 1280x720@60)

Milliseconds, p50 / p90 / p99 / max. `frames` counts everything the encoder emitted; `repeats`
is how many of those were keepalives the encoder re-sent because the picture had not changed —
they are excluded from the capture-relative stages, which is why those describe real capture.

| Encoder | Capture | frames (repeats) | RTP/s | capture delivery | scale | encode queue | encode | packetize | socket write | **capture → socket** |
|---|---|---|---|---|---|---|---|---|---|---|
| NVENC zero-copy ring | WGC | 211 (144) | 58 | 0.98 / 4.19 / 6.82 / 6.95 | 0.11 / 0.13 / 2.62 / 2.74 | 0.00 / 0.02 / 0.26 / 14.81 | 0.00 / 1.18 / 1.44 / 15.09 | 0.04 / 0.06 / 0.11 / 0.12 | 0.16 / 0.25 / 0.43 / 0.59 | **1.57 / 8.39 / 16.78 / 18.00** |
| x264 (software) | WGC | 223 (142) | 84 | 0.98 / 3.93 / 4.72 / 4.99 | — | 0.00 / 0.00 / 0.00 / 0.00 | 2.10 / 2.62 / 5.24 / 24.57 | 0.05 / 0.07 / 0.10 / 0.25 | 0.20 / 0.29 / 0.52 / 0.68 | **2.36 / 5.24 / 23.07 / 24.82** |
| Quick Sync (CPU bridge) | WGC | 219 (140) | 55 | 3.15 / 7.86 / 15.73 / 16.11 | — | 0.00 / 0.00 / 0.00 / 0.01 | 4.19 / 11.53 / 16.78 / 21.65 | 0.04 / 0.06 / 0.15 / 0.36 | 0.15 / 0.26 / 0.43 / 1.36 | **11.53 / 16.78 / 25.17 / 26.05** |
| any | DXGI | — | — | — | — | — | — | — | — | **unmeasurable** — see below |

## Matrix — Windows, `live` (real content, 1920x1080@360, no transport)

| Encoder | Capture | frames (repeats) | bytes | capture delivery | scale | encode queue | encode | **capture → encode-out** |
|---|---|---|---|---|---|---|---|---|
| NVENC zero-copy ring | WGC | 168 (119) | 1,518,706 | 6.29 / 9.44 / 20.97 / 22.54 | 0.18 / 0.23 / 0.85 / 0.87 | 0.00 / 0.02 / 0.05 / 0.05 | 0.00 / 5.77 / 5.77 / 10.30 | **12.58 / 15.73 / 27.26 / 28.45** |
| x264 (software) | WGC | 191 (140) | 1,235,186 | 8.39 / 9.44 / 16.78 / 18.60 | — | 0.00 / 0.00 / 5.24 / 11.24 | 7.86 / 11.53 / 20.97 / 24.53 | **20.97 / 23.07 / 37.75 / 40.31** |
| Quick Sync (CPU bridge) | WGC | 172 (141) | 3,108,831 | 9.44 / 10.49 / 37.75 / 39.73 | — | 0.00 / 0.00 / 1.84 / 4.80 | 7.34 / 16.78 / 46.14 / 55.27 | **27.26 / 31.46 / 83.89 / 88.25** |
| any | DXGI | — | — | — | — | — | — | **unmeasurable** — see below |

### Scripted content through the real capture path (core close-out)

Every `live` row above captured whatever happened to be on the desktop, because `--workload`
implied `--synthetic-pattern` and that path fed the encoder directly — no duplicator, no frame
pool, no scaler. `probe --live --workload <w>` now paints the generated frames into a borderless
window covering the captured display, so the content is repeatable *and* goes through capture.

| Workload | Encoder | Capture | frames | capture delivery | scale |
|---|---|---|---|---|---|
| motion | NVENC zero-copy ring | WGC | 817 in 12 s | 7.34 / 18.87 / 31.46 / 217.84 | 0.15 / 0.21 / 0.33 / 1.60 |

Debug build, so the absolute numbers are not comparable with the release rows above — the point
of the row is that `capture delivery` and `scale` have values at all, where every previous
workload run reported them empty. Re-run the other three workloads on a release build to extend
the table.

### Virtual-display mode changes (`report_selected_refresh_rate` / `report_mode_change_cost`)

Median of three trials, Windows 11 26200, bundled IDD:

| | median | trials |
|---|---:|---|
| in-ladder mode change, resolution unchanged | 111 ms | 89 / 111 / 112 |
| in-ladder mode change, new resolution | 308 ms | 296 / 308 / 381 |
| monitor departure alone | 179 ms | 178 / 179 / 187 |
| remove + recreate + reattach | 491 ms | 459 / 491 / 501 |
| then set the new mode | 281 ms | 276 / 281 / 333 |

A geometry inside the pre-registered ladder costs one mode change; one outside it costs ~772 ms
plus a desktop-wide window reflow and a capture-session rebuild that no stopwatch sees. See
`docs/OPTIMIZATIONS.md`, "Needs a driver rebuild, and what it is worth".

### Standalone encoder measurement (unit test, `cargo test --lib x264_encoder -- --nocapture`)

```
x264 smoke OK: 120 frames 1920x1080@60, 1228852 bytes, achieved 4915 kbps (ratio 0.61),
max_nal=13100B (ceiling 133332B), p95_encode=4537us (budget 16666us), wall=3334ms
```

x264 at 1080p60 spends 4.5 ms at p95 inside the encoder against a 16.7 ms frame budget — it
fits, with room. The `live` row above is higher because that display is 360 Hz, so the encoder
is configured for a 2.8 ms budget it cannot meet.

## Matrix — macOS, capture and encode (10.15.8 Intel, CGDisplayStream + VideoToolbox)

**Dated to the W5 branch, not to this document's baseline commit.** Every macOS cell was empty when
this baseline was taken, for a reason that was itself a defect: with a still screen the capture
backend never delivered a first frame and the pipeline had nothing to repeat, so the self-test
connected and sent nothing. Both halves are fixed (the first frame of a session is exempt from the
dirty-rect skip; the encode loop synthesizes a black IDR after 750 ms of nothing), and these are
the first numbers this machine has produced.

The dev Mac is the **slowest configuration the code supports**: no ScreenCaptureKit, no
`EnableLowLatencyRateControl`, no long-term reference frames, and a VideoToolbox implementation
that holds frames. A modern Mac should beat every number here.

Method: release build (`lto = "fat"`, `codegen-units = 1`), the display's own 60 Hz,
`caffeinate -u -t 5` before each trial and the trial itself under `caffeinate -dims`, three trials,
median reported. Machine load during the run: the Mac's own 4 cores, no other cargo build running
(checked with `ps -eo comm | grep -E '/cargo$|rustc$'` — the shared target directory is used by
five agents and a fat-LTO link next door is worth ~20 minutes of contention), no user at the
console, reached over SSH. Trials are sequential within one process invocation, so they are
≥15 s apart; the three E2E trials agreed to within 0.26 ms at p50. The animated content is the probe's own cursor animation, which is what makes
WindowServer produce dirty rects at all — the machine is reached over SSH and has no user at it.

```sh
cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate            -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate_downscaled -- --ignored --nocapture
```

### Capture (`cgds::tests::capture_rate`, 5 s, 1440x900@60)

| Capture size | delivered (fps) | dropped by WindowServer | callback mean | displayTime → callback p50 / p90 / p99 / max |
|---|---|---|---|---|
| native 1440x900 | 195 (39.0) | **0** | 311.0 µs | −15.53 / −15.12 / −11.97 / −11.42 ms |
| half 720x448, same process | 196 (39.2) | **0** | 88.3 µs | −15.33 / −14.76 / −11.23 / −11.11 ms |
| half 720x448, alone | 292 (58.4) | **0** | 121.4 µs | −15.57 / −15.29 / −14.80 / −3.88 ms |

Three things to read off this table.

**WindowServer drops are zero everywhere.** That is the whole point of the queue-depth change:
before it, two of two queued surfaces were pinned and the stream had nothing free to composite
into. The counter is now read on every delivery (`CGDisplayStreamUpdateGetDropCount`) rather than
assumed, so zero is a measurement.

**The callback cost is proportional to the output size, not to the frame count** — 311 µs at
1440x900 against 88 µs at 720x448 in the same process, a 3.5× drop for a 4× smaller surface.
Whatever the delivery callback spends its time on, it is not the three per-frame heap allocations:
those are size-independent. It also means asking CGDisplayStream for a smaller output really does
move work off this process, which is what the downscale question was about.

**`displayTime` is ahead of the callback, by almost exactly one frame period.** Every percentile
on every row is negative and tightly banded around −15.5 ms at 60 Hz. It is a scheduled
presentation time, not the moment the frame was composited, so anything that treats it as "when
this frame was captured" understates host latency by a frame. Recorded as
`findings/major-cgds-displaytime-is-a-future-presentation-time.md`.

### Capture → encoded (`pipeline::tests::end_to_end_latency`, 6 s, 1440x900@60)

| Build | captured (fps) | WindowServer drops | encoder output drops | capture → encoded p50 / p99 / max |
|---|---|---|---|---|
| before the VideoToolbox work (commit `719891df`, CGDS surface fixes only) | 61 (365 / 367 / 366 frames in 6 s) | 0 / 1 / 0 | not counted yet | **13.70 / 15.60 / 33.08** |
| after (commit `ef21bea4b`) | 62 (373 / 371 / 370 frames in 6 s) | 0 / 0 / 0 | **0 / 0 / 0** | **13.61 / 15.06 / 20.64** |

Medians of three trials, each 6 s, 360 frames submitted every time. The p50 barely moves — 13.70 →
13.61 ms — and that is expected: the median frame was never the problem. **The tail is the
result.** The before-run's worst trial had a p99 of 40.9 ms and a max of 66.9 ms; the after-run's
worst p99 is 15.3 ms and its worst max 31.5 ms, with the other two trials at 15.1 and 20.6 ms.
Two and a half frame periods of jitter came out of the 99th percentile.

Three supporting counters, all new and all zero: `windowserver_drops` (the capture side is not
starving), `output_drops` (the bounded encoder output channel never had to discard), and
`vt_pending_at_flush = Some(0)` (VideoToolbox is holding nothing when the session ends, so the
in-flight cap and the drain thread agree with the encoder's own count).

### Whole host path (`probe --whep-selftest`, 30 s)

**PASSED**: 1098 RTP packets over 30 s, `capture=cgdisplaystream encoder=videotoolbox-hardware`.

| Stage | p50 / p90 / p99 / max (ms) |
|---|---|
| encode | 201.33 / 201.33 / 201.33 / 221.87 |
| packetize | 0.09 / 0.12 / 0.33 / 0.57 |
| socket_write | 0.36 / 0.72 / 3.15 / 3.72 |
| pace_wait | 0.00 / 0.00 / 0.00 / 0.00 |
| capture → socket | 184.55 / 201.33 / 201.33 / 207.04 |

**Read the 201 ms as the keepalive, not as latency.** 156 of 161 frames in that window are
`frames_repeated`: the self-test runs against a machine with no user at it and a completely still
screen, so after the first real frame the 200 ms keepalive is what produces every subsequent one,
and a repeated frame carries the original capture instant. The number to take from this row is
that the path works end to end on a still display at all — which is the defect that made every
macOS cell in this document empty until now — plus `packetize`, `socket_write` and `pace_wait`,
which are real and are collectively under a millisecond at p50.

For actual encode latency on this machine use the capture → encoded table above (13.61 ms p50),
where the content is animated.

Other results from the same run (`src-tauri/target/w5-measure.sh`, log `/tmp/w5-measure.log`):

- **SPS VUI**: `max_num_reorder_frames = Some(0)`, `matrix_coefficients = 1`,
  `colour_primaries = 1`, `video_full_range_flag = false`. The decoder is told to hold zero frames
  before output, which is the half of `AllowFrameReordering = false` that a decoder actually obeys.
- **Hardware ladder, 2019 Intel**: hardware H.264 succeeds at 1280x720, 1024x576, 960x540,
  854x480, 768x432, 720x480 and **640x360**, and fails with `OSStatus -12915` at 576x324 and every
  size below. The threshold on this machine is width ≥ 640, exactly the disjunction Chromium
  documents (`width ≥ 640 OR height ≥ 480`) — 640x360 passes on width alone with a height well
  under 480.
- **Encoder instance limit**: 6 simultaneous hardware sessions created successfully, so the
  scarce-resource path is not reachable here and `instance_limited` awareness stays
  `COMPILE-CHECKED`.
- **Synthetic encode**: 59 frames / 466,139 bytes, frame 0 NALs `[7, 8, 6, 5]` (SPS, PPS, SEI,
  IDR), keyframes at indices `[0, 30]`.
- **Production pipeline smoke**: 299 `EncodedFrame`s / 3,215,113 bytes broadcast, 1 IDR,
  submit → broadcast p50 18.14 ms, max 19.87 ms.
- **dyld floor**: 540 undefined symbols, **0 offenders**, 0 weak imports, 0 allowlisted — every
  strongly bound undefined symbol exists at 10.15. `nm -u | grep -i displaystream` is empty, which
  is the guard that the obsoleted capture API stays behind `dlsym`.

## What the numbers say

1. **NVENC is not the bottleneck; capture delivery is.** On the static path the encoder returns
   in 1.18 ms at p90 and the whole host path is 8.39 ms at p90 — of which capture delivery is
   4.19 ms, half the budget, spent before we touch the frame. On the live 1080p path capture
   delivery is 9.44 ms at p90 against a 15.73 ms total. The compositor-to-callback hop is the
   largest single host-side term on every row.
2. **Quick Sync is on the wrong path and it costs.** Every Intel row reports
   `encoder=intel-cpu-bridge`, never the same-adapter path: cross-adapter Quick Sync is
   unimplemented, so on this hybrid machine each frame makes a GPU→CPU→GPU round trip
   (`findings/major-intel-cross-adapter-unimplemented-forces-cpu-bridge.md`). Measured cost:
   capture delivery 3.15 ms p50 versus 0.98 for the other two backends, and a capture→socket p50
   of 11.53 ms — **7x NVENC's 1.57 ms**. It is the slowest of the three and the only one whose
   p50 exceeds a 60 Hz frame period.
3. **x264 beats Quick Sync here outright**, on every percentile, and delivers the highest frame
   rate of the three (84 RTP/s). The software fallback is not the slow option on this machine;
   the misconfigured hardware one is.
4. **Two thirds of every stream is repeats.** 140-144 of ~220 frames on the static path, and
   119-141 of ~180 even on a live desktop. The keepalive is doing most of the work, which is
   correct for a still screen but means the frame counts are not frame *rates*.
5. **Only 2 IDRs in 30 s on the ring path**, both at session start — corroborating
   `findings/blocker-nvenc-ring-ignores-idr-on-static-desktop.md`. A client joining or recovering
   from loss mid-session would have had nothing to decode from.
6. **The `encode` p50 of 0.00 ms on the NVENC ring is real, not a bug**: the ring decouples
   submit from encode, so the measured span is the submit call, and the encode itself is
   overlapped. The p99 of 1.44 ms is the useful number.

## What could not be measured

Recorded honestly rather than left blank.

| Cell | Why |
|---|---|
| **Every DXGI cell** | Two separate defects, both found by trying: `DuplicateOutput` returns `DXGI_ERROR_UNSUPPORTED` on the primary display of this hybrid laptop (`findings/blocker-dxgi-duplicateoutput-unsupported-on-hybrid-graphics.md`), and against a fresh virtual display `AcquireNextFrame` dies with `DXGI_ERROR_INVALID_CALL` after repeated re-duplication (`findings/blocker-dxgi-acquirenextframe-invalid-call-on-a-fresh-virtual-display.md`). Combined with `findings/blocker-wgc-min-update-interval-forces-dxgi.md`, a Windows 10 hybrid laptop has **no** working capture path. |
| **Client arrival → paint, and glass-to-glass** | Needs a real browser, and browser automation was out of scope for this run. The apparatus is in place and tested (`node scripts/test-client-stats.mjs`, 21 assertions): the client reports its own per-frame stages over `OP::STATS` and the host stores them per session. The numbers arrive the moment a device joins. |
| **Every macOS cell, at the time this baseline was taken** | The self-test created the virtual display, started CGDisplayStream, initialized VideoToolbox and then sent nothing at all: with a static screen the capture backend never delivered a first frame, and the keepalive can only repeat a frame it already has. `probe --live` on the same box wrote 2 frames in 5 s for the same reason. **Both causes are now fixed** — the first frame of a session is exempt from the dirty-rect skip, and the encode loop synthesizes a black IDR after 750 ms of nothing — so the macOS matrix below is no longer empty. It is dated to the commit that filled it rather than to this document's baseline commit, and says so. The macOS `probe_live` still drives the encoder directly rather than through the pipeline, so it collects no stage timings. |
| **AMD** | No AMD GPU on either machine. See `docs/manual-tests/amd-amf.md`. |
| **Quick Sync at other geometries** | The 1280x720 encoder initializes and encodes; the extreme-fps and fused-downscale configurations report "no Intel hardware oneVPL implementation" from the *same* adapter — `findings/major-intel-quicksync-partly-unavailable-on-this-box.md`. |
| **Audio stages** | The Windows capture timestamp now comes from the endpoint hardware rather than encode time, but verifying it needs a client with system audio enabled; COMPILE-CHECKED, not measured. |
| **macOS WindowServer drop count** | `CGDisplayStreamUpdateGetDropCount` is now read on every delivery and `probe_main_display` prints it, which is the instrument the three open CGDisplayStream questions (`queueDepth`, `minimumFrameTime`, WindowServer contention) each need. Taking the number requires a 30 s run against an *animated* screen on the 10.15.8 box, and the expected signature is documented with the IOSurface use-count finding: near zero, then a spike, then near zero. COMPILE-CHECKED here; the run has not been done. |
| **Reordered input** | `input_reorder_drops` cannot move until a client sends the optional fast-channel sequence (`findings/minor-client-input-does-not-send-the-fast-channel-sequence.md`), and confirming it needs an induced-reorder link (`clumsy`, or a TURN relay path). The host-side comparison is unit-tested across the `u16` wrap. |

## Competitor baselines

None of these were run on this hardware. Each is the best independently measured figure
available, or the vendor's own where nothing better exists, with its source. They say what
"good" looks like; they are not a comparison that was performed. Marked *vendor* where the only
source is the seller, and *adversarial* where the source is a competitor's bench.

| Product | Glass-to-glass (ms) | Basis | Source |
|---|---|---|---|
| Parsec | 4–8 (LAN, 240 fps) | *vendor*, not independently reproduced | [Parsec](https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5) |
| Steam Link | 13–16 display latency at <1 ms ping | user reports | [Steam discussions](https://steamcommunity.com/app/353380/discussions/0/483367798505548057/) |
| Luna Display (LIQUID 3.0) | 11.3 mean USB, 22.4 mean Wi-Fi (50 samples) | *vendor* bench | [Astropad](https://astropad.com/blog/comparing-latency-luna-display/) |
| Moonlight / GameStream | 20–35 on a good LAN | community measurement | [Moonlight](https://moonlight-stream.org/) |
| Apple Sidecar | "consistently below 30" wired USB-C | reported | [Sidecar explainer](https://www.imore.com/ipad-new-mac-display-sidecar-explained) |
| Meta Air Link | 25–50 claimed; 59.1 measured | review | [mixed-news](https://mixed-news.com/en/meta-quest-2-pc-vr-streaming-with-air-link-latency-review/) |
| Air Display | 52.9 USB / 131.4 Wi-Fi | *adversarial* bench | [Astropad](https://astropad.com/blog/comparing-latency-luna-display/) |
| Duet Display | 95 USB / 108 Wi-Fi at 60 fps | *adversarial* bench | [Astropad](https://astropad.com/blog/comparing-latency-luna-display/) |
| Generic WebRTC remote control (Hopp) | 98 (good net, 1080p) after tuning libwebrtc, from 159 | published measurement | [gethopp](https://www.gethopp.app/blog/latency-exploration) |
| Miracast / Wi-Fi Display | 80–100 optimized; **spec ceiling 250** | certification spec | [Wi-Fi Miracast overview](https://tools.barco.com/kb-downloads/4814/Wi-Fi_CERTIFIED_Miracast_Technical_Overview_20170725.pdf) |

The two that matter for positioning are Luna Display (the same job — a tablet as a second
display, over Wi-Fi, 22.4 ms) and Hopp (the same transport — browser WebRTC — at 98 ms). A
browser client cannot get below its own decode and compositor cost, so Hopp is the honest
ceiling and Luna the target. ScreenExtend's host half is 8.39 ms at p90 on the fast path, which
leaves most of a Luna-class budget for network and client.

## Glass-to-glass procedure (repeatable)

The host telemetry stops at the socket and the client telemetry starts at the first byte, so
once the clock offset is established the two halves already sum to a glass-to-glass estimate
(see `docs/TELEMETRY.md`). The camera method is the independent check on that estimate — the
only measurement that includes the client's compositor and its panel.

1. Run the host with the frame counter:
   `ScreenExtend probe --live --duration 60 --synthetic-pattern --workload motion --counter`.
   The overlay is a large frame number plus an elapsed-millisecond clock drawn into the encoded
   frames themselves, so whatever the client displays carries the value the host encoded.
   **Caveat:** `--counter` paints into the *synthetic* generator, which bypasses screen capture
   (`findings/minor-probe-live-workloads-do-not-exercise-real-desktop-capture.md`), so the camera
   number excludes capture delivery. Add the measured `capture_delivery` p50 for the backend
   under test to compare like with like — on this machine that is 0.98 ms static, 6.29 ms live.
2. Put the client device beside the host display, both showing the same region.
3. Film both screens in one shot at 240 fps (a recent phone in slow-motion mode is enough).
4. Step through the recording. For each sample, subtract the counter shown on the client from the
   counter shown on the host and multiply by the frame period. Take at least 30 samples.
5. Report p50 / p90 / p99 alongside the camera model, its frame rate, and both refresh rates. A
   240 fps camera quantises to 4.17 ms; that is the floor on this method's resolution and should
   be stated with the result.

## Reproducing this table

```sh
# one cell
ScreenExtend probe --whep-selftest --duration 30 --encoder nvidia --capture-backend wgc

# the whole matrix, and read it back from a running host
ScreenExtend serve -v          # one summary line per session every 5 s
ScreenExtend status --json     # the same numbers, per session, from another shell
```

Run cells in separate processes and give a failed cell a few seconds before the next one: a
probe that exits without tearing down leaves its virtual display attached briefly, which breaks
the following run's WGC capture — `findings/minor-probe-leaves-its-virtual-display-behind-and-breaks-the-next-run.md`.
That is how the `live-nvidia-wgc` row above had to be re-measured, and the only reason the
interference was noticed.
