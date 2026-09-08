# Client browser — the fastest decode → present → audio → input path per engine

Research report for the ScreenExtend latency & compatibility overhaul.
Method and citation rules: `docs/research/README.md`. Findings-file schema: `docs/discovery/README.md`.
Line references are against the working tree at the time of writing.

---

## 1. Question

For every browser the ScreenExtend client page must run on — Chrome desktop, Edge, Chrome Android,
Safari iOS/iPadOS, Safari macOS, Firefox, Samsung Internet, and 2018-era WebKit — what is the
fastest available path for **decode**, **present**, **audio** and **input**; how do we select each
tier by probing; and what is the current page doing wrong?

---

## 2. Findings

### 2.1 The H.264 reorder-delay trap — and why WebRTC saves us from it

The best-known WebCodecs latency failure is a decoder holding N frames before emitting the first.
It is worth establishing precisely, because it decides whether the host encoder configuration is on
the client's latency path at all. **It turns out it is not, on the RTP transport — but only because
libwebrtc rewrites the bitstream for us.**

**The mechanism (Chromium).** `optimizeForLatency: true` maps to the `low_delay` argument of
`media::VideoDecoder::Initialize`:
```cpp
bool VideoDecoder::GetLowDelayPreference(const ConfigType& config) {
  return config.hasOptimizeForLatency() && config.optimizeForLatency();
}
```
— `third_party/blink/renderer/modules/webcodecs/video_decoder.cc:352-354`, passed on at `:237-245`
([source](https://chromium.googlesource.com/chromium/src/+/main/third_party/blink/renderer/modules/webcodecs/video_decoder.cc)).
The `media/base/video_decoder.h` contract is *"If `|low_delay|` is true then the decoder is not
allowed to queue frames, except for out-of-order frames"* — and that carve-out is the whole story.
`low_delay` is honoured by **exactly one** backend:

| backend | file | honours `low_delay`? |
|---|---|---|
| FFmpeg (software) | `media/filters/ffmpeg_video_decoder.cc:506` | **yes** — `thread_type = FF_THREAD_SLICE \| (low_delay ? 0 : FF_THREAD_FRAME)` |
| D3D11/D3D12 (Windows) | `media/gpu/windows/d3d_video_decoder.cc` | no — parameter never read |
| VideoToolbox (macOS) | `media/gpu/mac/video_toolbox_video_decoder.cc:118` | no — parameter never read |
| VA-API (Linux) | `media/gpu/vaapi/vaapi_video_decoder.cc` | no — `bool /*low_delay*/,` |
| V4L2 | `v4l2_video_decoder.cc` | no — `bool /*low_delay*/,` |
| MediaCodec (Android) | `media/gpu/android/media_codec_video_decoder.cc:717-725` | only if `kMediaCodecLowDelayMode`, which is `FEATURE_DISABLED_BY_DEFAULT` (`media/base/media_switches.cc:1333`) |

Every hardware path instead runs `media/gpu/h264_decoder.cc`, which takes its reorder depth
**exclusively from the SPS VUI**:
```cpp
if (sps->vui_parameters_present_flag && sps->bitstream_restriction_flag) {
  max_num_reorder_frames_ = sps->max_num_reorder_frames;   // …
} else if (sps->constraint_set3_flag) { switch (sps->profile_idc) {
    case 44: case 86: case 100: case 110: case 122: case 244:
      max_num_reorder_frames_ = 0; break;
    default: max_num_reorder_frames_ = dpb_.max_num_pics(); break; } }
else { max_num_reorder_frames_ = dpb_.max_num_pics(); }
```
— `h264_decoder.cc:1174-1207`. Note Baseline (66) and Main (77) are **not** in the case list, so
`constraint_set3_flag` cannot save them. The output loop then emits nothing until the DPB is full:
```cpp
while (num_remaining > max_num_reorder_frames_ ||
       ((dpb_.IsFull() && (!pic->outputted || pic->ref)) && num_remaining)) { … OutputPic(…) }
```
— `h264_decoder.cc:1124-1154`, with `max_dpb_size = max(MaxDpbMbs(level)/MBs capped at 16, …)`
(`:1255-1269`, table in `media/parsers/h264_level_limits.cc:28-72`). Steady state is one output per
input, delayed by `max_dpb_size` frames:

| resolution | MBs | L3.1 | L4.0/4.1 | L4.2 | L5.0 | L5.1/5.2 |
|---|---|---|---|---|---|---|
| 1280×720 | 3600 | 5 | 9 | 9 | 16 (cap) | 16 (cap) |
| 1920×1080 | 8160 | 2 | 4 | 4 | 13 | 16 (cap) |
| 2560×1440 | 14400 | 1 | 2 | 2 | 7 | 12 |

33-267 ms at 60 fps, invisible to `decodeQueueSize` (the chunks *are* accepted). Chromium's Dan
Sanders confirms the mechanism in [w3c/webcodecs#732](https://github.com/w3c/webcodecs/issues/732);
[#899](https://github.com/w3c/webcodecs/issues/899) reports ~3 s from a VideoToolbox-encoded stream
lacking the hint.

**Why ScreenExtend does not hit this: libwebrtc rewrites the SPS on the receive path.**
```cpp
// Check if VUI is present in SPS and if it needs to be modified to
// avoid excessive decoder latency.
SpsVuiRewriter::ParseResult result = SpsVuiRewriter::ParseAndRewriteSps(
    nalu_data, &sps, nullptr, &output_buffer, SpsVuiRewriter::Direction::kIncoming);
```
— `modules/rtp_rtcp/source/video_rtp_depacketizer_h264.cc:110-158`
([source](https://webrtc.googlesource.com/src/+/main/modules/rtp_rtcp/source/video_rtp_depacketizer_h264.cc)),
which on `kVuiRewritten` splices the rewritten SPS into the packet payload and fixes the STAP-A
length field. The rewriter is unconditional:
- SPS with no VUI at all ⇒ it **writes one**, with `bitstream_restriction_flag = 1` and
  `AddBitstreamRestriction(dest, sps.max_num_ref_frames)` (`sps_vui_rewriter.cc:294-321`).
- SPS with a VUI but no bitstream restriction ⇒ same (`:378-385`).
- SPS with a bitstream restriction ⇒ it **overwrites** `max_num_reorder_frames` with `0` and
  `max_dec_frame_buffering` with `sps.max_num_ref_frames` (`:398-412`).
- `AddBitstreamRestriction` (`:440-466`) hard-codes `max_num_reorder_frames = 0`.

Chromium, WebKit and Gecko all use libwebrtc for the RTP stack, and the depacketizer runs on RTP
packets — i.e. **before** frame assembly and therefore **before** the encoded transform (§2.4). So
the SPS that reaches the page's `VideoDecoder` already says `max_num_reorder_frames = 0`,
**regardless of what the host encoder wrote.**

**Two things follow, and both matter:**

1. The host-side VUI is *not* a client latency item today. It is a correctness item that becomes a
   blocker the moment video leaves RTP — e.g. the DataChannel video path contemplated in
   `speculative-webrtc-hevc-needs-a-datachannel-video-path.md`, or any WebTransport experiment.
2. The rewrite depends on the depacketizer **seeing and parsing an SPS NAL**. If the SPS never
   arrives — which is exactly what
   `major-webrtc-sps-pps-silently-dropped-when-stap-a-exceeds-mtu.md` describes — the frame is
   dropped outright (`kFailure` ⇒ `return std::nullopt`, `video_rtp_depacketizer_h264.cc:130-132`).
   That finding is therefore more serious than it looked: it is not just "the client waits for the
   next keyframe", it is "the depacketizer discards the packet".

### 2.2 `optimizeForLatency` and `hardwareAcceleration`, per spec and per engine

Spec (w3c/webcodecs `index.src.html:2078-2087`):
> Hint that the selected decoder *SHOULD* be configured to minimize the number of
> `EncodedVideoChunk`s that have to be decoded before a `VideoFrame` is output.
> NOTE: In addition to User Agent and hardware limitations, **some codec bitstreams require a
> minimum number of inputs before any output can be produced.**

And in Definitions: *"The underlying codec implementation MAY emit new outputs only when new inputs
are provided."* It is deliberately a hint and is ignored by `isConfigSupported()`
([w3c/webcodecs#206](https://github.com/w3c/webcodecs/issues/206)); a request for MUST semantics was
declined ([#698](https://github.com/w3c/webcodecs/issues/698)).

Per engine:
- **Chromium** — no-op on every hardware decoder (table above); on software it disables ffmpeg frame
  threading, which costs throughput: 4K/243 frames went 5.83 s → 11.94 s in
  [#491](https://github.com/w3c/webcodecs/issues/491).
- **WebKit** — parsed and dropped. `createVideoDecoderConfig()`
  (`Source/WebCore/Modules/webcodecs/WebCodecsVideoDecoder.cpp:121`) builds a
  `WebCore::VideoDecoder::Config` that has no latency field, and
  `kVTDecompressionPropertyKey_RealTime` is soft-linked but never set. Youenn Fablet: *"Safari is
  not yet really using `optimizeForLatency: true`"* (#698). Safari is nonetheless 1-in-1-out for
  H.264 by construction — it decodes through the libwebrtc VideoToolbox decoder, whose decode calls
  pass `kVTDecodeFrame_EnableAsynchronousDecompression` **without**
  `kVTDecodeFrame_EnableTemporalProcessing`, the flag that licenses reordering.
- **Gecko** — the only engine that plumbs it end to end: `AppleVTDecoder.cpp:914` sets the reorder
  queue depth to **0** under low-latency; `WMFVideoMFTManager.cpp:251` sets
  `CODECAPI_AVLowLatencyMode`; `FFmpegVideoDecoder.cpp:1415` sets `AV_CODEC_FLAG_LOW_DELAY` plus
  `FF_THREAD_SLICE`.

⇒ **Keep `optimizeForLatency: true`.** It is free on Chromium hardware, genuinely effective on
Firefox, harmless on Safari.

`HardwareAcceleration` (`index.src.html:2315-2400`):
> Setting a value of `prefer-hardware` or `prefer-software` **can significantly restrict what
> configurations are supported**… Hardware acceleration often has **higher startup latency** but
> more consistent throughput… **For lower resolution content, the overhead added by hardware
> acceleration can yield decreased performance**… Most authors will be best served by using the
> default of `no-preference`.

Implementation reality:
- Chromium `video_decoder_broker.cc:205-223` — `prefer-hardware` installs **only** the GPU/mojo
  factory with no software fallback, so `isConfigSupported` returns `false` and `configure()` throws
  on any machine without a compatible hardware decoder. `prefer-software` short-circuits to `false`
  for any codec that is not a built-in software codec (which is why `prefer-software` + HEVC is
  unsupported in Chrome).
- WebKit collapses the enum: `PreferSoftware ? No : Yes` — **`no-preference` is identical to
  `prefer-hardware` in Safari.**
- Gecko maps `prefer-software` to `HardwareDecoderNotAllowed`.

⇒ **`no-preference` is the correct default** (which is what the page gets today by omission).
`prefer-software` is a *diagnostic* for a suspected reorder hold, not a policy.

### 2.3 Annex B vs avcC — current choice correct; `avc3.` is a Safari landmine

W3C AVC codec registration: `description` present ⇒ `AVCDecoderConfigurationRecord`, bitstream in
`avc` format; absent ⇒ `annexb`, and *"key frames must include IDR pictures plus all necessary
parameter sets"* (<https://www.w3.org/TR/webcodecs-avc-codec-registration/>). There is no latency
difference — Blink converts avcC to Annex B internally before handing off. libwebrtc delivers Annex
B with 4-byte start codes and prepends the tracked SPS/PPS to every keyframe
(`modules/video_coding/h264_sps_pps_tracker.cc:34,124-160`), so **no `description` is correct**, and
the host's `repeatSPSPPS = 1` satisfies the parameter-set requirement.

One trap: WebKit's `isSupportedDecoderCodec()` tests only `codec.startsWith("avc1.")`, so an
`avc3.*` codec string throws `NotSupportedError` on Safari while Chromium accepts both.
`transform-worker.js:24-28` and `:45` only ever build `avc1.` strings, so the page is safe — but
this is worth a comment, because a future "use the SDP's codec string verbatim" change would break
Safari.

### 2.4 The insertable-streams hook sits *before* the jitter buffer

From libwebrtc, which all three engines use:
```cpp
void RtpVideoStreamReceiver2::OnAssembledFrame(std::unique_ptr<RtpFrameObject> frame) {
  … if (buffered_frame_decryptor_) { … }
  else if (frame_transformer_delegate_) { frame_transformer_delegate_->TransformFrame(…); }
  else { OnCompleteFrames(reference_finder_->ManageFrame(std::move(frame))); } }
```
— `video/rtp_video_stream_receiver2.cc:1108-1173`; the comment at `:1432` confirms *"the
`frame_transformer_delegate_` is called before the frames are inserted into the
`RtpFrameReferenceFinder`"*. MDN agrees: *"Network → Packetizer → RTCRtpScriptTransform → Decoder"*.

**Consequences:** the transform readable is fed the instant a frame is assembled from RTP — before
the reference finder, before `FrameBuffer`, before the decoder. `receiver.jitterBufferTarget = 0`
and `playoutDelayHint = 0` are therefore **inert on the WebCodecs path** (`index.html:831-833`,
`:873-875`); they matter only on the `<video>` fallback (`:922-923`). The client's own worklet is
the only jitter buffer on the video path. That is the right architecture and the page already has it.

### 2.5 `RTCRtpScriptTransform` now exists on Chrome — with `sendKeyFrameRequest`

MDN browser-compat-data (`api/RTCRtpScriptTransformer.json`, `api/RTCRtpReceiver.json`):

| API | Chrome/Edge | Firefox | Safari |
|---|---|---|---|
| `RTCRtpReceiver.transform` / `RTCRtpScriptTransform` | **141** | 117 | 15.4 |
| `RTCRtpScriptTransformer.sendKeyFrameRequest()` | **141** | 117 | 15.4 |
| `RTCRtpScriptTransformer.generateKeyFrame()` (sender side) | **not supported** | 117 | 15.4 |
| `RTCRtpReceiver.createEncodedStreams()` (legacy) | 86 | — | — |
| `RTCRtpReceiver.jitterBufferTarget` | 124 | 115 | 27 |

The Chromium "Intent to Ship: WebRTC Encoded Transform (V2)" confirms Chrome 141 for desktop,
Android and WebView; that `createEncodedStreams` applications keep working; and that only the
*sender-side* `generateKeyFrame` was deferred
(<https://groups.google.com/a/chromium.org/g/blink-dev/c/QeosFeu-d8A>). MDN marks both
`RTCRtpScriptTransform` and `sendKeyFrameRequest` Baseline "newly available since October 2025".

This upgrades `major-client-no-keyframe-request-on-chrome.md`'s option 1 from a hypothesis to a
confirmed fix. See §5 R1.

### 2.6 `decodeQueueSize` is not a latency signal, and `dequeue` is the right hook

Per spec, `decode()` increments `decodeQueueSize` synchronously and the decrement happens when the
control message is **dequeued toward the codec**, not when a frame comes out — so it can read 0 with
frames still in flight in the platform decoder. The saturation limit is implementation-defined and
*"may be any value greater than 1, including infinity"*
([#864](https://github.com/w3c/webcodecs/issues/864)); Chromium allows 1-8 in flight for video,
Firefox caps at 1. The counter update and the `dequeue` event are not atomic
([#724](https://github.com/w3c/webcodecs/issues/724),
[#494](https://github.com/w3c/webcodecs/issues/494)).

`dequeue` support: Chrome 106+, Firefox 130+, Safari 16.4+ (BCD `api/VideoDecoder.json`) — i.e.
everywhere `VideoDecoder` itself exists, except Chrome 94-105.

The pattern Chromium's own maintainer recommends
([#900](https://github.com/w3c/webcodecs/issues/900)) is to gate on **outstanding outputs**, not on
queue size:
```js
function maybeScheduleDecode() {
  if (activeOutputs >= TARGET_FRAME_COUNT /* 1 */) return;
  decoder.decode(getNextPacket());
}
decoder.ondequeue = maybeScheduleDecode;
```
> *"Providing inputs until you get an output one per dequeue is the only surefire approach at the
> moment."* — Dale Curtis

The official sample `samples/lib/video_renderer.js` uses
`while (frameBuffer.length < 3 && decoder.decodeQueueSize < 3)`.

### 2.7 Presentation: `desynchronized: true` is a no-op on an OffscreenCanvas in Chromium

Blink's low-latency canvas machinery lives entirely on `HTMLCanvasElement`:
```cpp
// canvas_rendering_context_host.h:113
virtual bool LowLatencyEnabled() const { return false; }
// html_canvas_element.cc:1230
bool HTMLCanvasElement::LowLatencyEnabled() const {
  return context_ && context_->CreationAttributes().desynchronized; }
```
`html_canvas_element.cc:591-596` creates the layer and the `CanvasResourceDispatcher`; `:712-728`
dispatches a low-latency frame straight to viz. **`OffscreenCanvas` overrides none of it** —
`grep -i "LowLatency\|desynchronized"` over
`third_party/blink/renderer/core/offscreencanvas/offscreen_canvas.{cc,h}` returns nothing.

Even on an `HTMLCanvasElement`, BCD (`api/HTMLCanvasElement.json`) records the 2D `desynchronized`
attribute as effective on **ChromeOS and Windows only** (Chrome 81; 75-81 ChromeOS-only), Chrome
Android 75, Safari 15, **Firefox: not supported**. A Chromium graphics-dev thread from Feb 2025
confirms macOS still does not work — IOSurface concurrent read/write is unsupported under Metal, the
first ~50 frames freeze — and the recommendation was to deprioritise it in favour of *"improving
synchronized-mode OffscreenCanvas"*
(<https://groups.google.com/a/chromium.org/g/graphics-dev/c/20qDm3ZD2f8>).

What a worker OffscreenCanvas *does* do:
```cpp
void OffscreenCanvas::DidDraw(const gfx::Rect& rect) {
  … needs_push_frame_ = true;
  if (!inside_worker_raf_) { … GetOrCreateResourceDispatcher()->SetNeedsBeginFrame(true); } }
bool OffscreenCanvas::BeginFrame() { … return PushFrameIfNeeded(); }
```
— `offscreen_canvas.cc:718-757`. A paint requests a BeginFrame; the newest content is pushed to the
display compositor at the next vsync, without touching the page's main thread. So the current
structure costs **≤1 vsync** and painting more often than vsync buys nothing. Reference context from
Chrome's own article: `desynchronized` exists because *"latencies longer than 50 milliseconds can
interfere with a user's hand-eye coordination"*, and it works by bypassing the renderer compositor
queue, with front-buffer rendering (and tearing) on ChromeOS
(<https://developer.chrome.com/blog/desynchronized>).

### 2.8 The `<video>` / track-generator alternative is not portable

- `MediaStreamTrackGenerator`: Chrome 94; **Firefox and Safari: no**.
- `VideoTrackGenerator`: **Safari 18 only**; Chrome: no.
(BCD `api/MediaStreamTrackGenerator.json`, `api/VideoTrackGenerator.json`.)

"Decode with WebCodecs, present through `<video>` to get a hardware overlay" therefore cannot be a
universal tier — it needs two different APIs and exists on neither Firefox nor Chrome-with-VTG. At
best a Chromium-Windows experiment.

`requestVideoFrameCallback` is broadly available (Chrome 83, Safari 15.4, Firefox 132 — BCD
`api/HTMLVideoElement.json`) and its `presentationTime`/`expectedDisplayTime` are the only
first-party way to measure real presentation time — but it exists only on `<video>`, i.e. only on the
fallback path.

### 2.9 Audio

| API | Chrome/Edge | Chrome Android | Firefox | Firefox Android | Safari | Safari iOS |
|---|---|---|---|---|---|---|
| `AudioDecoder` (WebCodecs) | 94 | 94 | 130 | **no** | **26** | 26 |
| `VideoDecoder` | 94 | 94 | 130 | **no** | 16.4 | 16.4 |
| `SharedArrayBuffer` | 68 | **89** | 79 | 79 | 15.2 | 15.2 (Android WebView: **no**) |
| `AudioContext` `latencyHint` | 58 | 58 | **not implemented** | — | 14.1 | 14.1 |
| `AudioContext.baseLatency` | 58 | 58 | 70 | 70 | 14.1 | 14.1 |
| `AudioContext.outputLatency` | 102 | 102 | 70 | 70 | **18.4** | 18.4 |
| `AudioContext.setSinkId` | 110 | 110 | no | no | no | no |
| `HTMLMediaElement.setSinkId` | 49 | **no** (crbug 41276355) | 116 | **no** (bug 1473346) | 18.4 | 18.4 |
| `mediaDevices.selectAudioOutput()` | **never shipped** | no | 116 | no | no | no |
| `navigator.audioSession` | no | no | preview | preview | 16.4 | 16.4 |
| `BaseAudioContext.renderQuantumSize` | **153** | no | no | no | no | no |
| `AudioContext.playbackStats` | 146 | 146 | no | no | no | no |
| `getOutputTimestamp()` | 57 | 57 | 70 | 70 | 14.1 | 14.1 |

(BCD `api/AudioDecoder.json`, `api/VideoDecoder.json`, `javascript/builtins/SharedArrayBuffer.json`,
`api/AudioContext.json`, `api/BaseAudioContext.json`, `api/HTMLMediaElement.json`,
`api/MediaDevices.json`, `api/Navigator.json`.)

Three consequences:

1. **Safari before 26 has no `AudioDecoder`.** Safari 16.4 shipped *"the video portion of Web Codecs
   API"* only (<https://webkit.org/blog/13966/webkit-features-in-safari-16-4/>). So every Safari
   from 16.4 to 18.x — the overwhelming majority of iPhones, iPads and Intel Macs — gets video on the
   fast path and audio on the browser-buffered Opus track. Detected correctly (`audio.js:15-20`) but
   not distinguished in the user-facing report.
2. **`outputLatency` is absent on Safari < 18.4**, so `audio.js:418-424` contributes 0 for it, the
   commanded worklet depth comes out too large by the true output latency, and audio lands *late* by
   typically 10-40 ms — on exactly the platform whose audio path is already the slow one.
3. **Firefox does not implement `latencyHint`**, so `new AudioContext({latencyHint:'interactive'})`
   there is just the default.

Spec definitions (<https://www.w3.org/TR/webaudio/>): `"interactive"` = *"Provide the lowest audio
output latency possible without glitching. This is the default."*; `baseLatency` = processing latency
from `AudioDestinationNode` to the audio subsystem; `outputLatency` = *"the interval between the time
the UA requests the host system to play a buffer and the time at which the first sample in the buffer
is actually processed by the audio output device."* The configurable render quantum is
`AudioContextRenderSizeCategory` with only two values, `"default"` (128 frames) and `"hardware"` —
not an arbitrary size — and the spec notes it *"exposes information about the host and can be used
for fingerprinting."*

**Device labels.** Chromium exposes `audiooutput` IDs and labels only after a camera/microphone
permission grant, which is why the page primes the mic. Firefox 116 changed `enumerateDevices()` to
enumerate output devices as well and has the permission-free `selectAudioOutput()` picker; BCD also
records a Firefox gotcha: *"The promise returned by `enumerateDevices()` only resolves when the
document has focus"* — a real hang risk for the 0.25 Hz background poll. There is no output selection
at all on Chrome Android or Firefox Android.

**Cross-origin isolation.** `SharedArrayBuffer` needs a secure context plus COOP `same-origin` +
COEP `require-corp`, which `server.rs:369-380` sets on the document. All sub-resources are
same-origin so `require-corp` does not block them. The page's own probe
(`crossOriginIsolated === true && SharedArrayBuffer && Atomics`, `audio.js:23-27`) is the correct and
sufficient test — there is no need to reason about whether a click-through self-signed certificate
"counts", because `crossOriginIsolated` answers that question directly at runtime, per engine, with
no version sniffing. That is the pattern to keep.

### 2.10 Input, screen and session

| API | Chrome/Edge | Chrome Android | Firefox | Safari | Safari iOS |
|---|---|---|---|---|---|
| `requestPointerLock` | 37 | **144** | 50 | 10.1 | **never** |
| `…({unadjustedMovement:true})` | **88** (macOS 10.15.1+, Windows, ChromeOS; **not Linux**) | 144 | 152 | **18.4** | never |
| `navigator.keyboard.lock()` | 68 | 68 | no | no | no |
| `getCoalescedEvents()` | 58 | 58 | 59 (**Android: always returns `[]`**) | **18.2** | 18.2 |
| `getPredictedEvents()` | 77 | 77 | 89 | 18.2 | 18.2 |
| `pressure` / `tiltX` / `tiltY` | 55 | 55 | 59 | 13 | 13 |
| `altitudeAngle` / `azimuthAngle` | 86 | 86 | 131 | 18.2 | 18.2 |
| `Element.requestFullscreen` | 71 | 71 | 64 | 16.4 | **16.4 iPad only — never iPhone**; non-suppressible overlay button; swipe-down exits |
| `Clipboard.readText` | 66 (`clipboard-read` permission) | 66 | 125 (gesture + paste prompt) | 13.1 | 13.1 |
| `ScreenOrientation.lock` | 38 but **always throws `NotSupportedError` on desktop** | 38 | 144 | no | no |
| Screen Wake Lock | 84 | 84 | 126 | 16.4 | **18.4** full; 16.4-18.4 partial — *"Does not work in standalone Home Screen Web Apps"* |
| `visualViewport` | 61 | 61 | 91 | 13 | 13 |
| `WebTransport` | 97 | 97 | 114 | **26.4** | 26.4 |

(BCD `api/Element.json`, `api/PointerEvent.json`, `api/Navigator.json`, `api/ScreenOrientation.json`,
`api/VisualViewport.json`, `api/WebTransport.json`, `api/WakeLock.json`, `api/Clipboard.json`.)

`unadjustedMovement: true` is the important one: it delivers raw device deltas with the OS pointer
curve removed, so the host applies acceleration once instead of the client OS and the host both
applying one. When unsupported, `requestPointerLock({unadjustedMovement:true})` **rejects** (the
promise form landed in Chrome 92 and Safari 18.4), so the pattern is try-with-options, catch, retry
bare.

`nosleep.js` is still load-bearing: below Safari iOS 18.4 (and in standalone home-screen web apps
even up to 18.4) and below Firefox 126, `navigator.wakeLock` is absent or partial.

### 2.11 Old-WebKit parse safety

Parse-time syntax is the JS analogue of the macOS dyld rule: one unsupported token kills the whole
file, not one call. BCD floors:

| syntax | Chrome | Firefox | Safari (⇒ iOS) |
|---|---|---|---|
| optional chaining `?.` | 80 | 74 | **13.1** (iOS 13.4) |
| nullish coalescing `??` | 80 | 72 | **13.1** (iOS 13.4) |
| logical assignment `\|\|= &&= ??=` | 85 | 79 | **14** |
| public class instance fields | 72 | 69 | 14.1 |
| private class fields `#x` | 74 | 90 | 14.1 |
| top-level `await` | 89 | 89 | 15 |
| optional catch binding `catch {}` | 66 | 58 | 11.1 |

None of these appear in `static/` today; the one parse-time hazard is `1000n` at
`audio-worker.js:137`, safe only because that file is never loaded without `AudioDecoder`.

The pattern for shipping modern code paths without breaking old parsers is the one the page already
half-uses: **keep every modern-syntax path in its own file, loaded only after a runtime probe**, and
when the probe itself needs new syntax, wrap it in `new Function('…')` inside a `try` — `new
Function` defers parsing to call time, turning a document-killing `SyntaxError` into a catchable
exception. The undocumented invariant that `audio-worker.js` is BigInt-gated should be written down
as a comment at the top of that file, and the legacy-webview checker
(`scripts/check-legacy-webkit.mjs`, which today only covers `dist/`) extended to lint `static/`.

---

## 3. How others do it

- **libwebrtc** rewrites the SPS VUI on both directions —
  `common_video/h264/sps_vui_rewriter.cc` on receive
  (`video_rtp_depacketizer_h264.cc:126`) and on send
  (`video/frame_encode_metadata_writer.cc`). This is the reference answer to the reorder problem and
  the reason ScreenExtend does not currently suffer it.
- **OpenH264** (Chrome's and Firefox's own WebRTC H.264 encoder) writes the restriction
  unconditionally: `bitstream_restriction_flag = 1`, `max_num_reorder_frames = 0`,
  `max_dec_frame_buffering = iNumRefFrames` — `codec/encoder/core/src/au_set.cpp:236,245-246`, with
  `bVuiParamPresentFlag = true` at `:546`
  (<https://github.com/cisco/openh264/blob/master/codec/encoder/core/src/au_set.cpp>).
- **x264** writes it too:
  `sps->vui.b_bitstream_restriction = !(sps->b_constraint_set3 && sps->i_profile_idc >= PROFILE_HIGH)`
  with `i_num_reorder_frames = param->i_bframe_pyramid ? 2 : param->i_bframe ? 1 : 0`
  — `encoder/set.c:147,234`
  (<https://code.videolan.org/videolan/x264/-/blob/master/encoder/set.c>). With `i_bframe = 0` that
  is zero reorder frames.
- **FFmpeg's NVENC wrapper** had exactly ScreenExtend's gap and fixed it:
  `vui->bitstreamRestrictionFlag = cc->gopLength != 1 || avctx->profile < AV_PROFILE_H264_HIGH;`
  in `libavcodec/nvenc.c`, commit `d724584d68` — *"avcodec/nvenc: set bitstreamRestrictionFlag when
  neccesary. This mimics the behaviour of libx264… Fixes #11131."*
- **Sunshine** (the LizardByte Moonlight host) sets it explicitly, in the same VUI block ScreenExtend
  copied its colour fields from:
  ```cpp
  vui.videoSignalTypePresentFlag = 1; … vui.colourMatrix = colorspace.matrix;
  vui.chromaSampleLocationFlag = …;
  vui.bitstreamRestrictionFlag = 1;               // src/nvenc/nvenc_base.cpp:322
  ```
  plus `frameIntervalP = 1`, `rcParams.zeroReorderDelay = 1`, `repeatSPSPPS = 1`, and `sliceMode = 3`
  with `slicesPerFrame` slices
  (<https://github.com/LizardByte/Sunshine/blob/master/src/nvenc/nvenc_base.cpp>).
- **oneVPL** documents the same intent at API level: *"The application can adjust
  `mfxExtCodingOption::MaxDecFrameBuffering` during encoding initialization to improve decoding
  latency. It is recommended to set this value equal to the number of reference frames"*, combined
  with `GopRefDist = 1`. ScreenExtend already does both (`intel/encoder.rs:411,434`).
- **Meta's `webcodecs-capture-play`** (WebCodecs + WebTransport) publishes its latency methodology —
  `Latency = Now − renderedSampleGeneratedWallClock` — and a `videoMaxMaxQueueSizeMs: 100` jitter
  default, but no decode-latency figures.

The only published WebCodecs decode numbers found were Bernard Aboba's on the
[WebCodecs Encode/Decode Worker sample](https://webrtc.internaut.com/wc/wcWorker3/): Chrome Canary
130, MacBook Air M2, H.265 @ 500 kbps, "realtime" mode (`optimizeForLatency: true`) — **encode < 10 ms,
decode < 2 ms** ([#698](https://github.com/w3c/webcodecs/issues/698)). Rigorous
WebCodecs-vs-`<video>` comparisons do not appear to exist publicly.

---

## 4. What ScreenExtend does today

**Video** (`src-tauri/src/streamer/static/`):

- `index.html:166-169` — capability probe and transport choice:
  ```js
  const useScriptTransform = hasScriptTransform && !hasLegacyEncodedStreams;
  ```
  Chrome 141+ has **both**, so this takes the legacy `createEncodedStreams` branch
  (`index.html:865-871`), never assigns `transformer` in the worker
  (`transform-worker.js:189-197`), and `requestKey()` becomes a permanent no-op
  (`transform-worker.js:178-185`). Safari and Firefox take the ScriptTransform branch and do have
  keyframe requests.
- `transform-worker.js:141-154` — `isConfigSupported({ codec: c })` with **only** a codec string: no
  `codedWidth`/`codedHeight`, no `optimizeForLatency`, no `hardwareAcceleration`. The configuration
  probed is not the configuration used (`:157`).
- `transform-worker.js:157` — `decoder.configure({ codec, optimizeForLatency: true })`. No
  `description` (correct, §2.3), no `hardwareAcceleration` (spec default `"no-preference"`, correct
  per §2.2).
- `transform-worker.js:24-28,43-46` — codec candidates are all `avc1.` and the SDP hint builds
  `'avc1.' + plid`, so the Safari `avc3.` trap is avoided.
- `transform-worker.js:42` — `getContext('2d', { alpha:false, desynchronized:true })` on an
  **OffscreenCanvas**; per §2.7 the `desynchronized` flag is inert there.
- `transform-worker.js:107-127` — paint inside the decoder output callback, no rAF, `frame.close()`
  in a `finally`. Correct structure. `:116` uses the 5-argument `drawImage` destination form, so a
  scale is applied even when source and destination sizes match.
- `transform-worker.js:229-243` — backlog policy is `decodeQueueSize > max(3, fps*0.05)` twice ⇒
  latch `waitingForKey` and demand a keyframe. Per §2.6 `decodeQueueSize` is a poor signal, the
  threshold of 3 is far above the ≤1 target, and there is no `ondequeue` handler.
- `index.html:831-833`, `:873-875` — `playoutDelayHint = 0` and `jitterBufferTarget = 0` on the
  WebCodecs receiver; per §2.4 both are inert there. `:922-923` sets `30` / `0.03` on the `<video>`
  path, where they do apply.

**Audio** (`audio.js`, `audio-worker.js`, `audio-worklet.js`):

- `audio.js:13-27` — correct probes for `AudioDecoder`/`EncodedAudioChunk`/`AudioData`,
  `audioWorklet`, and `crossOriginIsolated && SharedArrayBuffer && Atomics`.
- `audio.js:469` — `new AC({ latencyHint: 'interactive', sampleRate: 48000 })`.
- `audio.js:418-424` — `outputLatencyMs() = (baseLatency + outputLatency) * 1000`, both defaulting
  to 0 when absent (Safari < 18.4 has no `outputLatency`).
- `audio.js:426-451` — the A/V target: `videoDelay − preEnqueue − outputLatency`, wrapped modulo the
  RTP rollover, clamped to [10, 400] ms.
- `audio-worklet.js:125-190` — adaptive jitter buffer: hard-drop above `target + 250 ms`,
  silence-only drift correction, +3 ms on underrun, −1 ms per 750 stable blocks (≈2 s). Sound design;
  no change recommended.
- `audio.js:104` — `selectAudioOutput` is probed into `speakers.hasSelectOut` and **never read**;
  §2.9 says it is the only permission-free output picker and exists on Firefox 116+ only.
- `audio.js:118-149` — the microphone prime, which gates the join path for up to 12 s.

**Input and session:**

- `index.html:531-540` `acquirePointerLock()` — `el.requestPointerLock()` with **no options**, so no
  `unadjustedMovement`.
- `index.html:526-529` `acquireKeyboardLock()` — `navigator.keyboard.lock()` with no key list (locks
  everything, which is what a second-screen client wants). Chromium only.
- `input.js:299-303,333` — `getCoalescedEvents()` used **only for pen**; mouse and touch moves are
  decimated to one sample per task by the `MessageChannel` flush (`input.js:286-298`).
- `input.js:411` — `'onpointerrawupdate' in window` ⇒ `pointerrawupdate`, else `pointermove`.
  Chromium-only feature, correctly probed.
- `index.html:498-505` `enterFullscreen()` — four prefixes plus `video.webkitEnterFullscreen`, which
  per §2.10 is the only thing that works at all on iPhone; `#stage` fullscreen is impossible there.

**Host encoders** (the other end of §2.1):

- `windows_utils/streamer/nvidia/encoder.rs:632-640` sets seven VUI fields and **not**
  `bitstreamRestrictionFlag`, which exists in the binding at `nvenc_sys/nvEncodeAPI.rs:517` and is
  documented in the NVENC SDK header as *"If set to 1, it specifies the bitstream restriction
  parameters are present in the bitstream."*
- `windows_utils/streamer/intel/encoder.rs:411,414,434` — `GopRefDist = 1`, `NumRefFrame = 1`,
  `MaxDecFrameBuffering = 1`: the oneVPL-documented recipe, probably already correct.
- `windows_utils/streamer/x264/encoder.rs:71` — `i_bframe = 0` with the `zerolatency` preset:
  correct by construction (§3).
- `macos_utils/streamer/encoder.rs:159,215` — `AllowFrameReordering = false`,
  `MaxFrameDelayCount = 1`. Whether VideoToolbox then emits `bitstream_restriction_flag` is
  **unverified**; [w3c/webcodecs#899](https://github.com/w3c/webcodecs/issues/899) says it often
  does not.

---

## 5. Recommendations

Ranked by expected latency delta ÷ implementation cost. Each has a `findings/` file.

### R1 — Prefer `RTCRtpScriptTransform` wherever it exists (one line)
`findings/major-client-prefer-scripttransform-over-legacy-encoded-streams.md`

`index.html:169` becomes `const useScriptTransform = hasScriptTransform;`. On Chrome/Edge 141+ this
turns on `sendKeyFrameRequest()` (§2.5), which is dead today on the majority client. Keep the legacy
branch for Chrome 94-140.

**Expected win:** recovery from a decode error or backlog event goes from ≈4 s of frozen picture
followed by a *permanent* demotion to `<video>` (a step change in steady-state latency, not a
gradient) to ≈1 RTT with no demotion. Because the demotion is one-way per session
(`usingFallback`, `index.html:736`), one transient loss currently costs the whole session its fast
path. **Guard it:** if `new RTCRtpScriptTransform(worker, {kind:'video'})` throws, or no
`'transformstart'` with `hasReadable` arrives within ~1 s, fall back to the legacy branch *before*
falling back to `<video>`.

### R2 — Drive the decoder from `ondequeue`, target one outstanding output
`findings/major-client-decodequeuesize-is-the-wrong-backpressure-signal.md`

Replace the `decodeQueueSize > max(3, fps*0.05)` heuristic (`transform-worker.js:229-243`) with the
outstanding-output pattern from §2.6: count frames submitted minus frames delivered to the `output`
callback, hold at 1, and use `decoder.ondequeue` (Chrome 106+, Firefox 130+, Safari 16.4+) rather
than polling. Keep a `decodeQueueSize` read only as a *diagnostic*.

**Expected win:** up to (threshold − 1) frame intervals of queued work removed — 33 ms at 60 fps for
the current threshold of 3 — plus it stops a transient burst from latching `waitingForKey` and, on
Chrome today, freezing the picture (§R1). **Cost:** ~20 lines in one file.

### R3 — Probe the configuration you are actually going to use
`findings/major-client-isconfigsupported-probes-a-different-config.md`

`transform-worker.js:141-154` should call `isConfigSupported` with the full config — `codec`,
`codedWidth`, `codedHeight`, `optimizeForLatency: true` — and, when probing acceleration, in the
order `no-preference` → `prefer-hardware`, **never `prefer-hardware` alone**: per §2.2 Chromium
installs a GPU-only factory for it, so `configure()` throws on any machine without a compatible
hardware decoder and the page demotes straight to `<video>`. Report the accepted config to the host.

**Expected win:** eliminates a class of "probe says yes, configure throws" demotions; supplies the
data `speculative-client-decoder-no-hardware-acceleration-hint.md` asks for. **Cost:** small.

### R4 — Feed `unadjustedMovement: true` to pointer lock
`findings/major-client-pointerlock-missing-unadjustedmovement.md`

```js
var p = el.requestPointerLock({ unadjustedMovement: true });
if (p && p.catch) p.catch(function () { el.requestPointerLock(); });
```
Chrome 88+ (Windows, macOS 10.15.1+, ChromeOS; **not Linux**), Safari 18.4+, Firefox 152+, Chrome
Android 144+. **Expected win:** removes double pointer acceleration (client OS curve × host curve),
the usual cause of "the remote mouse feels wrong". No latency change, large feel change.
**Cost:** two lines at `index.html:531-540`.

### R5 — Use `getCoalescedEvents()` for touch and mouse, not only pen
`findings/major-client-coalesced-events-only-used-for-pen.md`

`input.js:299-303` already has `coalescedOf()`; `:317-327` uses it only on the pen branch, so
high-rate touch and 1000 Hz mice are decimated to one sample per task. Safari 18.2+ now has the API;
Firefox Android returns `[]` and the existing `|| [e]` fallback covers it.

**Expected win:** correct stroke shape and velocity for host-side inertia, with no extra round trips
— the samples ride the existing batch message (`input.js:119-141`, `batchBuf` 970 B).
**Precondition:** `major-input-batch-replayed-one-syscall-per-sample.md` must land first, or the host
turns each extra sample into a syscall and this makes things worse.

### R6 — Stop gating the join on the microphone; use `selectAudioOutput` where it exists
Extends `findings/major-client-mic-prompt-gates-the-stream.md`; new
`findings/minor-client-selectaudiooutput-never-used.md`

`speakers.hasSelectOut` (`audio.js:104`) is probed and never read. On Firefox 116+
`navigator.mediaDevices.selectAudioOutput()` yields a labelled device with **no microphone
permission**. On Chromium there is no such API, so the mic prime is genuinely required for a device
*list* — but it must run *after* first frame, never before ICE. Also handle the Firefox caveat that
`enumerateDevices()` only resolves while the document has focus, which can hang the 0.25 Hz
re-enumeration loop indefinitely in a background tab.

**Expected win:** up to 12 s off the join path (`index.html:1397-1399`), plus one fewer permission
prompt on Firefox.

### R7 — Substitute a real output latency when `AudioContext.outputLatency` is missing
`findings/minor-client-outputlatency-missing-skews-av-sync-on-safari.md`

On Safari < 18.4, `outputLatency` is `undefined`, `audio.js:418-424` contributes 0, and the commanded
worklet depth is too large by the true output latency, so audio lands late. Estimate it instead:
`getOutputTimestamp()` (Safari 14.1+) returns `{contextTime, performanceTime}`, and
`currentTime − contextTime` sampled at a known wall time is a usable proxy; failing that use a
conservative per-platform constant, and record which was used so it shows up in telemetry.

**Expected win:** 10-40 ms of A/V misalignment removed on every Safari below 18.4 — i.e. most iPhones
and iPads.

### R8 — Belt-and-braces: set the SPS VUI restriction on the host anyway
`findings/minor-nvenc-sps-vui-missing-bitstream-restriction.md`

libwebrtc's receive-side rewriter makes this a non-issue *on the RTP transport* (§2.1), so this is
**not** a latency fix today — it is insurance:

- `vui.bitstreamRestrictionFlag = 1;` in `nvidia/encoder.rs:632-640`, exactly as Sunshine and
  FFmpeg's NVENC wrapper do (§3). NVENC derives the values from `frameIntervalP = 1` /
  `maxNumRefFrames = 1`.
- Verify Intel and VideoToolbox emit it on the wire.
- x264 already correct.

It becomes a **blocker** the day video leaves RTP — the DataChannel video path in
`speculative-webrtc-hevc-needs-a-datachannel-video-path.md`, or any WebTransport experiment — because
nothing else rewrites the SPS on those paths. Add the check now, while the cost is one line.

**Related and more urgent:** `major-webrtc-sps-pps-silently-dropped-when-stap-a-exceeds-mtu.md` is
worse than it reads. When the depacketizer cannot parse an SPS it returns `std::nullopt` and the
packet is **discarded** (`video_rtp_depacketizer_h264.cc:130-132`) — so a too-large STAP-A does not
merely delay the keyframe, it destroys it.

### R9 — Measure before changing the presentation path
`findings/speculative-client-desynchronized-is-inert-on-offscreencanvas.md`

`desynchronized: true` at `transform-worker.js:42` does nothing (§2.7). The only way to reach the
front-buffer path is a **main-thread** `<canvas>`, which costs a `VideoFrame` transfer to the page
and a main-thread `drawImage` per frame, and is only real on Chromium/Windows and ChromeOS. Treat as
an A/B behind a `?lowlatency=1` flag, not a change. Meanwhile either drop the flag or comment it as
inert so nobody assumes it is doing work.

### R10 — Small, free corrections in the same files

- `transform-worker.js:116` — use the 3-argument `drawImage(frame, 0, 0)` when
  `frame.displayWidth === canvas.width`, so no scale is applied.
- `index.html:831-833`, `:873-875` — comment that `playoutDelayHint`/`jitterBufferTarget` are inert
  on the WebCodecs path (§2.4). Do **not** delete them; they are correct at `:922-923`.
- `index.html:1210-1266` `checkBrowserRequirements()` — split "WebCodecs audio and AudioWorklet" into
  a distinct **audio** entry, so a Safari 16.4-25 user learns their browser version is the reason
  audio is buffered, rather than reading one merged line as a general browser fault.
- `transform-worker.js:24-28` — add a comment that only `avc1.` strings are safe (Safari rejects
  `avc3.`, §2.3), so a future "use the SDP string verbatim" change does not break Safari.
- `audio-worker.js:1` — document the BigInt invariant (`1000n` at `:137` makes this file
  unparseable on pre-BigInt engines; it is only ever loaded behind an `AudioDecoder` probe), and
  extend `scripts/check-legacy-webkit.mjs` to lint `src-tauri/src/streamer/static/`.

### The tier table the client should select by probing

| tier | probe | video | audio | who lands here |
|---|---|---|---|---|
| **A** | `'transform' in RTCRtpReceiver.prototype` && `VideoDecoder` && `transferControlToOffscreen` | ScriptTransform → WebCodecs → worker OffscreenCanvas, **keyframe requests available** | `AudioDecoder` && `audioWorklet` && `crossOriginIsolated` ⇒ SAB ring | Chrome/Edge ≥141, Safari ≥26, Firefox ≥130 desktop |
| **B** | `createEncodedStreams` && `VideoDecoder` && `transferControlToOffscreen` | same, **no keyframe request** | as A | Chrome/Edge 94-140 |
| **A′/B′** | tier A or B video but no `AudioDecoder` | as above | Opus track (browser NetEQ) | **Safari 16.4-25, macOS and iOS** |
| **C** | `AudioDecoder` && `audioWorklet` but `crossOriginIsolated === false` | as above | postMessage ring | plain HTTP, Android WebView, in-app browsers |
| **D** | none of the above | `<video>` + `jitterBufferTarget = 30` | Opus track | Firefox Android, iOS < 16.4, 2018-era WebKit |

Every row is already probed correctly today, with two exceptions: tier A/B selection is inverted
(R1), and tier A′/B′ is not distinguished in the user-facing compatibility report (R10).

---

## 6. Verification plan

**Measurable on the Windows dev box (11 26200 + RTX 3080 Ti):**

1. **Confirm the VUI rewrite is happening.** In the transform worker, parse the first keyframe's SPS
   and log `vui_parameters_present_flag`, `bitstream_restriction_flag`, `max_num_reorder_frames`,
   `max_dec_frame_buffering`, `level_idc`. Expected: `1 / 1 / 0 / 1`. If it is not, §2.1's whole
   analysis inverts and R8 becomes R1 — so run this **first**, before any code change. Also dump the
   SPS host-side for comparison; the two differing is the proof that libwebrtc rewrote it.
2. **Decoder hold.** Record `performance.now()` at each `decoder.decode()` keyed by `timestamp`, and
   again in the `output` callback. Median difference = decoder hold. Expected ≈1 frame interval given
   (1). Repeat with `--encoder software` as a control.
3. **Keyframe recovery (R1).** Join from Chrome, confirm `useScriptTransform === false` today.
   Throttle the link to provoke `[webcodecs] decoder backlog`; today the canvas freezes and
   `falling back to <video>` appears ~4 s later. After R1 the host log should show
   `RTCP PLI received, requesting IDR` (`webrtc_session.rs:476`) within ~250 ms and the canvas should
   recover with no demotion. Measure: time-to-recover after an injected loss burst, and `<video>`
   demotions per hour.
4. **Queue depth (R2).** Log the outstanding-output count and `decodeQueueSize` percentiles
   before/after; the paint-to-paint interval should not regress while the submitted-to-output delay
   drops.
5. **Glass-to-glass.** Needs the host↔client clock offset that
   `major-client-no-glass-to-glass-telemetry.md` asks for — the `PING`/`PONG` RTT is already computed
   at `input.js:530` and thrown away. Until that exists, use a 240 fps phone camera on host + client
   screens showing a rolling counter.
6. **A/V offset (R7).** `SEAudio.getSyncInfo()` already returns `videoDelayMs`, `preEnqueueMs`,
   `targetMs`, `depthMs`, `underruns`, `overruns` (`audio.js:594-608`) and has no caller. Log it
   every 2 s under `?debug` and compare before/after.
7. **Input (R4, R5).** Count `pointerrawupdate` samples per second before/after R5 and compare with
   the host's injected-sample count. For R4, move the mouse a fixed physical distance with OS
   acceleration on and off and compare host cursor travel.

**Cannot be verified on the dev boxes:**

- Anything on Safari — the macOS 10.15.8 box caps at Safari 15.6, below the 16.4 WebCodecs floor. The
  whole A′/B′ tier, `outputLatency` ≥18.4, `unadjustedMovement` ≥18.4, `getCoalescedEvents` ≥18.2 and
  `AudioDecoder` 26 all need a borrowed modern Mac/iPhone or a BrowserStack-class service.
- Whether VideoToolbox emits `bitstream_restriction_flag` (needs a macOS host capture).
- Chrome Android pointer lock (144+) and the Android `setSinkId` absence.
- Firefox Android's empty `getCoalescedEvents()` and absent WebCodecs.
- Any front-buffer `desynchronized` behaviour, which needs ChromeOS or a Windows box with the
  low-latency swapchain actually engaged.

---

## 7. Open questions

1. **Does the SPS rewrite actually fire on our stream?** Verification step 1 settles it. There is one
   plausible way it does not: if the host's SPS never reaches the depacketizer as a parseable NAL —
   which is exactly the STAP-A-over-MTU case. Worth confirming both together.
2. **Should the host pin the H.264 level?** `nvidia/encoder.rs` never sets `h264.level`, so NVENC
   picks. If the rewrite ever fails, the hold is `MaxDpbMbs(level)/MBs`, so the level is the severity
   multiplier. Pinning the lowest level that fits bounds the damage for free. It also interacts with
   `major-h264-profile-hardcoded-baseline-never-negotiated.md`.
3. **Is the `<video>` fallback tier still worth its current tuning?** Once R1 restores keyframe
   recovery, demotion should become rare. The `jitterBufferTarget = 30` / `playoutDelayHint = 0.03`
   at `index.html:922-923` were presumably chosen for a path that was being entered constantly, and
   deserve re-tuning downward once it is not.
4. **`renderQuantumSize` / `renderSizeHint`** is Chrome 153 desktop only and not yet in BCD as an
   `AudioContextOptions` member. If it delivers a hardware-sized render quantum it would shave the
   AudioWorklet's fixed 2.67 ms. Probe-only for now.
5. **WebTransport** is now Chrome 97 / Firefox 114 / Safari 26.4 — plausible one day for the *audio*
   and *input* channels (unreliable datagrams without SCTP head-of-line behaviour), but it needs an
   HTTP/3 server on the host and does nothing for video, which is already on RTP. Parked. Note that
   any such path loses the libwebrtc SPS rewrite, which is R8's whole argument.
6. **`AudioContext.playbackStats`** (Chrome 146) exposes glitch and underrun counters the worklet
   currently has to infer. Worth adding to telemetry when available.
7. **Telemetry is a prerequisite for everything here.** `major-client-no-glass-to-glass-telemetry.md`
   and §15 item 3 of `docs/discovery/B-client-page.md` both say so; this report is a third vote. Every
   recommendation above is measurable only if the client can report to the host, and the RTT needed
   to make the numbers comparable is already computed and discarded at `input.js:530`.
