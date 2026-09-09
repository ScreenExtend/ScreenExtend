# Manual tests — transport (W1)

Everything here needs hardware or a network condition neither development machine has. Each
section states what it decides, so a result is worth recording even when it is negative.

Development machines for reference: Windows 11 build 26200 with an RTX 3080 Ti Laptop GPU
(hybrid graphics), and macOS 10.15.8 Catalina on Intel with Command Line Tools only.

## 1. Wired versus wireless client (`minor-session-usb-tether`)

**Decides:** whether ScreenExtend should document the USB-tether route as a low-latency tier, or
close it as "measured, not worth documenting". Both outcomes are results.

The host already reports which it is: half a second after the connection comes up, the log shows

```
session link: local candidate 192.168.1.20 is on a wired adapter
```

from `platform::link_medium_for_local_ip` (Windows: `MSFT_NetAdapter.NdisPhysicalMedium`, where
14 is 802.3 and 9/1 are 802.11; macOS: `getifaddrs` then `SIOCGIFMEDIA`). **USB tethering and
RNDIS both report Ethernet**, which is the point — a phone on a cable is an ordinary IP interface.

Setup, one Android phone (iOS has no USB tethering, so the honest documentation is Android-only):

1. Phone: Settings → Hotspot & tethering → USB tethering, with the cable in the host.
2. `ScreenExtend serve -v --no-cloud`. The join URL on the tether subnet is the one to open.
3. Confirm the log line above says `wired`. If it says `wireless`, the phone joined over Wi-Fi
   instead and the whole measurement is void.

Measure glass-to-glass with the 240 fps camera method — a millisecond counter on the host monitor
and the client screen in one frame, count frames between them — for each of:

| Case | Host link | Client link |
|---|---|---|
| A | wired Ethernet | USB-tethered Android |
| B | wired Ethernet | 5 GHz Wi-Fi, quiet AP |
| C | wired Ethernet | 5 GHz Wi-Fi, with a competing 4K stream on the same AP |

Report p50 and p99 for each. Published figures to compare against: Luna Display's own 50-sample
bench is 11.3 ms mean over USB against 22.4 ms over Wi-Fi.

Also verify the detection does not misclassify a Wi-Fi adapter behind a virtual switch
(Hyper-V, VirtualBox host-only), which is a realistic false positive on a development box.

## 2. DSCP / WMM marking (`major-webrtc-ice-sockets-get-no-socket-options`)

**Decides:** whether the qWAVE and `SO_NET_SERVICE_TYPE` marking reaches the air, and whether it
changes anything on a real AP. Many consumer APs ignore DSCP entirely, so "no effect on this AP"
is a real and useful result.

Needs two stations on one AP and, ideally, a monitor-mode capture.

**The tier is off by default and this is the measurement that decides whether it ships on.** Bound
to the wildcard address it made the join flaky — the WHEP self-test connected on 3 of 5 runs,
because the muxed gather emits a host candidate per local address (24 on the dev box) while one
socket can only send from one route. It now binds the adapter the streamer serves and filters ICE
to that address, which removes the cause, but the self-test cannot exercise that path — its config
leaves `bind_ip` unspecified. So: `SCREENEXTEND_MEDIA_SOCKET=1`, and **check the join still
connects 10 times out of 10** before believing any latency number from it.

1. Host and client on the same AP; a second station saturating the link with `iperf3`.
2. `SCREENEXTEND_MEDIA_SOCKET=1 ScreenExtend serve -v`. On a same-subnet join the log should show
   `media socket: muxing ICE over <adapter ip>:NNNNN with SO_SNDBUF, SO_RCVBUF, SIO_UDP_CONNRESET=off`
   and, once the pair is nominated,
   `media socket: marked as interactive video for <client ip:port>`.
   `DSCP marking unavailable` means qWAVE is off or absent — record which.
3. Record p99 of the client's display lag and `fraction_lost` from the BWE log line, with the
   marking and with it disabled (comment out the `mark_video_flow` call).
4. If a monitor-mode capture is available, confirm the QoS control field's TID carries AC_VI.

Separately, with `SO_SNDBUF` at 1 MB, count `track write_rtp failed` lines during a keyframe
storm, before and after.

