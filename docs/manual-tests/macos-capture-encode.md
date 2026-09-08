# Manual test: macOS capture and encode on a modern Mac

Everything the macOS capture and encode path does above macOS 12.3 is unverifiable
on this project's hardware. The development Mac is 10.15.8 Catalina on Intel, where
ScreenCaptureKit does not exist, `EnableLowLatencyRateControl` does not exist,
long-term reference frames do not exist, and the only capture API is the one Apple
has since obsoleted. Every change in those bands is `COMPILE-CHECKED` at best.

This script is how they become `VERIFIED`. Run it on **13, 14, 15 and 26**, and on
at least one Apple silicon Mac and one Intel Mac, before trusting any of the rows
in `docs/OPTIMIZATIONS.md` marked UNVERIFIED for the macOS capture or encode tier.

Everything here reads out of the log bus. Start the host with
`ScreenExtend serve -v` so the lines below go to the terminal, or run the GUI and
read them in Settings → the log terminal; `ScreenExtend logs --lines 400` prints
the same backlog after the fact.

## What "correct" means

| Situation | Required behaviour |
|---|---|
| macOS 13.0+ | Capture starts on ScreenCaptureKit; CGDisplayStream is never touched |
| macOS 12.3–12.x | Either tier is acceptable; whichever starts must say so |
| Apple silicon, any size | VideoToolbox reports `hardware=true` |
| Intel, encode ≥ 640×480 | VideoToolbox reports `hardware=true` |
| Any Mac, hardware busy elsewhere | The software fallback is **logged loudly**, never silent |
| macOS 11.3+ | The low-latency rate-control rung wins: `low_latency=true` |
| Any Mac | The client paints within ~1 s of joining, even on a completely still display |

The last row is the one that used to fail everywhere: a static screen produced no
first frame, so a session connected and then showed nothing, forever.

---

## 1. Which capture tier started

Join one device and look for exactly one of these two lines.

```
[sck] capture started: display=1 1280x720 @ 60fps (queueDepth=8, minimumFrameInterval=0.9/fps)
[cgds] capture started: display=1 1280x720 @ 60fps (queueDepth=5, no minimumFrameTime)
```

**On 13.0 and later, `[cgds]` here is a failure.** CGDisplayStream is obsoleted in
the macOS 15 SDK and using it is what triggers Sequoia's recurring "ScreenExtend is
recording your screen" alert. When the fallback is taken it is always preceded by
its reason:

```
[capture] ScreenCaptureKit failed to start for display 1: <reason>; falling back to CGDisplayStream
[sck] no frame within 1500ms of startCapture; treating ScreenCaptureKit as unusable on this display
```

The second line is the frame-level probe. ScreenCaptureKit on 12.3 is reported by
Chromium to start cleanly and deliver nothing for a full-display filter, and a
capture path that starts and delivers nothing is indistinguishable from a slow one
from the outside — so the tier is not committed until a frame has actually arrived.
If you see it on 13+, that is a bug worth a finding: record the OS build, the
display (built-in / external / virtual) and whether Screen Recording was granted.

**Record**: which line appeared, on which OS build, for the built-in display and
for a ScreenExtend virtual display.

## 2. Whether the encoder is the hardware one

Once per process, before any session:

```
[vt] encoder: id=com.apple.videotoolbox.videoencoder.h264.gva hardware=true gpu_registry_id=4294967… instance_limited=true
```

Then per session, in order:

```
[vt] 1280x720 would use encoder com.apple.videotoolbox.videoencoder.h264.rtvc advertising 41 properties
[vt] properties: RealTime=ok AllowFrameReordering=ok ProfileLevel=ok … EnableLTR=available …
[vt] H.264 encoder ready: 1280x720 @ 60fps, 5529 kbps, profile=Baseline, low_latency=true
[vt] encoder identity: id=com.apple.videotoolbox.videoencoder.h264.rtvc hardware=true gpu_registry_id=none (integrated or software)
```

The identity line is the one that matters. `hardware=false` is followed by:

```
[vt] this session is on the SOFTWARE H.264 encoder. Expect higher latency and CPU. …
```

`gpu_registry_id=none` is **not** evidence of software: Apple documents NULL there
for a built-in-GPU hardware encoder as well. Only `hardware` answers that question,
and for the low-latency `…rtvc` encoders Chromium reports the property is sometimes
unset — which is why the line prints `unknown` rather than guessing.

**Record**: `id`, `hardware` and `gpu_registry_id` for
- the built-in display at 100 % scale,
- a phone-sized client (e.g. 390×844) at 100 %,
- the same phone at 50 % video scale (195×422 — below Intel's documented hardware
  minimum of 640 wide *or* 480 tall; on Intel this should be the one that says
  `hardware=false`).

### The scarce-resource case

`instance_limited=true` in the catalog line means Apple has told us this encoder is
a global scarce resource. Reproduce the contention:

1. Start a 4K screen recording in QuickTime, or a video call, or OBS with hardware
   encode.
