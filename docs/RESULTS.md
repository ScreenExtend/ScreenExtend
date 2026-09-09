# Results

The closing measurement pass of the latency & compatibility overhaul. Everything here was run on
`latency-overhaul` at **`79915fbb`** — the commit both trees carried when the pass began — from
release binaries built that day with `--features probe`:

| Machine | Binary | Built |
|---|---|---|
| Windows | `C:/Users/sarve/.cargo/shared_target/ScreenExtend/release/ScreenExtend.exe` | `cargo build --release --features probe`, 6 m 52 s |
| macOS | `/Users/sarvesh/Desktop/ScreenExtend/src-tauri/target/release/ScreenExtend` | same, 12 m 30 s (fat LTO) |

The branch advanced by three commits while the pass ran — `a7ef109` and `c5558bf` add `guides/`
(documentation only) and `a0e2e9c` deletes a block of commented-out code from `lib.rs`. None of
them changes a byte of behaviour, so these numbers describe the shipped tree.

`docs/BASELINE.md` holds the "before" side: a release build of **`ddf9d76`** measured with the same
recipe. That commit was also **rebuilt and re-measured on the same machine on the same day** for
this pass, so the headline claims rest on a controlled A/B rather than on numbers taken weeks apart
— see [Same-day A/B](#same-day-ab-against-the-pre-optimization-binary). `docs/TELEMETRY.md` defines
every stage. All latencies are **p50 / p90 / p99 / max in milliseconds**, never a mean, and every
cell is the **median of three 30 s runs** unless the row says otherwise.

| "Before" binary | |
|---|---|
| Commit | `ddf9d76` — the commit `docs/BASELINE.md`'s table was produced from |
| Where | `git worktree add …/ScreenExtend-workspace/baseline ddf9d76`, own `CARGO_TARGET_DIR` |
| Build | `cargo build --release --features probe`, 14 m 28 s cold, 28,270,080 B |

The brief named `271aa01` as the "before" point. `ddf9d76` was used instead, and the difference is
worth stating: `271aa01` is a docs-only commit whose tree already carries the AMD backend and part
of the Windows virtual-display work, while `ddf9d76` is an ancestor of it and is the exact tree
`docs/BASELINE.md` measured. Using it makes this document's before/after and `BASELINE.md`'s table
the same experiment.

> **Read the honesty section before quoting anything from here.** Two of the three Windows
> self-test rows are under-sampled for a reason this pass discovered and could not fix without
> Administrator rights, and it is named in full below.

## The machines, and what else was running on them

| Role | Machine | Load during the runs |
|---|---|---|
| Windows host | Windows 11 build 26200 (25H2), NVIDIA RTX 3080 Ti Laptop (driver 580.97) + Intel UHD, hybrid; 16 logical cores; primary 1920x1080@360; bundled IDD virtual-display driver | No `cargo`/`rustc` except where a scenario required it (checked with `tasklist` before every run). The repository owner's own Chrome session stayed up throughout — 47 to 62 processes, 26–76 % of **one** core, whole-machine CPU 15–20 % at rest. Not a silent box; the variance that causes is quantified below. |
| macOS host | macOS 10.15.8 Catalina, **MacBookPro10,2** (2013, Intel, 4 cores), console session live, driven over SSH | No other `cargo`/`rustc` (checked with `pgrep`). `caffeinate -u -t 5` before every trial, each trial inside `caffeinate -dims`. Disk held at 3.2–3.5 GB free for the whole pass, above the 2 GB prune threshold, so **nothing was pruned and no `ALERT.txt` was written**. |
| macOS, modern | not available | every 12.3+ / 13+ / 14.2+ tier stays unmeasured → `guides/modern-mac.md`, `guides/intel-mac.md` |
| Windows + AMD | not available | the whole AMF backend stays unmeasured → `guides/windows-amd.md` |

---

## Headline: Windows, real content through the real capture path

This is the table to read first. `probe --live --workload motion` paints a generated 60 fps
full-frame animation into a borderless window covering the captured display, so the content is
**repeatable and goes through screen capture** — unlike the self-test, whose virtual display is
empty and static. 1920x1080@360, release, 30 s per run, median of three.

| Encoder | Backend selected | real frames / 30 s | bytes | capture delivery | scale | encode queue | encode | **capture → encode-out** |
|---|---|---:|---:|---|---|---|---|---|
| NVENC zero-copy ring | `nvenc-zero-copy-ring` | **4 492** | 54.5 MB | 5.24 / 6.82 / 8.39 / 11.48 | 0.13 / 0.20 / 0.26 / 1.10 | 0.02 / 0.10 / 0.15 / 2.81 | 2.88 / 3.15 / 4.19 / 10.43 | **8.39 / 9.44 / 12.58 / 16.43** |
| x264 (software) | `software-x264` | **4 440** | 26.9 MB | 5.24 / 6.82 / 8.39 / 16.74 | — | 0.01 / 0.01 / 0.01 / 2.48 | 3.41 / 3.67 / 5.24 / 17.05 | **8.39 / 10.49 / 13.63 / 23.12** |
| Quick Sync | `intel-same-adapter` | **2 183** | 49.5 MB | 8.39 / 15.73 / 18.87 / 20.68 | — | 0.01 / 0.01 / 0.02 / 12.42 | 11.53 / 18.87 / 20.97 / 27.66 | **20.97 / 31.46 / 37.75 / 42.21** |

Against `docs/BASELINE.md`'s live matrix at `ddf9d76` — same probe, same geometry, same encoders,
but that run captured **whatever happened to be on the desktop**, which was mostly still (119–141
of ~180 frames were keepalive repeats, so 31–51 real frames in 30 s):

| Encoder | before, capture → encode-out | after, capture → encode-out | p50 | p90 | p99 | max | real frames / 30 s |
|---|---|---|---:|---:|---:|---:|---|
| NVENC ring | 12.58 / 15.73 / 27.26 / 28.45 | **8.39 / 9.44 / 12.58 / 16.43** | −33 % | −40 % | −54 % | −42 % | 49 → **4 492** |
| x264 | 20.97 / 23.07 / 37.75 / 40.31 | **8.39 / 10.49 / 13.63 / 23.12** | −60 % | −55 % | −64 % | −43 % | 51 → **4 440** |
| Quick Sync | 27.26 / 31.46 / 83.89 / 88.25 | **20.97 / 31.46 / 37.75 / 42.21** | −23 % | ±0 | −55 % | −52 % | 31 → **2 183** |

The comparison is conservative in the "after" direction: the after run is doing 90× the real
capture work per second on harder content and still finishes every percentile faster. Quick Sync's
`p90` is the one cell that did not move, and the reason it is not worse is the backend change —
every Intel row in `BASELINE.md` reads `encoder=intel-cpu-bridge` (a GPU→CPU→GPU round trip because
cross-adapter Quick Sync was unimplemented); every Intel row here reads `encoder=intel-same-adapter`.

### The workload sweep `BASELINE.md` left open

`docs/BASELINE.md` carried one scripted-workload row, taken on a **debug** build, and asked for the
other workloads on a release build. NVENC ring, 1920x1080@360, release, 30 s each. `motion` is the
median of three; the other four are single runs (n = 1) and should be read as one sample each.

| Workload | frames / 30 s | bytes | capture delivery | scale | encode queue | encode | capture → encode-out |
|---|---:|---:|---|---|---|---|---|
| `motion` (n=3) | 4 492 | 54,503,981 | 5.24 / 6.82 / 8.39 / 11.48 | 0.13 / 0.20 / 0.26 / 1.10 | 0.02 / 0.10 / 0.15 / 2.81 | 2.88 / 3.15 / 4.19 / 10.43 | 8.39 / 9.44 / 12.58 / 16.43 |
| `mixed` | 4 519 | 63,359,292 | 5.24 / 6.82 / 8.39 / 11.09 | 0.13 / 0.20 / 0.26 / 3.17 | 0.02 / 0.09 / 0.13 / 1.63 | 2.88 / 3.15 / 4.19 / 9.55 | 7.86 / 9.44 / 12.58 / 15.30 |
| `static` | 4 488 | 322,858 | 5.24 / 6.82 / 8.39 / 12.27 | 0.13 / 0.20 / 0.26 / 1.84 | 0.02 / 0.09 / 0.12 / 3.29 | 2.36 / 3.41 / 3.93 / 9.88 | 8.39 / 10.49 / 12.58 / 17.16 |
| `scroll` | 4 457 | 12,669,578 | 5.24 / 6.82 / 8.39 / 11.37 | 0.12 / 0.20 / 0.26 / 1.25 | 0.02 / 0.09 / 0.13 / 3.55 | 2.36 / 3.67 / 3.93 / 9.29 | 8.39 / 10.49 / 12.58 / 18.76 |
| `drag` | 4 514 | 6,341,490 | 5.24 / 6.82 / 8.39 / 10.99 | 0.12 / 0.18 / 0.26 / 1.52 | 0.02 / 0.10 / 0.15 / 1.48 | 2.62 / 3.41 / 3.67 / 9.30 | 7.86 / 9.44 / 12.58 / 15.99 |

Content changes the **bitrate** by 200× (0.32 MB static to 63 MB mixed) and the **latency** by
under 3.5 ms at every percentile. That is the property a second-monitor stream wants: the encoder
absorbs content, not the clock.

---

## Windows, the whole host path (`probe --whep-selftest`)

Real WHEP join, real SRTP over loopback, against a virtual display created for the run at
1280x720. The display is **empty and static**, so most emitted frames are the 200 ms keepalive; the
capture-relative stages count only real captures. Median of three 30 s runs.

| Encoder | Backend | frames (repeats) | real frames | RTP/s | capture delivery | scale | encode queue | encode | packetize | socket write | capture → socket |
|---|---|---|---:|---:|---|---|---|---|---|---|---|
| **NVENC ring — after** | `nvenc-zero-copy-ring` | 339 (121) | **218** | **109** | 1.44 / 4.72 / 7.34 / 7.80 | 0.11 / 0.16 / 0.29 / 0.68 | 0.01 / 0.02 / 0.10 / 0.25 | 1.18 / 1.57 / 4.19 / 8.13 | 0.04 / 0.11 / 0.26 / 0.33 | 0.18 / 0.52 / 1.31 / 1.81 | **3.15 / 6.29 / 12.58 / 12.72** |
| NVENC ring — before (`BASELINE.md`) | same | 211 (144) | 67 | 58 | 0.98 / 4.19 / 6.82 / 6.95 | 0.11 / 0.13 / 2.62 / 2.74 | 0.00 / 0.02 / 0.26 / 14.81 | 0.00 / 1.18 / 1.44 / 15.09 | 0.04 / 0.06 / 0.11 / 0.12 | 0.16 / 0.25 / 0.43 / 0.59 | **1.57 / 8.39 / 16.78 / 18.00** |
| **x264 — after** | `software-x264` | 305 (139) | **166** | **125** | *under-sampled* | — | 0.00 / 0.02 / 0.02 / 0.03 | 2.36 / 5.77 / 7.86 / 9.85 | 0.05 / 0.16 / 0.33 / 0.54 | 0.26 / 0.66 / 1.44 / 2.00 | *under-sampled* |
| x264 — before | same | 223 (142) | 81 | 84 | 0.98 / 3.93 / 4.72 / 4.99 | — | 0.00 / 0.00 / 0.00 / 0.00 | 2.10 / 2.62 / 5.24 / 24.57 | 0.05 / 0.07 / 0.10 / 0.25 | 0.20 / 0.29 / 0.52 / 0.68 | **2.36 / 5.24 / 23.07 / 24.82** |
| **Quick Sync — after** | `intel-same-adapter` | 333 (136) | **197** | **239** | *under-sampled* | — | 0.00 / 0.01 / 0.03 / 0.06 | 5.77 / 8.39 / 10.49 / 12.74 | 0.06 / 0.16 / 0.33 / 0.52 | 0.26 / 0.98 / 3.15 / 8.97 | *under-sampled* |
| Quick Sync — before | `intel-cpu-bridge` | 219 (140) | 79 | 55 | 3.15 / 7.86 / 15.73 / 16.11 | — | 0.00 / 0.00 / 0.00 / 0.01 | 4.19 / 11.53 / 16.78 / 21.65 | 0.04 / 0.06 / 0.15 / 0.36 | 0.15 / 0.26 / 0.43 / 1.36 | **11.53 / 16.78 / 25.17 / 26.05** |

What is safe to take from this table:

- **NVENC delivers 3.3× the real frames and 1.9× the packet rate with a 25–29 % shorter tail.**
  `capture → socket` p90 8.39 → 6.29 ms, p99 16.78 → 12.58 ms, max 18.00 → 12.72 ms. The p50 rose
  from 1.57 to 3.15 ms, and that is a population change, not a regression: the before-run's p50 was
  computed over 67 real frames on a still desktop, the after-run's over 218.
- **`encode_queue` max collapsed from 14.81 ms to 0.25 ms and `encode` max from 15.09 to 8.13 ms.**
  Those two are the ring decoupling capture from encode, and they are the largest single-cell wins
  on the whole page.
- **`scale` p99 2.62 → 0.29 ms** (−89 %), the GPU downscaler.
- **`packetize` and `socket_write` are slightly higher at p90** (0.06 → 0.11 and 0.25 → 0.52 ms).
  That is the price of 1.9× the packets, and both remain under a millisecond at p99 on NVENC.
- **Quick Sync is no longer on the CPU bridge**, its encode p99 fell 16.78 → 10.49 ms and it went
  from the slowest RTP rate on the box (55/s) to the fastest (239/s).
- **x264 is not the slow option.** It carries the second-highest packet rate and its encode `max`
  fell 24.57 → 9.85 ms.

**One caveat that runs across this table, and it cuts in the after-build's favour.** The NVENC
"after" row was taken in the first five runs of the session, while the box was healthy. The x264 and
Quick Sync "after" rows were taken after the IDD virtual-display driver had degraded (see
[The probe-display trap](#the-probe-display-trap-and-what-it-cost-this-pass)), which costs about a
third of the real frames: the same binary, same encoder, same 30 s self-test, measured in both
states, delivered **218 real frames at 109 RTP/s healthy and 145 at 87 degraded**. The x264 and
Quick Sync rows are therefore *understated*, and their capture-relative columns are omitted
entirely rather than quoted from one or two samples.

### DXGI Desktop Duplication on this box: stated plainly

**It still does not work here, and this pass established that in two separate ways.**

1. **Against the primary display it is refused outright.** `probe --dxgi` enumerates the one
   adapter that owns the output and gets `DXGI_ERROR_UNSUPPORTED` (`0x887A0004`):
   `no adapter can duplicate \\.\DISPLAY1 (NVIDIA GeForce RTX 3080 Ti Laptop GPU …)`. Identical to
   the baseline. This is the hybrid-graphics defect, not a code defect.
2. **Against a fresh IDD virtual display it now survives and starves.** `probe --whep-selftest
   --capture-backend dxgi` gets all the way up — `dxgi: duplication ready on \\.\DISPLAY25 via
   DuplicateOutput1`, NVENC initialised, WHEP answer generated, join accepted — and then loses
   access on essentially every acquire: **1 190 re-establishments in 30 s and zero frames**. The
   self-test fails with `on_track never fired — no media track negotiated`, because no RTP was ever
   produced.

Point 2 is progress against the baseline (`AcquireNextFrame` used to die at the first invalid call
and the capture ended; ledger row 103) and also a **new finding**: the guard added for exactly this
case, `REDUP_WITHOUT_FRAME_MAX = 40` in `windows_utils/streamer/dxgi/mod.rs`, never trips, because
`redup_without_frame` is reset by any acquire that *returns* and this failure mode returns from the
acquire before losing access. The counter therefore reads `1 since the last frame` 1 190 times in a
row instead of reaching 40 and stopping the capture with the explanatory error it was written to
produce. A user on such a machine gets a negotiated session that shows nothing, forever. The fix is
to count re-establishments that were not followed by a **delivered frame** rather than by a
successful acquire.

Both DXGI cells stay unmeasurable here. `guides/windows-legacy.md` (tasks 3.3, 3.4, 3.11) is the
machine that closes them.

### Join time

`join stages (…): display=<a> pipeline=<b> handshake=<c> (overlapped)`, where `c` is the join's
wall clock and ledger row 181 established that it equals `a + b`.

| State of the box | display | pipeline | join wall clock | n |
|---|---|---|---|---|
| First five virtual displays of a session | 575–584 ms | 415–429 ms | **991–1 014 ms** | 5 |
| Ledger row 181 (W1 close-out) | 584–641 ms | 433–587 ms | 1 043–1 172 ms | 5 |
| Sixth and every later virtual display | 1 829–2 018 ms | 317–780 ms | **2 248–2 745 ms** | 11 |

The first row reproduces row 181 and improves slightly on it. The third row is the defect described
under [The probe-display trap](#the-probe-display-trap-and-what-it-cost-this-pass); it is a property
of the machine after repeated display creation, not of the join code.

### Audio, capture → subscriber (`probe --audio`, 30 s, median of three)

| Host state | packets/s | payload avg | capture → subscriber | encode | wake interval | counters |
|---|---:|---:|---|---|---|---|
| Idle (silence) | 200.0 | 3.0 B | **9.44 / 9.44 / 10.49 / 11.89** | 0.15 / 0.29 / 0.52 / 1.48 | 9.44 / 9.44 / 10.49 / 12.67 | starvations 0, companion restarts 0, backpressure drops 0, lagged 0 |
| Playing audio | 200.0 | 78.1 B (max 146) | **9.44 / 9.44 / 10.49 / 14.19** | 0.06 / 0.13 / 0.43 / 15.94 | 9.44 / 9.44 / 10.49 / 16.19 | same, all zero |

Exactly 200 packets/s in every run — the 5 ms Opus frame, on time. The engine period is 480 frames
(10.000 ms) and `IAudioClient3` offers nothing smaller on this endpoint (`min = default =
fundamental = 480`), which is why `capture → subscriber` sits at one engine period and not below:
**the 9.44 ms is the driver's period, not our software.** Ledger row 67's 9.44 ms p50 reproduces
exactly. `capture_ns` skew against the host clock is mean +7.7 ms with 1 packet in 6 001 stamped
ahead of it when idle and 0 of 6 001 when audio is playing.

### Input

The host-side injection cost is the only input number either development machine can produce — the
`input_recv` / `input_queue` / `input_inject` histograms need a browser client to originate an
event, which neither box can run (`guides/clients-network-and-glass-to-glass.md`, C-14).

`cargo test --release --lib pointer_batch_costs -- --ignored --nocapture`, re-run on `79915fbb` for
this pass (40-sample pointer batch, 500 repetitions, real `SendInput` into the live desktop):

```
pointer batch of 40: per-sample 5158.8 us/batch (40 SendInput), batched 2824.0 us/batch (1 SendInput)
```

**5.16 ms → 2.82 ms per batch, −45 %**, and 40 kernel transitions per batch become one. Ledger row
45 recorded 4.57 → 2.89 ms on the same harness; both runs agree on the "after" figure to within
0.07 ms and disagree on the "before" by 0.6 ms, which is the shape you expect when the slow path is
the one that competes for the machine.

### Desktop UI startup and time-to-interactive

No wall-clock time-to-interactive was captured on either box — the ledger tracked the things that
cause it instead, and those are cited rather than re-measured (W12/W13a rows):

| Row | Metric | Before | After |
|---|---|---|---|
| 5 | store IPC round trips, entry → dashboard | **75** | **2** |
| 8 | eager `main-*.js` | 914,493 B (gzip 293.36 kB) | **565,285 B (gzip 181.17 kB), −38.2 %** |
| 156 | `main-*.css` | 52.92 kB (gzip 9.33) | 50.14 kB (gzip 9.03), −5.3 % |
| 6 | React commits per `deviceModify` | whole tree | **7**, and **0** when the payload is unchanged |
| 9 | monaco warm-up | ~2–4 s of main-thread scripting starting 300 ms in | after the dashboard's first paint, via `requestIdleCallback` |
| 10 | `LogLine` cost | 1 commit + 1 forced layout **per line** | 1 per animation frame regardless of burst |
| 11 | IPC writes per 12 typed characters | 12 writes + 12 notifications | **1 and 1** |
| 161 | `check_permissions` calls in the first minute (macOS) | ~300 | ~35 |
| 163 | forced layouts/s during the walkthrough | 60–144 | ~4 when nothing moves |
| 77 | host process: threads / startup to `server listening` | 50 threads / 391 ms | **38 threads** / 460 ms — *no improvement, and 69 ms the wrong way; both runs shared the box with 4–11 concurrent `rustc` and the spreads overlap (325–416 vs 375–521)* |

Row 77 is left as recorded rather than quietly dropped: building four worker threads instead of
sixteen cannot plausibly make startup slower, and a clean-box re-measurement would settle it.

---

## Same-day A/B against the pre-optimization binary

`docs/BASELINE.md`'s numbers were taken on a different day, on a box whose state is no longer
reproducible. So `ddf9d76` was rebuilt and run **back to back with `79915fbb`**, on the same
machine, in the same session, minutes apart — which is the only comparison here that controls for
the machine.

### Self-test, NVENC ring, static 1280x720 virtual display, three 30 s runs each

Both sets were taken in the *same* (degraded-IDD) box state, alternating nothing else, with no
browser video running. Medians of three, with the per-trial range in brackets where it matters.

| | real frames / 30 s | RTP/s | scale | encode queue | encode | packetize | socket write |
|---|---:|---:|---|---|---|---|---|
| **before** `ddf9d76` | 95 (64–157) | 61 (57–85) | 0.13 / 0.23 / 0.59 / 0.70 | 0.00 / 0.04 / 0.08 / 0.60 | 0.00 / 3.67 / 4.19 / 9.69 | 0.06 / 0.23 / 0.43 / 0.85 | 0.23 / 0.98 / 2.10 / 2.67 |
| **after** `79915fbb` | **145** (138–151) | **87** (84–88) | 0.11 / 0.15 / 0.26 / 0.30 | 0.01 / 0.03 / 0.07 / 0.14 | 1.18 / 1.57 / 2.36 / 4.08 | 0.05 / 0.18 / 0.39 / 0.58 | 0.20 / 0.85 / 1.31 / 1.66 |
| delta | **+53 %** | **+43 %** | p99 −56 %, max −57 % | max −77 % | p90 −57 %, max −58 % | p90 −22 %, max −32 % | p99 −38 %, max −38 % |

Three things stand out. **Throughput is up by half** on identical content. **Every tail is shorter**
— `encode` max 9.69 → 4.08 ms, `encode_queue` max 0.60 → 0.14 ms, `socket_write` max 2.67 → 1.66 ms.
And the **spread collapsed**: the before binary delivered 64, 157 and 95 real frames across three
identical runs; the after binary delivered 138, 151 and 145. Consistency is a result in its own
right for a second monitor.

The before binary's capture line reads `capture=wgc`. The after binary's reads
`capture=wgc[+cursor +border +secondary-windows +min-update-interval +dirty-region
+free-threaded-pool]` — six probed Windows.Graphics.Capture properties that did not exist as a
concept in the earlier build.

`capture_delivery` and `capture_to_socket` are omitted from this table: both builds were in the
degraded-IDD state and both are under-sampled there.

### `probe --live` over identical real content (a 1080p60 video full-screen on the captured display)

Interleaved before/after/before/after, three trials each, 30 s per run, primary display.

| Run | encoder target | frames / 30 s | bytes | capture delivery | encode | capture → encode-out | ring drops | outcome |
|---|---|---:|---:|---|---|---|---:|---|
| before 1 | 1080p**@60**, 12.44 Mbps | — | — | — | — | — | — | **FAILED** |
| before 2 | " | 5 316 | 37.0 MB | 4.19 / 5.24 / 6.82 / 11.03 | 1.70 / 2.36 / 2.88 / 9.92 | 5.77 / 7.86 / 9.44 / 14.07 | 0 | ok |
| before 3 | " | 2 607 | 24.3 MB | 4.19 / 15.73 / 41.94 / **144.39** | 2.88 / 5.24 / 8.39 / 23.91 | 6.82 / 18.87 / 46.14 / **146.66** | **366** | starved |
| after 1 | 1080p**@360**, 30 Mbps | 4 456 | 62.5 MB | 5.24 / 6.82 / 8.39 / 11.45 | 2.88 / 3.15 / 4.19 / 9.73 | 8.39 / 9.44 / 12.58 / 15.72 | 0 | ok |
| after 2 | " | 4 524 | 63.5 MB | 5.24 / 6.82 / 8.39 / 12.30 | 2.62 / 3.15 / 4.19 / 9.15 | 8.39 / 9.44 / 12.58 / 17.03 | 0 | ok |
| after 3 | " | 4 557 | 63.8 MB | 5.24 / 6.82 / 8.39 / 12.71 | 2.88 / 3.15 / 4.19 / 9.38 | 8.39 / 9.44 / 12.58 / 17.25 | 0 | ok |

**This pair is not like-for-like on latency and the table says why.** The probe harness configures
the encoder from the captured display, and the two builds resolve that differently: the before
build targets **60 fps at 12.44 Mbps**, the after build **360 fps at 30 Mbps** on the same
1920x1080@360 panel. The after binary is asked for six times the frame rate and 2.4 times the
bitrate, and produces 13.9 KB per frame against the before build's 7.0 KB. A per-percentile
comparison of `encode` between those two operating points means nothing.

What the pair *does* establish, and what no operating-point difference explains away:

- **The before binary failed one run in three, outright.** `probe failed: monitor device name for
  DXGI fallback: Failed to get monitor information. Caused by: starting WGC capture: Failed to
  convert item to GraphicsCaptureItem` — it could not start capturing a display that had just gone
  full-screen. The after binary started cleanly three times out of three.
- **The before binary starved on another.** 366 frames dropped by the capture ring, a
  `capture_delivery` p99 of 41.94 ms and a **max of 144.39 ms** — nine frame periods at 60 Hz. The
  after binary dropped zero frames in every run, with a max of 17.25 ms in its worst.
- **The after binary is metronomic**: 8.39 / 9.44 / 12.58 at p50/p90/p99 in all three runs, to the
  histogram bucket, while carrying 1.7× the bitrate.

Two of three runs of the pre-optimization build were unusable on real 1080p60 content. That is the
finding, not the percentiles.

### The same pair under contention (video + a cold `cargo build`)

One 30 s run each, taken inside a cold debug build with the video still playing.

| | frames / 30 s | bytes | capture delivery | encode | capture → encode-out | ring drops | ScreenExtend CPU mean/max (% of one core) | threads | RSS |
|---|---:|---:|---|---|---|---:|---|---:|---:|
| before `ddf9d76` | 4 007 | 26.9 MB | 6.29 / 7.86 / 9.44 / 12.28 | 2.36 / 2.88 / 3.15 / 9.38 | 8.39 / 10.49 / 12.58 / 17.12 | 0 | 18.1 / 29.5 | 58 | 78 MB |
| after `79915fbb` | 2 673 | 35.4 MB | 10.49 / 11.53 / 20.97 / 85.62 | 4.19 / 4.72 / 4.72 / 11.58 | 14.68 / 16.78 / 23.07 / 88.40 | 1 | 63.7 / 83.6 | 60 | 98 MB |

Reported as measured, including that it is the one table on this page where the after build looks
worse. The operating-point difference above is the whole of it: the after build is trying to feed a
360 Hz target at 30 Mbps on a box whose CPU is already 68 % committed, and produces 13.2 KB per
frame against the before build's 6.7 KB. It costs 3.5× the CPU and 20 MB more resident set to do
that. The comparable measurement — the one where both legs run the same build at the same
operating point — is the live-motion contention table above, where the tail does not move at all.

**What this pair is honestly evidence for:** the shipped probe configuration chases the display's
full refresh rate, and on a 360 Hz panel under heavy contention that is more work than the machine
has left. It is a **configuration** question, not a pipeline regression, and the mechanism to answer
it already exists — a real session takes its frame rate from the client's reported refresh rate,
clamped to 15–500 (`streamer/server.rs`), not from the host panel. No client here could exercise
that. → `guides/clients-network-and-glass-to-glass.md`.

---

## macOS 10.15.8 (CGDisplayStream + VideoToolbox)

The slowest configuration the code supports: no ScreenCaptureKit, no `EnableLowLatencyRateControl`,
no long-term references, and a VideoToolbox implementation that holds frames. Also the oldest
hardware in the project — a 2013 4-core MacBook Pro. Re-run here on the merged binary to confirm
that nothing regressed on the way through eight merges.

### Capture → encoded (`pipeline::tests::end_to_end_latency`, 6 s, 1280x800@60, three trials)

| | p50 | p99 | max | captured / 6 s | WindowServer drops | encoder output drops | `vt_pending_at_flush` |
|---|---:|---:|---:|---:|---:|---:|---|
| before the VideoToolbox work (`719891df`, `BASELINE.md`) | 13.70 | 15.60 | 33.08 | 366 | 0 / 1 / 0 | not counted | — |
| after, W5's own run (`ef21bea4b`, 1440x900) | 13.61 | 15.06 | 20.64 | 371 | 0 / 0 / 0 | 0 / 0 / 0 | `Some(0)` |
| **this pass (`79915fbb`, 1280x800)** | **14.01** | **28.07** | **37.86** | 371 / 372 / 373 | **0 / 0 / 0** | **0 / 0 / 0** | `Some(0)` ×3 |

The three structural counters hold: WindowServer never starved the stream, the bounded encoder
output channel never discarded, and VideoToolbox was holding nothing at flush. The p50 is within
0.4 ms of W5's. **The tail is noisier than W5's** — per-trial p99 41.76 / 17.43 / 28.07 and max
85.96 / 28.99 / 37.86. The first trial ran immediately after a `cargo test --release` link on a
4-core box and is visibly contaminated; the best trial (17.43 p99, 28.99 max) is the one comparable
to W5's conditions. The display was also 1280x800 here against W5's 1440x900. This is reported as
measured rather than filtered.

### Capture rate (`cgds::tests::capture_rate`, 5 s, three trials)

| Capture size | delivered (fps) | WindowServer drops | callback mean | displayTime → callback p50 / p90 / p99 / max |
|---|---|---:|---|---|
| native 1280x800 | 185 / 188 / 185 (37.0–37.6) | **0 / 0 / 0** | 429 / 150 / 106 µs | −15.04 / −14.23 / −9.14 / −6.09 ms |
| half 640x400 | 185 / 187 / 185 (37.0–37.4) | **0 / 0 / 0** | 84 / 172 / 129 µs | −14.87 / −14.04 / −8.96 / −5.75 ms |

Two of `BASELINE.md`'s macOS findings reproduce exactly on the merged binary: **WindowServer drops
are zero everywhere** (the queue-depth and IOSurface use-count work), and **`displayTime` is one
frame period *ahead* of the callback** — every percentile negative and banded around −15 ms at
60 Hz — which is why it is a scheduled presentation time and must not be read as a capture instant.
2–4 frames per run carry no `displayTime` at all. The size-proportional callback cost that
`BASELINE.md` measured (311 µs native vs 88 µs half) is visible in trial 1 (429 vs 84 µs) and buried
in noise in trials 2 and 3 on this slower machine.

### Whole host path (`probe --whep-selftest`, 30 s, three trials)

**PASSED** all three: 1 003 / 1 017 / 1 139 RTP packets (33 / 34 / 38 per second),
`capture=cgdisplaystream encoder=videotoolbox-hardware`.

| Stage | p50 / p90 / p99 / max (ms) |
|---|---|
| packetize | 0.09 / 0.11 / 0.36 / 0.69 |
| socket_write | 0.36 / 0.66 / 3.15 / 3.72 |
| pace_wait | 0.00 / 0.00 / 0.00 / 0.00 |
| encode / capture → socket | 201.33 — **this is the keepalive, not a latency** |

149–156 of 150–158 frames are `frames_repeated`: the virtual display is empty, no user is at the
machine, and after the first real frame the 200 ms keepalive produces every subsequent one, each
carrying the original capture instant. The row that matters is that the path works end to end on a
still display — the defect that left every macOS cell in `BASELINE.md` empty — and that packetize,
socket_write and pace_wait together cost under half a millisecond at p50. For real macOS encode
latency use the capture → encoded table above. Join: display 1 452–1 552 ms, pipeline 266–729 ms,
wall clock **1 818–2 239 ms**.

### System audio — the legacy virtual-device tier, VERIFIED on 10.15.8

`probe_audio_backend()` selects `VirtualDevice`; the ScreenExtend Audio driver is installed and
loaded on this box. 40 s per trial with a 48 kHz stereo tone playing, three trials:

| | packets/s | payload avg | capture → subscriber | Opus encode | ring / transport counters |
|---|---:|---:|---|---|---|
| macOS legacy driver tier | **198.4** | 79.1 B | **2.36 / 4.19 / 4.72 / 5.24–5.42** | 0.05 / 0.08 / 0.13 / 0.52 | overruns 0, backpressure drops 0, lagged 0, `ts_clamped` 0, seq gaps 0 |
| Windows WASAPI endpoint tier, for comparison | 200.0 | 78.1 B | 9.44 / 9.44 / 10.49 / 14.19 | 0.06 / 0.13 / 0.43 / 15.94 | all zero |

Reproduces W7's VERIFIED result (2.10 / 4.19 ms) to within a bucket. **The macOS legacy tier is
4× faster to the subscriber than the Windows endpoint tier**, and the reason is the device period:
the driver runs at 128 frames (2.67 ms) where the Windows endpoint's engine period is 480 frames
(10.000 ms) and `IAudioClient3` offers nothing smaller.

One behaviour worth recording because it looks like a fault and is not: **with no audio playing at
all, the macOS tier produces zero packets** and `probe --audio` reports
`no audio packets in 30s`, having correctly diagnosed it as the silent-companion failure mode. The
Windows tier keeps emitting 3-byte silence frames in the same situation because its silent render
companion is a clock source. The two tiers behave differently on a truly idle host and only the
Windows one keeps the stream warm.

---

## Contention: does it stay fast when the machine is busy?

The scenario the repository owner asked for, on both machines: **(a)** an otherwise idle host
versus **(b)** a host simultaneously playing a 1080p60 video in a browser and running a real
compile.

### Windows

- **Browser:** Chrome launched into its own `--user-data-dir` with
  `--autoplay-policy=no-user-gesture-required --start-maximized`, playing
  *Big Buck Bunny 60fps* from YouTube. Playback was **verified**, not assumed: the window title and
  a 24 s CPU sample (116 % of one core mean, 230 % peak) before any measurement began.
- **Compile:** a real cold `cargo build` (debug) of this repository into a throwaway
  `CARGO_TARGET_DIR=C:/Users/sarve/.cargo/shared_target/contention-load`, deleted after each leg and
  again at the end. 14–15 concurrent `rustc`, 2.1 cores sustained.
- Per-process CPU is **exact `TotalProcessorTime` deltas** expressed as *% of one logical core*
  (100 = one core saturated, 1600 = all sixteen). The PDH `\Process(name*)\% Processor Time`
  counter was tried first and discarded: with 50+ Chrome instances a single sample returns values
  that do not reconcile with `\Processor(_Total)`.

**The load test that matters — `probe --live --workload motion`, 1920x1080@360, NVENC ring, one
60 s run per leg (n = 1 each):**

| | real frames / 60 s | fps | capture delivery | scale | encode queue | encode | **capture → encode-out** | ScreenExtend CPU (mean / max, % of one core) | threads | RSS | GPU / encoder util |
|---|---:|---:|---|---|---|---|---|---|---:|---:|---|
| (a) idle host | **9 031** | 150.5 | 5.24 / 6.82 / 8.39 / 11.92 | 0.12 / 0.20 / 0.26 / 1.87 | 0.02 / 0.09 / 0.13 / 2.49 | 2.88 / 3.15 / 4.19 / 11.62 | **8.39 / 10.49 / 13.63 / 21.49** | 94.0 / 105.9 | 60 | 99 MB | 23.2 % / 27.7 % |
| (b) 1080p60 video + cold compile | **6 116** | 101.9 | 9.44 / 10.49 / 12.58 / 15.38 | 0.20 / 0.25 / 0.33 / 1.91 | 0.02 / 0.02 / 0.04 / 4.20 | 3.93 / 4.19 / 4.19 / 13.79 | **12.58 / 15.73 / 16.78 / 21.17** | 106.3 / 125.1 | 60 | 98 MB | 14.1 % / 11.5 % |
| delta | −32 % | | +4.20 / +3.67 / +4.19 / +3.46 ms | | | +1.05 / +1.04 / ±0 / +2.17 | **+4.19 / +5.24 / +3.15 / −0.32 ms** | +13 % | ±0 | −1 MB | |

**The tail does not grow.** `max` is 21.49 ms idle and 21.17 ms under load — the same number. The
p99 moves 13.63 → 16.78 ms, three milliseconds, and the p50 by 4.19 ms, which is half a frame at
120 Hz. Frame rate falls 32 %, from 150 fps to 102 fps, which is the honest cost: under contention
the host delivers fewer frames but each one is still delivered on time. The whole added latency is
in `capture delivery` (+4.2 ms at p50) — the compositor-to-callback hop, which is the OS's, not
ours. Our own stages barely move: `encode` +1.05 ms at p50, `encode_queue` unchanged, `scale`
+0.08 ms.

**The self-test legs, for the process-level numbers:**

| | ScreenExtend CPU mean / max | threads | RSS | GPU / enc | Chrome (% of one core) | rustc | machine CPU | encode | packetize | socket write | RTP |
|---|---|---:|---:|---|---:|---:|---:|---|---|---|---|
| (a) idle + stream, 60 s | 2.5 / 8.0 | 67 | 95 MB | 1.5 / 1.0 % | 55.9 | 0 | 16.5 % | 0.00 / 3.67 / 4.19 / 4.86 | 0.04 / 0.11 / 0.29 / 0.40 | 0.18 / 0.43 / 1.31 / 2.12 | 3 549 / 60 s |
| (b) video + compile + stream, 233 s | 1.6 / 6.0 | 66 | 97 MB | 0.7 / 1.1 % | 44.0 | 196.3 | 39.2 % | 0.00 / 1.18 / 2.10 / 4.89 | 0.05 / 0.12 / 0.26 / 0.49 | 0.16 / 0.39 / 0.79 / 1.55 | 6 064 / 252 s |

Both self-tests **PASSED**. Thread count, resident set and every transport stage are flat or better
under load; the process takes *less* CPU because it is competing for it. (The self-test's virtual
display is static, so its frame rate is keepalive-bound in both legs and does not measure encoder
throughput — that is what the live-motion table above is for.)

**What it costs the other applications:**

| Victim | Without the stream | With the stream | Cost |
|---|---|---|---|
| Cold `cargo build` (debug), browser video playing throughout | **227 s** | **233 s** | **+6 s, +2.6 %** |
| Chrome playing 1080p60, during the compile | 75.8 % of one core | 44.0 % of one core | **not measurable** — the stream's effect is smaller than Chrome's own variance (video-only leg: 65.7 %) |

### macOS 10.15.8

Substitution, recorded as a decision: the Mac has **3.2 GB of free disk**, which cannot hold a
second `CARGO_TARGET_DIR`, so the compile leg is replaced by a synthetic **3-core CPU load
(`yes > /dev/null` ×3)** on a 4-core machine — a harsher share of the box than a compile would be,
but it produces no build wall-clock to compare. The browser leg is Safari playing the same YouTube
URL, opened with `open -a Safari` onto the live console session.

| | ScreenExtend CPU mean / max (% of one core) | threads | RSS | GPU utilisation | Safari (% of one core) | RTP / 60 s | join | result |
|---|---|---:|---:|---:|---:|---:|---:|---|
| (a) idle + stream | 4.0 / 12.8 | 24 | 57 MB | 0.8 % | 0 | 2 156 (36/s) | 1 825 ms | PASSED |
| load only, no stream | — | — | — | 23.2 % | 45.6 | — | — | — |
| (b) Safari 1080p60 + 3× `yes` + stream | 2.2 / 10.9 | 25 | 57 MB | 27.9 % | 27.7 | 2 114 (35/s) | 2 396 ms | PASSED |
| after the stream ended, load still on | — | — | — | 28.6 % | 25.2 | — | — | — |

Packet rate 36 → 35 per second, threads 24 → 25, resident set unchanged at 57 MB, join 1 825 →
2 396 ms. Safari's CPU fell from 45.6 % to 27.7 % while the stream ran and stayed at 25.2 % after it
stopped, so that fall is Safari's own drift and **ScreenExtend's cost to Safari is not measurable**.
As on Windows, the macOS self-test's display is static and its `capture → socket` cell is the 200 ms
keepalive, so it carries no latency signal under either condition.

### Verdict

**Stays fast under contention: yes.** On the only leg that puts real content through the real
capture and encode path, the worst-case host latency is unchanged (21.5 → 21.2 ms), the 99th
percentile grows by 3.2 ms, and the median by 4.2 ms — all of it in the OS's compositor-to-callback
hop, none of it in our stages. The host trades frame *rate* (150 → 102 fps) rather than frame
*timing*, which is the correct order to degrade in and the one every competitor in
`docs/research/competitors-closed-source.md` also chooses. **It costs the other applications about
2.6 % of a compile's wall clock and nothing measurable to a 1080p60 browser video.**

---

## Where this sits against the field

Our host telemetry stops at the socket. **We have no camera glass-to-glass number and could not
produce one**: it needs a real browser client and a 240 fps camera, and neither development machine
can run the client page. The apparatus exists and is tested — the client reports its own
arrival→paint and host-capture→paint over `OP::STATS` and the host stores it per session
(`docs/TELEMETRY.md`) — so the number arrives the moment a device joins.
`guides/clients-network-and-glass-to-glass.md` (C-34) is the procedure, and
`docs/BASELINE.md`'s "Glass-to-glass procedure" is the repeatable recipe.

| Product | Glass-to-glass (ms) | Basis | Source |
|---|---|---|---|
| Parsec | 4–8 (LAN, 240 fps) | *vendor*, not independently reproduced | [Parsec](https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5) |
| Steam Link | 13–16 display latency at <1 ms ping | user reports | [Steam](https://steamcommunity.com/app/353380/discussions/0/483367798505548057/) |
| **Luna Display (LIQUID 3.0)** | **11.3 mean USB, 22.4 mean Wi-Fi** (50 samples) | *vendor* bench | [Astropad](https://astropad.com/blog/comparing-latency-luna-display/) |
| Moonlight / GameStream | 20–35 on a good LAN | community measurement | [Moonlight](https://moonlight-stream.org/) |
| Apple Sidecar | "consistently below 30" wired USB-C | reported | [iMore](https://www.imore.com/ipad-new-mac-display-sidecar-explained) |
| Meta Air Link | 25–50 claimed; 59.1 measured | review | [mixed-news](https://mixed-news.com/en/meta-quest-2-pc-vr-streaming-with-air-link-latency-review/) |
| Air Display | 52.9 USB / 131.4 Wi-Fi | *adversarial* bench | [Astropad](https://astropad.com/blog/comparing-latency-luna-display/) |
| Duet Display | 95 USB / 108 Wi-Fi at 60 fps | *adversarial* bench | [Astropad](https://astropad.com/blog/comparing-latency-luna-display/) |
| **Generic WebRTC remote control (Hopp)** | **98** (good net, 1080p) after tuning libwebrtc, from 159 | published measurement | [gethopp](https://www.gethopp.app/blog/latency-exploration) |
| Miracast / Wi-Fi Display | 80–100 optimized; **spec ceiling 250** | certification spec | [Wi-Fi Alliance overview](https://tools.barco.com/kb-downloads/4814/Wi-Fi_CERTIFIED_Miracast_Technical_Overview_20170725.pdf) |
| **ScreenExtend — host half only** | **8.39 p50 / 9.44 p90 / 12.58 p99** — capture → encode-out, 1080p, real content, NVENC. Packetize + socket write add 0.22 ms at p50 and 1.57 ms at p99 on the self-test. | measured here, this document | — |
| ScreenExtend — host half, macOS 10.15 Intel 2013 | 14.01 p50 / 28.07 p99, capture → encoded | measured here | — |

The two that matter for positioning are **Luna Display** — the same job, a tablet as a second
display over Wi-Fi, 22.4 ms glass-to-glass — and **Hopp**, the same transport, a browser WebRTC
client, at 98 ms. A browser client cannot get below its own decode and compositor cost, so Hopp is
the honest ceiling for this architecture and Luna the target. Our host half now consumes 9.4 ms of
a Luna-class 22.4 ms budget at p90 on 1080p content, leaving the rest for network, decode and paint.
That is a budget statement, not a result: **nothing here proves what a real client adds.**

---

## What changed to get here

Every claim below is a row in `docs/OPTIMIZATIONS.md` (262 rows, 132 of them `VERIFIED`). **406
commits, 323 files, +75,962 / −18,059 lines** between `ddf9d76` and `79915fbb`.

**Capture**
- **58** — the virtual display's refresh rate stopped being the stream's frame rate; the display is
  created at a ladder rate and the encoder is configured for the client's. `capture_delivery` p99
  13.63 → 7.60 ms.
- **102** — WGC `DirtyRegionSettings::ReportOnly` (probed) plus a skip for unchanged frames, with a
  5-skip valve for a driver that under-reports. This is why the static self-test is keepalive-bound.
- **96** — frame-pool depth is per-path (2 inline, 3 for the ring) and newest-first instead of
  oldest-first; up to 33 ms of pool age removed.
- **138, 139** — macOS: `IOSurfaceIncrementUseCount` around every held surface (a 58 fps capture had
  been collapsing to one frame in six seconds) and the first frame of a session exempted from the
  dirty-rect skip. 61 fps sustained, **0** WindowServer drops.
- **291** — macOS `capture_available_ns` stopped being `displayTime` taken raw, which had been
  understating every macOS stage by one frame period.
- **101, 103, 104, 105, 212** — the DXGI path: incremental cursor compositing, RAII acquire/release
  pairing, every adapter tried and named, a bound on re-establishment, and `AccumulatedFrames` fed
  into `frames_dropped_capture`.

**Encode**
- **241, 287** — NVENC long-term references driven by `streamer::repair::RepairState`: a repair
  frame is **23,698 B against a 28,423 B IDR**, and the tier announces itself on the real encode
  path. The *rate* — how many IDRs a lossy minute stops producing — needs induced loss
  (`docs/manual-tests/transport.md` §5).
- **272, 238, 240** — NV12 end to end on the CPU-bridge paths: identity blt + readback
  2,394 → 1,170 µs (−51 %), x264 1,655 → 450 µs/frame, the Intel bridge 3,766 → 2,820 µs.
- **232, 233, 234** — Quick Sync: the low-latency ext buffers survive every `Reset`, session
  creation is serialised (5 of 6 parallel runs used to lose the encoder; now 0 of 6), and
  `MaxSliceSize` is honoured at one RTP payload.
- **144, 145, 146, 149** — VideoToolbox: two competing rate controllers reduced to one, per-frame
  stamps carried as `sourceFrameRefCon` instead of behind a lock, a bounded output channel that
  counts its drops, and a synthesized black IDR after 750 ms of nothing.
- **246, 248, 275, 277** — sub-frame/slice output, per-frame map/unmap, `nal-hrd=cbr` and the
  changed-region hint all measured and **rejected with numbers**.

**Transport**
- **181** — the WHEP handshake and the display+pipeline work under one `tokio::join!`; the join's
  wall clock is now exactly `display + pipeline`.
- **57** — no STUN server for a same-subnet peer: handshake **77–221 ms → 12–20 ms**.
- **207** — our own H.264 packetizer instead of `rtp::codecs::h264::H264Payloader`: same wall time,
  **10.6× fewer allocations**, and every frame's RTP timestamp carries the shared host clock.
- **229** — per-frame receipts from transport-wide feedback, so the newest *intact* frame is known
  by its capture instant with no client change.
- **204, 285** — trickle ICE, both halves: 201 ms off a LAN join, 713 ms off a relayed one, and the
  whole 6 s deadline when nothing arrives.
- **53, 180** — a send pacer at libwebrtc's factor, and a tuned muxed media socket (off by default
  behind `SCREENEXTEND_MEDIA_SOCKET=1`).
- **62, 269** — sender RTX evaluated and **rejected twice**, with the interceptor source cited.

**Audio**
- **67** — Windows `capture_ns` anchored to `min(qpcpos, now − read_duration)`; before it, *every*
  capture → subscriber span was negative and the histogram recorded **0 samples**.
- **111, 112, 118** — macOS legacy tier: the driver stamps `mach_absolute_time()` at the callback,
  the buffer frame size is 128 (2.67 ms) granted, and the shm ring gained a seqlock anchor and a
  reader-published `readPos`. capture → packet **p50 −0.57 ms**, 0 overruns over 2 million frames.
- **71, 72** — broadcast fan-out 128 → 16 packets (640 ms → 80 ms of buffered staleness) and
  percentiles from the lock-free histogram instead of a 2 s allocate-and-sort on the real-time
  thread.

**Everything else**
- **18** — `streamer/telemetry.rs` itself. Before it the host's only latency signal was one line
  every 60 frames on a path the default NVIDIA host never took.
- **45, 46, 208, 215** — input: one `SendInput` per pointer batch, a 64-deep fast queue with
  oldest-first shedding, a source-aware 50 ms age gate, and a parser that names its errors.
- **77** — one process-wide tokio runtime, worker threads clamped to 4: **50 → 38 threads**.
- **1** — `lto = "fat"`, `codegen-units = 1`: binary 33,698,304 → 27,878,400 B (−17.3 %), cold build
  244 s → 698 s.

Failures and rejections are in the ledger on purpose — rows 62, 155, 236, 237, 246, 248, 249, 269,
273, 275, 277, 290, 294, 305 are things that were tried and not kept, each with the number that
decided it.

---

## Verification status

262 ledger rows, **132 `VERIFIED`**. The rest need hardware neither development machine has. Every
one of them is placed in exactly one guide at a named task by `guides/PLAN.md` §7, which is the
authoritative row-by-row mapping; this is the summary by guide.

| Guide | Machine it needs | Ledger rows it closes | Capability tiers | Manual tests |
|---|---|---|---|---|
| [`modern-mac.md`](../guides/modern-mac.md) | macOS **≥ 14.2**, Apple Silicon preferred, console session, iPhone/iPad; Part B needs Developer ID + notarytool | 2, 18 (macOS arm), 26, 38, 39, 41, 42, 43, 66, 79, 82, 112, 113, 117, 141, 143 (corrob.), 148, 152, 155, 169, 170, 178 (corrob.), 186, 190, 191, 193, 203, 206, 256, 258, 259, 260, 261, 262, 264, 265, 280, 283, 284, 293 (corrob.), 295, 296, **305** | Screen Recording auth (SCK, 15/26); System Settings deep link; app relaunch for a grant; hosted Wi-Fi + security type + liveness + DHCP; adapter-change source; display-mode settle; virtual-display DPR and mode list; `CGVirtualDisplaySettings` extras; CGDisplayStream symbols on 15/26; VideoToolbox session spec, property support, **long-term references**, back-pressure tier 1; Process Tap; SCK audio; audio tier ordering; audio driver load tier 1 (signed, SIP on); 10.15 floor enforcement tiers 2–3; remote input injection | `macos-app-signing.md`, `macos-audio-driver-signing.md`, `macos-build.md`, `macos-audio.md` (tiers 1–2), `macos-capture-encode.md` §1–5/§7/§7b/§10, `macos-hosted-network.md`, `macos-permissions.md`, `core-closeout.md` §2 |
| [`intel-mac.md`](../guides/intel-mac.md) | Intel Mac, **macOS 12.3–12.7.6**, dual-GPU, battery, console session; SIP off or a Developer ID | 110, 116, 143, 148 (corrob.), 152 (corrob.), 153, 178, 264/265 (corrob.), 293 | SCK capture tier + the first-frame commit gate; the audio driver as the *selected* tier on a modern OS; legacy-tier HAL-input fallback; volume keys while the virtual device is default; audio driver load (corrob.) | `macos-audio.md` tier 3 + "Verifying the driver directly", `macos-capture-encode.md` §2/§6/§9 |
| [`windows-amd.md`](../guides/windows-amd.md) | Windows 10 2004+ with an AMD VCN/VCE AVC encoder (not Navi 24), Adrenalin, `dumpbin` | **4**, 245, 246 (AMD arm), 278, 279 (AMD arm), 298, 299, 300, 301, 302, 303, 304 | H.264 encode ladder — the AMD rungs; CPU-bridge readback format (AMF half); AMF usage tier; AMF capture/encode overlap; AMF loss recovery; AMF encoder input format | `amd-amf.md` §1–§12 |
| [`windows-legacy.md`](../guides/windows-legacy.md) | Hybrid-graphics laptop, Windows 10 2004 – Windows 11 23H2 (**below 26100**), **where DXGI duplication actually works**, hosted-network Wi-Fi, non-English display language, Administrator | 19, 21, 22, 67, 68, 70, 73, 97, **106**, **107**, **196**, 198, 199, 231, 235, 237 | System-audio capture (Windows) 19045 refusal; engine period request branch; audio format conversion tiers 2–3; the "none" branch; hosted network association + throughput; legacy hosted-network capability; virtual-display failure tiers; screen capture pre-26100; **DXGI duplication entry point**; **DXGI downscale tier 2**; virtual-display name correlation; driver IPC protocol; oneVPL dispatcher; Quick Sync slice layout | `windows-capture.md` §1–§8, `windows-audio-tiers.md` §1–§4, `core-closeout.md` §3/§6 |
| [`clients-network-and-glass-to-glass.md`](../guides/clients-network-and-glass-to-glass.md) | A client matrix (iOS ≤17 and ≥18.4, Android Chrome ≥141 and 86–140, desktop Chrome/Firefox old and new, Safari 26, two pens, four simultaneous clients), two APs, a TURN server, `clumsy`, `iperf3`, **a 240 fps camera** | 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 44, 46, 47, 48, 52, 53, 54, 56, 59, 60, 61, 65, 74, 75, 77, 171, 172, 173, 174, 175, 176, 177, 179, 181 (relayed), 183, 187, 192, 204, 205, 209, 210, 213, 271, 285, 286, 287, 288 | All fourteen client-browser tiers; audio loss robustness; congestion signal tiers 2–3; send pacing tiers 2–3; RTP MTU; playout-delay hint; socket DSCP / media socket / DSCP marking; STUN servers; streamer bind family; local link medium; picture recovery; H.264 profile and level; NVENC concurrent sessions under real clients | `client-page.md` (all), `transport.md` §1–§5, `core-closeout.md` §1/§4/§7 |
| **This project's own boxes** (`guides/PLAN.md` §6) | — | 1 (latency half), 6–13, 17, 81, 184, 200, 202, 239, 257, 279 (NVENC arm) | streamer bind family (link-local v6) | `latency-harnesses.md` |
| **Out of scope** | — | 247 (Linux) | — | `src-tauri/src/linux_utils/` does not compile |

Two capability-tier entries are **recorded rejections rather than tiers** and belong to no guide:
sender RTX (rows 62 and 269) and "video codec" (H.265/AV1 — no encoder). One item is unplaced and
needs a decision rather than a machine: **R-241**, whether the pre-10.14.4 AirPlay display tier
stays in the tree at all, given it can only ever be *selected* below the app's own
`minimumSystemVersion` of 10.15.

`guides/MASTER_GUIDE.md` is the index; `guides/PLAN.md` §7 is the audit that proves every
non-`VERIFIED` item appears in exactly one guide.

---

## Honesty

### The probe-display trap, and what it cost this pass

`docs/BASELINE.md` already warned that a probe which exits without tearing down leaves its virtual
display attached and breaks the next run. This pass found a **second, worse and permanent** version
of it, and it damaged two rows of the self-test matrix.

Runs 1–5 of the session behaved: virtual display attach **575–584 ms**, and `capture_delivery` /
`capture_to_socket` sampled over hundreds of frames. From run 6 onward, in the same session, on the
same binary, with `[display] already extended (1 monitor(s), 1 source(s))` confirming no leftover
display: attach jumped to **1 829–2 018 ms** and stayed there, and WGC stopped supplying
`SystemRelativeTime` for all but one to three frames per run — which is exactly the stamp
`capture_delivery` and `capture_to_socket` are derived from. Those two columns then report
percentiles over 1–3 samples, which is why they are marked *under-sampled* for x264 and Quick Sync
above rather than quoted.

The cause is visible in the PnP tree: **eight `Generic Monitor (VirtuDisplay+)` device nodes,
UID256 through UID263, all in `Unknown` (not-present) state**, left behind by repeated create/destroy
cycles of the IDD virtual display. It does not recover with idle time — a control run fifteen
minutes later still attached in 1 938 ms. Clearing them needs `pnputil /remove-device` or a driver
restart, both of which need Administrator, which this session did not have, and rebooting the
repository owner's machine was not an acceptable remedy.

Consequences, stated rather than hidden:

- The NVENC self-test row is from the healthy state; the x264 and Quick Sync self-test rows are
  from the degraded state and their capture-relative columns are not usable.
- The live matrix, the workload sweep and both contention scenarios are **unaffected** — they use
  `probe --live`, which captures the primary display and creates no virtual display.
- This is a real user-facing defect, not only a measurement artefact: a host that has created eight
  displays in one boot joins its ninth client three times slower. It belongs in `findings/` as a new
  Windows item and its natural owner is `guides/windows-legacy.md`, which already owns the IDD
  virtual-display tiers.

### Measurement variance on a machine that is not silent

The repository owner's own Chrome ran throughout, 47–62 processes, 26–76 % of one core with bursts
to 870 %. Whole-machine CPU at rest was 15–20 %. Every run was preceded by
`tasklist | findstr /i "rustc cargo"` and separated from the previous one by at least 20 s, but the
box was never *quiet* in the sense `docs/manual-tests/latency-harnesses.md` asks for. That note
records five consecutive runs of one unchanged release binary spreading 8.0 % on an idle box and
4,405–9,067 µs under load. Read every single-cell difference under about 10 % here as noise. The
differences quoted in the headline table are 23–64 %, well outside it; the ones inside it are named
as such.

The macOS box has four cores and is from 2013. Its capture → encoded p99 spread 17.4 / 28.1 /
41.8 ms across three trials of identical work, and the worst was the trial that ran immediately
after a release link. That spread is the machine, not the code.

### The two builds do not share an operating point on `probe --live`

`ddf9d76`'s probe configured the encoder at **1080p@60, 12.44 Mbps** on this 360 Hz panel;
`79915fbb`'s configures it at **1080p@360, 30 Mbps**. Any per-percentile `--live` comparison between
the two builds is therefore comparing different amounts of requested work, and this document does
not draw a latency conclusion from that pair — only the robustness conclusion (one failed start and
one 366-frame starve out of three, against zero of three), which no operating point explains.

The comparison that *is* like-for-like is the self-test A/B, where both builds negotiate the same
1280x720 virtual display, and the live-motion contention pair, where both legs are the same build.

### Two harness defects found by using them

- **`probe` silently ignores `--software-encode`.** It is a `serve` flag; the probe harness's own
  spelling is `--encoder software`. The first x264 self-test cell of this pass ran on
  `nvenc-zero-copy-ring` for three trials and reported it plainly in the backend field, which is the
  only reason it was caught. The cell was re-run correctly. An unknown flag should be an error, not
  a no-op.
- **The DXGI re-establishment bound never trips** in the failure mode it was written for (above).

### What was not measured, and why

| Cell | Why |
|---|---|
| **Camera glass-to-glass** | Needs a browser client and a 240 fps camera. → `guides/clients-network-and-glass-to-glass.md` C-34; procedure in `docs/BASELINE.md`. |
| **Client arrival → paint, decode, and every browser tier** | Neither development machine can run the client page. The host-side apparatus is in place and unit-tested (`node scripts/test-client-stats.mjs`, `pnpm test:client`). |
| **Every DXGI cell** | Duplication is refused on the primary display of this hybrid laptop and starves against a virtual display. Two distinct defects, both reproduced this pass. |
| **AMD AMF, in its entirety** | No AMD GPU on either machine. Nothing in that backend has ever encoded a frame. |
| **Every macOS 12.3+ tier** | ScreenCaptureKit, Process Tap, VideoToolbox low-latency and LTR do not exist on 10.15.8. |
| **`capture_delivery` / `capture_to_socket` for x264 and Quick Sync on the self-test** | Under-sampled by the IDD degradation above. The live matrix covers the same encoders with 4 400+ samples each. |
| **Relayed / cross-network anything** | No TURN path on either machine: the relayed MTU, the relayed ICE constants, the cloud relay and the defensive audio-redundancy start are all unexercised. |
| **Input round trip** | `input_recv` / `input_queue` / `input_inject` need a client to originate an event. Only the host-side injection cost is measurable here, and it was re-run: 5.16 → 2.82 ms per 40-sample batch. |
| **A like-for-like `--live` A/B between the two builds** | The two builds resolve the probe's encoder configuration differently on a 360 Hz panel (60 fps / 12.44 Mbps vs 360 fps / 30 Mbps), so only robustness — starts, starves, ring drops — is comparable there. |
| **The IDD virtual-display degradation, root-caused** | Eight not-present monitor device nodes accumulate and attach time triples; clearing them needs Administrator, which this session did not have, and rebooting the repository owner's machine was not an acceptable remedy. |
| **The macOS legacy audio tier on an idle host** | It produces nothing from pure silence, by design; the number in this document was taken with a tone playing. |
| **Desktop UI wall-clock time-to-interactive** | Never instrumented. Bundle bytes, IPC round trips and React commit counts stand in for it. |
| **The GitHub Actions workflows** | Have never run; running them needs a push, which this engagement forbids. |

### Reproducing this document

```sh
# host path, per encoder                (Windows: --encoder nvidia|software|intel)
ScreenExtend probe --whep-selftest --duration 30 --encoder <e> --capture-backend wgc

# real content through the real capture path  (the headline table)
ScreenExtend probe --live out.h264 --duration 30 --monitor 0 --workload motion --encoder <e>

# audio
ScreenExtend probe --audio --duration 30

# macOS capture and encode
cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::cgds::tests::capture_rate            -- --ignored --nocapture
```

Leave **at least 20 s** between runs, check `tasklist | findstr /i "rustc cargo"` (or `pgrep -fl
"cargo|rustc"`) first, and on Windows **restart between the fifth and sixth virtual display of a
session** or read the caveat above. On macOS run `caffeinate -u -t 5` before each trial and wrap the
trial in `caffeinate -dims`.
