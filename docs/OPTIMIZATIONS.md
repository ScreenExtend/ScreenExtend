# Optimizations ledger

A running record of every performance change attempted during the latency & compatibility
overhaul: the hypothesis, the measurement before, the measurement after, whether it was kept or
reverted, and why. Failures are recorded on purpose — a reverted change with a number attached
is information the next person needs.

Verification status vocabulary (used here and in `CAPABILITY-TIERS.md`):

- `VERIFIED` — executed and measured on one of the two development machines.
- `COMPILE-CHECKED` — builds and passes static/layout tests, never executed.
- `UNVERIFIED` — written from research and review; no local runtime signal at all.

Development machines: Windows 11 build 26200 with an RTX 3080 Ti Laptop GPU (hybrid graphics,
NVENC driver 580.97); macOS 10.15.8 Catalina on Intel with Command Line Tools only.

Measurement conventions: nanosecond timestamps off the shared host clock
(`streamer::audio::host_now_ns`), reported as p50 / p90 / p99 / max, never as a bare mean.

| # | Area | Change | Hypothesis | Before | After | Status | Kept? | Notes |
|---|------|--------|------------|--------|-------|--------|-------|-------|
| 1 | build | `[profile.release]`: `lto="fat"`, `codegen-units=1`, `opt-level=3`, `debug="line-tables-only"`, `strip=false` | Shipping builds ran on cargo's stock release profile (LTO off, 16 CGUs); the hot path crosses crate boundaries every frame (encoder FFI -> `streamer` packetizer -> `webrtc`/`bytes` -> SRTP), so cross-crate inlining should pay. | cold `cargo build --release` **244 s**, `ScreenExtend.exe` **33,698,304 B** | cold build **698 s** (11.6 min), binary **27,878,400 B** (-17.3%) | VERIFIED (build); UNVERIFIED (latency) | yes | `fat` measured under the 15-min budget so `thin` was not needed. Binary shrank despite adding line tables. Runtime effect not yet measured -- no baseline run existed to compare against when this landed. |
| 2 | permissions (macOS) | Screen Recording is read with `CGWindowListCopyWindowInfo` + `kCGWindowName` instead of a capture probe; `CGPreflightScreenCaptureAccess` demoted to a positive-only signal; `CGDisplayStream` removed from the authorization decision. | The gate polls `check_permissions` at 5 Hz on the main thread, and the first call ran a real capture probe inline. The window-list read answers the same question without opening a capture session. | first `check_permissions` 700 ms (CGDisplayStream) to 5 s (SCK `getShareableContentWithCompletionHandler` timeout); every later call a hard `false` | window-list probe **37 ms cold, 0.6 ms warm** (median of 5 consecutive calls, 10.15.8 Intel, `cargo test --lib permissions::tests::window_name`); SCK moved to a background slot | VERIFIED (10.15.8, CGDS/window-list paths); UNVERIFIED (12.3+ SCK confirmation, 15/26 behaviour) | yes | The old ordering made the gate unclosable, not merely slow: the preflight is stale by design and it returned before the probe built to work around it. Grant-to-gate-close is now bounded by the 200 ms poll wherever any other titled window is on screen. |
| 3 | permissions (macOS) | `accessibility_api_enabled()` off the primary read: `AXIsProcessTrusted()` alone decides, the system-wide `AXUIElementCopyAttributeValue` becomes a background-refreshed secondary whose timeout reads as *unknown*. | The helper could only ever return `false` on `kAXErrorAPIDisabled`, a pre-10.9 condition — so it contributed no signal while paying an AX IPC round trip per call. | one system-wide AX round trip per `check_permissions`, 5x/second, ceiling 0.5 s each | **20-23 ms** per call measured on 10.15.8 (`doctor --json`, `macos.probes.axSystemWideElementUs`), now at most once per 2 s on a background thread | VERIFIED (10.15.8) | yes | 20 ms x 5/s was ~10% of the main thread for zero information. It also silently set the process-wide AX messaging timeout; that side effect survives, see `findings/minor-permissions-ax-messaging-timeout-still-set-process-wide.md`. |
| 4 | audio-win | `capture_ns` anchored to `min(qpcpos, now - read_duration)` instead of `qpcpos` alone | `qpcpos` is documented as when "the audio endpoint device read the device position", but on a **loopback** stream the endpoint is a *render* device and that position is its play cursor — so the stamp is a playout time, in the future. The client sizes its jitter buffer from `enqueue - capture_ns`, so a future capture time is surplus audio delay. | `capture -> subscriber` recorded **0 samples** (every span negative); `capture_ns` skew min -20.36 / mean -14.65 / max +5.61 ms, **1596/1601 packets stamped ahead of the host clock**, spread 26 ms, offset moving with host load | `capture -> subscriber` p50 9.44 / p90 9.44 / p99 9.44-10.49 / max 10.9 ms; skew min -3.04 / mean +7.69 / max +10.9 ms, <=1/1601 ahead | VERIFIED (Win 11 26200, `probe --audio`, 3 runs, idle) | yes | One engine period plus the intra-burst offset, which is what it should physically be. `ts_clamped` counts how often the endpoint stamp was in the future: on this box, every drain. |
| 5 | audio-win | Silent render companion negotiates the engine period via `IAudioClient3::GetSharedModeEnginePeriod` + `InitializeSharedAudioStream(min)` | Microsoft documents that any stream on an endpoint can lower the whole endpoint's period, and the loopback capture stream cannot ask for itself (`AUDCLNT_E_INVALID_STREAM_FLAG`). The companion can, and capture inherits it. Expected -5 to -7.3 ms where a driver exposes a small buffer. | wake interval p50/p90/p99 9.44 ms, max 10.3-10.6 ms; engine periods `default = fundamental = min = max = 480 frames (10.000 ms)` | identical — the probe correctly declines, because there is no smaller period to request on this driver | VERIFIED **no-op on this hardware**; the request path is COMPILE-CHECKED | yes | Not a failure: this OEM driver exposes exactly one period. The in-box High Definition Audio Device driver documents 128-480 frames and is how the other branch gets exercised; that needs a driver swap plus a reboot, so it is a manual test. Landing it also fixed "nothing on any machine could say what period it was running at". |
| 6 | audio-win | Process-loopback tier added (`ActivateAudioInterfaceAsync` + `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK`, excluding our own pid), probed by attempting activation | A second tier for the cases endpoint loopback structurally cannot serve: an exclusive-mode endpoint, device churn, an endpoint that will not activate. Ordering to be decided by measurement, not assumption. | endpoint tier: wake 9.44/9.44/9.44/23.11 ms, capture->packet 5.77/9.44/9.44/24.07 ms | process tier: wake 9.44/9.44/10.49/11.32 ms, capture->packet 6.29/9.44/10.49/11.41 ms | VERIFIED (both tiers, 8 s each, idle) | yes, as **tier 2** | Neither is meaningfully faster — same shared engine. The endpoint tier keeps p99 by one bucket, so the long-shipping path stays first. Settled two open questions from the research: process loopback needs **no** silent companion (999 packets in 5 s with none anywhere), and its event does fire. |
| 7 | audio (all) | `audio-bridge` copy thread deleted; capture backends write straight into the broadcast through `AudioSink` | `broadcast::Sender::send` is synchronous and non-blocking, so the bridge was a pure type adapter — running at default priority between a Pro Audio thread and the network. | capture -> subscriber p50 9.44 ms (idle) | capture -> subscriber p50 9.44 ms (idle) | VERIFIED (no p50 change); UNVERIFIED under load | yes | Honest result: on an idle host the hop cost nothing at p50. It was a tail risk under load. Removing it also removed the unbounded capture->hub channel and the `bridge.join()` that could wedge the hub mutex forever. |
| 8 | audio (all) | Broadcast fan-out capacity 128 -> 16 packets | 128 packets is 640 ms of audio held for a subscriber that stopped reading; a real-time consumer that far behind should skip to live rather than catch up. | 640 ms of buffered staleness before `Lagged` | 80 ms; `lagged=0` over 10 s with subscriber inter-arrival p99 9.44 ms | VERIFIED | yes | Sized against the measured burst (two 5 ms packets per 10 ms wake) rather than guessed. |
| 9 | audio (all) | Percentiles from `telemetry::Histogram` instead of cloning and sorting a 4096-element `Vec` every 2 s on the capture thread; the 2 s log emit moved to its own thread | A 32 KiB allocation, a 4096-element sort and a logbus mutex acquisition on an MMCSS "Pro Audio" thread is a periodic priority inversion against the exact guarantee MMCSS was registered to get. | encode p50 0.26 / p99 0.33 ms, plus a 2 s periodic allocate+sort+lock on the RT thread | encode p50 0.05-0.09 / p99 0.20-0.33 ms, no allocation and no lock on the RT thread | VERIFIED (same percentiles, allocation removed) | yes | Applies to both platforms; the macOS worker had the identical helper. The stall itself was never measured directly — that needs an ETW trace — so the claim is "the allocation and the lock are gone", not "N microseconds saved". |
| 10 | audio (all) | Opus `OPUS_SET_VBR_CONSTRAINT` 0 -> 1; `OPUS_SET_DTX` / `OPUS_SET_INBAND_FEC` removed; encoder state read back and logged | libopus defaults `VBR_CONSTRAINT` to 1 and the code opted out with no recorded reason. Unconstrained VBR spikes packet size on transients; at 200 packets/s that is arrival jitter, which the client answers by growing its buffer. DTX and in-band FEC are LPC-layer (SILK) features and structurally inert under `RESTRICTED_LOWDELAY` at 5 ms. | settings applied and never confirmed; `vbr_constraint=0` | `application=2051 bitrate=128000 complexity=5 signal=3002 vbr=1 vbr_constraint=1 lsb_depth=24 prediction_disabled=0 lookahead=120 samples (2.50 ms)` | VERIFIED in force (read back); the packet-size distribution is UNVERIFIED — an idle host encodes 3-byte silence frames, so the peakiness claim needs real audio | yes | `OPUS_SET_PREDICTION_DISABLED` is wired as a config field defaulting **off**: right for a zero-retransmit transport, but it costs bitrate at fixed quality and deserves a listening test rather than a blind landing. |
| 11 | audio (all) | DataChannel backpressure checked every packet against 40 ms of audio, instead of one packet in four against a flat 16 KiB | 16 KiB at the measured ~93-byte message is about a second of queued audio before dropping starts, and sampling detected both entry *and exit* up to 20 ms late. | ~1 s of queued audio; 20 ms detection lag in both directions | 40 ms, evaluated per packet, threshold derived from the message size actually being sent | COMPILE-CHECKED — needs an induced-congestion run to observe | yes | Drops now also set `FLAG_DISCONTINUITY` on the next packet sent, so the client resyncs instead of splicing. |
| 12 | audio (all) | Opus-track fallback samples spaced by real elapsed `capture_ns` instead of a hardcoded 5 ms | `TrackLocalStaticSample` derives RTP timestamps by accumulating `duration`, so every producer-side gap left audio permanently 5 ms per lost packet behind video, which rides the true host clock. | offset grows 5 ms per lost packet and never recovers | audio and video RTP timelines both follow `host_now_ns` | COMPILE-CHECKED — needs a browser forced onto the NetEQ path | yes | This is the path old mobile browsers land on, i.e. the compatibility target of this engagement. |