2. Join a device in ScreenExtend.
3. Expect `[vt] HW encoder unavailable for …` followed by the ladder falling
   through, and then the loud software line.

**Fail if** the session silently lands on software with no line naming the reason.
This is the documented mechanism behind "it only works if I quit everything else",
and it must be visible in the log.

## 3. Whether low-latency rate control is on

`low_latency=true` in the "encoder ready" line means the first rung of the ladder
won — `EnableLowLatencyRateControl`, macOS 11.3+. On 10.15 and 11.0–11.2 it must be
`false` and the ladder must fall through silently to a working session.

Cross-check against the property line: with low latency on, `AverageBitRate` is the
only rate control set. There is deliberately **no `DataRateLimits`** on any OS —
Chromium documents it as incompatible with `EnableLowLatencyRateControl`, and QA1958
defines it as a hard cap fighting the loss-based controller in `webrtc_session.rs`.
If a future change reintroduces it, this line is where it will show up.

## 4. Long-term reference frames

`EnableLTR=available` in the property line means this Mac's encoder can repair a
lost frame with a P frame against an acknowledged reference instead of a full IDR.
It is **probed and reported, not enabled**: the other half is a client that
acknowledges tokens over the control DataChannel, which is transport work.

**Record**: whether `EnableLTR` reads `available` or `absent`, per OS. macOS 12.0+
should say `available`.

## 5. The still-display case (the one that used to be broken)

1. Create a virtual display and join a device.
2. Do not move the mouse. Do not drag any window onto that display. Leave the
   desktop completely still.
3. Expect the client to paint a picture — black is correct — within about a second.

The mechanism is a first-frame deadline in the encode loop: if capture has produced
nothing after 750 ms, a black frame is encoded as an IDR so the client's video
element starts. It is counted, so it is visible afterwards:

```
ScreenExtend status --json     # degradation.capture_no_first_frame > 0
```

**Fail if** the client stays blank. A non-zero `capture_no_first_frame` on a *busy*
display is also a failure — it means real capture never started.

## 6. Scheduling

Once per session:

```
[qos] encode thread on the deadline scheduler: period=16.67ms computation=12.50ms constraint=16.67ms preemptible=true
```

If this line is missing, `thread_policy_set` failed and a `[qos] … failed (rc=…)`
line says so; the thread then runs at the default priority and every latency number
below it means something different. On Apple silicon expect
`thread_policy_set(AFFINITY) failed` — `THREAD_AFFINITY_POLICY` is unimplemented
there and returns an error rather than doing nothing quietly. That one is expected
and harmless.

## 7. Back-pressure

Under load (drag a video around the extended display for a minute), watch for:

```
[vt] holding frames back: 2 in flight, 1 skipped so far
[vt] the drain thread fell behind; 1 encoded frame(s) discarded, forcing an IDR
```

The first is normal and self-correcting — it is the cap that stops VideoToolbox
accumulating a queue nothing can see. The second should be rare; if it is frequent,
the drain thread is losing to something and that is a finding.

## 8. What latency to expect

Measured on the 10.15.8 Intel dev box (CGDisplayStream + VideoToolbox, no low
latency, no SCK) — the slowest configuration the code supports. A modern Mac should
be at or below every one of these.

See `docs/BASELINE.md` for the filled matrix. Take the numbers from the same
harness so they are comparable:

```sh
cargo test --release --lib macos_utils::streamer::pipeline::tests::end_to_end_latency -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::hw_capability_probe -- --ignored --nocapture
cargo test --release --lib macos_utils::streamer::encoder::tests::the_sps_promises_no_reordering -- --ignored --nocapture
cargo build --release --features probe
./target/release/ScreenExtend probe --whep-selftest --duration 30
```

Run every WindowServer-dependent step with the screen awake: `caffeinate -u -t 5`
immediately before, and the whole run under `caffeinate -dims`. A locked screen
stalls compositing and the capture numbers become meaningless without saying so.

`the_sps_promises_no_reordering` decodes the emitted SPS and asserts the decoder is
told to hold zero frames before output. `AllowFrameReordering=false` is a request;
`max_num_reorder_frames` in the VUI is what a decoder obeys, and a non-zero value
there costs the client that many frames of delay no matter how fast the host is.

## 9. Power and the discrete GPU

On a dual-GPU Intel Mac (2016–2019 15-inch MacBook Pro, Mac Pro, eGPU):

1. `ScreenExtend serve -v`, join a device, and note `gpu_registry_id` in the
   encoder identity line.
2. Compare it with the display's own GPU. The encoder specification asks for
   `PreferredEncoderGPURegistryID` taken from
   `CGDirectDisplayCopyCurrentMetalDevice` of the captured display, so the two
   should match; if they do not, every frame's IOSurface is migrating across the
   PCIe bridge and that is the finding.
3. Check whether the app has forced the discrete GPU on for its whole lifetime —
   `NSSupportsAutomaticGraphicsSwitching` in `Info.plist`. Watch the battery menu.

**Record**: both registry IDs, `powermetrics` package power idle vs streaming, and
whether the integrated GPU stays in use.
