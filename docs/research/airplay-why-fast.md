# Why AirPlay mirroring from a 10.15 Mac feels faster than ScreenExtend

## 1. Question

The user observes that AirPlay mirroring from an old Intel Mac (macOS 10.15 Catalina) to an iPad
has visibly lower latency than ScreenExtend on the same Mac, even though ScreenExtend already uses
hardware H.264 (VideoToolbox) and WebRTC. Why? Specifically: what does AirPlay do at the radio, the
capture, the encoder, the wire, and the receiver that we do not — and which of those are reachable
from a third-party app on 10.15 with a browser client?

A note on the premise before anything else. **macOS 10.15 cannot AirPlay-mirror to an iPad.**
Apple's AirPlay receiver on iPad does not exist (AirPlay-to-Mac arrived in macOS 12.4; there has
never been an AirPlay-to-iPad receiver). What a 10.15 Mac *can* do to an iPad is **Sidecar**, which
is selected from the same AirPlay menu-bar picker and is what most users call "AirPlaying to the
iPad" ([Apple](https://support.apple.com/en-us/102597), [MacRumors Sidecar
guide](https://www.macrumors.com/guide/sidecar/)). The other possibility is a third-party AirPlay
receiver app on the iPad, in which case the transport is ordinary infrastructure Wi-Fi and *none* of
the AWDL advantage below applies. Both readings are covered; where the answer differs, it is called
out. The engineering conclusions are the same either way, because Sidecar and AirPlay share the
radio (AWDL), the encoder family (VideoToolbox), and the "WindowServer owns the display" capture
model.

## 2. Findings

### 2.1 AWDL — the radio is the single biggest structural difference

AWDL (Apple Wireless Direct Link) is Apple's undocumented 802.11-based ad-hoc protocol, reverse
engineered by Stute et al. in *One Billion Apples' Secret Sauce* (MobiCom '18,
[arXiv:1808.03156](https://arxiv.org/abs/1808.03156),
[ar5iv](https://ar5iv.labs.arxiv.org/html/1808.03156)). It is what AirDrop, AirPlay and Sidecar ride.

Mechanism, from the paper:

- Each node announces a sequence of **Availability Windows (AWs)** of **16 TU ≈ 16.384 ms**
  (1 TU = 1024 µs); an **Extended AW** is 64 TU ≈ 65.5 ms (4 consecutive AWs, presence mode 4).
- The announced channel sequence covers **(15+1)·(3+1) = 64 AWs ≈ one second**, over 16 slots.
- An elected **master** synchronises the sequences. Measured synchronisation error is Gaussian with
  mean −0.45 ms, σ = 0.98 ms; the 3 TU target is met in **>99 %** of cases. Master re-election on a
  no-master timeout of 96 AWs ≈ 1.5 s.
- Default **social channels are 6, 44 and 149** (country dependent).
- Outside its AWs a node retunes to the infrastructure AP channel, so **AWDL and infra time-slice
  the one radio**. In the "data+infra" state each gets ~50 % of airtime (25 % minimum for AWDL in
  low-power mode). When the AP is on a different channel, cumulative throughput drops **~13 %**.
- Peak observed AWDL throughput is limited only by the PHY (~866.7 Mbit/s on 802.11ac 2×2).

Why that beats us: an AWDL link is **one hop**. Mac → iPad, directly, on a channel the two peers
agreed on, with rate adaptation tuned to that one link. ScreenExtend's browser client goes
Mac → AP → iPad: **two transmissions, two contention windows, and the AP's queue in between**. On a
loaded or distant AP the AP queue is where bufferbloat and the latency tail live. AWDL also avoids
the client's infrastructure power-save state machine, which is the classic source of 100 ms-class
Wi-Fi RTT spikes ([Cisco U-APSD](https://www.cisco.com/c/en/us/td/docs/wireless/controller/ewc/17-3/config-guide/ewc_cg_17_3/unscheduled_automatic_power_save_delivery.pdf),
[Sniff Wi-Fi on 802.11 power save](https://www.sniffwifi.com/2016/05/go-to-sleep-go-to-sleep-go-to-sleep.html));
inside an AW both peers are awake by construction.

**AWDL is not reachable by a third party in any useful way.** Three independent confirmations:

1. The paper itself: *"Apple drops packets for regular TCP and UDP servers that directly bind to the
   `awdl0` interface"* — you must opt in through the Bonjour/Network-framework peer-to-peer APIs
   (`NSNetService.includesPeerToPeer`, `NWParameters.includePeerToPeer`, Multipeer Connectivity),
   which give you a socket the OS may route over AWDL, not a socket you control.
2. Even with `includePeerToPeer`, the link runs in a degraded mode. An Apple developer-forums thread
   ([819926](https://origin-devforums.apple.com/forums/thread/819926)) documents that peer-to-peer
   video over AWDL is *"extremely choppy"* until the OS flips the link into **"realtime mode"**
   (visible in the logs as `Received event realtimeMode` / `Realtime mode updated true`), which
   happens when Universal Control/Sidecar engages. Asked whether there is a programmatic way to
   trigger it, an Apple DTS engineer answered **"No."** — not `includePeerToPeer`, not
   `serviceClass.interactiveVideo`, nothing. The only documented control is `WAPerformanceMode.realtime`
   in the Wi-Fi Aware API (iOS 26+, **no macOS support**). So AirPlay/Sidecar get a link quality
   third parties provably cannot ask for.
3. **A browser cannot reach an AWDL peer at all.** Safari's WebRTC stack gathers host candidates
   from routed interfaces and, since iOS 15, replaces local IPs with mDNS `.local` candidates
   ([draft-ietf-mmusic-mdns-ice-candidates](https://www.ietf.org/archive/id/draft-ietf-mmusic-mdns-ice-candidates-02.html),
   [BlogGeek](https://bloggeek.me/webrtcglossary/mdns/)). There is no web API that opts a page into
   peer-to-peer Wi-Fi. **The dependency-free-browser-client constraint permanently rules AWDL out.**

There is a sting in the tail that works in our favour, and it is actionable. AWDL costs the *host's*
radio time even when we are not using it. Christoff Visser (IIJ Research Lab, RIPE 91, reported by
[The Register](https://www.theregister.com/2025/10/23/apple_airdrop_awdl_latency_research/)) measured
a Moonlight stream at 20 ms with 25 ms variance degrading to **3–90 ms fluctuating RTT** because the
Mac periodically hopped its radio to the AWDL social channels; the effect disappears when the link
itself sits on a social channel. Community measurements put the hop at roughly **1–2 s every 10–12 s,
costing 50–200 ms stalls** ([Meter](https://www.meter.com/mac-osx-awdl-psa),
[Network Weather](https://www.networkweather.com/learn/awdl/),
[Fortinet](https://community.fortinet.com/t5/Blogs/AppleTV-AirPlay-and-AWDL-protocol-with-Wi-Fi-hard-to-diagnose/ba-p/238440)).
AirPlay does not suffer this because AirPlay *is* the AWDL traffic. **We do suffer it, on every
macOS host, today.**

### 2.2 AirPlay mirroring's transport — much simpler than WebRTC, and it is TCP

The unofficial protocol documentation ([nto.github.io/AirPlay.html](https://nto.github.io/AirPlay.html))
and the reference receivers ([UxPlay](https://github.com/FDH2/UxPlay),
[RPiPlay](https://github.com/FD-/RPiPlay), openairplay/airplay2-receiver, shairplay) agree:

- Mirroring is set up over an **RTSP/1.0-flavoured control socket** (port 7000/7100). `POST /stream`
  carries `latencyMs` (the example in the docs is **90**), plus `param1`/`param2` — the AES key
  encrypted with FairPlay, and its IV.
- The **video stream is a single plain TCP connection** to the `dataPort` returned in the type-110
  SETUP response. Verified in UxPlay's `lib/raop_rtp_mirror.c`: `listen(dsock, 1)` / `accept()`;
  socket options are only `SO_RCVTIMEO` (5 ms), `SO_KEEPALIVE` and the TCP keepalive tuning — **no
  `TCP_NODELAY`, no `SO_RCVBUF`, and no application-level retransmission or NACK whatsoever.**
  Reliability is TCP's. This is the same conclusion the repo's own design doc reached
  (`docs/airplay-fallback-design.md:660-680`: *"grep -n 'send|write(|sendto' over all 965 lines of
  `raop_rtp_mirror.c` returns nothing: the channel is strictly one-way"*).
- Framing: a **128-byte header** + payload. `u32` LE payload size at offset 0; **packet type at
  offset 4** (`0x00` video, `0x01` unencrypted codec data = SPS/PPS in avcC form, `0x02` heartbeat
  once per second, `0x05` a binary-plist streaming report); `u16` at 6–7; **`u64` NTP timestamp at
  8–15** (remote wall clock in ns); source/display dimensions as floats at 16–23, 40–47 and 56–63;
  zero padding to 128.
- Payload NAL units are **4-byte big-endian length-prefixed**, converted to Annex-B start codes by
  the receiver; the codec-data packet sets a `prepend_sps_pps` flag so the *next* frame carries its
  parameter sets.
- Video payload is **AES-CTR**, keyed from `ekey` through the FairPlay derivation plus a SHA-512 over
  the `streamConnectionID`. Decryption is a solved problem in the open-source world (playfair /
  `omg_hax` tables, GPLv3): RPiPlay measures large-packet AES at **~2 ms average, 7 ms worst case on
  a Pi Zero**, so it is free on a Mac.
- Timing is **NTP (RFC 5905) on ports 7010/7011**, with the *receiver* acting as NTP client, polling
  the sender **every 3 s**. That is the whole clock-sync mechanism — no RTCP SR/RR, no ICE, no
  consent freshness.
- No DTLS, no SRTP, no ICE, no STUN/TURN, no pacer, no congestion controller, no jitter buffer, no
  RTX stream, no keyframe-request protocol.

What WebRTC adds on top of that, quantified:

| WebRTC element | Cost | Notes |
|---|---|---|
| ICE gathering + DTLS handshake | one-time, typically **hundreds of ms to ~2 s** | Firefox telemetry puts DTLS alone at ~200 ms ([Hancke](https://medium.com/@fippo/how-long-does-the-dtls-handshake-take-86718dd966bf)). Affects *time to first pixel*, **not** steady-state glass-to-glass. |
| SRTP encrypt/decrypt | µs/packet | AES-CTR-128 or GCM; the same order as AirPlay's own AES-CTR. Not a factor. |
| Pacer | 0 for us | We write RTP directly (see §4). A stock `libwebrtc` pacer would add ~frame-interval-scale delay. |
| Receiver **jitter buffer** | **~90 ms** | This is the big one. Multi's engineering write-up measured *"~90 ms in latency reduction — the biggest change we had made since moving from a server-based to a P2P solution"* from setting `max_playout_delay` to zero ([multi.app](https://multi.app/blog/making-illegible-slow-webrtc-screenshare-legible-and-fast)). libwebrtc's default `kStartDelayMs` is 80 ms. |
| Packetization/depacketization | ~0 | |

So the fair statement is: **WebRTC's structural latency tax over AirPlay's wire format is the
receiver jitter buffer, and almost nothing else.** ScreenExtend has already removed it on its fast
path (§4) — but not on its fallback path, and the iPad is exactly the client most likely to land on
the fallback.

### 2.3 Capture on the sending Mac

AirPlay mirroring runs inside **WindowServer** (which lives in
`/System/Library/PrivateFrameworks/SkyLight.framework/.../WindowServer`). The mirrored display is
created by WindowServer itself — this is precisely the mechanism this repo already exploits in
`src-tauri/src/macos_utils/airplay/`, where standing up a fake receiver makes **macOS** create a real
extended-desktop `CGDirectDisplayID` (`airplay/mod.rs:1-20`).

What the internal path skips relative to `CGDisplayStream`:

- **No cross-process surface handoff.** CGDisplayStream is a client API: WindowServer composites,
  then vends an `IOSurface` to your process's dispatch queue. The compositor's own encoder feed does
  not leave WindowServer's address space.
- **No `kCGDisplayStreamMinimumFrameTime` coalescing window** (see §5, R3).
- **No queue depth negotiated by a client** — `kCGDisplayStreamQueueDepth` exists because the client
  may be slow; the in-process path has no such contract.
- Apple's own successor confirms the layer difference: ScreenCaptureKit captures "at the compositor
  level rather than through WindowServer", which is where its 30–40 % CPU reduction over
  CGDisplayStream comes from — but SCK needs 12.3+ and is unavailable on the user's box.

Is any of it reachable? There are private SkyLight entry points —
`SLSHWCaptureStreamCreateWithWindow` (introduced in 10.15), `CGSHWCaptureWindowList`,
`SLSHWCaptureWindowList`, `CGSCaptureWindowsContentsToRectWithOptions` — reverse-engineered by
projects like AltTab ([alt-tab-macos#122](https://github.com/lwouis/alt-tab-macos/issues/122),
[SkyLight survey](https://github.com/zhutao100/macos-private-framework-research-skill/blob/main/framework-surveys/skylight-agent-survey/seed/research_SkyLight_framework.md)).
They are **window** capture, not display capture; they are undocumented, unsigned-symbol-resolved,
and their signatures change between releases. They are not a credible foundation, and nobody ships a
display-mirroring product on them. **Verdict: the WindowServer capture advantage is real but not
harvestable.** Its magnitude is also modest — one IOSurface handoff, not tens of milliseconds.

### 2.4 AirPlay's encoder configuration

There is no published, reliable teardown of AirPlay's `kVTCompressionPropertyKey_*` values. What the
open receivers establish from the bitstream itself:

- H.264 (H.265 in AirPlay 2 to capable receivers), **avcC codec data re-sent whenever geometry
  changes** (this is what `mirror.rs` keys its geometry-change detection off).
- SPS/PPS prepended to the next frame after a codec-data packet — i.e. **parameter sets are repeated
  in-band**, the standard low-latency live-streaming choice.
- One access unit per 128-byte-framed packet with a per-frame NTP capture timestamp; **no evidence of
  B-frames or frame reordering** — the receiver decodes and renders in arrival order, which is only
  correct with reordering off.
- A `latencyMs` field in `POST /stream` (documented example: **90**) that the sender advertises to
  the receiver, i.e. Apple's own budget for the pipeline is around 90 ms, not 10.

This is, feature for feature, the configuration ScreenExtend already uses on macOS: `RealTime = true`,
`AllowFrameReordering = false`, `MaxFrameDelayCount = 1`, in-band parameter sets, per-frame host
timestamps (§4). **The encoder is not where the gap is.** Treat any claim about AirPlay's exact QP or
GOP settings as unverified — I found no packet-capture study that publishes them.

### 2.5 Receiver buffering and published latency numbers

Reliable numbers are scarce and the field is full of content-farm fabrications. Grading what exists:

- **Luna Display / Astropad (trustworthy, same problem domain, published method).** Photographing a
  timer on both screens simultaneously: **Luna 11.3 ms over USB, 22.4 ms over Wi-Fi**; Air Display
  52.9 / 131.4 ms; Duet 60 fps 95.0 / 108.1 ms
  ([Astropad](https://astropad.com/blog/comparing-latency-luna-display/)). Astropad's own LIQUID
  targets *"at least 16 ms … 1–4 ms wired, 16–32 ms over Wi-Fi"*
  ([Astropad technology](https://astropad.com/technology/)). Note the shape: **a well-built Wi-Fi
  second-display product lands in the 20–30 ms band**, and the AP-relayed competitors land at
  100 ms+.
- **AirPlay's own advertised budget: `latencyMs: 90`** in the protocol docs — the most defensible
  AirPlay-specific figure available, and it is a *sender-advertised* budget, not a measurement.
- **Sidecar:** no instrumented public measurement I can stand behind. Apple Pencil latency figures
  (20 ms → 9 ms) are input latency, not display latency. One reviewer network trace attributed
  "hundreds of milliseconds" of Sidecar latency spikes to AWDL channel hopping
  ([domos.ai](https://domos.ai/blog/apple-disrupts-your-video-conferences-with-airplay-and-sidecar)).
- **Discard:** the widely-syndicated "AirPlay 87–94 ms, down from 168–212 ms, oscilloscope-measured"
  figure traces to `lifetips.alibaba.com`, an AI-generated content farm with no rig description.
  Likewise the "Sidecar 14–18 ms, MacWorld Labs & Six Colors" citation — those publications have no
  such study. **Do not put either in a design document.**
- Receiver behaviour in the open implementations: UxPlay/RPiPlay **render on arrival**. They use the
  header's NTP timestamp for logging and A/V alignment, not to schedule a delayed presentation. Since
  they visibly work, an AirPlay receiver's video buffer can be ~0 frames. Apple TV presumably
  schedules against the NTP-derived clock to hold A/V sync, and the ~90 ms `latencyMs` is the number
  audio is aligned to.

**Bottom line on the perceived gap:** if the user's iPad session is Sidecar, the credible causes in
descending order are (1) one radio hop instead of two, on a link the OS has put into a privileged
realtime mode, (2) the client is a native HEVC decoder rendering on arrival rather than a browser,
(3) the compositor feeds the encoder in-process. If the iPad session is a third-party AirPlay
receiver app, cause (1) evaporates and the answer is almost entirely (2) plus our own AWDL
self-interference.

### 2.6 Sidecar, for comparison

Sidecar uses **HEVC** with Apple's dedicated hardware encode/decode blocks (the T2 on Macs that have
one; the iPad's own block), Bluetooth LE for discovery and handshake, and Wi-Fi (AWDL) for the data
([iMore](https://www.imore.com/ipad-new-mac-display-sidecar-explained)). Wired-over-USB is offered
and is reliably better than wireless — the same wired/wireless split Astropad measures (11.3 vs
22.4 ms). Nothing in Sidecar is a codec trick we can copy: HEVC on a 2015-era Intel Mac would be
*slower* to encode than H.264, and no browser decodes HEVC in WebCodecs on the iPads in question.

### 2.7 The repo's own fake AirPlay receiver as a lower-latency path

Read: `src-tauri/src/macos_utils/airplay/{mod,receiver,rtsp,fairplay,mirror}.rs` and
`docs/airplay-fallback-design.md`.

Today the module exists only to make **macOS** create a display on 10.13–10.14 where
`CGVirtualDisplay` does not exist (`airplay/mod.rs:1-14`); it advertises `displays[0].maxFPS = 1`,
answers `/fp-setup` from static tables, and **frames-and-discards** the mirror stream
(`mirror.rs:1-18`, `docs/airplay-fallback-design.md:660-680`). `VirtualDisplayController::max_concurrent_displays()`
returns `Some(1)` for this tier because macOS owns that display.

Could the incoming H.264 be decrypted and relayed instead of captured and re-encoded?

- **Decryptable: yes.** The static `/fp-setup` tables in `fairplay.rs` are only the *handshake*; the
  actual key recovery is `fairplay_decrypt` over `ekey`, implemented in the open by playfair
  (RPiPlay `lib/playfair/`, ~483 KB of GPLv3 tables) and used by UxPlay, RPiPlay, airplay2-receiver
  and shairplay-rust. The payload cipher is AES-CTR; RPiPlay measures ~2 ms average per large packet.
  Licence-wise GPL-3 combines with this repo's AGPL-3 under AGPL-3 §13, as the repo's own doc already
  notes (`docs/airplay-fallback-design.md:815-826`).
- **Would it be faster? Almost certainly not.** A relay replaces
  `CGDisplayStream → VideoToolbox → RTP` with
  `WindowServer capture → WindowServer's VideoToolbox → AES-CTR → TCP → our AES-CTR decrypt →
  re-packetize → RTP`. We save our own capture+encode (~one frame interval) and pay Apple's, plus a
  full TCP loopback hop with **no `TCP_NODELAY`** on the reference receivers, plus a stream whose
  bitrate, GOP, QP and resolution we no longer control, plus the `POST /stream` `latencyMs` budget
  the sender is already targeting (90 ms). It also forfeits every knob the whole overhaul depends on:
  per-client bitrate from loss-based BWE, on-demand IDR for a rejoining client, DPR ladder control.
- **Structurally disqualifying:** one display maximum, macOS chooses the geometry (the module already
  has to fight this via `sender_prefs.rs` and `topology.rs`), the picker must be driven through
  Accessibility (`ax.rs`, 1036 lines), the whole tier sits **below the supported 10.15 floor** and is
  kept only as a safety net, and the session is a 40 s budget with a 6 s discovery settle
  (`airplay/mod.rs` `SESSION_BUDGET`, `DISCOVERY_SETTLE`).

**Verdict: no.** Keep the module as the sub-10.15 display fallback it is. Do not build a relay.

## 3. How others do it

| Product | Display source | Capture | Codec | Transport | Published latency |
|---|---|---|---|---|---|
| **AirPlay mirroring** | WindowServer creates it | in-process compositor feed | H.264/HEVC, VideoToolbox, no reorder, in-band SPS/PPS | **plain TCP**, 128-byte header, AES-CTR, NTP every 3 s, no NACK/pacer/jitter buffer | sender advertises `latencyMs: 90` |
| **Sidecar** | native second display | in-process | HEVC, HW blocks both ends | AWDL (BLE for handshake); USB option | none credible |
| **Luna Display** | **hardware dongle** presents a real display to the GPU | normal capture of a real display | LIQUID (H.264/HEVC + GPU) | proprietary, "Velocity Control" rate loop | **11.3 ms USB / 22.4 ms Wi-Fi** |
| **Duet Display** | virtual display | capture | H.264 | proprietary over TCP | 95 ms USB / 108 ms Wi-Fi |
| **Multi (WebRTC screenshare)** | n/a | getDisplayMedia | VP8/H.264 | WebRTC | **−90 ms from `max_playout_delay = 0`**; QP 4/36; `maintainResolution` |
| **ScreenExtend** | `CGVirtualDisplay` (10.15+) / AirPlay-borrowed (10.13–14) | CGDisplayStream or SCK | H.264 VideoToolbox | WebRTC/WHEP, hand-packetized RTP, NACK+RTX | to be measured |

The instructive row is Luna: it beats everyone by making the Mac believe a **real** display is
attached (a DisplayPort dongle), so there is no virtual-display driver in the path — and it still
doubles its latency going from USB to Wi-Fi. **The radio is worth more than the codec.**

## 4. What ScreenExtend does today

Grounded in the tree. The good news is that most of the WebRTC-specific tax is already gone.

**Wire (already AirPlay-like):**
- Raw RTP, hand-packetized, **no pacer**: `TrackLocalStaticRTP` +
  `track.write_rtp_with_extensions` (`src-tauri/src/streamer/webrtc_session.rs:449`, `:539`).
- **`playout-delay` header extension pinned to (0, 0)** on every packet
  (`webrtc_session.rs:167-172` registers the extension, `:505-506` constructs
  `PlayoutDelayExtension::new(0, 0)`). This is exactly Multi's −90 ms change, done at the sender.
- Per-frame RTP timestamp from the shared host clock: `audio::host_ns_to_rtp90k(host_instant_to_ns(frame.capture))`
  (`webrtc_session.rs:512`) — the same idea as AirPlay's per-frame NTP stamp, and better placed
  (in the RTP timestamp, so the client can invert it).
- NACK + RTX registered (`webrtc_session.rs:95-153`) — strictly better than AirPlay's TCP-only
  recovery for a lossy link.
- UDP-only ICE (`udp_only_setting_engine`, `webrtc_session.rs:185-188`).

**Client fast path (already bypasses the jitter buffer):**
- `transform-worker.js` pumps `transformer.readable` from an `RTCRtpScriptTransform` and decodes
  with **WebCodecs `VideoDecoder`** configured `optimizeForLatency: true`
  (`static/transform-worker.js:158`, `:186-196`), drawing to an `OffscreenCanvas` context created
  `{ alpha: false, desynchronized: true }` (`static/index.html:157` area / worker `onmessage`).
  Frames never re-enter the WebRTC pipeline. In Chromium the receiver transform is invoked from
  `RtpVideoStreamReceiver2::OnAssembledFrame` **before** `OnCompleteFrame` reaches the frame buffer,
  so this genuinely sits ahead of the jitter buffer.
- Belt and braces on top: `index.html:832` sets `receiver.playoutDelayHint = 0` and `index.html:874`
  sets `receiver.jitterBufferTarget = 0`, both feature-guarded.
- It self-limits queueing: if `decoder.decodeQueueSize` exceeds `max(3, 0.05·fps)` twice, it drops to
  the next keyframe rather than accumulating (`transform-worker.js:230-247`) — an AirPlay-grade
  "never buffer, resynchronise instead" policy.

**Client fallback path (does *not*):**
- `index.html:166-168` gates the fast path on `RTCRtpScriptTransform` **or**
  `createEncodedStreams`, **and** `VideoDecoder`, **and** `canvas.transferControlToOffscreen`.
  Failing any of those, or on a 5 s watchdog / stall / decoder-config error, `fallbackToVideo()`
  (`index.html:736`, `:764`, `:844`, `:912`, `:993`) attaches the MediaStream to a `<video>` element
  (`index.html:920`) — **the full libwebrtc receive path, including the jitter buffer**. It is not
  left at the default: `index.html:922-923` sets `jitterBufferTarget = 30` and
  `playoutDelayHint = 0.03`, both feature-guarded and both **no-ops on WebKit**, which implements
  neither. So on an iPad the fallback runs at whatever Safari's own default is, and the only lever
  left is the sender-side `playout-delay` extension — whether Safari honours a (0,0) hint is
  unverified. Budget the fallback at *up to* Multi's ~90 ms on Safari, ~30 ms where the hint lands.

**macOS capture:**
- `cgds.rs` requests **420f** (`pipeline.rs:256/282/370`, passed as `cfg.pixel_format` into
  `CGDisplayStream::with_dispatch_queue`, `cgds.rs:184`), `kCGDisplayStreamQueueDepth = 2`, ITU-R 709
  matrix, and **`kCGDisplayStreamMinimumFrameTime = (1/fps)·0.95`** (`cgds.rs:70-73`). Frames arrive
  as `IOSurface` → `CVPixelBuffer` with no copy. The dispatch queue is created against the
  user-interactive global queue.
- `sck.rs` uses `queueDepth = 3` (SCK's minimum) and `minimumFrameInterval = 1/fps` — 12.3+ only, so
  **not available on the user's 10.15 box**.

**macOS encode (`macos_utils/streamer/encoder.rs`):** `RealTime = true` (`:151`),
`AllowFrameReordering = false` (`:159`), `MaxFrameDelayCount = 1` (`:215`), `ExpectedFrameRate`,
`MaxKeyFrameInterval(+Duration)`, `AverageBitRate` + `DataRateLimits`, 709 primaries/transfer/matrix,
and optional-by-probe `MaximizePowerEfficiency` / `PrioritizeEncodingSpeedOverQuality` /
`Quality` / `Max|MinAllowedFrameQP` / `EnableLowLatencyRateControl` (`:505`). On 10.15 the last group
is silently rejected (they are 11.0–13.0 keys), so rate control there is `AverageBitRate` +
`DataRateLimits` only. **This already matches everything the AirPlay bitstream evidences.**

**Radio:** `macos_utils/hosted_network.rs` brings up a real **HostAP** via the private
`startHostAPModeWithSSID:securityType:channel:password:error:` selector, preferring 5 GHz channels
`PREFERRED_5GHZ_CHANNELS = [36, 40, 44, 48]` and falling back to channel 11
(`hosted_network.rs:12`, `:36`, `:38-57`, `:99-180`). **Channel 44 is an AWDL social channel.**
Windows has the `netsh`-based equivalent. In both cases this is a manual, side-door feature, not the
default and not presented as the low-latency path.

## 5. Recommendations

Ranked by expected latency delta ÷ implementation cost. Every one is 10.15-safe: none introduces a
link-time reference to an API newer than 10.15, and every capability is probed.

---

### R1 — Make the one-hop direct link a first-class, measured "low latency" mode

**This is the AirPlay lesson.** We cannot have AWDL, but we can have the property that makes AWDL
fast: **one wireless transmission instead of two, with no AP queue in the middle.** Both platforms
already have the machinery.

- **API/flag:** existing `start_hosted_network` — macOS `CWInterface` +
  `startHostAPModeWithSSID:securityType:channel:password:error:` (`hosted_network.rs:60-98`),
  Windows `netsh wlan start hostednetwork`. Nothing new to write on the transport side.
- **Platform:** macOS 10.15+ (private selector, already resolved by `NSInvocation` with an
  `objc_exception` guard, so a missing selector degrades rather than crashes); Windows 10.
- **Probe:** `is_hosted_network` / `interfaceMode() == CWInterfaceMode::HostAP` already exists.
  Add a probe for whether the selector resolves at all before advertising the mode.
- **Expected win:** removes one 802.11 transmission and the AP's queue. Median ~3–10 ms; the tail is
  where it pays — Astropad's own data shows a good Wi-Fi product at 22 ms and AP-relayed competitors
  at 100 ms+, and the difference is not the codec. On a busy AP, expect 50–200 ms of p99 to vanish.
- **Measure:** the harness in R6, run twice — associated to the AP vs associated to the host AP —
  reporting p50/p95/p99 glass-to-glass and the `getStats` `jitter` / `framesDropped` series.
- **Constraint:** the frontend UI may not change appearance, so surface this through the CLI
  (`network start`) and the existing hosted-network UI, plus an automatic *recommendation* in the log
  bus when we detect a poor link — not a new visual element.

Findings file: `findings/major-network-direct-link-lowlatency-mode.md`. **Blocked on two findings
from the discovery pass that must land first:**
`findings/blocker-hostap-private-spi-called-without-respondstoselector.md` (the private selector is
invoked without a `respondsToSelector:` guard) and
`findings/major-hostap-offers-only-wep-or-a-silently-open-network.md` (a modern iPad will refuse or
warn about the security type currently offered). Neither is a latency question, but the mode cannot
be recommended to users until both are fixed.

---

### R2 — Guarantee (and measure) the jitter-buffer-free client path on iPad; never fall back silently

The `<video>` fallback costs the full libwebrtc receive path. Multi measured that at **~90 ms**. The
iPad is precisely the client where the fast path is fragile: it needs `RTCRtpScriptTransform`
(Safari 15.4+), `VideoDecoder` (Safari 16.4+) **and** `transferControlToOffscreen` (Safari 16.4+),
and `index.html:166-168` requires all three. An iPad on iPadOS 15–16.3 silently gets the slow path,
and WebKit exposes no `jitterBufferTarget` to shrink it from JS.

- **Do:** (a) report which path was chosen and *why it was rejected* back to the host (a `/reconfig`
  or WHEP-header field) and log it via `tprintln!`, so "why is this device slow" is answerable;
  (b) on the fallback, verify empirically whether Safari honours the sender's `playout-delay(0,0)`
  extension — if not, add `video.playsInline` + the smallest possible `<video>` pipeline and consider
  a **WebCodecs-without-insertable-streams** path (decode from a `RTCDataChannel`-delivered
  Annex-B stream) for clients that have `VideoDecoder` but not the transform;
  (c) treat "fell back to `<video>`" as a first-class diagnostic, not a `console.log`.
- **Probe:** already present at `index.html:166-168` — the change is reporting it, not detecting it.
- **Expected win:** up to **~90 ms** on any client that is currently falling back, 0 on clients that
  are not. Worth knowing which one the user's iPad is before anything else in this document.
- **Measure:** the client computes `videoDelayEmaMs` from the RTP host timestamp
  (`transform-worker.js:57-90`), but **it is not an absolute latency today** — the host's RTP stamp
  rides `HOST_EPOCH`, a process-relative monotonic `Instant` (`streamer/audio/mod.rs:17-27`), while
  the client differences it against `performance.timeOrigin + performance.now()`, a Unix epoch. The
  offset cancels in the A/V-sync arithmetic and does not cancel here. Fix that first (see R6), then
  build the same estimate for the `<video>` path from `RTCRtpReceiver.getSynchronizationSources()` /
  `requestVideoFrameCallback` and report both.

Findings file: `findings/major-client-video-fallback-jitter-buffer.md`

---

### R3 — Stop coalescing frames in CGDisplayStream

`cgds.rs:70` sets `kCGDisplayStreamMinimumFrameTime = (1/fps)·0.95`. That is a **hold-off**: after a
delivery, the stream will not deliver another frame for that long, so a display update that lands
just after a delivery waits up to one interval — **a uniform 0…16.7 ms delay at 60 fps, mean ~8 ms**,
paid on every frame. AirPlay's in-WindowServer path has no such client contract. We do not need it:
`pipeline.rs` already drops to the latest frame and the encoder is the real rate limiter.

- **API/flag:** set `kCGDisplayStreamMinimumFrameTime` to `0.0` (or omit it), keep
  `kCGDisplayStreamQueueDepth = 2`, and enforce the frame rate downstream where we already do.
- **Platform:** macOS 10.8+ — the deprecated API `cgds.rs` already uses. No new symbols.
- **Probe:** none needed; guard behind a config flag so it can be A/B'd.
- **Expected win:** **~8 ms mean, ~17 ms worst case at 60 fps** (proportionally more at 30 fps:
  ~17 ms mean). Cost: more capture callbacks on a busy screen, which the existing drop-to-latest
  absorbs. Watch CPU on the 10.15 Intel box.
- **Measure:** `pipeline.rs`'s existing `capture→encoded latency ms (mean/p50/p99/max)` test harness
  (`pipeline.rs:669`) before/after, plus CPU.

Findings file: `findings/minor-cgds-minimum-frame-time-coalescing.md`. **Measure it together with
`findings/blocker-cgds-queue-depth-starvation.md`**, which reports that `queueDepth = 2` is fully
consumed by the two IOSurfaces the pipeline retains in steady state — if capture is already starved
of free surfaces, the hold-off is not the binding constraint and removing it will show nothing. Both
values live in the same property dictionary (`cgds.rs:70-73`).

---

### R4 — Keep our host AP off the AWDL social channels (6, 44, 149)

`hosted_network.rs:36` prefers `[36, 40, 44, 48]`. **44 is one of AWDL's three social channels**
(Stute et al.). Worse, the host Mac's *own* AWDL activity retunes its radio to 6/44/149 periodically
whether or not we use it — measured as 3–90 ms RTT swings by Visser at RIPE 91, and reported as
1–2 s hops every 10–12 s costing 50–200 ms stalls.

- **Do:** drop 44 from `PREFERRED_5GHZ_CHANNELS` (`36, 40, 48` first, then 149-avoiding UNII-3 like
  153/157/161 if supported); on the *fallback* path prefer 1 or 11 over 6. Log the chosen channel.
  Second half: when the host is **not** on a hosted network, log a warning if the AP channel is 6/44/149,
  because that is the case where AWDL hopping is *cheapest* (Visser's finding: the stutter disappears
  when you sit on a social channel) — i.e. the advice inverts, and only measurement will say which
  side of it we are on.
- **Platform:** both (macOS `pick_channel`; Windows `netsh` channel selection).
- **Expected win:** removes a recurring 50–200 ms stall class from the tail. **Not** a median win.
- **Measure:** ping/RTT histogram over 60 s on the client link, with an AirDrop sheet opened and
  closed mid-run to force AWDL activity.

Findings file: `findings/minor-hostap-avoid-awdl-social-channels.md`

---

### R5 — Adopt AirPlay's "the receiver never schedules, it resynchronises" as an explicit invariant

AirPlay receivers (UxPlay, RPiPlay) render on arrival and have no video jitter buffer at all; their
only recovery is that the next codec-data/IDR resets them. `transform-worker.js` already implements
this (backlog → request keyframe → skip to it, `:230-247`). The gap is that the policy is not applied
to the **audio** worklet's depth command or to the `<video>` fallback, and it is not documented as an
invariant, so it can regress.

- **Do:** document it in `CLAUDE.md`'s client section; assert it in the client's stats reporting
  (`postStats`) so a regression is visible; make the backlog threshold configurable per device via
  the existing `/reconfig` epoch mechanism rather than a constant.
- **Expected win:** 0 today; prevents a future regression worth tens of ms.
- **Cost:** trivial.

Findings file: `findings/minor-client-no-scheduling-invariant.md`

---

### R6 — Fix and surface the glass-to-glass measurement first

Every number above is an estimate. The client computes something close to a real one —
`transform-worker.js` recovers each frame's host capture time from the RTP timestamp and maintains
`videoDelayEmaMs` (`:57-90`), posting it as an `avsync` message — but **it is off by a whole epoch
and nothing surfaces it**. The host's RTP timestamp rides `HOST_EPOCH`, a process-relative monotonic
`Instant` (`streamer/audio/mod.rs:17-27`); the client subtracts it from
`performance.timeOrigin + performance.now()`, which is Unix milliseconds. The A/V-sync arithmetic
cancels the offset (the audio term carries the same one), so sync works and the absolute latency is
nonsense.

- **Do:** estimate the host↔client clock offset once (the existing `PING`/`PONG` on the reliable
  DataChannel is the cheapest carrier), subtract it in `reportAvSync`, then plumb the corrected value
  plus the audio offset back to the host, ring-buffer it in `session.rs`, and expose it through
  `ScreenExtend status --json` and the log bus. Add a `--probe-latency` mode to the developer harness
  (`streamer/cli.rs`) that prints p50/p95/p99. Cross-check once with a phone camera at 240 fps
  photographing the host and client showing the same millisecond timer — the method Astropad used.
- **Expected win:** 0 ms directly; it is the prerequisite for trusting R1–R4.
- **Cost:** low.

Findings file: `findings/major-client-no-glass-to-glass-telemetry.md` (written by the client
discovery pass; it carries the epoch analysis and the telemetry design — no separate file from this
report).

---

### R7 — Do **not** build an AirPlay-mirror relay (recorded so it is not re-proposed)

Decrypting macOS's own AirPlay H.264 and re-sending it is technically possible (playfair + AES-CTR,
GPL-3 compatible with our AGPL-3) but loses on every axis that matters: it adds a TCP hop with no
`TCP_NODELAY`, surrenders bitrate/GOP/QP/IDR control that the loss-based BWE and rejoin paths depend
on, caps at one display, lets macOS pick the geometry, needs the 1036-line Accessibility picker
driver, and targets a sender-advertised 90 ms budget. Keep `macos_utils/airplay/` as the sub-10.15
display fallback it is.

Findings file: `findings/speculative-airplay-relay-rejected.md`

---

## 6. Verification plan

**Measure before/after, on the dev boxes:**

1. **Capture→encoded latency** (macOS 10.15.8 Intel): the existing ignored test in
   `macos_utils/streamer/pipeline.rs` (`capture→encoded latency ms … mean/p50/p99/max`,
   `pipeline.rs:625-720`) with `kCGDisplayStreamMinimumFrameTime` at `(1/fps)·0.95` vs `0.0`.
   Expect the mean to drop by ~half a frame interval. Also record CPU.
2. **Encoder dwell**: log `CompressedSample` emit time minus submit time, per frame, on 10.15 —
   establishes how much of the budget VideoToolbox owns without `EnableLowLatencyRateControl`
   (11.0+, unavailable there).
3. **Client path selection**: from the R2 reporting — record which of fast/fallback the actual iPad
   lands on. **Do this first**; it may explain the entire observation.
4. **End-to-end**: R6's `videoDelayEmaMs`, p50/p95/p99 over 5 minutes, in three configurations —
   (a) both on the home AP, (b) client associated to the ScreenExtend host AP, (c) host AP on a
   non-social channel — with an AirDrop sheet opened mid-run in each.
5. **Camera cross-check**: 240 fps phone video of both screens showing the same timer, 20 samples,
   to validate that `videoDelayEmaMs` is not systematically optimistic.

**Cannot be verified on the dev boxes:**

- Anything about **AWDL as a transport** — no third-party API reaches it, and no browser can.
  Permanently unverifiable, and that is the answer, not a gap.
- **Sidecar's actual latency** — needs a Sidecar-capable Mac and iPad plus a high-speed camera; the
  10.15.8 Intel box can run Sidecar but there is no instrumented rig.
- **ScreenCaptureKit** capture latency (12.3+) — the 10.15 box cannot run it; the SCK path's
  `queueDepth = 3` / `minimumFrameInterval` behaviour is untested here.
- **The macOS host AP path end-to-end** — `startHostAPModeWithSSID:` is private and its behaviour on
  10.15.8 with a modern iPad is unproven in this tree. This is the largest verification risk in R1.
- **Windows**: the RTX 3080 Ti box can measure R1/R4/R6 on the `netsh` hosted network, and R3's
  analogue does not apply (DXGI/WGC have no `minimumFrameTime`).

## 7. Open questions

1. **Which is the user actually running — Sidecar or a third-party AirPlay receiver app on the
   iPad?** The recommendation weighting changes materially. Ask; it is one question.
2. **Which client path does the iPad take** (WebCodecs fast path vs `<video>`)? R2 answers it and it
   may be worth ~90 ms on its own.
3. **Does WebKit honour the sender's `playout-delay(0,0)` extension** on the `<video>` path? I found
   no source either way. Empirical, cheap to test.
4. **Does `startHostAPModeWithSSID:` still work on 10.15.8**, and does an iPad associate to it and
   route to the Rust HTTP server? Untested in this tree.
5. **What is VideoToolbox's actual frame dwell on a 2015-era Intel Mac at 1080p60** without the
   11.0+ low-latency rate control? If it is large, R3's 8 ms is noise and the real fix is resolution
   or frame rate, not scheduling.
6. **No published packet-capture study of AirPlay's `kVTCompressionPropertyKey_*` values exists.** If
   one is needed, the way to get it is to run our own fake receiver (`macos_utils/airplay/`) against
   a real macOS sender, decrypt with playfair, and parse the SPS/PPS and slice headers — a
   research-only use of code we already have, and the only route to a first-party answer.