## Notes

### Audio silence costs 3 bytes, not a payload

Measured with `probe --audio` on an idle host: a CELT silence frame encodes to **3 bytes**, so
the 13-byte header dominates and the whole idle audio stream is ~16 B x 200/s = **3.2 kB/s**
before SCTP/DTLS/UDP/IP framing. `speculative-audio-win-dtx-off-so-silence-is-fully-encoded-and-sent`
worried about the byte cost; the byte cost is negligible, and the *packet rate* is the only
real consideration. Header-only silence keepalives would cut 200 packets/s to 10, but they need
client-side gap fill (W11) and would interact with the worklet's silence-boundary drift
correction, so this is recorded rather than acted on. Note also that the process-loopback tier
never sets `AUDCLNT_BUFFERFLAGS_SILENT` at all — it delivers real zeros — so on that tier the
silence shortcut does not fire and the client never sees `FLAG_SILENT`.

### `AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY` reads backwards from its meaning

It does not select the default sample-rate converter. Microsoft documents it as selecting "a
sample rate converter with **better** quality than the default conversion but with a **higher
performance cost**". That is the right choice for audio humans will hear, but it is a filter
with group delay and it is only ever reached on a non-48 kHz endpoint. Now commented at the
constant rather than left to read as its opposite.

### windows-rs `PROPVARIANT` must not be dropped over a borrowed blob

