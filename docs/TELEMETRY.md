# Telemetry

Two halves of one measurement. The host measures its own pipeline from the compositor to the
socket; the client measures from the first byte off the wire to the pixel on the glass. Both
express every timestamp against the **same clock** — the host's `streamer::audio::host_now_ns()`,
a monotonic nanosecond counter whose epoch is the host process's start — so the two halves add up
to a glass-to-glass number without a camera.

The client cannot read that clock. It estimates the offset to it (see
[Clock synchronization](#clock-synchronization)) and everything else follows from that.

## Why it exists

Before this, the host's only latency signal was one line every 60 frames on the WGC classic path
reporting an *average*, and the NVENC ring path — the default on any NVIDIA host — returned before
reaching even that. Nothing at all reported which path a client had actually landed on. A device
that fell back from hardware decode into a canvas to the `<video>` element, or from a
SharedArrayBuffer audio ring to `postMessage`, looked identical to a device on the fast path.
Every one of those degradations is now a field the host can read.

## Clock synchronization

The reliable input DataChannel already exchanged a `PING`/`PONG` pair once a second, with the host
echoing the client's stamp back so the client could compute an RTT. The `PONG` now carries the
host's own clock alongside it:

```
PONG   0x61
  +1   u64  echoed client stamp, ns (as sent in the PING)
  +9   u64  host_now_ns at the moment the PONG was built
       17 bytes total
```

A client older than this reads the first 9 bytes and ignores the rest; a host older than this
sends 9 bytes and the client simply never gets an offset.

The client keeps the **minimum-RTT** sample, because the least-queued exchange gives the tightest
midpoint estimate, and ages it out after 30 s so a route change is not remembered forever:

```
offsetMs = hostMs − (timeOrigin + (t_send + t_recv) / 2)
hostMs   = absMs + offsetMs
absMs    = hostMs − offsetMs
```

The offset is expressed against **absolute** time (`performance.timeOrigin + performance.now()`),
not against `performance.now()`, because a dedicated worker's time origin is its own creation
rather than the page's — only the absolute form is comparable across the page, the video worker
and the audio worker. `window.SEClock` publishes it and `SEClock.subscribe(fn)` pushes each new
estimate; the page forwards it to both workers in the same task, so their numbers never step
apart.

Until the first host-stamped `PONG` arrives, both workers use an offset of 0. That reproduces the
pre-existing behaviour exactly: the reported delays are then the distance between two unrelated
epochs, meaningless on their own but valid as a *difference*, which is all A/V sync uses.

## Host-side stages

`streamer/telemetry.rs`. Every stamp is `host_now_ns()`; `FrameTiming` travels with the frame.

| Stamp | Set where |
|---|---|
| `capture_available_ns` | The OS's own frame time: WGC `SystemRelativeTime`, DXGI `LastPresentTime`, CGDisplayStream `displayTime`, ScreenCaptureKit PTS. 0 when the backend supplies none. |
| `capture_cb_entry_ns` | Entry to our capture callback. |
| `scale_done_ns` | After the GPU downscale (ring path). |
| `encode_submit_ns` | Immediately before handing the frame to the encoder. |
| `encode_done_ns` | Immediately after the encoder returns the access unit. |
| `packetize_done_ns` | After H.264 payloading, before the first RTP write. |
| `socket_write_done_ns` | After the last RTP packet of the frame is written. |

Derived histograms: `capture_delivery`, `scale`, `encode_queue`, `encode`, `packetize`,
`socket_write`, `capture_to_encode`, `capture_to_socket`. A stage with `count == 0` did not fire —
that absence is a finding, not a zero.

Histograms are fixed log-scale buckets, 8 per octave (any bucket is under 13% wide), 512 buckets
of `AtomicU64`. `record` is three relaxed `fetch_add`s and a `fetch_max`: no allocation, no lock,
no logging, which is what makes it safe on a real-time capture callback. Reported as
p50/p90/p99/max — never a bare mean, which hides the tail that is actually felt.

### Degradation counters

`capture_fallback`, `frames_repeated`, `frames_dropped_ring`, `nacks_received`,
`keyframe_requests`, `idrs_sent`, `encoder_resets`, `encoder_transient_errors`, `bitrate_cuts`,
`audio_broadcast_lag_drops`, plus the client-reported `client_jitter_underruns` /
`client_jitter_overruns`.

NACKs are counted and never logged: a lossy link produces them continuously and every logbus line
is a JSON IPC event to the desktop UI.

## Client → host stats frame (`OP::STATS`, `0x62`)

Sent on the **reliable** DataChannel once a second, alongside the `PING`. Fixed offsets, little
endian. All durations are **microseconds**; a value of 0 means "not measured", which is what a
path with no hook for that stage reports.

Version 2 (96 bytes) appends the tier bytes and the degradation field below. Bytes 0..72 are
byte-identical to v1, which is what lets a host built against v1 read the prefix and ignore the
rest.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | opcode `0x62` |
| 1 | 1 | version (currently 2; 0 is rejected) |
| 2 | 1 | video path |
| 3 | 1 | audio path |
| 4 | 4 | flags (below) |
| 8 | 4 | arrival → paint, p50 |
| 12 | 4 | arrival → paint, p90 |
| 16 | 4 | arrival → paint, p99 |
| 20 | 4 | decode submit → paint, p50 |
| 24 | 4 | decode submit → paint, p90 |
| 28 | 4 | decode submit → paint, p99 |
| 32 | 4 | host capture → paint, p50 |
| 36 | 4 | host capture → paint, p90 |
| 40 | 4 | host capture → paint, p99 |
| 44 | 4 | frames painted |
| 48 | 4 | frames dropped |
| 52 | 4 | decode errors |
| 56 | 4 | keyframe requests made |
| 60 | 4 | audio worklet underruns |
| 64 | 4 | audio worklet overruns |
| 68 | 4 | RTT, µs |
| | | **72 bytes (v1)** |
| 72 | 1 | video transport tier |
| 73 | 1 | render tier |
| 74 | 1 | keyframe-request tier |
| 75 | 1 | audio-sink tier |
| 76 | 4 | errors the page swallowed (the `catch (_) {}` count) |
| 80 | 4 | degradation flags (below) |
| 84 | 4 | commanded audio jitter depth, µs |
| 88 | 4 | audio packets concealed |
| 92 | 4 | audio output latency, µs (0 = unknown) |
| | | **96 bytes (v2)** |

The length is a **floor**, never an equality: a newer client may append fields and an older host
reads the prefix it understands. A frame shorter than 72 bytes, or carrying version 0, is dropped.

**The v2 tail is not yet parsed by the host.** `input/protocol.rs::parse_client_stats` reads the
first 72 bytes and stops, so the tier bytes and the degradation field reach the host and are
discarded. Surfacing them is `findings/major-telemetry-client-stats-v2-tail-is-discarded.md`.
Until it lands, read them with `SEVideo.getStats()` on the device.

`host capture → paint` is the client half of glass-to-glass. It is only populated once the clock
offset has arrived; before that the client reports 0 there and clears `CLOCK_VALID`.

### Path codes

| Code | Video | Audio |
|---:|---|---|
| 0 | none | none |
| 1 | WebCodecs → canvas | worklet, SharedArrayBuffer ring |
| 2 | WebCodecs → video | worklet, `postMessage` ring |
| 3 | `<video>` element | Opus track (NetEQ) |

### Flags

| Bit | Meaning |
|---:|---|
| 0 | `SharedArrayBuffer` constructor exists |
| 1 | `crossOriginIsolated` |
| 2 | wake lock held |
| 3 | pointer lock held |
| 4 | keyboard lock available |
| 5 | fullscreen |
| 6 | `setSinkId` supported |
| 7 | decoder reports hardware acceleration |
| 8 | clock offset valid |

### Tier bytes (v2)

Each says which mechanism is actually live, so a device on a slow path is distinguishable from
one on the fast path rather than looking identical to it.

| Byte | 0 | 1 | 2 | 3 | 4 |
|---|---|---|---|---|---|
| transport (72) | none | `RTCRtpScriptTransform` | `createEncodedStreams` | | |
| render (73) | none | OffscreenCanvas 2D in the worker | track generator → `<video>` | `<video>` element | |
| keyframe (74) | **cannot ask** | `sendKeyFrameRequest()` | `KEYFRAME_REQUEST` (`0x64`) on the input channel | | |
| audio sink (75) | none | `AudioContext.setSinkId` | `HTMLMediaElement.setSinkId` | `navigator.audioSession` | `selectAudioOutput()` |

A keyframe tier of 0 with a transport tier of 2 is the Chrome 94–140 case: the legacy transform
runs instead of libwebrtc's reference finder, so nothing downstream emits a PLI and the client
has no way at all to ask for an IDR.

### Degradation flags (v2, offset 80)

Every one of these was previously a bare `catch (_) {}` or an untaken branch, invisible to the
host. The field is a union of the page's own bits and the audio module's.

| Bit | Meaning |
|---:|---|
| 0 | fullscreen was requested and refused |
| 1 | pointer lock was requested and refused |
| 2 | `unadjustedMovement` unavailable — the OS pointer curve is compounding with the host's |
| 3 | Screen Wake Lock API absent; on the `nosleep.js` shim |
| 4 | a wake lock request was refused |
| 5 | `AudioContext`/worklet setup failed |
| 6 | `AudioDecoder` cannot decode Opus (or failed after saying it could) |
| 7 | `AudioContext` is suspended |
| 8 | output latency is estimated from `getOutputTimestamp()`, not reported |
| 9 | audio-output labels unavailable (no microphone grant) |
| 10 | `RTCRtpScriptTransform` unavailable |
| 11 | no way to request a keyframe |
| 12 | `transferControlToOffscreen` unavailable |
| 13 | demoted to the `<video>` element |
| 14 | `getCoalescedEvents` unavailable |
| 15 | ICE gathering ran to its deadline instead of settling |
| 16 | no H.264 `fmtp` in the answer to pin the decoder to |
| 17 | no hardware decoder for the negotiated configuration |
| 18 | refresh rate was measured with rAF, not reported by the browser |
| 19 | stepped down from `RTCRtpScriptTransform` to `createEncodedStreams` |
| 20 | `requestVideoFrameCallback` unavailable |
| 21 | Keyboard Lock unavailable |

### Opcode note

`0x62` is used in both directions. Host → client it is the (currently unused) input-queue stats
frame built by `protocol::build_stats`, 25 bytes; client → host it is this frame. Direction
disambiguates: the host parser only ever sees the client's, and the client's `onReliableMessage`
only handles `PONG` and `BYE`.

## Reading it

- **`serve -v`** prints one summary line per active session every 5 s, from a dedicated thread.
  Off by default: the summary goes through the logbus, and every logbus line is an IPC event.
- **`ScreenExtend status`** and **`status --json`** ask the *running host* for its live telemetry
  over a new `STATS` verb on the single-instance control socket. `status` is a separate process
  and cannot otherwise see counters that exist only in the host's memory; without a running host
  the field is absent rather than zero.
- **`SEVideo.getStats()`** in the client page's console returns the same numbers the client is
  sending, plus the tier detail, in milliseconds. `SEAudio.getSyncInfo()` is its audio
  counterpart.

## Testing it

`node scripts/test-client-stats.mjs` runs `input.js` under Node with minimal browser stubs,
drives one PING/PONG exchange and one stats frame, and asserts the bytes against the layout
above (including the v2 tail and the 72-byte floor an older host reads) plus the clock-offset
arithmetic. It is not a substitute for a browser — it stubs the
DOM, and it cannot exercise the workers — but it catches the failure that would otherwise be
invisible: a `TypeError` inside `sendStats()` silently kills the once-a-second timer, and the
host would simply see a client that never reports.

The host half is covered by unit tests in `streamer/telemetry.rs` (bucket boundaries,
percentiles against a known distribution, missing and out-of-order stamps),
`streamer/input/protocol.rs` (a stats round trip, a truncated frame, a frame from a newer
client), and the clock conversions in `windows_utils/streamer/clock.rs` and
`macos_utils/streamer/clock.rs`.

## Adding a stage

1. Add the stamp to `FrameTiming` and a `mark_*` for it.
2. Add the derived histogram to the `stages!` macro invocation.
3. Fold it in `SessionTelemetry::record_frame`.
4. Set the stamp on every pipeline that has that stage; the ones that do not will report
   `count == 0`, which is the correct answer.
