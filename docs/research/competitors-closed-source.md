# Closed-source competitors: how they get low latency

## 1. Question

What are the closed-source products in the "stream a display to a screen" category actually doing
to achieve low latency, as far as it can be established from engineering blogs, patents, support
docs, packet-capture write-ups, conference talks, and reverse engineering? Products in scope:
spacedesk, Duet Display, Luna Display, Parsec, Steam Link / Steam Remote Play, Rainway, NVIDIA
GameStream, Apple Sidecar, Microsoft Miracast / Wireless Display, Deskreen, Splashtop, Jump
Desktop (Fluid), Stadia / GeForce NOW / Xbox Cloud Gaming, Amazon Luna, plus VR streamers
(Virtual Desktop, ALVR, Meta Air Link) which have the strictest latency budgets in the field.

For each: transport, codec, packetization, latency claims vs. independently measured reality,
loss handling, input handling, virtual display method, published latency breakdowns. Then: which
techniques appear in **two or more independent products** (strong evidence they are both possible
and worth doing), and what ScreenExtend would have to change.

## 2. Findings

### The headline numbers, collected

| Product | Claimed | Independently measured | Source |
|---|---|---|---|
| Parsec | "4–8 ms total pipeline on LAN" at 240 fps; "7 ms added on LAN ethernet" | not independently reproduced | [Parsec 240 fps post](https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5), [Parsec technology page](https://parsec.app/technology) |
| Luna Display (LIQUID 3.0) | 1–4 ms wired, 16–32 ms Wi-Fi | vendor's own bench: **11.3 ms mean USB, 22.4 ms mean Wi-Fi** (50 samples, AnyDesk latency tool) | [Astropad latency comparison](https://astropad.com/blog/comparing-latency-luna-display/), [LIQUID tech page](https://astropad.com/technology/) |
| Duet Display | "zero-latency" over USB | same vendor bench (Astropad, so treat as adversarial): **95 ms USB / 108 ms Wi-Fi at 60 fps**, and "slightly less than 60 fps on USB, significantly less on Wi-Fi" | [Astropad latency comparison](https://astropad.com/blog/comparing-latency-luna-display/) |
| Air Display | — | 52.9 ms USB / 131.4 ms Wi-Fi (same bench) | ditto |
| Moonlight / GameStream | — | 20–35 ms end-to-end on a good LAN | [Moonlight](https://moonlight-stream.org/) |
| Steam Link | — | user reports of <1 ms input latency, **13–16 ms display latency** with <1 ms ping | [Steam Link discussions](https://steamcommunity.com/app/353380/discussions/0/483367798505548057/) |
| Stadia | "imperceptible" | RTT 10–15 ms typical, **jitter buffer 45.35 ms** at 1080p (2–3 frames) — the jitter buffer is the single biggest client-side term | [Carrascosa & Bellalta, *Cloud-gaming: Analysis of Google Stadia traffic*](https://ar5iv.labs.arxiv.org/html/2009.09786) |
| Apple Sidecar | — | wired USB-C reported "consistently below 30 ms" | [Sidecar explainer](https://www.imore.com/ipad-new-mac-display-sidecar-explained) |
| Miracast / Wi-Fi Display | spec ceiling | **spec allows up to 250 ms** end-to-end at max res/fps; optimized implementations claim <80–100 ms | [Wi-Fi CERTIFIED Miracast Technical Overview](https://tools.barco.com/kb-downloads/4814/Wi-Fi_CERTIFIED_Miracast_Technical_Overview_20170725.pdf) |
| Meta Air Link | 25–50 ms motion-to-photon | one review measured 59.1 ms | [mixed-news Air Link latency review](https://mixed-news.com/en/meta-quest-2-pc-vr-streaming-with-air-link-latency-review/) |
| Virtual Desktop (VDXR) | 35–45 ms motion-to-photon, "comparable to wired Link" | — | [VD guide](https://ovrdoz.com/en/virtual-desktop/) |
| Generic WebRTC remote control (Hopp) | — | **347 ms → 159 ms (poor net), 159 ms → 98 ms (good net, 1080p)** after tuning libwebrtc; processing delay 232→51 ms, jitter buffer 189→46 ms | [gethopp latency exploration](https://www.gethopp.app/blog/latency-exploration) |

The Hopp numbers are the most useful data point in the table for ScreenExtend, because that is
*exactly* our architecture (browser WebRTC + remote control) with default settings, and it shows
where the milliseconds live: **encode/decode processing and the receiver jitter buffer, not the
network.**

### Per-product teardown

#### spacedesk

- **Virtual display:** a real Windows Indirect Display Driver. The release notes record
  "spacedesk display driver support of IddCx v1.4" at v2.1.48
  ([release notes](https://manual.spacedesk.net/ReleaseNotes.html)). Same class of mechanism as
  ScreenExtend's own signed IDD.
- **Transport:** LAN TCP/IP, TCP and UDP both on port 28252
  ([firewall docs](https://manual.spacedesk.net/FirewallSettingsonPrimaryMachine.html)). No
  WebRTC, no ICE, no NAT traversal — it is explicitly a same-LAN product.
- **"Encoding" options:** the Viewer exposes **Software Encoding / Hardware Encoding /
  Automatic** under Settings → Quality/Performance
  ([Encoding Type](https://manual.spacedesk.net/EncodingType.html)). The software path is
  **MJPEG** — the documented Android workaround is literally "Software Encoding MJPEG (forced)"
  ([support thread](https://www.spacedesk.net/support-forum/topic/hardware-decorder-h264-tablet-android/)).
  Hardware encoding is H.264 ("Refined H264 encoder and frame updates parameters", v2.2.22–2.2.24).
  Hardware encoding arrived only in v2.2.02; before that everything was MJPEG.
- **Why this matters:** MJPEG is intra-only — every frame is a keyframe. That is a huge bitrate
  cost but it means **there is no loss-propagation problem and no keyframe spike at all**: a lost
  packet damages one frame and the next frame is clean. spacedesk bought loss resilience by
  throwing away inter-frame compression. It also gives them a "frame updates parameters" damage
  path (they only push changed regions).
- **Latency:** no published number. The support answer for lag is "use direct USB connection"
  (USB tethering → RNDIS → an ordinary IP link) "and reduce Quality/Performance settings".

#### Duet Display

- **USB path:** proprietary display protocol over the Lightning/USB-C link, marketed as
  "zero-latency" ([features page](https://duetdisplay.co.com/features/)). Measured by a competitor
  at 95 ms.
- **Wi-Fi path ("Duet Air"):** "adaptive compression to maintain low latency". In Feb 2022 they
  "redesigned the network protocol from the ground up" and gained "hardware acceleration like
  never before" ([9to5Mac](https://9to5mac.com/2022/02/18/duet-display-for-mac-pc-update/)) — i.e.
  they were on a software codec until 2022.
- Astropad's characterisation, which is uncontested in public: Duet "uses traditional video
  playback technology that was designed for pre-recorded films"
  ([Luna vs Duet](https://astropad.com/luna-display-versus-duet-display/)) — i.e. a player-style
  buffer, which is precisely the 100 ms they measure.

#### Luna Display (Astropad LIQUID) — the closest analogue to ScreenExtend

- **Hardware dongle:** a USB-C / Mini DisplayPort device that *emulates a real display*, so macOS
  hands it a genuine hardware-accelerated framebuffer with Metal GPU support, rather than a
  software virtual display ([Luna vs Duet](https://astropad.com/luna-display-versus-duet-display/)).
  This is the single structural difference from Duet, and Astropad attributes the GPU-acceleration
  benefit to it.
- **Transport:** LIQUID is **UDP**, explicitly chosen so that "when packets are dropped or delayed,
  LIQUID can react more quickly by sending more data through the network"
  ([LIQUID tech page](https://astropad.com/technology/)) — i.e. loss-triggered *repair by
  retransmission/redundancy*, not by stalling.
- **Congestion control: "Velocity Control" analyses network conditions "dozens of times per
  second"** to trade resolution against latency. That is a 20–60 Hz control loop.
- **LIQUID 3.0 (Jan 2019):** "4x better GPU performance, 2x more responsive"
  ([TechCrunch](https://techcrunch.com/2019/01/24/luna-display-updates-its-video-engine-for-faster-performances/)),
  and "Adaptive Image Rendering — an advanced algorithm that balances fast and high-quality video
  compression" (i.e. per-region quality selection, tile/damage driven, not a single global QP).

#### Parsec (BUD)

Parsec is the most explicit closed-source vendor about its design
([BUD post](https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007)):

- **Transport:** UDP, encrypted with **DTLS 1.2 (OpenSSL)**, with "basic reliability semantics like
  TCP, but with a highly tuned and custom congestion control algorithm". 97% NAT traversal.
- **Buffering: "Parsec has no buffers of any kind on video."** They therefore "have to detect a
  congestion event before it starts by reading networking metrics and making split micro-second
  decisions", with the goal to "saturate the connection right at the edge without introducing loss
  or latency". This is a *delay/derivative-based* controller, not a loss-reactive one.
- **Loss handling:** "the best move with a video stream is to tweak the encoder's bitrate (H264 is
  a lossy codec)" — they degrade quality rather than retransmit or stall. Priority order is stated
  outright: **"latency, frame rates, and then video quality, in that order."**
- **Design target:** <50 ms ping, 30 Mb/s up/down. 60 s tolerance before a session is dropped.
- **Client decode/render:** hardware decode by default ("a chip made specifically for streaming
  video for a lower latency stream"), a selectable renderer (DirectX recommended on Windows), and
  **VSync off by default** — "VSync increases latency, but eliminates screen tearing"
  ([VSync article](https://support.parsec.app/hc/en-us/articles/32361386984212-VSync-Setting-In-Parsec-Window),
  [software decoding article](https://support.parsec.app/hc/en-us/articles/32381511532820-Using-software-decoding-instead-of-hardware-decoding)).
- **Published per-stage stats:** the overlay reports **encode latency (ms on host) and decode
  latency (ms on client)** separately, plus network
  ([overlay stats](https://support.parsec.app/hc/en-us/articles/32381603663636-Stream-Overlay-Stats-and-Logging)).
  `encoder_bitrate` defaults to **10** (Mbps) and their own docs say **"higher bitrates increase
  latency"** — an explicit acknowledgement that serialization delay dominates on the last hop.
- **Virtual display:** an IddCx Indirect Display Driver ("Parsec VDD"), up to 3 extra displays, up
  to 4K@240 ([VDD docs](https://support.parsec.app/hc/en-us/articles/32381178803604-VDD-Overview-Prerequisites-and-Installation),
  [nomi-san/parsec-vdd](https://github.com/nomi-san/parsec-vdd)). Same mechanism as ScreenExtend.
- **Latency claim:** 4–8 ms whole-pipeline on a 1 Gbps LAN at 1080p240 with a 100 Mbps cap, GTX
  1070 host → GTX 1050 Ti client, "2 frames behind the server". At 240 fps, 2 frames = 8.3 ms, so
  the claim is internally consistent and is essentially *"two frames of pipeline"* — which is the
  honest way to read every claim in this table.

#### Steam Link / Steam Remote Play

- **Transport:** UDP; host encodes, packets traverse the router, client hardware-decodes.
- **Published stats:** the client's advanced streaming display separates **input latency** from
  **display latency** and shows incoming bitrate. Valve's own framing is that the reported number
  excludes the TV's own panel latency.
- **Settings that matter:** bandwidth cap (or auto), resolution, hardware decoding on/off, HEVC.
  Community consensus target is "<20 ms added network latency and near-zero loss".
- **Notable:** Valve exposes a bandwidth *cap* as the primary latency knob, again because
  serialization + queueing on the last hop is the dominant network term.

#### Rainway

- Browser client (Chrome/Firefox HTML5) plus native, 60 fps, "super low-latency".
- The one durable engineering claim in public: they **mint a unique valid certificate per user**
  specifically so that they "avoid needing a TURN server and maintain low latency"
  ([alax.info write-up](https://alax.info/blog/1824)) — i.e. they treat relaying as a latency
  *bug*, not a connectivity feature. Their HN thread ("Building a Cross-Browser, Cross-Platform,
  Real-Time Game Streaming Protocol", [HN 18242300](https://news.ycombinator.com/item?id=18242300))
  is the primary source but is currently rate-limiting fetches.
- Relevance to ScreenExtend: same shape (browser client, no install), and the same conclusion —
  a relayed path is a latency failure, and a same-LAN pair must never go through one.

#### NVIDIA GameStream (reverse-engineered by Moonlight)

The most completely documented protocol in the category, because
[moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c) *is* the
reverse-engineering.

- **Transport:** RTSP for negotiation (`RtspConnection.c`), **ENet** for the reliable control
  channel (with a forked ENet for IPv6 + retransmission fixes), raw UDP for video/audio.
- **FEC:** Reed–Solomon forward error correction with a **dynamically adjusted FEC percentage**.
  Sunshine's default is **`fec_percentage = 20`** — "percentage of error correcting packets per
  data packet in each video frame"
  ([Sunshine configuration](https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2configuration.html)).
  This is the field's answer to Wi-Fi loss: **repair in-band, never wait an RTT.**
- **Reference Frame Invalidation (RFI):** the client tells the host *exactly which frames it lost*
  and the host encodes the next frame without referencing them — "recovering from a loss doesn't
  require re-encoding the entire screen, just the portion that was lost"
  ([moonlight-common-c #120](https://github.com/moonlight-stream/moonlight-common-c/issues/120)).
  NVENC, Intel VPL and AMD AMF all support the underlying **long-term reference (LTR)** frames.
  The proposed successor protocol in that issue: treat the RFI request as a NACK, encode a
  non-recovery LTR P-frame referencing the last good LTR frame, and **never send an IDR again**
  after the first one.
- **Encoder tuning:** NVENC P1 preset + **Ultra Low Latency** tune, CBR, B-frames off, one-frame
  VBV. NVIDIA's own tuning is described as "optimized for very fast encode times to ensure low
  latency".
- **Client frame pacing:** optional and *documented as a latency cost* — "frame pacing drops frames
  when they arrive too early or too late"; the render path "must wait for V-Sync", so the average
  render latency "includes a V-Sync period (16 ms for a 60 Hz display)". Moonlight surfaces **frame
  queue delay** as a first-class stat and says it "should typically stay under one frame interval".
- **Virtual display:** Sunshine forks use IddCx drivers (SudoVDA in
  [Apollo](https://github.com/ClassicOldSong/Apollo), VirtualDisplayDriver elsewhere). Apollo also
  ships **"Intra Refresh for Xbox"** as a named feature.

#### Apple Sidecar

- **Virtual display:** macOS creates a genuine second display and "sends a second, virtual display
  to an iPad using an encrypted encoded stream" — the same private `CGVirtualDisplay` family that
  ScreenExtend's macOS backend already drives.
- **Codec:** HEVC, using the dedicated hardware encode/decode blocks (T2 on the Mac side, the Apple
  SoC media engine on iPad). It degrades gracefully — Sidecar works on iPad Air 2, which has no
  hardware HEVC decode.
- **Transport:** AWDL (Apple Wireless Direct Link) peer-to-peer for the wireless path, or the
  USB-C/Lightning cable. AWDL is a *shared-radio* protocol: it periodically pulls the Wi-Fi radio
  onto a social channel (ch. 6 / ch. 149), roughly every 10–12 s for 1–2 s, producing **50–200 ms
  latency spikes on the host's own Wi-Fi**
  ([Network Weather](https://www.networkweather.com/learn/awdl/),
  [gyorgy.sh](https://gyorgy.sh/blog/macos-awdl-network-jitter)). Even Apple's first-party solution
  eats a periodic radio-scheduling hit; this is the same class of problem as AP client isolation,
  which `webrtc_session.rs:248-254` already warns about.
- **Wired is the low-latency tier:** <30 ms wired versus a much wider wireless distribution.

#### Microsoft Miracast / Wireless Display

- **Transport:** Wi-Fi Direct (or TDLS) for the link; **H.264 in MPEG2-TS, encapsulated in
  RTP/UDP/IP**. Negotiation and session control over RTSP.
- **Latency:** the certification spec's ceiling is **250 ms** at max resolution and frame rate.
  MPEG2-TS was, as the patent literature bluntly puts it, "designed for broadcast or offline
  applications where latency rarely poses concern"
  ([US 9,819,604, "Real time network adaptive low latency transport stream muxing … for
  miracast"](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9819604)).
- **Takeaway:** the transport container is the latency. This is the negative example — everything
  else in this table deliberately avoids a mux/container and packetizes the codec directly into
  RTP or a custom UDP frame, which is what ScreenExtend already does.

#### Deskreen

- Electron + **WebRTC** to a browser, "connect as many devices as you want at the same time",
  adds a virtual screen ([pavlobu/deskreen](https://github.com/pavlobu/deskreen)). Architecturally
  the closest open comparator to ScreenExtend, and it publishes **no latency numbers at all** —
  it uses stock browser WebRTC with stock jitter buffering, which per the Hopp measurements puts
  it at ~150–350 ms.

#### Splashtop

- H.264 with GPU offload on both ends; hardware acceleration on by default from v3520
  ([Splashtop docs](https://support-splashtopbusiness.splashtop.com/hc/en-us/articles/360039820771-Enable-hardware-acceleration-on-Splashtop-Streamer)).
  Exposes frame-rate, quality and GPU-usage knobs. No published protocol detail, no latency
  breakdown. Positioned on breadth, not latency.

#### Jump Desktop (Fluid)

- **Codec:** Fluid 2.0 moved to **H.265/HEVC**, claiming ~50% bandwidth saving versus H.264 at
  equal quality, with hardware encode *and* decode on both ends
  ([Fluid 2.0 changelog](https://changelog.jumpdesktop.com/jump-desktop-with-fluid-2.0-1yMoBG)).
  Claims 60 fps at 1/10th the bandwidth of RDP/VNC.
- **Transport:** UDP with **DTLS**; the 2.0 networking stack adds "better bandwidth estimation,
  **forward error correction**, and faster recovery from transient network issues".
- **Adaptive Quality Control** monitors the connection and adjusts quality.
- **Live connection statistics** are a documented, first-class feature.

#### Stadia / GeForce NOW / Xbox Cloud / Amazon Luna

Stadia is the only one of the four with real published engineering detail, and it is the most
directly transferable because **Stadia was WebRTC in a browser**, exactly like ScreenExtend's
client:

- **Congestion control:** a customised **BBR** — chosen because it "attempts to reduce congestion
  and packet loss *before it happens*", explicitly to defeat last-mile bufferbloat
  ([PCGamesN interview with Google engineers](https://www.pcgamesn.com/stadia/google-game-streaming-technology-latency-quality)).
- **Receiver:** "**WebRTC extensions provided by our team in Sweden to disable buffering and
  display things as soon as they arrive**" — i.e. Google shipped custom Chromium code to defeat
  the jitter buffer, because the stock one cost them ~45 ms (see the measured number below).
- **Rate control:** "the controller is constantly adjusting a target bitrate based on **per-packet
  feedback signals** that arrived from the receiver device", with filters and models synthesising a
  bandwidth-availability estimate at "millisecond, and sometimes microsecond" resolution.
- **Measured** ([Carrascosa & Bellalta](https://ar5iv.labs.arxiv.org/html/2009.09786)):
  - A single UDP flow carrying multiple RTP SSRCs; VP9 or H.264.
  - **RTP packets arrive in batches separated by ~16.67 ms** (the frame interval), and *within*
    each batch they are **grouped in ~2 ms sub-bursts** of 7–9 packets. Stadia **paces** the
    packets of a frame; it does not blast them.
  - Payload ~1,116 B mean, 68.79% of packets exactly 1,194 B.
  - 1080p60: 23.6 Mbps (H.264) / 27.6 Mbps (VP9) — **H.264 produced *less* traffic than VP9** in
    their tests.
  - **Jitter buffer 45.35 ms** at 1080p — 2–3 frames, and the single largest client-side term.
  - RTT 10–15 ms typical, <25 ms across all tests, >35 ms under congestion.
  - RTCP sent **every second, unless the bitrate estimate changes by more than 3%**, in which case
    immediately.
  - Under a bandwidth cap it holds 1080p60 as long as possible, drops **frame rate to 5–10 fps**
    before it drops resolution, and only falls to 720p as a last resort. Recovery transient can
    exceed 200 s.
  - Audio: Opus, ~120 kbps, ~360 B packets every 20 ms.
- **"Negative latency"** was a suite, not one thing: run the game at a very high frame rate so the
  most recent input is sampled as late as possible, plus speculative rendering of likely inputs
  ([VentureBeat](https://venturebeat.com/business/how-google-stadias-negative-latency-might-work/)).
  The high-frame-rate half is the part that generalises: **sample later, encode sooner.**
- **GeForce NOW:** AV1 on RTX 40/50-class servers plus NVIDIA Reflex *inside the game engine*
  (which "optimizes the GPU render queue … preventing a backlog of frames, so frames are rendered
  just in time"). The streaming layer itself is undocumented.
- **Xbox Cloud / Amazon Luna:** no primary engineering material found beyond marketing. AWS's own
  Luna write-up is architectural, not protocol-level
  ([AWS Builder Center](https://builder.aws.com/content/3Fu6PnBaMRXq61mJ2PMTYr7hQLc/inside-amazon-luna-cloud-gaming-at-scale)).

#### Sony's cloud-gaming patents — the sub-frame technique, spelled out

This is the strongest primary source for sub-frame streaming, because the claims are explicit
([US 11,446,572 "Early scan-out of server display buffer at flip-time for cloud gaming
applications"](https://patents.google.com/patent/US11446572B2/en); see also 11,395,963, 12,370,442,
12,491,433, and 11,524,230 "Encoder tuning to improve tradeoffs between latency and video quality
in cloud gaming applications"):

- Begin scan-out **at flip-time rather than at the next VSYNC**, scanning the frame
  scanline-by-scanline into the encoder.
- The encoder "may begin processing as soon as minimal data from the video frame is delivered
  (e.g. **16 or 64 scanlines**)".
- Transmit on an **"encoded slice-by-slice basis"**, and slice boundaries "may not necessarily
  occur at the edges of the display screen".
- The client "decodes … again on an encoded slice-by-slice basis", and "**the display process
  begins before the decode process has fully completed** for a corresponding video frame".

The generic ceiling for this is well known: with N slices, "it's possible to reduce processing
latency from one frame to one-frame divided by the number of slices"
([AMD VCU latency modes](https://docs.amd.com/r/en-US/pg252-vcu/VCU-Latency-Modes)).

#### VR streaming — the strictest budget in the field

- **ALVR** supports both IPPP GOP and **intra-refresh (IR) HEVC coding**, where "IR coding
  disperses intra-coded macroblocks across every frame, **flattening frame-size distributions and
  yielding lower latency variability**", and is documented as needing **sliced encoding** to reach
  ideal latencies ([EmergentMind ALVR summary](https://www.emergentmind.com/topics/air-light-vr-alvr)).
- **Meta patents** (e.g. US 10,432,988, 11,070,829, 11,831,888) describe splitting a frame's
  encode into a left-eye step and a right-eye step so that "**once the left-eye portion has been
  encoded, [it] is sent over the network in parallel with the right-eye portion being encoded**" —
  sub-frame pipelining again, independently invented.
- **Meta Air Link:** NVENC H.264/H.265 at 200–300 Mbps with dynamic bitrate; "dynamic bitrate helps
  ensure that the only time a network spike causes frame drops is when it is either unprecedented …
  or is not mitigated by reducing video frame size"
  ([AFI blog](https://developers.meta.com/horizon/blog/air-link-framerate-insurance-afi/)). AFI
  itself is on-headset reprojection — a *concealment* technique, applicable to VR only.
- **Virtual Desktop:** VDXR, a proprietary OpenXR runtime that "bypasses SteamVR to reduce
  latency"; H.264 explicitly described as "lowest latency, lowest efficiency" versus HEVC/AV1.
- The VR consensus, in one line: **H.264 when you want minimum latency, HEVC/AV1 when the link is
  the bottleneck.**

### The browser-WebRTC control surface (relevant because our client is a browser)

- `playout-delay` RTP header extension: the *sender* bounds the receiver's smoothing delay.
  "Confirmed to work properly in Chrome and Safari… Firefox removes this attribute in the answer"
  ([WebRTC playout-delay doc](https://webrtc.googlesource.com/src/+/main/docs/native-code/rtp-hdrext/playout-delay/README.md),
  [Mozilla bug 1585009](https://bugzilla.mozilla.org/show_bug.cgi?id=1585009)).
- `RTCRtpReceiver.jitterBufferTarget` and `playoutDelayHint` are the receiver-side equivalents;
  the W3C discussion notes the standing gap: "there is control over minimum jitter buffer delay
  but nothing to limit maximum delay"
  ([public-webrtc](https://lists.w3.org/Archives/Public/public-webrtc-logs/2024Feb/0225.html)).
- `VideoDecoderConfig.optimizeForLatency` exists precisely because "some software decoders are
  optimized for throughput rather than latency via frame threading"
  ([w3c/webcodecs #206](https://github.com/w3c/webcodecs/issues/206)).
- **Hard constraint discovered:** WebCodecs `EncodedVideoChunk` takes a *complete access unit*, and
  the WebRTC receiver reassembles a whole frame before it reaches an insertable-streams transform.
  **Sub-frame / slice-by-slice delivery (the Sony and Meta technique) is therefore unreachable
  through the WebRTC media track.** It is only reachable by moving video onto a DataChannel and
  hand-rolling the decode feed — and even then WebCodecs will not decode a partial AU. This is the
  one major technique in the field that ScreenExtend structurally cannot adopt on a browser client.

## 3. How others do it — technique frequency table

Ranked by how many *independent* products do it (the brief's bar: two or more).

| # | Technique | Products doing it | Independent? |
|---|---|---|---|
| 1 | **Delay/derivative-based congestion control, not loss-reactive** | Stadia (BBR + per-packet feedback), Parsec (BUD, "detect congestion before it starts"), Luna (Velocity Control, dozens of Hz), Jump Fluid ("better bandwidth estimation"), WebRTC GCC | 5 |
| 2 | **No receiver jitter buffer / minimum playout delay** | Stadia (custom Chromium extensions to "disable buffering"), Parsec ("no buffers of any kind on video"), Moonlight (frame pacing optional, documented as a latency cost), Steam Link | 4 |
| 3 | **Hardware decode on the client, present immediately, VSync off** | Parsec (VSync default off, HW decode default), Moonlight, Steam Link, Sidecar, Jump Fluid | 5 |
| 4 | **In-band FEC instead of waiting for a retransmit** | GameStream/Sunshine (Reed–Solomon, 20% default), Jump Fluid, ALVR/VR stacks, Miracast (TS-level FEC) | 4 |
| 5 | **Avoid full IDR on loss** — RFI / LTR / intra-refresh | GameStream+Moonlight (RFI, LTR), Apollo ("Intra Refresh for Xbox"), ALVR (IR HEVC "flattening frame-size distributions"), Intel VPL `MFX_REFRESH_SLICE`, spacedesk's MJPEG (the brute-force version of the same idea) | 4–5 |
| 6 | **Pace a frame's packets rather than bursting them** | Stadia (measured: ~2 ms sub-bursts of 7–9 packets within each 16.67 ms frame), WebRTC's own pacer, Parsec ("saturate right at the edge without introducing loss") | 3 |
| 7 | **Degrade frame rate / quality before you degrade latency** | Parsec ("latency, frame rates, then video quality, in that order"), Stadia (drops to 5–10 fps before it drops to 720p), Air Link ("reducing video frame size"), Luna (Velocity Control), Duet Air ("adaptive compression") | 5 |
| 8 | **Ultra-low-latency encoder preset: no B-frames, no lookahead, 1-frame VBV, 1 ref frame** | GameStream/Sunshine (NVENC P1 + ULL, CBR, B off), Air Link, Parsec, generic remote-desktop practice ("1080p60 ultra-low-latency presets with B-frames disabled and single-frame VBV buffering") | 4 |
| 9 | **A wired/USB tier that is a first-class product feature** | Sidecar (USB-C), Luna (USB, 11.3 ms vs 22.4 ms), Duet (USB primary), spacedesk (support's first answer to lag), Steam Link (Ethernet) | 5 |
| 10 | **Per-stage latency telemetry exposed to the user** | Parsec (encode ms / decode ms / network), Moonlight (host processing, network, decode, render, **frame queue delay**), Steam Link (input vs display latency), Jump Fluid (live connection statistics) | 4 |
| 11 | **HEVC/AV1 as an optional codec for constrained links** | Jump Fluid 2.0 (HEVC, ~50%), GFN (AV1), Virtual Desktop (HEVC/AV1), Sidecar (HEVC), Moonlight, Steam Link | 6 |
| 12 | **Sub-frame / slice-by-slice encode-and-send** | Sony patents (16–64 scanlines), Meta patents (per-eye pipelining), ALVR (sliced encoding), AMD VCU low-latency slice mode | 4 — **but unreachable through a browser WebRTC track** |
| 13 | **IddCx Indirect Display Driver for the virtual monitor** | spacedesk, Parsec VDD, Sunshine forks (SudoVDA), ScreenExtend | 4 — already done |
| 14 | **Never relay a same-LAN pair** | Rainway (per-user certs specifically to avoid TURN), Parsec (97% NAT traversal to avoid relaying) | 2 — already warned about at `webrtc_session.rs:248` |

## 4. What ScreenExtend does today

Read: `src-tauri/src/streamer/webrtc_session.rs`, `src-tauri/src/streamer/server.rs`,
`src-tauri/src/streamer/bitrate.rs`, `src-tauri/src/streamer/config.rs`,
`src-tauri/src/windows_utils/streamer/nvidia/encoder.rs`,
`src-tauri/src/macos_utils/streamer/encoder.rs`,
`src-tauri/src/streamer/static/transform-worker.js`, `src-tauri/src/streamer/static/index.html`.

**Already matching the field — do not regress these:**

- **Receiver buffering is already defeated** (technique #2). The client sets
  `videoTransceiver.receiver.playoutDelayHint = 0` (`static/index.html:832`) and
  `jitterBufferTarget = 0` (`static/index.html:874`), and the host stamps the
  `playout-delay` header extension with `PlayoutDelayExtension::new(0, 0)` on every RTP packet
  (`webrtc_session.rs:505-507`). The extension is registered in the media engine at
  `webrtc_session.rs:165-173`. That is the same move Google shipped custom Chromium for.
- **Client decode is hardware-accelerated and presented immediately** (technique #3).
  `decoder.configure({ codec, optimizeForLatency: true })`
  (`static/transform-worker.js:157`); the decoder's `output` callback draws straight to a
  `desynchronized`, `alpha: false` OffscreenCanvas 2D context
  (`transform-worker.js:41`, `transform-worker.js:110-124`) with **no `requestAnimationFrame`
  pacing and no vsync wait**. There is no client-side frame queue.
- **Encoder presets are already ULL** (technique #8). NVENC:
  `NV_ENC_PRESET_P1_GUID` + `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY`,
  `gopLength = NVENC_INFINITE_GOPLENGTH`, `frameIntervalP = 1`,
  `vbvBufferSize = bitrate/fps` (one-frame VBV), `vbvInitialDelay` equal to it,
  `enableLookahead(0)`, `multiPass = DISABLED`, `zeroReorderDelay(1)`,
  `maxNumRefFrames = 1`, `repeatSPSPPS(1)`, filler data off
  (`windows_utils/streamer/nvidia/encoder.rs:543-611`). macOS VideoToolbox:
  `RealTime = true`, `AllowFrameReordering = false`, `MaxFrameDelayCount = 1`,
  `PrioritizeEncodingSpeedOverQuality = true`, and `EnableLowLatencyRateControl` is already
  probed as an optional encoder-specification key (`macos_utils/streamer/encoder.rs:151-233, 505`).
  This part of the stack is genuinely at parity with Sunshine/Parsec.
- **Direct codec-into-RTP packetization, no container** — we hand-packetize with `H264Payloader`
  at a 1188-byte payload MTU and stamp the shared host clock into the RTP timestamp
  (`webrtc_session.rs:502, 512-513`), which is the opposite of Miracast's MPEG2-TS mistake.
- **IddCx virtual display** (technique #13) — already the shipping mechanism.
- **Relay is treated as a defect** (technique #14) — `detect_session_locality` warns explicitly
  when a same-subnet pair routes via relay and names AP client isolation
  (`webrtc_session.rs:243-256`).
- **Separate input channels** — `fast` / `reliable` / `bulk` DataChannels
  (`webrtc_session.rs:402-404`), mirroring Moonlight's ENet-reliable + unreliable split.

**Gaps against the field:**

| Technique | Status here | Where |
|---|---|---|
| #1 delay-based CC | **Missing.** BWE is loss-only (`estimate_from_loss`: cut only above 10% loss, probe +8% below 2%) plus one crude `RTT > 80 ms → ×0.85` clamp | `bitrate.rs:103-121`, `webrtc_session.rs:863-960` |
| #1 feedback rate | 120 ms poll of `get_stats()`, 500 ms minimum interval between raises, ±10%/4% deadbands | `webrtc_session.rs:40`, `bitrate.rs:20, 87-91` |
| #4 FEC | **None.** No ULPFEC/RED, no application FEC. Loss handling is NACK/RTX (registered at `webrtc_session.rs:95-98` + the `video/rtx` codecs at `webrtc_session.rs:127-146`) and then PLI → full IDR | `webrtc_session.rs:477-484` |
| #5 avoid IDR on loss | **Inverted.** `pipeline.request_idr()` fires on every PLI, every FIR, on `RecvError::Lagged`, and on payloader failure. Intra-refresh is implemented for NVENC (`nvidia/encoder.rs:594-598`) and Intel (`intel/encoder.rs:448-459`) but `Config::intra_refresh` defaults to `false` (`config.rs:167`) and `server.rs` **never sets it** — only the dev probe harness's `--intra-refresh` can turn it on | see finding 1 |
| #6 pacing | **Missing.** Every RTP packet of a frame is written back-to-back in a tight loop with no inter-packet delay | `webrtc_session.rs:527-546` |
| #7 degrade fps first | Partial — bitrate adapts, frame rate does not | `bitrate.rs` |
| #9 wired tier | Undocumented. USB tethering works incidentally (it presents an ordinary IP) but nothing detects it, reports it, or tells the user | — |
| #10 telemetry | Only an A/V-sync EMA posted to the page (`transform-worker.js:57-93`) and `SEAudio.getSyncInfo()`. No encode-ms, no decode-ms, no network-ms, no frame-queue delay. Already filed as `findings/major-client-no-glass-to-glass-telemetry.md` | — |
| #11 HEVC/AV1 | H.264 only, three profiles (`webrtc_session.rs:74-81`) | — |
| #12 sub-frame | `h264.sliceMode = 0; sliceModeData = 0` (`nvidia/encoder.rs:599-600`). **Structurally unreachable** through a browser WebRTC track — see §2 | — |

Three of these overlap findings already in the queue and are *not* re-filed here:
`major-webrtc-transport-cc-advertised-but-never-stamped.md`,
`blocker-webrtc-rtt-unit-mismatch-collapses-bitrate.md` (the RTT clamp reads ms as s),
`major-webrtc-lagged-viewer-requests-idr-and-makes-it-worse.md`, and
`major-client-no-glass-to-glass-telemetry.md`.

## 5. Recommendations

Ranked by expected latency delta ÷ implementation cost.

### R1 — Turn intra-refresh on in the shipped path (finding: `speculative-nvenc-intra-refresh-is-unreachable-from-any-shipping-path.md`, extended by this research)

- **What:** set `Config::intra_refresh = true` by default for the NVENC and Intel backends, keep
  IDR only for (a) the first frame of a session and (b) an explicit PLI after a decode error.
- **API/flag:** NVENC `NV_ENC_CONFIG_H264.enableIntraRefresh / intraRefreshPeriod /
  intraRefreshCnt` — already written, `nvidia/encoder.rs:594-598`. Intel oneVPL
  `MFX_REFRESH_SLICE` — already written, `intel/encoder.rs:448-459`. macOS VideoToolbox has **no**
  rolling intra-refresh (the code already logs this, `macos_utils/streamer/encoder.rs:231-236`);
  macOS keeps PLI→IDR.
- **Probe:** attempt the encoder init with intra-refresh, fall back to the current config if
  `nvEncInitializeEncoder` rejects it. Never gate on driver version.
- **Expected win:** removes the frame-size spike. With `vbvBufferSize = bitrate/fps`
  (`nvidia/encoder.rs:583`) an IDR blows a one-frame VBV by 5–20×; at 20 Mbps that is
  ~40–100 ms of extra serialization on the last hop for that frame plus everything queued behind
  it. ALVR's published result for the same change is "flattening frame-size distributions and
  yielding **lower latency variability**". Expect the p99 to move far more than the mean.
- **Measure:** encoded-frame-size histogram (p50/p99 ratio) and the client's `videoDelayEmaMs`
  p99, before/after, on a loaded 5 GHz link.
- **Cost:** hours. The code exists; it is a default and a call site.
- **Caveat:** with `idrPeriod = INFINITE` and intra-refresh on, the transform-worker's
  `waitingForKey` gate (`transform-worker.js:224-227`) will stall forever if it ever misses the
  one IDR. The PLI→IDR path must stay, and `KEY_REQUEST_MIN_INTERVAL_MS = 250` gives the retry.

### R2 — Pace a frame's RTP burst (finding: `major-webrtc-no-send-pacer.md`, extended by this research)

- **What:** spread the ~20 packets of a 1080p frame across a fraction of the frame interval
  instead of writing them back to back.
- **Evidence:** Stadia, measured on the wire, sends 7–9 packets per ~2 ms sub-burst inside each
  16.67 ms frame period. WebRTC's own stack has a pacer for exactly this reason; we bypass it by
  writing to `TrackLocalStaticRTP` directly.
- **Where:** `webrtc_session.rs:527-546`, the `for (i, payload) in payloads.into_iter()` loop.
- **How:** budget `min(frame_interval × 0.5, 8 ms)` for the burst, sleep
  `budget / packet_count` between packets using a coarse timer, and **skip pacing entirely when
  `SessionLocality::SameNetwork`** if measurement shows it is not needed on a clean LAN.
- **Expected win:** none on an idle wired LAN; the win is on congested Wi-Fi, where a 20-packet
  burst into a 40 MHz link at a low MCS is exactly the queue that produces the tail latency and
  the loss that triggers the IDR cascade. Expect p99 improvement, not p50.
- **Measure:** inter-packet arrival histogram at the client (via `getStats()`
  `jitterBufferDelay` / `framesDropped`), and `fraction_lost` at the same offered bitrate.
- **Cost:** low-medium. Risk: a sleep in the RTP writer task must not be a `std::thread::sleep`
  and must not hold any lock — see the `.await` rule in `CLAUDE.md`.

### R3 — Tighten and re-shape the bitrate controller (finding: `major-webrtc-bitrate-control-is-slower-and-slacker-than-the-field.md`)

- **What:** react on the *derivative* of queueing delay, not on 10% loss.
- **Evidence:** Parsec "detect a congestion event before it starts … split micro-second
  decisions"; Stadia "constantly adjusting a target bitrate based on per-packet feedback signals";
  Luna "analyses network conditions dozens of times per second"; Stadia sends RTCP immediately
  whenever its estimate moves >3%.
- **Concretely:** (a) `estimate_from_loss` currently does nothing between 2% and 10% loss
  (`bitrate.rs:113-119`) — 5% loss on Wi-Fi is already a broken picture; make the cut continuous
  from ~1%. (b) The 500 ms `min_interval` on raises (`bitrate.rs:20`) is fine; the 120 ms
  `BWE_POLL_INTERVAL` is not, given `get_stats()` is a full report walk. (c) Feed the
  delay-gradient from the transport-cc feedback we already negotiate — this depends on
  `major-webrtc-transport-cc-advertised-but-never-stamped.md` landing first.
  (d) Fix `blocker-webrtc-rtt-unit-mismatch-collapses-bitrate.md` before touching any of this,
  or every measurement will be against a session pinned at the 1 Mbps floor.
- **Expected win:** the difference between "quality collapses and recovers over 200 s" (Stadia's
  measured transient) and a controller that never lets the queue build. On Wi-Fi this is the
  single largest source of multi-hundred-millisecond excursions.
- **Cost:** medium-high; blocked on two existing findings.

### R4 — Add video FEC (finding: `speculative-webrtc-no-fec-on-relayed-paths.md`, extended by this research)

- **What:** RED + ULPFEC on the video track, or an application-level XOR/Reed–Solomon parity
  packet per frame.
- **Evidence:** Sunshine ships `fec_percentage = 20` by default; Jump Fluid 2.0 added FEC as a
  headline networking change; every VR stack uses it; Miracast carries it in the TS.
- **Why here:** today a single lost packet on a P-frame produces a browser NACK (an RTT of
  latency, best case) or a PLI (an RTT *plus* a full IDR, which is the very spike R1 removes). On
  a 1–2% loss Wi-Fi link that is a visible hitch every few seconds.
- **Probe:** negotiate `red`/`ulpfec` payload types only if the browser offers them in the SDP —
  Chrome does for video, Safari's support is unreliable, so this must be a per-session
  capability check on the offer, never a UA sniff.
- **Expected win:** removes the RTT-plus-IDR penalty for isolated loss. Cost is 10–20% bandwidth,
  which per Parsec's own doc ("higher bitrates increase latency") must be taken *out of* the video
  budget, not added on top.
- **Cost:** medium. Do it after R1, since R1 already removes the worst consequence of a PLI.

### R5 — Publish a per-stage latency breakdown (findings: `major-host-no-per-stage-encode-latency-metric.md`, plus the existing `major-client-no-glass-to-glass-telemetry.md`)

Every serious competitor exposes one — Parsec (encode ms / decode ms), Moonlight (host
processing / network / decode / render / **frame queue delay**), Steam Link (input vs display),
Jump (live connection statistics). **ScreenExtend cannot verify R1–R4 without it**, and the
client half is worse than absent: `major-client-no-glass-to-glass-telemetry.md` shows the one
latency-shaped number we do compute is off by a whole epoch. Stages to instrument, all of which
already have timestamps available: capture→encode-submit, encode-submit→bitstream-out,
bitstream→first RTP write, RTP→client depacketize (the RTP timestamp already carries the shared
host clock, `webrtc_session.rs:512`), depacketize→`decoder.decode()`, `decode()`→`output`
callback, `output`→`drawImage`. The last four are all already inside `transform-worker.js` and
simply are not measured; the first three are host-side and have no measurement at all.
**The desktop UI's appearance may not change** — surface this through
`tprintln!`/`get_log_backlog` and the CLI (`ScreenExtend status --json`), not a new panel.

### R6 — Make the wired path a first-class, detected tier

- **Evidence:** every one of Sidecar, Luna, Duet, spacedesk and Steam Link treats wired as the
  low-latency tier, and Luna's own bench shows the size of it (11.3 ms vs 22.4 ms — roughly half).
- **What:** detect that the selected ICE candidate pair is on a USB-tether / RNDIS-class interface
  (Windows: `IF_TYPE_IEEE80211` vs `IF_TYPE_ETHERNET_CSMACD` on the adapter behind the local
  candidate; macOS: the `bridge`/`en` interface family) and log it, report it in
  `ScreenExtend status --json`, and use it to skip R2's pacing.
- **Cost:** low. `get_network_adapters` already enumerates adapters; the local candidate IP maps
  onto one.

### R7 — HEVC/AV1 (finding: `speculative-webrtc-hevc-needs-a-datachannel-video-path.md`)

Six products offer it, and Jump Fluid's ~50% bandwidth claim maps directly onto serialization
delay on a weak link. **Blocked by the browser:** Chrome and Safari do not offer an H.265 RTP
payload type in a WebRTC offer, so the browser's depacketizer cannot feed our insertable-streams
transform. The only route is to move video onto an unreliable, unordered DataChannel and feed
WebCodecs ourselves — which is a large architectural change that would also unlock our own FEC and
our own pacing (and is roughly what Rainway did). Filed as speculative; not recommended now.

### R8 — Do not pursue sub-frame/slice delivery

Sony and Meta both patent it, ALVR needs it, AMD documents the 1/N latency reduction — and it is
unreachable here, because WebCodecs `EncodedVideoChunk` requires a complete access unit and the
WebRTC receiver reassembles a frame before the transform sees it. Setting NVENC `sliceMode` would
still localise loss damage to one slice instead of a whole frame, which is worth a cheap
experiment once FEC is in (filed as speculative), but the latency half of the technique is closed
to us as long as the client is a browser.

## 6. Verification plan

**Before/after, on each change:**

1. **Frame-size distribution** (host-side, `tprintln!` histogram): p50, p95, p99 of encoded frame
   bytes. R1 should collapse p99/p50 from ~10× to ~2×.
2. **Glass-to-glass**, camera method: 240 fps phone camera filming host monitor + client screen
   showing a millisecond counter; count frames. This is the only measurement that is not
   self-reported, and it is what Astropad used for the table in §2.
3. **`videoDelayEmaMs`** (`transform-worker.js:57-93`) is the obvious candidate and is
   **currently not a latency at all**: `findings/major-client-no-glass-to-glass-telemetry.md`
   establishes that the host RTP timestamp rides a process-relative monotonic epoch
   (`HOST_EPOCH`, `streamer/audio/mod.rs:17-27`) while the client's
   `performance.timeOrigin + performance.now()` is a Unix epoch, so the reported number is
   ~1.75e12 plus the real delay. Its *first difference* is still a usable jitter/tail metric
   (the constant offset cancels), which is how it is used everywhere above — but the **absolute**
   before/after comparison needs that finding's clock-offset fix landed first, or method 2.
4. **Loss/queue behaviour:** run each scenario clean, then with a competing 4K YouTube stream on
   the same AP, then with `clumsy` (Windows) injecting 1%, 3% and 5% loss, and record
   `fraction_lost`, the count of `request_idr()` calls, and p99 `videoDelayEmaMs`.
5. **Baseline against a competitor on the same hardware:** spacedesk (free) and Deskreen
   (open-source, same architecture) on the identical host/client/AP, using method 2. Without a
   side-by-side, all of our numbers are unfalsifiable.

**Cannot be verified on the current dev boxes:**

- Anything Intel Quick Sync (`MFX_REFRESH_SLICE`) — the Windows box is an RTX 3080 Ti laptop;
  whether it also has a usable iGPU is unknown, and the Intel path may never be exercised.
- Anything AMD — `amd.rs` is a stub.
- macOS: the 10.15.8 Intel box has no ScreenCaptureKit (12.3+), no Process Tap (14.2+), and
  `EnableLowLatencyRateControl` needs 11.0+, so the macOS encoder recommendations are unverifiable
  locally. The CGDisplayStream path is all that can be exercised.
- Safari's `red`/`ulpfec` behaviour for R4 needs a real iOS/macOS Safari client on a lossy link.
- Firefox strips the `playout-delay` answer attribute (Mozilla bug 1585009) — the Firefox client
  path needs its own measurement, and `speculative-client-jitter-buffer-target-diverges-between-paths.md`
  is already open on the adjacent issue.

## 7. Open questions

1. Does `webrtc-rs`'s `register_default_interceptors` actually run a **sender-side** BWE, or only
   the TWCC *receiver* interceptor? R3 hinges on this and it should be answered by reading the
   crate, not inferred.
2. What is our real encode time on the NVENC ring? Parsec publishes encode-ms and decode-ms
   separately; we have no equivalent, so we do not know whether R1–R4 (all network-side) are even
   attacking the dominant term. R5 must land first.
3. Does the Windows IDD driver present frames on a vblank cadence, or on damage? Sony's patents
   turn on scanning out at *flip time* rather than at VSYNC; if our capture is already
   damage-driven then the equivalent saving is already banked, and if it is vblank-locked there is
   up to a frame sitting there.
4. Is the `X-Device-Token`/OTP join path adding a measurable first-frame delay?
   `major-client-join-blocks-on-ice-gathering.md` and `major-webrtc-first-idr-discarded-before-track-bound.md`
   are both open on adjacent issues; join latency is a different metric from steady-state latency
   and this report did not measure it.
5. Rainway's HN thread (item 18242300) is the one primary source in this report that could not be
   fetched (HTTP 429). It is the closest published account of a browser-client game-streaming
   protocol and is worth one retry.

## Sources

- [Parsec — A Networking Protocol Built For Low Latency Interactive Game Streaming](https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007)
- [Parsec — 240 FPS at approximately 4–8 ms](https://parsec.app/blog/parsec-game-streaming-total-latency-at-240-frames-per-second-c0818cc0daa5)
- [Parsec — VSync setting](https://support.parsec.app/hc/en-us/articles/32361386984212-VSync-Setting-In-Parsec-Window) · [Software vs hardware decoding](https://support.parsec.app/hc/en-us/articles/32381511532820-Using-software-decoding-instead-of-hardware-decoding) · [Stream overlay stats](https://support.parsec.app/hc/en-us/articles/32381603663636-Stream-Overlay-Stats-and-Logging) · [VDD overview](https://support.parsec.app/hc/en-us/articles/32381178803604-VDD-Overview-Prerequisites-and-Installation)
- [Astropad — Expected latency in Luna Display](https://astropad.com/blog/comparing-latency-luna-display/) · [LIQUID technology](https://astropad.com/technology/) · [Luna vs Duet](https://astropad.com/luna-display-versus-duet-display/)
- [TechCrunch — Luna Display updates its video engine (LIQUID 3.0)](https://techcrunch.com/2019/01/24/luna-display-updates-its-video-engine-for-faster-performances/)
- [9to5Mac — Duet Display overhauled network protocol](https://9to5mac.com/2022/02/18/duet-display-for-mac-pc-update/)
- [spacedesk — Encoding Type](https://manual.spacedesk.net/EncodingType.html) · [Release notes](https://manual.spacedesk.net/ReleaseNotes.html) · [Firewall / ports](https://manual.spacedesk.net/FirewallSettingsonPrimaryMachine.html) · [MJPEG forced workaround](https://www.spacedesk.net/support-forum/topic/hardware-decorder-h264-tablet-android/)
- [moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c) · [RFI → LTR proposal (issue #120)](https://github.com/moonlight-stream/moonlight-common-c/issues/120) · [Moonlight FAQ](https://github.com/moonlight-stream/moonlight-docs/wiki/Frequently-Asked-Questions)
- [Sunshine configuration (fec_percentage, qp, presets)](https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2configuration.html) · [Performance tuning](https://docs.lizardbyte.dev/projects/sunshine/latest/md_docs_2performance__tuning.html) · [Apollo fork](https://github.com/ClassicOldSong/Apollo)
- [PCGamesN — Google engineers detail the streaming tech behind Stadia](https://www.pcgamesn.com/stadia/google-game-streaming-technology-latency-quality)
- [Carrascosa & Bellalta — Cloud-gaming: Analysis of Google Stadia traffic](https://ar5iv.labs.arxiv.org/html/2009.09786)
- [VentureBeat — How Stadia's "negative latency" might work](https://venturebeat.com/business/how-google-stadias-negative-latency-might-work/)
- [US 11,446,572 — Early scan-out of server display buffer at flip-time for cloud gaming](https://patents.google.com/patent/US11446572B2/en)
- [US 9,819,604 — Real time network adaptive low latency transport stream muxing for Miracast](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/9819604)
- [Wi-Fi CERTIFIED Miracast Technical Overview](https://tools.barco.com/kb-downloads/4814/Wi-Fi_CERTIFIED_Miracast_Technical_Overview_20170725.pdf)
- [Jump Desktop — Fluid 2.0 changelog](https://changelog.jumpdesktop.com/jump-desktop-with-fluid-2.0-1yMoBG) · [Fluid recommendations](https://support.jumpdesktop.com/hc/en-us/articles/360047204251-Fluid-Remote-Desktop-Recommendations-for-very-high-performance-scenarios)
- [Meta — Air Link Framerate Insurance](https://developers.meta.com/horizon/blog/air-link-framerate-insurance-afi/)
- [EmergentMind — ALVR streaming bridge (intra-refresh, sliced encoding)](https://www.emergentmind.com/topics/air-light-vr-alvr)
- [AMD VCU latency modes (slice-mode 1/N latency)](https://docs.amd.com/r/en-US/pg252-vcu/VCU-Latency-Modes)
- [Hopp — Achieving <100 ms latency for remote control with WebRTC](https://www.gethopp.app/blog/latency-exploration)
- [WebRTC playout-delay header extension](https://webrtc.googlesource.com/src/+/main/docs/native-code/rtp-hdrext/playout-delay/README.md) · [Mozilla bug 1585009](https://bugzilla.mozilla.org/show_bug.cgi?id=1585009) · [w3c/webcodecs #206 (optimizeForLatency)](https://github.com/w3c/webcodecs/issues/206)
- [Network Weather — AWDL interference](https://www.networkweather.com/learn/awdl/) · [gyorgy.sh — macOS Wi-Fi lag and the AWDL problem](https://gyorgy.sh/blog/macos-awdl-network-jitter)
- [alax.info — Meet Rainway](https://alax.info/blog/1824) · [HN 18242300](https://news.ycombinator.com/item?id=18242300)
- [pavlobu/deskreen](https://github.com/pavlobu/deskreen) · [nomi-san/parsec-vdd](https://github.com/nomi-san/parsec-vdd)
- [Splashtop — enable hardware acceleration](https://support-splashtopbusiness.splashtop.com/hc/en-us/articles/360039820771-Enable-hardware-acceleration-on-Splashtop-Streamer)
