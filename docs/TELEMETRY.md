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
  +17  u16  host capabilities — docs/INPUT-PROTOCOL.md, "Host capabilities"
       19 bytes total
```

Bytes 0..17 are frozen. A client older than a field reads the prefix it knows and ignores the
rest; a host older than a field sends a shorter frame, and a client that sees fewer than 19 bytes
reads no capability bits, which is the correct answer for that host.

`input.js::noteHostCaps` is the client half of that rule — `dv.byteLength >= 19 ?
dv.getUint16(17, true) : 0` — and it is the only source of `RemoteInput.hostCapabilities()`.
Nothing on the page may assume an optional mechanism works before the first `PONG` carrying the
bit lands: a request a host drops is not a mechanism, and a client that believed otherwise
disarmed the fallback that would have rescued it.

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
| `capture_available_ns` | The OS's own frame time: DXGI `LastPresentTime`, CGDisplayStream `displayTime` (only when in the past), ScreenCaptureKit PTS. 0 when the backend supplies none — **including WGC**, whose `SystemRelativeTime` is the composition's target present time, 3–14 ms after our callback on almost every frame (measured 2026-10-05; see `windows_utils/streamer/clock.rs`). On WGC, `capture_delivery` is therefore empty and the capture-relative stages start at callback entry. |
| `capture_cb_entry_ns` | Entry to our capture callback. |
| `scale_done_ns` | After the GPU downscale (ring path). |
| `encode_submit_ns` | Immediately before handing the frame to the encoder. |
| `encode_done_ns` | Immediately after the encoder returns the access unit. |
| `packetize_done_ns` | After H.264 payloading, before the first RTP write. |
| `socket_write_done_ns` | After the last RTP packet of the frame is written. |

Derived histograms: `capture_delivery`, `scale`, `encode_queue`, `encode`, `packetize`,
`socket_write`, `pace_wait`, `capture_to_encode`, `capture_to_socket`.

`pace_wait` is the odd one out: it is not derived from a `FrameTiming` pair but recorded
directly by the RTP writer, and it holds **only the time the send pacer slept** — not the
writes that sleeping is interleaved with. That separation is the point. Pacing and sending
both show up inside `socket_write`, and "the pacer is free on this path" has to be
distinguishable from "the socket is slow". On a `SessionLocality::SameNetwork` session the
pacer is off and this histogram is all zeroes, which is the correct reading.

A stage with `count == 0` did not fire — that absence is a finding, not a zero.

Histograms are fixed log-scale buckets, 8 per octave (any bucket is under 13% wide), 512 buckets
of `AtomicU64`. `record` is three relaxed `fetch_add`s and a `fetch_max`: no allocation, no lock,
no logging, which is what makes it safe on a real-time capture callback. Reported as
p50/p90/p99/max — never a bare mean, which hides the tail that is actually felt.

### Input stages

Three more stages ride the same histograms, but they are recorded per *event* by
`streamer::input`, not per frame, so `record_frame` does not feed them. The wire layouts they
depend on are in `docs/INPUT-PROTOCOL.md`.

| Stage | Span |
|---|---|
| `input_recv` | client send → host receive, minus the best case (see below) |
| `input_queue` | host receive → the injector thread picking the event up |
| `input_inject` | host receive → injection returned: the whole host-side cost |

`input_recv` needs a clock offset the host does not have — the client estimates one from
PING/PONG and never sends the result back — so the host estimates its own. What it can see
unaided is `host_recv − client_send` for every stamped frame: the true offset plus that frame's
one-way delay. The *minimum* of that over a rolling 30 s window is therefore the tightest offset
obtainable from one direction alone, and `input_recv` reports the difference — **excess one-way
delay**, 0 on the best sample and growing with queueing. It is a relative number by construction,
which is what a latency budget wants and is all one direction can honestly give. The window is
the same 30 s the client uses to age out its own best RTT sample, so a route change is not
remembered for the whole session.

Only frames carrying the optional client stamp are measured; an unstamped frame from an older
client contributes nothing rather than a wrong number.

## Delivered frame rate

The summary line reports `fps=<rate>` between consecutive summaries alongside the running
`frames=<total>`. A running total cannot distinguish a capture path keeping up at 60 fps from one
silently delivering 45, which is the failure this exists to make visible; the rate is computed
from the frame counter and the host clock at each summary, so it costs one atomic load per
reporting interval and nothing per frame.

## Degradation counters

`capture_fallback`, `capture_no_first_frame`, `frames_repeated`, `frames_dropped_ring`,
`frames_dropped_broadcast`, `encoder_output_drops`,
`nacks_received`, `keyframe_requests`, `client_keyframe_requests`,
`keyframe_requests_suppressed`, `delay_overuse_events`, `pacer_deadline_misses`, `idrs_sent`,
`encoder_resets`, `encoder_transient_errors`, `bitrate_cuts`, `audio_broadcast_lag_drops`, the
client-reported `client_jitter_underruns` / `client_jitter_overruns`, `frames_dropped_capture`,
and the four input counters `input_queue_drops` / `input_stale_drops` /
`input_reorder_drops` / `input_parse_errors`.

`capture_no_first_frame` counts the black placeholder frames the macOS encode loop synthesizes when
capture has produced nothing at all — a still virtual display, or a capture backend that started and
delivered nothing. Non-zero at the start of a session on an empty display is expected; non-zero on a
busy one means real capture never began. `encoder_output_drops` counts the times the encoder's
bounded output channel was full and an access unit was discarded to make room; each one breaks the
reference chain, so each one also forces an IDR.

`keyframe_requests` counts RTCP PLI and FIR; `client_keyframe_requests` counts the out-of-band
`KEYFRAME_REQUEST` (`0x64`) a client sends when it has no other way to ask, so the two sources
stay distinguishable.

Four of these describe the transport's own decisions rather than its failures, and are read as
pairs:

- `keyframe_requests_suppressed` is how many requests the per-session gate declined. A decoder
  that has lost its reference chain emits a PLI for every frame in its backlog, and on an
  infinite-GOP encoder answering each one produces the largest frame the session can send — which
  is what turns a stumble into a keyframe storm. So a large suppressed/requested ratio is healthy;
  `idrs_sent` is what the encoder was actually told to do. (Checked against the bitstream on the
  Windows NVENC ring path, 2026-10-05: 241 `idrs_sent`, 241 SPS-carrying access units written to
  the socket in the same session.)
- `frames_dropped_broadcast` is frames the encoder-to-viewer broadcast dropped on a viewer that
  fell behind. Distinct from `frames_dropped_ring`, which is the capture side.
- `delay_overuse_events` is intervals where the delay-gradient estimator said a queue was filling.
  It fires *before* loss, so a non-zero count against a flat loss count is the signal working, not
  a fault.
- `pacer_deadline_misses` is packets the pacer released early because holding them any longer
  would have outlasted the frame interval it was smoothing. A pacer that grows unbounded is worse
  than no pacer, so this must stay small.

The process-level `live_transport_sessions` sits alongside the per-session objects in
`snapshot_json`: peer connections, ICE agents and injector threads not yet given back. It should
equal the number of live sessions, and a number that only climbs across joins is a leak. `input_queue_drops` is the fast input queue shedding its oldest entry under
pressure; `input_stale_drops` is a positional event that waited more than 50 ms and was skipped
rather than injected late — never a key or a button, which are state transitions the host cannot
recover from, and never a pen or touch sample, which is ink rather than a position.
`input_reorder_drops` is a fast-channel frame whose sequence was behind one already injected: the
channel is unordered with no retransmits, so this is reorder, not loss, and it stays at zero until
a client starts sending the optional sequence (`docs/INPUT-PROTOCOL.md`).
`input_parse_errors` counts frames the input parser rejected — a client whose cached `input.js`
has drifted from the host shows up here and nowhere else, alongside one log line per distinct
error per channel.

`frames_dropped_capture` is presents the OS coalesced away because the host was still busy with
the previous frame: DXGI's `AccumulatedFrames` past the first, which the dirty-region check read
as a `> 1` early-out and never counted. **macOS feeds the same counter**, from
`CGDisplayStreamUpdateGetDropCount` on every CGDisplayStream delivery — read before the
dirty-rect early-out, because a delivery with nothing dirty can still be reporting frames dropped
on the way to it. The two platforms are therefore comparable in this cell, which was the point of
not filing the macOS number under `frames_dropped_ring` (ours, between capture and encode).

The macOS backend also logs it: once a second while it changes, and once on the stop line beside
the frame count and the mean time spent in the delivery callback. The log is not redundant with
the counter — a capture backend is constructed before the session's telemetry exists, so the very
first drops of a session reach the log and nothing else.

NACKs are counted and never logged: a lossy link produces them continuously and every logbus line
is a JSON IPC event to the desktop UI.

## Transport-derived signals (not counters)

Two things the transport now knows that are read rather than counted, because they are inputs to a
decision rather than a tally of what went wrong.

**`SharedFeedback::acked_frame_capture_ns()`** — the host-clock capture instant of the newest
access unit the client has confirmed receiving **in full**. Transport-wide feedback names every
sequence number that did and did not arrive, and the writer stamps those sequence numbers itself,
so `SendHistory` can keep a per-frame tally beside its per-packet ring and `congestion::interpret`
can vote every reported symbol against the frame its packet belonged to. A frame missing even one
packet is never acknowledged. The identity is the capture instant because the encoder already
stamps it on the frame, so nothing new has to be carried to match an acknowledgement back to a
reference frame. Nothing consumes it yet: it is the input VideoToolbox's
`kVTEncodeFrameOptionKey_AcknowledgedLTRTokens` and NVENC's reference invalidation both need.

**The session link medium** — `wired` / `wireless` / `other` for the local end of the nominated ICE
pair, resolved half a second after the connection comes up and printed as
`session link: local candidate <ip> is on a <medium> adapter`. Windows reads
`MSFT_NetAdapter.NdisPhysicalMedium`; macOS uses `getifaddrs` then `SIOCGIFMEDIA`. USB tethering and
RNDIS both report Ethernet, which is the point — it is the only way the host can tell a cabled
client from a Wi-Fi one, and every competitor in this category treats the wired path as its
low-latency tier.


## Audio stages

Audio does not travel with a `FrameTiming`; there is one host-wide capture fanned out to every
session, so its numbers live in `AudioDiagnostics` (`streamer/audio/mod.rs`) and are shared. Same
rules as the video histograms — fixed log-scale buckets of `AtomicU64`, no allocation and no lock
on the capture thread, reported as p50/p90/p99/max.

| Histogram | What it measures |
|---|---|
| `wake_interval` | Capture-thread wake to wake: the engine period **as delivered**, which is what says whether a low-latency period request actually took effect. |
| `encode` | Opus encode duration per 5 ms frame. |
| `capture_to_packet` | `host_now_ns()` when a packet is handed to the transport, minus that packet's own `capture_ns`. The host-side half of the audio budget. |

Counters alongside them: `period_frames` and `buffer_frames` (the engine period and the whole
engine buffer — different numbers, and they used to be reported as the same one),
`discontinuity_count`, `silent_count`, `device_changes`, `companion_restarts`,
`event_starvations`, `dropped_backpressure`, `lagged_packets`, `capture_ts_clamped`.

`companion_restarts` and `event_starvations` are the two that say the endpoint stopped producing:
before they existed, a dead silent-render companion looked exactly like a healthy idle host,
because the 2 s diagnostic kept printing with frozen counters. `capture_ts_clamped` counts drains
whose endpoint timestamp was in the future — on a Windows loopback stream that is the normal
case, not an anomaly, because the endpoint is a render device and its "device position" is a play
cursor.

### `ScreenExtend probe --audio`

The audio counterpart of `--whep-selftest`. It drives an `AudioHub` the way a session's forwarder
does, so it exercises the shipping capture backend, encoder and fan-out rather than a parallel
harness, and it needs no virtual display. `--duration <secs>` sets the sampling window (default
10); `--process-loopback` selects the second Windows capture tier, which is otherwise reached only
when the endpoint tier fails and so cannot be measured.

It prints packet rate and payload size, subscriber inter-arrival, capture -> subscriber latency,
the signed skew of `capture_ns` against the host clock (including how many packets were stamped
*ahead* of it, which is how the clamp above was found), and then the full `AudioDiagnostics`
summary. A tier that delivers nothing reports that as a failure with the two explanations for it,
rather than hanging.

## Client → host stats frame (`OP::STATS`, `0x62`)

Sent on the **reliable** DataChannel once a second, alongside the `PING`. Fixed offsets, little
endian. All durations are **microseconds**; a value of 0 means "not measured", which is what a
path with no hook for that stage reports.

Version 2 (96 bytes) appends the tier bytes and the degradation field below; version 3
(104 bytes) appends the two audio-redundancy counters. Bytes 0..72 are byte-identical to v1 and
0..96 to v2, which is what lets a host built against either read the prefix and ignore the rest.

| Offset | Size | Field |
|---:|---:|---|
| 0 | 1 | opcode `0x62` |
| 1 | 1 | version (currently 3; 0 is rejected) |
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
| 96 | 4 | redundant audio copies used — a copy that filled a real hole |
| 100 | 4 | redundant audio copies discarded — the cost on a link that did not need one |
| | | **104 bytes (v3)** |

The length is a **floor**, never an equality: a newer client may append fields and an older host
reads the prefix it understands. A frame shorter than 72 bytes, or carrying version 0, is dropped.

The host reads the tail whenever all 96 bytes arrived and leaves the fields at 0 otherwise, which
is also what "not measured" means for each of them — so a v1 client and a v2 client that measured
nothing are correctly indistinguishable. A frame one byte short of 96 reads as tail-absent rather
than half-parsed. `serve -v` names the four tiers in words and lists the set degradation bits by
name; `status --json` carries the raw codes, the decoded names and the audio numbers.

Each tail is read whole or not at all, and independently: a frame one byte short of 104 reads as
v3-tail-absent with everything before it intact, and a host built against v2 reads a v3 client's
first 96 bytes and drops the rest. `serve -v` prints `redundant=<used>/<discarded>` only when the
feature did something; `status --json` always carries both.

### Audio redundancy — the capability, the flag, and the two counters (v3)

The audio DataChannel header is 13 bytes (`streamer/audio/protocol.rs`, the authority): `seq` u32
LE, `capture_ns` u64 LE, `flags` u8, then the Opus payload. Two flag bits are defined host-side
today — `FLAG_SILENT` (1 << 0) and `FLAG_DISCONTINUITY` (1 << 1). The client additionally reads a
third:

```
FLAG_REDUNDANT   1 << 2      audio-worker.js
  body, when set:  u16 LE primary length | primary Opus packet | previous frame's Opus packet
  body, when clear: the Opus packet, as it always was
