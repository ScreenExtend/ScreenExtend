# Competitors — open-source low-latency screen streaming

## 1. Question

What proven low-latency screen-streaming engineering exists in open-source projects that
ScreenExtend does not do? Torn down from actual source: Sunshine, Moonlight (common-c + Qt),
Chromium/libwebrtc, Apache Guacamole, RDP/MS-RDPEGFX, OBS Studio, FFmpeg encoder wrappers, NDI,
Barrier/Input Leap, `webrtc-rs` itself (the crate ScreenExtend depends on), and libwebrtc's
jitter buffer.

## 2. Findings

### 2.1 `webrtc-rs` 0.17.1 has no pacer and no bandwidth estimator

Read locally from the vendored crate sources at
`~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/{webrtc,interceptor}-0.17.1/`.

`interceptor-0.17.1/src/` contains exactly: `chain.rs error.rs lib.rs mock nack noop.rs
registry.rs report stats stream_info.rs stream_reader.rs twcc`. There is **no pacing module and
no congestion-control module**. `grep -rn "pacer\|Pacer"` over both crates returns nothing.

This is the single biggest architectural gap versus libwebrtc, whose
[Paced Sending doc](https://chromium.googlesource.com/external/webrtc/+/master/modules/pacing/g3doc/index.md)
says the pacer exists "to prevent network congestion by smoothing out bursty transmission
patterns into a more uniform flow", pacing at `kDefaultPaceMultiplier` = **2.5×** the estimated
bitrate, and notes it matters most for screensharing because "video encoders overshoot the
target frame size in case of sudden movement especially when dealing with screensharing".

### 2.2 `register_default_interceptors` does *not* install the TWCC **sender**

`webrtc-0.17.1/src/api/interceptor_registry/mod.rs:20-31`:

```rust
pub fn register_default_interceptors(mut registry: Registry, media_engine: &mut MediaEngine)
    -> Result<Registry> {
    registry = configure_nack(registry, media_engine);
    registry = configure_rtcp_reports(registry);
    registry = configure_twcc_receiver_only(registry, media_engine)?;   // <-- receiver only
    Ok(registry)
}
```

`configure_twcc_receiver_only` (same file, lines 134-171) registers the `transport-cc` **feedback
attribute** and the **header-extension capability** for video and audio, and adds
`twcc::receiver::Receiver` — but not `twcc::sender::Sender`. Only the `Sender` interceptor stamps
the transport-wide sequence number onto outgoing RTP
(`interceptor-0.17.1/src/twcc/sender/mod.rs:43-90`: "Sender adds transport wide sequence numbers
as header extension to each RTP packet").

Consequence: ScreenExtend advertises `transport-cc` in its SDP answer
(`webrtc_session.rs:102-105`) but never puts the extension on the wire, so **the browser cannot
generate a single TWCC feedback packet for the video stream**. The host's only congestion signal
is the RTCP Receiver Report, which Chrome emits roughly once a second.

### 2.3 NACK repairs go out on the primary SSRC; RTX is registered but disabled

`webrtc-0.17.1/src/rtp_transceiver/rtp_sender/mod.rs:153` — `enable_rtx: setting_engine.enable_sender_rtx`,
and `src/api/setting_engine/mod.rs:106,376-381` shows `enable_sender_rtx` is a plain `bool` in a
`#[derive(Default)]` struct, i.e. **false** unless `enable_sender_rtx(true)` is called.

The NACK responder (`interceptor-0.17.1/src/nack/responder/mod.rs:74-80`) simply re-writes the
buffered packet to `stream3.next_rtp_writer`, i.e. the primary SRTP stream with the original
sequence number. Its history is a ring of `1 << 13` = **8192 packets**
(`responder/mod.rs:41`, `responder_stream.rs:18-25`), which at ~1.2 KB/packet is ~10 MB of
send history — plenty.

NACK *is* installed for ScreenExtend's video stream: `create_stream_info` is built from the
**negotiated** codec returned by `track.bind()` (`rtp_sender/mod.rs:479-487`), whose
`rtcp_feedback` comes from the MediaEngine registration (which does contain `nack`), not from the
empty `rtcp_feedback: vec![]` on the `TrackLocalStaticRTP` capability.

### 2.4 Sunshine (host)

Source: `LizardByte/Sunshine`.

| Aspect | What Sunshine does |
| --- | --- |
| Transport | ENet control channel + raw UDP/RTP video socket, `send_buffer_size(1024*1024)`, `platf::batched_send()` (batches capped at 64 KB or 64 packets "to avoid Windows SO_SNDBUF bypass") |
| Codec (NVENC) | preset P1..P7 (default P1 "superfast"), `tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`, `rateControlMode = NV_ENC_PARAMS_RC_CBR`, `zeroReorderDelay = 1`, `enableLookahead = 0`, `gopLength = NVENC_INFINITE_GOPLENGTH`, `vbvBufferSize = bitrate*1000/framerate` |
| Slicing | `format_config.sliceMode = 3; format_config.sliceModeData = client_config.slicesPerFrame;` — client asks for N slices per frame |
| Reference frames | `configure_reference_frames()`, H.264 default `maxNumRefFrames = 5` |
| Loss handling | **Reference frame invalidation**: `for (auto i = first_frame; i <= last_frame; i++) nvenc->nvEncInvalidateRefFrames(encoder, i)`, guarded by `last_frame - first_frame + 1 < ref_frames_in_dpb`. Frame header `frameType` = 2 for IDR, **5 for post-invalidation**, 1 for P |
| Intra refresh | `intraRefreshPeriod = 300; intraRefreshCnt = 299;` when the client requests it |
| FEC | Reed-Solomon, default `fec_percentage = 20`, up to `MAX_FEC_BLOCKS = 4` per frame, data shards `D = (255*100)/(100+F)`, parity `(data_shards*F + 99)/100`; FEC is disabled for a frame that would need >4 blocks |
| Pacing | `ratecontrol_packets_in_1ms = giga::num * 80/100 /1000 /blocksize /8` (~80 % of 1 Gbps), then `timer->sleep_for(due - now)` |
| Per-frame telemetry | `frame_processing_latency` in the packet header, **1/10 ms units** |
| Capture | DXGI Desktop Duplication *and* WGC, both to a `display_vram_t`; not dirty-rect aware, full `CopyResource`, repeats the last frame when `!frame_update_flag`; RGB→NV12 with pixel shaders; `IDXGIKeyedMutex` between capture and encode |
| FFmpeg option sets | nvenc: `delay=0 forced-idr=1 zerolatency=1 surfaces=1 cbr_padding=false`. amf: `filler_data=false forced_idr=1 latency=1 async_depth=1 usage=2 (ULTRA_LOW_LATENCY)`. qsv: `forced_idr=1 async_depth=1 low_delay_brc=1 low_power=1 vcm=1 recovery_point_sei=0 pic_timing_sei=0 max_dec_frame_buffering=1`. vaapi: `async_depth=1 sei=0 idr_interval=INT_MAX`. videotoolbox: `realtime`, `prio_speed=1`, flag `-low_delay`. libx265: `x265-params=info=0:keyint=-1` |
| Virtual display | Sunshine itself does not ship one; the community pairs it with `virtual-display-rs` / the same Windows IDD ScreenExtend bundles |

### 2.5 Moonlight (clients)

`moonlight-common-c/src/Limelight.h`:

- `packetSize`: "Max video packet size in bytes (use 1024 if unsure). If STREAM_CFG_AUTO
  determines the stream is remote … it will cap this value at **1024** to avoid MTU-related
  issues like packet loss and fragmentation."
- `streamingRemotely` = `STREAM_CFG_AUTO` uses "whether the target address is in the RFC 1918
  address blocks" to decide remote vs local — i.e. the same locality classification ScreenExtend
  computes and then ignores.
- `CAPABILITY_DIRECT_SUBMIT` (0x1) — "audio/video data to be submitted directly from the receive
  thread. This should only be specified if the renderer is non-blocking."
- `CAPABILITY_REFERENCE_FRAME_INVALIDATION_AVC` (0x2) / `_HEVC` (0x4).
- `CAPABILITY_SLICES_PER_FRAME(x)` — "the renderer supports slicing to increase decoding
  performance."
- `DECODE_UNIT.frameHostProcessingLatency` — "Optional host processing latency of the frame, in
  1/10 ms units", plus `receiveTimeUs` and `enqueueTimeUs`.

`moonlight-common-c/src/RtpVideoQueue.c` — Reed-Solomon `reed_solomon_decode()` recovery, a fast
path for in-order packets that flips to `queue->useFastQueuePath = false` on any reorder, and a
`SPECULATIVE_RFI_COOLDOWN_PERIOD_US = 300000000` (5 min) cooldown that disables "speculative RFI
recovery logic" after an out-of-order packet. Unrecoverable frame → `notifyFrameLost()` → IDR.

`moonlight-qt/app/streaming/video/ffmpeg.cpp` — decoder flags `AV_CODEC_FLAG_LOW_DELAY`
("Always request low delay decoding"), `AV_CODEC_FLAG_OUTPUT_CORRUPT`, `AV_CODEC_FLAG2_SHOW_ALL`,
`AV_EF_EXPLODE` so a silent frame drop becomes an explicit IDR request. Software decode uses
`FF_THREAD_SLICE` with `thread_count = min(MAX_SLICES, cores)`; hardware decode uses
`thread_count = 1`.

`.../pacer/pacer.cpp` — `#define MAX_QUEUED_FRAMES 3` (asserted equal to
`PACER_MAX_OUTSTANDING_FRAMES`), `#define TIMER_SLACK_MS 3`. Adaptive drop: over a 500 ms window,
`frameDropTarget = 3` when the stream FPS exceeds display FPS and queues stay short, otherwise
`frameDropTarget = 1`. **When frame pacing is disabled the V-sync thread is not created at all
and frames go straight to the render queue.** Crucially, Moonlight recovers from backlog by
*dropping frames*, never by requesting a keyframe.

### 2.6 libwebrtc / Chromium

- `playout-delay` RTP header extension
  ([spec](https://webrtc.googlesource.com/src/+/main/docs/native-code/rtp-hdrext/playout-delay/README.md)):
  12 bits each for MIN and MAX, granularity **10 ms**, range 0–40950 ms. "A value of 0 … indicates
  to the receiver that the frame should be rendered as soon as possible."
- `RTCRtpReceiver.playoutDelayHint` (Chrome M79+) and the standardized
  `RTCRtpReceiver.jitterBufferTarget` are the receiver-side equivalents.
- Pacer: leaky bucket at 2.5× the estimated bitrate.
- The Selkies project's ["Optimize the WebRTC stack to the maximum"
  issue](https://github.com/selkies-project/selkies/issues/157) — the closest public checklist
  from a comparable WebRTC remote-desktop — calls the `playout-delay` extension "a MUST
  implementation", lists `abs-send-time`, `transport-wide-cc`, and `video-timing` extensions, and
  **warns against pinning `jitterBufferTarget`/`playoutDelayHint` to exactly 0** because it causes
  stuttering; it also recommends `sps-pps-idr-in-keyframe=1` in the H.264 fmtp line.

### 2.7 RDP / MS-RDPEGFX

The interesting latency mechanism is closed-loop client backpressure, not the codec:
`RDPGFX_FRAME_ACKNOWLEDGE_PDU` (MS-RDPEGFX 2.2.2.8) carries a **`queueDepth`** field — the client
reports its own decode backlog and the server throttles frame rate / encoding quality in
response. FreeRDP's `SetQueueDepthHint` exposes it: 0 = report the real depth, 100+ = heavy
throttle, `0xFFFFFFFF` = pause new frames entirely. The RemoteFX **Progressive** codec sends a
low-quality frame immediately and refines it with delta passes — latency-first, quality-second.

### 2.8 NDI, Guacamole, Input Leap

- **NDI** high-bandwidth uses SpeedHQ, an **all-intra** codec: no inter-frame dependency at all,
  so no IDR/PLI/reference-invalidation problem exists, at 100–150 Mbps for 1080p60 and
  sub-frame (<16 ms) encode+decode latency. Not applicable to a Wi-Fi second monitor, but it
  bounds the "what if we never had reference chains" question.
- **Guacamole** is a lossless-ish instruction protocol over WebSocket/TCP; nothing to copy for a
  video path.
- **Input Leap / Barrier** — plain TCP with `TCP_NODELAY` (Nagle disabled) is the whole trick.
  ScreenExtend's input already rides an unordered SCTP DataChannel, which is strictly better.

## 3. What ScreenExtend does today

Already at parity or better — do not "fix" these:

- Host sends `PlayoutDelayExtension::new(0, 0)` on every video packet
  (`src-tauri/src/streamer/webrtc_session.rs:505-507,539`), and registers the extension
  (`webrtc_session.rs:164-172`).
- Client sets `receiver.playoutDelayHint = 0` (`static/index.html:832`),
  `receiver.jitterBufferTarget = 0` (`index.html:874`), and
  `decoder.configure({ codec, optimizeForLatency: true })` (`static/transform-worker.js:157`).
- RTP timestamps carry the shared host clock so the client can compute true glass-to-glass delay
  (`webrtc_session.rs:511-512`, `transform-worker.js:57-93`).
- NVENC is already ULL-tuned: `NV_ENC_PRESET_P1_GUID` + `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`,
  CBR, `vbvBufferSize = bitrate/fps`, `zeroReorderDelay(1)`, lookahead 0, AQ off, multipass
  disabled, infinite GOP, `frameIntervalP = 1`
  (`windows_utils/streamer/nvidia/encoder.rs:542-590`). This matches Sunshine field for field.
- Intel/QSV already sets `AsyncDepth = 1`, `LowPower = ON`, `TargetUsage = BEST_SPEED`,
  `GopRefDist = 1`, `MaxDecFrameBuffering = 1`, `LowDelayBRC = ON`
  (`windows_utils/streamer/intel/encoder.rs:354,408-434,463`).
- macOS already builds the encoder spec with `EnableLowLatencyRateControl`
  (`macos_utils/streamer/encoder.rs:499-505`), `RealTime`, `AllowFrameReordering = false`,
  `MaxFrameDelayCount = 1`.
- Capture is dirty-driven with a frame-duration rate limit and a repeat thread for idle content
  (`windows_utils/streamer/pipeline.rs:531-540,554-629`) — Sunshine's `minimum_fps_target`
  equivalent.

Gaps, with `file:line`:

| # | Gap | Where |
| --- | --- | --- |
| G1 | Whole access unit's RTP packets written in a tight unpaced loop | `webrtc_session.rs:527-545` |
| G2 | No transport-cc sequence numbers on outbound RTP; BWE is loss+RTT from ~1 Hz RTCP RR, polled at 120 ms | `webrtc_session.rs:175-176, 862-951`; `bitrate.rs:105-122` |
| G3 | `maxNumRefFrames = 1`, `numRefL0/L1 = 1`; every PLI/FIR ⇒ full IDR; no reference-frame invalidation | `nvidia/encoder.rs:601-603`; `webrtc_session.rs:475-481` |
| G4 | NVENC `sliceMode = 0` (one slice per frame) while the repo's own QSV path uses 4–8 slices | `nvidia/encoder.rs:599-600` vs `intel/encoder.rs:413` |
| G5 | `SessionLocality` computed then only logged — MTU, FEC and bitrate policy are identical LAN vs relayed | `webrtc_session.rs:587`; `RTP_PAYLOAD_MTU` fixed at `1200 - 12` at `webrtc_session.rs:502` |
| G6 | Host encode latency measured but only printed every 60 frames, never sent to the client | `pipeline.rs:631-649` |
| G7 | Client recovers from decoder backlog by requesting a keyframe | `transform-worker.js:229-243` |
| G8 | RTX codecs registered in SDP but `enable_sender_rtx` left false | `webrtc_session.rs:126-141`, `udp_only_setting_engine()` at `:185-194` |
| G9 | No FEC of any kind | (absent) |
| G10 | `intra_refresh: false` by default | `streamer/config.rs:167` |

## 4. Recommendations, ranked by expected delta ÷ cost

### R1 — Pace RTP output (leaky bucket at ~2.5× target). **Biggest win per line.**

API: none new. Insert a token bucket between `payloader.payload(...)` and
`track.write_rtp_with_extensions(...)` in `webrtc_session.rs:527-545`, refilled at
`2.5 * current_target_bps` (the value the bitrate driver already computes at `:945`), burst floor
2 packets, and a hard ceiling so pacing never delays a packet by more than one frame interval.
Platform: all. Probe: none needed. Expected win: removes a 30–60 packet microburst per frame into
the Wi-Fi TXOP; on congested 802.11 this is where 10–40 ms of queuing delay and most induced loss
comes from, and induced loss is what triggers the PLI→IDR→bigger-burst spiral. Measure: p95/p99 of
the client's `videoDelayEmaMs` (`transform-worker.js:91`) and `fraction_lost` from the BWE log
line at `webrtc_session.rs:941-944`, before/after, on a busy 2.4 GHz AP.

### R2 — Turn on send-side TWCC and add a delay-based estimator.

API: add `registry.add(Box::new(interceptor::twcc::sender::Sender::builder()))` after
`register_default_interceptors` in `build_api()` (`webrtc_session.rs:174-176`) — the transport-cc
header extension is already registered for video by `configure_twcc_receiver_only`, so this is the
only missing piece. Then parse `rtcp::transport_feedbacks::transport_layer_cc::TransportLayerCc`
in the RTCP read loop already running at `webrtc_session.rs:466-485` and feed an arrival-time
delay gradient (GCC trendline: `d(i) = (arrival_i - arrival_{i-1}) - (send_i - send_{i-1})`,
EWMA, over/under threshold) into `BitrateController` alongside `estimate_from_loss`. Platform:
all. Probe: if the browser's answer omits the extension, fall back to today's loss-only path.
Expected win: cuts bitrate *before* the queue fills instead of after 10 % loss
(`bitrate.rs:113`); on a shared Wi-Fi link this is the difference between 20 ms and 200 ms of
standing queue. Measure: standing RTT from `RemoteInboundRTP.round_trip_time` during a bandwidth
step-down, and time-to-recover after starting a competing download.

### R3 — Stop paying a full IDR for every loss.

Three steps, cheapest first:

1. Default `intra_refresh` to true (`config.rs:167`) and adopt Sunshine's rolling parameters
   (`intraRefreshPeriod ≈ 300`, `intraRefreshCnt = period - 1`) instead of today's
   `fps*4 / fps/2` (`nvidia/encoder.rs:594-598`). A rolling refresh self-heals loss with no
   keyframe bitrate spike at all.
2. Raise `maxNumRefFrames` from 1 to 4–5 and `numRefL0` accordingly (`nvidia/encoder.rs:601-603`)
   so a repair frame can reference an older good frame.
3. Add reference-frame invalidation: `nvEncInvalidateRefFrames(encoder, frame_index)` per frame in
   the damaged range (Sunshine's exact call), driven from the NACK responder's misses — a NACK for
   a sequence number older than the 8192-packet history
   (`interceptor-0.17.1/src/nack/responder/responder_stream.rs:51-62` returns `None`) is
   unrepairable and identifies the damaged frame.

Platform: NVENC now; QSV/VideoToolbox have no direct RFI equivalent, so step 1 covers them.
Probe: `NV_ENC_CAPS_SUPPORT_INTRA_REFRESH` / `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION` via
`nvEncGetEncodeCaps`. Expected win: eliminates the periodic 5–10× frame-size spike (and the burst
it causes) on every lossy moment. Measure: count IDRs per minute and correlate with
`videoDelayEmaMs` spikes.

### R4 — Multi-slice NVENC.

API: `h264.sliceMode = 3; h264.sliceModeData = N;` at `nvidia/encoder.rs:599-600`, N chosen the
way the repo's QSV path already does (`(height/256).clamp(4,8)`, `intel/encoder.rs:413`); OBS uses
`sliceMode = 3, sliceModeData = 1` and Sunshine uses the client-requested count. Platform:
Windows/NVENC. Probe: `NV_ENC_CAPS_SUPPORT_MULTIPLE_SLICES`. Expected win: decode-side slice
parallelism on weak client CPUs (Moonlight sets `FF_THREAD_SLICE` for exactly this) and loss
containment — one lost RTP packet corrupts one slice, not the whole frame. Measure: client
`framesDecoded` vs `framesIn` under injected loss; decode time on a low-end phone.

### R5 — Make `SessionLocality` actually change policy.

API: thread the detected locality (`webrtc_session.rs:587`) into the writer task and set
`RTP_PAYLOAD_MTU` to ~988 (1000 − 12) when `CrossNetwork`, keeping 1188 on `SameNetwork`.
Rationale: Moonlight caps `packetSize` at **1024** for remote streams "to avoid MTU-related issues
like packet loss and fragmentation", and Sunshine's default packetsize is 1024. A relayed path
adds SRTP tag + TURN ChannelData + outer UDP/IP on top of the 1200-byte payload. Platform: all.
Probe: already implemented. Expected win: eliminates IP fragmentation and PMTU black-holing on
cross-network sessions — a correctness/reliability win more than a millisecond win, but
fragmentation loss is catastrophic (one lost fragment kills the whole datagram). Measure: relayed
session `fraction_lost` and time-to-first-frame over a TURN path.

### R6 — Send per-frame host processing latency to the client.

API: reuse the existing `reliable` DataChannel and the pong path (`webrtc_session.rs:432-437`).
Sunshine puts `frame_processing_latency` in **1/10 ms units** in every video packet header;
Moonlight surfaces it as "host processing latency". ScreenExtend already computes the number
(`pipeline.rs:632-649`) and throws it away. Expected win: zero latency directly — but without it
neither we nor a user can attribute a stall to capture, encode, network, or decode, and every
other recommendation here is unmeasurable in the field. Do this first if measurement is the
bottleneck.

### R7 — Client backlog: drop frames, don't ask for a keyframe.

`transform-worker.js:229-243` sets `waitingForKey = true` and calls `requestKey()` when
`decoder.decodeQueueSize` exceeds ~5 % of fps twice in a row. Moonlight's pacer instead keeps
`MAX_QUEUED_FRAMES 3` and drops with `frameDropTarget` 1–3 and never requests an IDR for
backlog. Change: drop the delta frame (already done — the `continue` at `:239`) but do **not**
flip `waitingForKey`; only request a key frame if the backlog persists past ~1 s. Platform:
client page, no build step, no new API. Expected win: removes a self-inflicted keyframe + freeze
every time the client hiccups.

### R8 — Either enable sender RTX or drop the RTX codec registration.

`se.enable_sender_rtx(true)` in `udp_only_setting_engine()` (`webrtc_session.rs:185-194`), or
delete the rtx codec registration at `webrtc_session.rs:126-141`. Today the SDP answer advertises
`rtx/90000` with `apt=`, but no RTX stream is ever created, so retransmissions go out on the
primary SSRC with the original sequence number. It works (Chrome accepts it) but it is misleading
and it inflates the primary stream's loss accounting. Speculative — measure before believing.

### R9 — FEC for relayed sessions only.

Sunshine ships Reed-Solomon at a 20 % default because a NACK round trip is unaffordable when RTT
is high; `webrtc-rs` has no flexfec/ulpfec implementation, so this means writing one (or an
in-band scheme over the existing DataChannel, which is non-standard). On a LAN with 1–3 ms RTT,
NACK+retransmit is already cheaper than 20 % parity overhead, so this is **cross-network only**
and should be gated on R5's locality plumbing. Lowest priority; largest cost.

### Findings files

Five of these gaps were already logged by the discovery phase; I did not duplicate them. The
mapping from recommendation to `findings/` file:

| Rec | Findings file | New here? |
| --- | --- | --- |
| R1 | `major-webrtc-no-send-pacer.md` | **new** |
| R2 | `major-webrtc-transport-cc-advertised-but-never-stamped.md` | pre-existing (discovery) |
| R3 | `major-nvenc-idr-only-loss-recovery.md` | **new** (RFI + `maxNumRefFrames`); intra-refresh reachability already in `speculative-nvenc-intra-refresh-is-unreachable-from-any-shipping-path.md` |
| R4 | `major-nvenc-single-slice-no-subframe-readback.md` | pre-existing, and stronger — it also found `enableSubFrameWrite` is never set |
| R5 | `major-webrtc-rtp-mtu-fixed-at-1188-on-relayed-paths.md` | **new**; the locality plumbing it needs is `major-webrtc-session-locality-is-computed-then-thrown-away.md` (pre-existing) |
| R6 | `major-client-no-glass-to-glass-telemetry.md` | pre-existing, and stronger — it found the client's `videoDelayEmaMs` compares two different clock epochs, i.e. the number is not currently a latency at all |
| R7 | `minor-client-backlog-threshold-triggers-keyframe-thrash.md` | pre-existing |
| R8 | `speculative-webrtc-rtx-registered-but-disabled.md` | **new** |
| R9 | `speculative-webrtc-no-fec-on-relayed-paths.md` | **new** |

Note on R6: because `major-client-no-glass-to-glass-telemetry.md` shows the existing client-side
delay number is measured against mismatched epochs, **every "measure `videoDelayEmaMs` before and
after" instruction in this report is blocked on that fix landing first.** Treat it as the
prerequisite for the whole verification plan below.

## 5. Verification plan

Measure before/after, per change, on the Windows 11 + RTX 3080 Ti box with a phone client:

0. **Prerequisite**: fix the client's latency metric first. `videoDelayEmaMs`
   (`transform-worker.js:65-73`) subtracts `performance.timeOrigin + performance.now()` (Unix epoch)
   from a host RTP timestamp derived from a monotonic `Instant` epoch captured at host process
   start (`streamer/audio/mod.rs:17-27`) — two different epochs, so the absolute value is
   meaningless and only its *changes* are usable. See
   `findings/major-client-no-glass-to-glass-telemetry.md`.
1. **Glass-to-glass**: once (0) lands, log p50/p95/p99 of the corrected delay over 60 s.
2. **Burst/queue behaviour** (R1, R2): `fraction_lost` and `round_trip_time` from the existing BWE
   log line (`webrtc_session.rs:941-944`), with and without a competing iperf/download on the same
   AP.
3. **IDR cost** (R3, R7): count `RTCP PLI received` lines (`webrtc_session.rs:476`) per minute and
   the encoded size of the frames that follow.
4. **Decode** (R4): client `framesIn`/`framesDecoded`/`decodeQueueSize` from `postStats()`
   (`transform-worker.js:95-103`) on the weakest available client.
5. **Relayed path** (R5, R9): a cross-network session through the configured TURN server.

Cannot be verified on the dev boxes:

- Anything AMD (`windows_utils/streamer/amd.rs` is a stub).
- macOS ScreenCaptureKit / VideoToolbox low-latency behaviour — the dev Mac is 10.15.8 Intel with
  Command Line Tools only, so SCK (12.3+) and the newer VT paths never select.
- Multi-slice VideoToolbox (`kVTCompressionPropertyKey_MaxH264SliceBytes`) — same reason.
- Real cellular/relayed conditions (R5, R9) need a second network, not a loopback TURN.

## 6. Open questions

1. Does enabling send-side TWCC change Chrome's own behaviour in a way we don't want (e.g. Chrome
   starting to apply REMB/TWCC-derived limits to its receive path)? Needs an A/B on a real device.
2. Is the 8192-packet NACK history in `webrtc-rs` long enough at 20 Mbps to cover a relayed RTT?
   At ~1.2 KB/packet that is ~9.8 MB, i.e. ~4 s at 20 Mbps — yes, comfortably.
3. Does a browser `VideoDecoder` with `optimizeForLatency: true` handle a multi-slice AVC stream
   without extra buffering on all target platforms (Android Chrome, iOS Safari)? Must be
   measured, not assumed, before R4 ships.
4. Selkies warns that pinning `jitterBufferTarget = 0` causes stuttering. ScreenExtend does
   exactly that at `static/index.html:874` (and 30 ms on the non-WebCodecs path at `:922-923`).
   Since the WebCodecs path bypasses the browser renderer entirely, this is probably correct here
   — but it is worth an A/B against a 10–20 ms target on a lossy link.
5. Nothing in the tree acts on `SessionLocality`. Was that deliberate, or is the plumbing simply
   unfinished?

## Sources

- [Sunshine](https://github.com/LizardByte/Sunshine) — `src/nvenc/nvenc_base.cpp`, `src/stream.cpp`, `src/video.cpp`, `src/config.cpp`, `src/platform/windows/display_vram.cpp`
- [moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c) — `src/Limelight.h`, `src/RtpVideoQueue.c`
- [moonlight-qt](https://github.com/moonlight-stream/moonlight-qt) — `app/streaming/video/ffmpeg.cpp`, `app/streaming/video/ffmpeg-renderers/pacer/pacer.cpp`
- [webrtc-rs](https://github.com/webrtc-rs/webrtc) — read locally from `webrtc-0.17.1` / `interceptor-0.17.1` crate sources
- [webrtc-rs interceptor](https://github.com/webrtc-rs/interceptor)
- [libwebrtc Paced Sending](https://chromium.googlesource.com/external/webrtc/+/master/modules/pacing/g3doc/index.md)
- [playout-delay RTP header extension](https://webrtc.googlesource.com/src/+/main/docs/native-code/rtp-hdrext/playout-delay/README.md)
- [Selkies — Optimize the WebRTC stack to the maximum](https://github.com/selkies-project/selkies/issues/157)
- [OBS Studio obs-nvenc](https://github.com/obsproject/obs-studio/blob/master/plugins/obs-nvenc/nvenc.c)
- [FFmpeg libavcodec/nvenc.c](https://github.com/FFmpeg/FFmpeg/blob/master/libavcodec/nvenc.c)
- [MS-RDPEGFX RemoteFX Progressive Codec](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/1dcd953d-672b-457e-9dec-a6fe639bae8f), [RDPGFX_FRAME_ACKNOWLEDGE](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/17aaf205-23fe-467f-a629-447f428fdda0)
- [NDI bandwidth/latency docs](https://docs.ndi.video/all/getting-started/white-paper/bandwidth)
- [Input Leap](https://github.com/input-leap/input-leap)
- [kVTVideoEncoderSpecification_EnableLowLatencyRateControl](https://developer.apple.com/documentation/videotoolbox/kvtvideoencoderspecification_enablelowlatencyratecontrol), [WWDC21 "Explore low-latency video encoding with VideoToolbox"](https://developer.apple.com/videos/play/wwdc2021/10158/)