**Also confirm the LAN tier did not cost anything it should not have:** the muxed socket disables
server-reflexive gathering, so check that a same-subnet join still connects and that
`ICE candidates in local answer (host)` reports **one** host candidate, on the adapter's own
address. More than one means the IP filter is not doing its job and the flakiness above is back.

## 3. Trickle ICE against a real browser (`major-client-can-now-trickle-ice-...`)

**Decides:** whether the host's PATCH route works with what a browser actually sends.

The host half has landed and is unit-tested; the client half has not. Until it does, the route can
still be exercised by hand:

```sh
# join, keeping the headers
curl -k -i -X POST https://<host>:8443/whep -H 'content-type: application/json' -d @join.json
# -> 200, Location: /whep/<id>

curl -k -i -X PATCH https://<host>:8443/whep/<id> \
  -H 'content-type: application/trickle-ice-sdpfrag' \
  --data-binary $'a=ice-ufrag:AbCd\r\nm=video 9 UDP/TLS/RTP/SAVPF 0\r\na=mid:0\r\na=candidate:1 1 udp 2130706431 192.168.1.5 54321 typ host\r\n'
# -> 204, and the host log shows: trickle: added 1 remote candidate(s)
```

A `PATCH` after the session has ended must return 404, not 500.

## 4. H.264 High profile against the client matrix (`major-webrtc-h264-profile-...`)

**Decides:** whether negotiating High is safe on the whole client matrix, and what it is worth.

The host now picks High when the client offers it. Verify on Safari 13.1, an old iPad, an old
Android, Chrome, and Firefox that each still decodes what it negotiated — the host log prints

```
H.264 negotiated: profile=High level=0x2a (offered=[Baseline, Main, High], level-asymmetry=true, hevc=false)
```

Then A/B the same 60 s desktop workload at a fixed rate-control target, Baseline versus High, and
compare VMAF or SSIM; or hold quality and compare achieved bitrate. The expected saving is 5-15 %
from CABAC alone, more on flat regions and sharp text from the 8x8 transform.

`ffprobe` on a captured stream should show the SPS's `level_idc` matching the advertised
`profile-level-id`.

## 5. Repair from a long-term reference instead of an IDR (`major-nvenc-reference-invalidation-...`)

**Decides:** how much of the keyframe storm the repair path actually removes on a lossy link. The
byte cost of one repair is already measured (`nvenc_ltr_repair_is_cheaper_than_an_idr`: 23698 B
against a 28423 B IDR, on a synthetic worst case where every pixel changes every frame); what
cannot be measured without induced loss is the *rate* — how many IDRs a real lossy minute stops
producing.

Neither the probe harness nor the WHEP self-test can inject loss, so this needs an external tool.

1. `ScreenExtend serve -v --no-cloud` on an NVIDIA host, and join from a real browser. Confirm the
   tier is live:

   ```
   NVENC: 2 long-term reference slot(s); a lost frame can be repaired without an IDR
   ```

   If that line is absent the encoder has no slots and every recovery is a keyframe, which is the
   pre-existing behaviour and not a failure.

2. Induce loss on the client side with `clumsy` (Windows) at 2 % and then 5 %, or `dnctl`/`pfctl`
   on a macOS client.

3. Count per minute, from `ScreenExtend status --json` and the log:
   - `idrs_sent` — the number this change exists to reduce;
   - `keyframe_requests` and `keyframe_requests_suppressed`;
   - `picture recovery: … (repairing from a long-term reference, no IDR)` log lines, which are the
     repairs that replaced a keyframe.

4. Compare the client's p95 inter-frame gap and `framesDropped` across the two runs. The target is
   that a single lost packet costs one P frame's worth of artefact rather than an IDR burst.

**Also worth watching:** `nvEncInvalidateRefFrames` refusing (`the frame has probably already left
the DPB, so this loss needs a keyframe`). A high refusal rate means feedback is arriving later than
the DPB holds the picture, and the marking cadence should be slowed rather than the tier abandoned.

The path is wired into the **NVENC zero-copy ring only** — the default on any NVIDIA host. DXGI
duplication, the synthetic probe path and macOS all report zero long-term slots, so on those the
transport goes on asking for keyframes and nothing here applies.