`ActivateAudioInterfaceAsync` takes its parameters as a `VT_BLOB` PROPVARIANT whose
`pBlobData` points at caller-owned memory, exactly as the API requires. Letting that
`PROPVARIANT` go out of scope **killed the process outright** — after a fully successful
activation, which made it read as though the activation had failed. `std::mem::forget` on the
variant is the fix and leaks nothing (it is a stack value whose only pointer is into the same
frame). Recorded because the symptom points nowhere near the cause.

### `CGPreflightScreenCaptureAccess` is not present on 10.15.8

Measured, not assumed: `dlsym(RTLD_DEFAULT, "CGPreflightScreenCaptureAccess")` returns
null on macOS 10.15.8, so `doctor` reports the probe as `unknown` there.
`docs/research/macos-permissions-tcc.md` §2.1 states both preflight/request functions are
"macOS 10.15+, resolvable by `dlsym` on every host ScreenExtend supports" — that is wrong on
this project's own floor. It does not change the design (the preflight was already
positive-only and the code already handles `None`), but it does mean the original
short-circuit bug could only bite on 11+, which matches the reports coming from new Macs.

### macOS 15 Sequoia: apply for `com.apple.developer.persistent-content-capture`

**This is a request for the maintainer, not a code change. Do not add the key to
`src-tauri/Entitlements.plist` — Apple has to grant it first, and an unapproved
entitlement breaks signing.**