```

The trailing copy is the payload for `seq - 1` and nothing else. This is RFC 2198-style
redundancy without the RTP framing, and it is the only loss-robustness tool this path has: the
host's encoder is CELT-only at 5 ms, where libopus's in-band FEC is structurally inert.

**The capability is negotiated in the WHEP join body, not in a stats bit.** `audioCapabilities`
carries `redundancy` alongside `webcodecsOpus` and `worklet`:

```json
{ "webcodecsOpus": true, "worklet": true, "redundancy": true,
  "verified": true, "wantsFallbackTrack": true }
```

`redundancy` describes *this client build* rather than the browser — `audio-worker.js` always
understands the split — so `detectCapabilities()` returns it as a constant `true`, and
`index.html` overwrites it with `false` on the Opus-track path, which has no DataChannel to carry
a redundant body at all. A client older than the field sends nothing and serde defaults it to
`false`. A host must set `FLAG_REDUNDANT` only against a client that sent `true`: a client that
cannot split the body decodes the two payloads as one packet.

Both halves are live. `streamer/audio/protocol.rs::build_message` takes an optional previous
payload and sets the flag; `webrtc_session.rs::redundancy_now()` turns it on from the loss the
video BWE already measures, and only for a session whose join advertised `redundancy: true`. The
join body carries two more fields the client is the only side that can answer — `verified` (the
decoder was actually probed, not sniffed) and `wantsFallbackTrack` (put a spare Opus track in
the answer, because WHEP cannot add one later) — and the host logs the first and honours the
second.

Both counters reach the host: `parse_client_stats` reads bytes 96..104 into
`ClientStats::audio_redundant_{used,discarded}`.

The two counters are what the feature actually did once a host does send it. **used** (offset 96)
is a copy that filled a hole the client would otherwise have concealed; **discarded** (offset
100) is a copy whose frame had already arrived, and it is the payload the feature cost. `used`
climbing while `audio packets concealed` stays flat is redundancy working; `discarded` climbing
with `used` at zero is payload spent for nothing, which points at the host's loss threshold
rather than at the client. The client de-duplicates by sequence number, so a copy is never played
twice — playing it unconditionally would emit the same 5 ms of audio twice, which is worse than
the loss it was sent to repair. A gap wider than one frame still conceals the part the copy
cannot reach, and `FLAG_DISCONTINUITY` — the host saying the capture itself broke — suppresses
both the copy and the concealment, because there is nothing to bridge.

`host capture → paint` is the client half of glass-to-glass. It is only populated once the clock
offset has arrived; before that the client reports 0 there and clears `CLOCK_VALID`.

### Path codes

| Code | Video | Audio |
|---:|---|---|
| 0 | none | none |
| 1 | WebCodecs → canvas | worklet, SharedArrayBuffer ring |
| 2 | WebCodecs → video | worklet, private ring relayed by the page |
| 3 | `<video>` element | Opus track (NetEQ) |
| 4 | | worklet, private ring over a `MessagePort` |

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

Sink tier 4 is reachable from the Join button's click: `selectAudioOutput()` needs transient
activation, and until the join handler spent that click on it there was no gesture on the page to
hang it from, so the tier existed in this table and nothing could ever report it. It is claimed
only once the picked device is actually routed — a picker that returns a device the engine then
refuses to play on leaves the tier at whatever is true.

A keyframe tier of 0 with a transport tier of 2 is the Chrome 94–140 case: the legacy transform
runs instead of libwebrtc's reference finder, so nothing downstream emits a PLI and the client
has no way at all to ask for an IDR.

**Tier 2 is claimed only against a host that has said it parses `0x64`.** The client reads the
host-capability bitfield off the `PONG` tail (`docs/INPUT-PROTOCOL.md`) and treats bit 0 as the
gate: an older host advertises no bits, `RemoteInput.requestKeyframe()` returns false, the tier
stays 0, and the page keeps the two remedies that are actually available to it — the worker's
`nokeyframe` give-up after 8 s and the stall demotion to `<video>` after two 2 s ticks. Claiming
the tier unconditionally disarmed both and left a frozen picture with the host being told the
client had a working keyframe path.

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
| 9 | audio-output labels unavailable — the ids are named by the page, not the OS |
| 10 | `RTCRtpScriptTransform` unavailable |
| 11 | no way to request a keyframe |
| 12 | `transferControlToOffscreen` unavailable |
| 13 | demoted to the `<video>` element |
| 14 | `getCoalescedEvents` unavailable |
| 15 | an ICE wait ran to its deadline instead of settling |
| 16 | no H.264 `fmtp` in the answer to pin the decoder to |
| 17 | no hardware decoder for the negotiated configuration |
| 18 | refresh rate was measured with rAF, not reported by the browser |
| 19 | stepped down from `RTCRtpScriptTransform` to `createEncodedStreams` |
| 20 | `requestVideoFrameCallback` unavailable |
| 21 | Keyboard Lock unavailable |
| 22 | trickle ICE unavailable — the offer waited for gathering before it was posted |
| 23 | input was suspended so the user could reach a dialog on the page |
| 24 | the host named no session, so `/reconfig` and `/leave` reach nothing |

Bit 9 says the names beside the output ids are the client page's own numbering ("Output 1",
"Output 2", "Default output") rather than what the operating system calls the hardware. It is
**not** a permission failure: a browser withholds output labels until a microphone grant exists,
and the page no longer asks for one just to get nicer text — `setSinkId` takes the bare id, so an
unlabelled list routes exactly as well as a labelled one. The same fact reaches the device panel
as `labelled: false` on the `/audio-outputs` payload, `AudioOutputsReport` and the
`DeviceAudioOutputs` event, so the host can render the list without presenting invented names as
device names. The bit is still set when the microphone card *was* raised — enumeration returned
nothing at all — and declined, and when the device is reported with the default output alone.

### `/audio-outputs` — the output list and how it was found

The client posts this once its join exists (right after the answer), again on a device change
and whenever the sink in use moves. The body, every field but `outputs` optional so an older page
still parses:

| field | meaning |
|---|---|
| `supported` | `false` only when the browser has no way at all to move audio off the default output: no `setSinkId` on `AudioContext` or `HTMLMediaElement`, no `selectAudioOutput`, no iPhone audio-session category, and a secure context (on plain HTTP `navigator.mediaDevices` and `AudioContext.setSinkId` are hidden — Chrome 154 keeps the element's — which is the connection, not the browser). The desktop sheet's "This device's browser can't switch speakers" is exactly this value. The host treats a join that has not posted, or a body without the field, as `true` |
| `outputs` | `[{id, label}]`: the iPhone's `se:loudspeaker` / `se:earpiece` where they apply, then every routable device; when discovery found nothing, **exactly one** entry, `{id: "", label: "Default output"}` |
| `labelled` | whether any device entry carries the operating system's name (`false` for positional names and for the default-only report; degraded bit 9 mirrors it) |
| `selected`, `current` | as before: the sink the host asked for, and the one actually in use |
| `discovery` | **new.** One entry per output-discovery API run at Connect, in order, each `ok`, `unsupported`, `denied`, `empty`, `error` or `timeout` (below). Absent from a page that predates it |

`discovery` keys, in the order they run (owner spec, `findings/overhaul-new/OWNER-SPEC-audio-discovery.md`):

| key | API | bound |
|---|---|---|
| `selectAudioOutput` | the browser's chooser, started synchronously inside the Connect click; a dismissal is `denied`, a pick with no id `empty` | 30 s |
| `permissions.speaker-selection` (+ `.state`) | `permissions.query`; a `TypeError` for an unknown name is `unsupported` | 2 s |
| `permissions.microphone` (+ `.state`: `granted` / `prompt` / `denied`) | `permissions.query`; Safari's `TypeError` is `unsupported` | 2 s |
| `enumerateDevices` | `audiooutput` entries with a non-empty id; none is `empty` | 3 s |
| `AudioContext.setSinkId`, `HTMLMediaElement.setSinkId`, `audioSession`, `devicechange` | presence probes, `ok` / `unsupported` | — |
| `getUserMedia` | only when nothing above yielded a routable output: the page's microphone card, then one request. `denied` also covers Skip on the card and a permission already `denied` (no card is raised then); `unsupported` where there is no `getUserMedia` or no sink API that could route a list | 30 s |
| `enumerateDevices.afterMicrophone` | the one retry while the microphone is held; every track is stopped after it | 3 s |

A timeout is a definite failure of that API. An `error` or a `timeout` also counts as a
suppressed failure. The whole sequence is bounded (at most about 70 s plus however long the user
leaves the card up; the card always has Skip), and it finishes before the offer is sent, so the
report posted at the answer is the one fixed before the offer. The host logs one
`[audio-outputs] join <id>: supported=… outputs=N (kinds) labelled=… current=… discovery=k:v,…`
line per post and shows the last one per join in `ScreenExtend status --json` as
`telemetry.joins[].audioOutputs` (`supported`, `outputs`, `labelled`, `discovery`).

Bit 15 covers two different waits, both of which mean "ICE did not finish on its own". On the
gather-then-post tier it is the wait in front of the offer (`waitForIceGathering`). On the
trickle tier nothing waits for gathering at all, and it is instead the 8 s hold on the streaming
stage expiring — a browser that emitted neither a null candidate nor
`iceGatheringState === 'complete'`, so the page showed the picture rather than waiting forever.
Bit 22 tells the two apart: **set** is the gather wait, **clear** is the trickle hold.

On the trickle tier the hold's deadline is armed when the **answer names the trickle resource**,
not when the offer is posted. Before that the sender has no URL to PATCH and the peer connection
does not exist, so nothing about gathering is being waited on and there is no picture to hold
back; armed at the offer, the deadline also covered the WHEP round trip — which builds a virtual
display, with a 5 s attach timeout serialised across joins — and a host that was merely slow made
the page report a *gathering* failure for a join that then succeeded. A host that never answers
at all is bounded by the WHEP POST's own 10 s timeout, not by this bit. The hold is also released
the moment ICE reaches `connected`: from there a trickle failure cannot produce a rejoin, so the
picture has nothing left to wait for.

Bit 23 is set the first time the page took the pointer lock, the keyboard lock and the input
forwarding back from the host to put something in front of the user — a card, a modal, or the
browser's own output picker. Without it the host sees a device that simply stopped sending input
and cannot tell a dialog from a wedge. It is only set where a session was actually forwarding:
the microphone card raised before the join suspends nothing and reports nothing.

Bit 24 means the answer carried neither an `X-Join-Id` header nor a `Location` resource and no
`se_join` cookie was set, so this tab has no name for its own session. Only a proxy that strips
all three can produce it. The consequence is that `/reconfig` reads stable zeros and `/leave`
reaches nothing — the host will not resolve a request by the peer's address, because an address
moves under a live session and is handed on to the next device that asks for one, so guessing is
how one device ends up driving another's display. The picture still works; what is lost is every
live settings change and the prompt release of the display when the tab closes, which then waits
out the disconnect grace instead.

Bit 22 is set when `/net-config` did not say `trickleIce: true` (an older host, or the cloud
relay, which tunnels `/whep` but not `/net-config`), and in the cases the client cannot detect
before it posts: an answer with no `Location` header, a `PATCH` refused with 403/405/415/501, two
consecutive `PATCH` failures, or a first `PATCH` still unacknowledged after 2 s. In all of those
the page latches trickle off and rejoins once with a gathered offer. The bit says the join paid
the gather wait, which is the thing worth seeing on a slow join — bit 15 then says whether that
wait ran all the way to its deadline. `no_trickle_ice` is its name in
`CLIENT_DEGRADED_NAMES`, so it reaches the host's `degraded=…` summary line like every other
bit.

### Opcode note

`0x62` is client → host only. It used to be defined in both directions — the host → client twin
was a 25-byte injector-queue frame built by `protocol::build_stats` that was never sent — and that
builder is gone: the injector's counters live in `SessionTelemetry` instead, where `status --json`
reads them beside every other counter. The full input opcode table, and the optional client
timestamp several frames now carry, are in `docs/INPUT-PROTOCOL.md`.

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
above (including the v2 tail and the 72-byte floor an older host reads), the clock-offset
arithmetic, and the host-capability gate: a 17-byte `PONG` advertises nothing and
`requestKeyframe()` must then refuse *and send nothing*, a 19-byte one enables it, and an
attempt that could not go out must not burn the 200 ms limiter. It also asserts the optional
input tails: the stamp on each frame that carries one, the `MOUSE_DELTA` flags byte, and the
fast-channel sequence — its four offsets, that it advances by exactly one per fast frame, that a
reliable frame carries none, and that nothing at all changes until capability bit 5 arrives.

`node scripts/test-client-audio.mjs` runs `audio-worker.js` under Node with stub WebCodecs
classes and feeds it real DataChannel frames: a plain packet, a redundant one with no loss
(the copy must be discarded, not played — playing it doubles the audio), a redundant one after
a single loss (the copy is decoded first, at the earlier timestamp), a duplicate, a late
packet, a discontinuity, and a truncated redundant body.

`node scripts/test-client-ice.mjs` lifts `waitForIceGathering` and its two helpers out of
`index.html` and drives them against a fake peer connection: the candidate/grace table, that a
relayed wait is *not* ended by a host or reflexive candidate, the relay grace, gathering
completing, listener cleanup, and the deadline setting `ICE_GATHERING_DEADLINE`. It lifts the
trickle sender out of the same file and drives that too: the `trickle-ice-sdpfrag` body, that
nothing is PATCHed before the answer names a resource, that a candidate the offer already carried
is not sent twice, `a=end-of-candidates`, a 404 stopping the sender rather than retrying into a
dead session, and a burst larger than one batch going out in order.

Neither is a substitute for a browser — they stub the DOM and cannot exercise the workers — but
each catches a failure that is otherwise invisible: a `TypeError` inside `sendStats()` silently
kills the once-a-second timer and the host simply sees a client that never reports, and an ICE
wait that ends too early ships an offer with no usable candidate and looks exactly like a
network fault.

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

An event-shaped stage skips steps 1 and 3 — `FrameTiming` and `record_frame` are frame-shaped —
and calls `telemetry.stages.<name>.record(ns)` directly, the way the three input stages do.