Sequoia re-asks for screen-recording consent roughly every 30 days (weekly plus
every reboot in the early betas, softened to monthly in beta 6 and relaxed further in
15.1 for regularly used apps). There is no supported way for an app to suppress the
prompt. It applies to ScreenCaptureKit clients too; the only construct that avoids it is
`SCContentSharingPicker`, which makes the user pick a capture target every time — useless
for ScreenExtend, which captures a virtual display it created itself for a device that is
already connected.

The documented escape is the `com.apple.developer.persistent-content-capture`
entitlement. Apple scopes it to **VNC applications** and grants it through a request form
on developer.apple.com with essentially no public criteria. ScreenExtend qualifies on the
merits — it is a persistent remote display of the user's own machine for the user's own
devices — so it is worth applying. Until it is granted, a host left running as a permanent
second monitor will be interrupted about once a month, and the interruption is expected
rather than a bug.

What ships in the meantime: `ensure_screen_recording_access()` now consults the live
authorization read (never frame delivery, which an unauthorized process still gets) and
logs a `[tcc]` line naming the monthly re-consent as the likely cause. It runs at every
pipeline start — every device join and every stream refresh — so a lapsed consent is
noticed there. There is still no re-probe *while* a session streams; see
`findings/major-permissions-no-periodic-reprobe-while-a-session-streams.md`.

### Rejected: feeding measured loss into `OPUS_SET_PACKET_LOSS_PERC`

`major-audio-core-no-fec-on-an-unreliable-datachannel` proposed driving the encoder's
packet-loss-percentage from the link loss the BWE already computes. Not done, and not because
the number is unavailable — it is, and the redundancy switch now uses it.

The encoder is **host-wide**. There is one `AudioHub`, one capture, one `OpusEncoder`, fanned
out to every audio-enabled device; loss is **per session**. Feeding one session's loss into a
shared encoder degrades the stream for every other device on the host, and with two devices on
different links there is no correct value to write. Loss robustness therefore belongs on the
per-session transport, which is where `FLAG_REDUNDANT` sits.

The lever that *is* encoder-wide and does apply is `OPUS_SET_PREDICTION_DISABLED`: it makes
frames independent regardless of who is listening. It is wired as a config field defaulting off
because it costs bitrate at fixed quality and the trade needs a listening test — see
`docs/manual-tests/windows-audio-tiers.md`.
